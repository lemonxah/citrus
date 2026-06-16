//! FBX import via ufbx.
//!
//! Geometry is converted to y-up meters by ufbx itself; meshes are split per
//! material part (mirroring the glTF loader's one-mesh-per-primitive model),
//! and PBR factors plus base color / normal / emission textures are mapped
//! onto the standard shader. Roughness/metalness *textures* are not packed
//! into ORM yet (factors only); tracked in TODO.md.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context as _, Result, anyhow, bail};
use citrus_render::{AlphaMode, MaterialFeatures, MaterialParams, MeshData, TextureData, Vertex};
use glam::{Mat4, Quat, Vec3};

use crate::skeleton::{AnimChannel, AnimationClip, ChannelPath, Joint, Skeleton};
use crate::{Instance, MeshSlot, ModelImport, Scene, SceneMaterial};

/// Import an FBX with default settings.
pub fn load_fbx(path: impl AsRef<Path>) -> Result<Scene> {
    load_fbx_with(path, &ModelImport::default())
}

/// Import an FBX, applying the asset's [`ModelImport`] settings (scale, flip-UV,
/// import-materials) from its `.meta` sidecar.
pub fn load_fbx_with(path: impl AsRef<Path>, settings: &ModelImport) -> Result<Scene> {
    let path = path.as_ref();
    let opts = ufbx::LoadOpts {
        target_axes: ufbx::CoordinateAxes::right_handed_y_up(),
        target_unit_meters: 1.0,
        space_conversion: ufbx::SpaceConversion::TransformRoot,
        generate_missing_normals: true,
        load_external_files: true,
        ignore_missing_external_files: true,
        ..Default::default()
    };
    let fbx = ufbx::load_file(path.to_str().context("non-UTF8 path")?, opts)
        .map_err(|e| anyhow!("loading FBX {}: {}", path.display(), e.description.as_ref()))?;

    let base_dir = path.parent().unwrap_or(Path::new("."));
    let mut scene = Scene {
        meshes: Vec::new(),
        textures: Vec::new(),
        materials: Vec::new(),
        instances: Vec::new(),
        skeletons: Vec::new(),
        animations: Vec::new(),
    };

    // Default material for parts without one.
    scene.materials.push(SceneMaterial {
        name: "default".into(),
        params: MaterialParams::default(),
        features: MaterialFeatures::default(),
        albedo: None,
        normal: None,
        orm: None,
        emission: None,
    });

    // element_id -> scene material index
    let mut material_cache: HashMap<u32, usize> = HashMap::new();
    // (texture element_id, srgb) -> scene texture index
    let mut texture_cache: HashMap<(u32, bool), usize> = HashMap::new();
    // (mesh element_id, part index) -> (scene mesh index, scene material index)
    let mut part_cache: HashMap<(u32, usize), (usize, usize)> = HashMap::new();

    for node in &fbx.nodes {
        let Some(mesh) = &node.mesh else { continue };
        if mesh.num_triangles == 0 {
            continue;
        }
        // Build the armature + animation clips from the first skinned mesh's
        // deformer (cluster order matches the skin-local joint indices on the
        // vertices); sample every FBX anim stack onto the bone nodes.
        if scene.skeletons.is_empty() {
            if let Some((skel, clips)) = build_rig(&fbx, mesh) {
                scene.skeletons.push(skel);
                scene.animations.extend(clips);
            }
        }
        let transform = matrix_to_mat4(&node.geometry_to_world);
        let node_name = if node.element.name.is_empty() {
            format!("node {}", node.element.element_id)
        } else {
            node.element.name.to_string()
        };

        // Parts: one per material, or the whole mesh when unpartitioned. All
        // parts of a node become one instance with a material slot per part.
        let part_count = mesh.material_parts.len().max(1);
        let mut slots: Vec<MeshSlot> = Vec::new();
        for part_index in 0..part_count {
            let key = (mesh.element.element_id, part_index);
            let entry = match part_cache.get(&key) {
                Some(&entry) => entry,
                None => {
                    let Some(mesh_data) = convert_part(mesh, part_index)? else {
                        continue;
                    };
                    scene.meshes.push(mesh_data);
                    let mesh_index = scene.meshes.len() - 1;

                    let material_index = mesh
                        .materials
                        .get(part_index)
                        .map(|material| {
                            material_for(
                                material,
                                base_dir,
                                &mut scene,
                                &mut material_cache,
                                &mut texture_cache,
                            )
                        })
                        .unwrap_or(0);
                    part_cache.insert(key, (mesh_index, material_index));
                    (mesh_index, material_index)
                }
            };
            slots.push(MeshSlot {
                mesh: entry.0,
                material: entry.1,
            });
        }
        if !slots.is_empty() {
            scene.instances.push(Instance {
                name: node_name,
                transform,
                slots,
            });
        }
    }

    if scene.instances.is_empty() {
        bail!("FBX file {} produced no renderable meshes", path.display());
    }

    // Apply per-asset import settings (.meta) to the converted scene.
    if settings.scale != 1.0 || settings.flip_uv {
        for mesh in &mut scene.meshes {
            for v in &mut mesh.vertices {
                if settings.scale != 1.0 {
                    v.position[0] *= settings.scale;
                    v.position[1] *= settings.scale;
                    v.position[2] *= settings.scale;
                }
                if settings.flip_uv {
                    v.uv[1] = 1.0 - v.uv[1];
                }
            }
        }
    }
    if !settings.import_materials {
        // Strip imported materials: keep geometry, fall back to plain defaults.
        for m in &mut scene.materials {
            m.params = MaterialParams::default();
            m.features = MaterialFeatures::default();
            m.albedo = None;
            m.normal = None;
            m.orm = None;
            m.emission = None;
        }
    }

    tracing::info!(
        meshes = scene.meshes.len(),
        materials = scene.materials.len(),
        textures = scene.textures.len(),
        instances = scene.instances.len(),
        "FBX scene loaded"
    );
    Ok(scene)
}

/// Skin weights for one triangle corner: up to 4 (joint = skin-cluster index,
/// weight) from the mesh's first skin deformer, keyed by control-point index.
/// Returns zeros for unrigged meshes. Cluster indices line up with the skeleton
/// the importer builds from the same deformer's cluster order.
fn skin_corner(mesh: &ufbx::Mesh, corner: usize) -> ([u32; 4], [f32; 4]) {
    let mut joints = [0u32; 4];
    let mut weights = [0.0f32; 4];
    let Some(skin) = (&mesh.skin_deformers).into_iter().next() else {
        return (joints, weights);
    };
    if corner >= mesh.vertex_indices.count {
        return (joints, weights);
    }
    let cp = mesh.vertex_indices[corner] as usize;
    if cp >= skin.vertices.count {
        return (joints, weights);
    }
    let sv = skin.vertices[cp];
    let begin = sv.weight_begin as usize;
    let n = (sv.num_weights as usize).min(4);
    for k in 0..n {
        let w = skin.weights[begin + k];
        joints[k] = w.cluster_index;
        weights[k] = w.weight as f32;
    }
    let sum: f32 = weights.iter().sum();
    if sum > 1e-6 {
        for w in &mut weights {
            *w /= sum;
        }
    }
    (joints, weights)
}

fn ufbx_v3(v: ufbx::Vec3) -> Vec3 {
    Vec3::new(v.x as f32, v.y as f32, v.z as f32)
}
fn ufbx_quat(q: ufbx::Quat) -> Quat {
    Quat::from_xyzw(q.x as f32, q.y as f32, q.z as f32, q.w as f32)
}

/// Build a [`Skeleton`] + animation clips from a mesh's first skin deformer.
/// Joints are emitted in cluster order (matching the skin-local `cluster_index`
/// stored on vertices); each cluster's `geometry_to_bone` is the inverse-bind,
/// parents come from the bone nodes' FBX parent chain, and every FBX anim stack
/// is sampled (30 fps) onto the bone nodes into per-joint TRS channels.
fn build_rig<'a>(
    fbx: &'a ufbx::Scene,
    mesh: &'a ufbx::Mesh,
) -> Option<(Skeleton, Vec<AnimationClip>)> {
    let skin = (&mesh.skin_deformers).into_iter().next()?;
    if skin.clusters.count == 0 {
        return None;
    }
    let mut id_to_joint: HashMap<u32, usize> = HashMap::new();
    for (i, cluster) in (&skin.clusters).into_iter().enumerate() {
        if let Some(node) = &cluster.bone_node {
            id_to_joint.insert(node.element.element_id, i);
        }
    }
    let mut joints = Vec::new();
    let mut bone_nodes: Vec<Option<&ufbx::Node>> = Vec::new();
    for cluster in &skin.clusters {
        let Some(node) = &cluster.bone_node else {
            joints.push(Joint {
                name: "joint".into(),
                parent: None,
                inverse_bind: Mat4::IDENTITY,
                rest_translation: Vec3::ZERO,
                rest_rotation: Quat::IDENTITY,
                rest_scale: Vec3::ONE,
            });
            bone_nodes.push(None);
            continue;
        };
        let parent = node
            .parent
            .as_ref()
            .and_then(|p| id_to_joint.get(&p.element.element_id).copied());
        let t = node.local_transform;
        joints.push(Joint {
            name: if node.element.name.is_empty() {
                format!("joint {}", node.element.element_id)
            } else {
                node.element.name.to_string()
            },
            parent,
            inverse_bind: matrix_to_mat4(&cluster.geometry_to_bone),
            rest_translation: ufbx_v3(t.translation),
            rest_rotation: ufbx_quat(t.rotation),
            rest_scale: ufbx_v3(t.scale),
        });
        bone_nodes.push(Some(node.as_ref()));
    }
    let skeleton = Skeleton { joints };

    // Sample each anim stack at 30 fps over its time range into dense TRS channels.
    const FPS: f64 = 30.0;
    let mut clips = Vec::new();
    for stack in &fbx.anim_stacks {
        let anim = stack.anim.as_ref();
        let duration = (stack.time_end - stack.time_begin).max(0.0);
        if duration <= 1e-4 {
            continue;
        }
        let frames = (duration * FPS).ceil() as usize + 1;
        let times: Vec<f32> = (0..frames).map(|f| f as f32 / FPS as f32).collect();
        let mut channels = Vec::new();
        for (j, node) in bone_nodes.iter().enumerate() {
            let Some(node) = node else { continue };
            let mut trans = Vec::with_capacity(frames);
            let mut rots = Vec::with_capacity(frames);
            let mut scales = Vec::with_capacity(frames);
            for f in 0..frames {
                let t = stack.time_begin + f as f64 / FPS;
                let tr = node.evaluate_transform(anim, t);
                trans.push(ufbx_v3(tr.translation));
                rots.push(ufbx_quat(tr.rotation));
                scales.push(ufbx_v3(tr.scale));
            }
            channels.push(AnimChannel {
                joint: j,
                path: ChannelPath::Translation,
                times: times.clone(),
                vec_values: trans,
                quat_values: Vec::new(),
            });
            channels.push(AnimChannel {
                joint: j,
                path: ChannelPath::Rotation,
                times: times.clone(),
                vec_values: Vec::new(),
                quat_values: rots,
            });
            channels.push(AnimChannel {
                joint: j,
                path: ChannelPath::Scale,
                times: times.clone(),
                vec_values: scales,
                quat_values: Vec::new(),
            });
        }
        clips.push(AnimationClip {
            name: if stack.element.name.is_empty() {
                "clip".into()
            } else {
                stack.element.name.to_string()
            },
            duration: duration as f32,
            channels,
        });
    }
    Some((skeleton, clips))
}

/// Convert one material part of a ufbx mesh into MeshData (un-indexed:
/// one vertex per triangle corner; index dedup is a later optimization).
fn convert_part(mesh: &ufbx::Mesh, part_index: usize) -> Result<Option<MeshData>> {
    let mut vertices: Vec<Vertex> = Vec::new();
    let mut tri_indices = vec![0u32; mesh.max_face_triangles.max(1) * 3];
    // Second UV set = lightmap UVs, if the mesh has one.
    let uv_set1 = mesh.uv_sets.get(1).filter(|s| s.vertex_uv.exists);

    let face_indices: Vec<u32> = match mesh.material_parts.get(part_index) {
        Some(part) => part.face_indices.as_ref().to_vec(),
        None => (0..mesh.faces.len() as u32).collect(),
    };

    for &face_index in &face_indices {
        let face = mesh.faces[face_index as usize];
        let num_tris = ufbx::triangulate_face(&mut tri_indices, mesh, face);
        for &corner in &tri_indices[..num_tris as usize * 3] {
            let i = corner as usize;
            let p = mesh.vertex_position[i];
            let n = if mesh.vertex_normal.exists {
                let n = mesh.vertex_normal[i];
                [n.x as f32, n.y as f32, n.z as f32]
            } else {
                [0.0, 1.0, 0.0]
            };
            let uv = if mesh.vertex_uv.exists {
                let uv = mesh.vertex_uv[i];
                // FBX uses bottom-left UV origin; textures are top-left.
                [uv.x as f32, 1.0 - uv.y as f32]
            } else {
                [0.0, 0.0]
            };
            let color = if mesh.vertex_color.exists {
                let c = mesh.vertex_color[i];
                [c.x as f32, c.y as f32, c.z as f32, c.w as f32]
            } else {
                [1.0; 4]
            };
            let tangent = if mesh.vertex_tangent.exists {
                let t = mesh.vertex_tangent[i];
                [t.x as f32, t.y as f32, t.z as f32, 1.0]
            } else {
                [1.0, 0.0, 0.0, 1.0]
            };
            let uv1 = match uv_set1 {
                Some(set) => {
                    let uv = set.vertex_uv[i];
                    [uv.x as f32, 1.0 - uv.y as f32]
                }
                None => [0.0, 0.0],
            };
            // Skin weights are resolved in a second pass (skin_for_part) keyed by
            // control-point index, since ufbx weights are per control point.
            let (joints, weights) = skin_corner(mesh, i);
            vertices.push(Vertex {
                position: [p.x as f32, p.y as f32, p.z as f32],
                normal: n,
                uv,
                color,
                tangent,
                uv1,
                joints,
                weights,
            });
        }
    }

    if vertices.is_empty() {
        return Ok(None);
    }
    // No authored lightmap UVs: generate a simple per-triangle atlas (the mesh
    // is already un-indexed, so each triangle is 3 consecutive vertices).
    if uv_set1.is_none() {
        generate_lightmap_grid(&mut vertices);
    }
    let indices = (0..vertices.len() as u32).collect();
    Ok(Some(MeshData { vertices, indices }))
}

/// Pack each triangle into its own cell of a square grid in the [0,1] UV
/// square, writing the result to `uv1`. Non-overlapping (valid for lightmap
/// baking) but seam-heavy; a real chart-based unwrap (xatlas) is a follow-up.
fn generate_lightmap_grid(vertices: &mut [Vertex]) {
    let tri_count = vertices.len() / 3;
    if tri_count == 0 {
        return;
    }
    let cols = (tri_count as f32).sqrt().ceil().max(1.0) as usize;
    let cell = 1.0 / cols as f32;
    let margin = cell * 0.08;
    let lo = cell - 2.0 * margin;
    for t in 0..tri_count {
        let cx = (t % cols) as f32 * cell + margin;
        let cy = (t / cols) as f32 * cell + margin;
        // A right triangle filling the cell (with a small margin gutter).
        let corners = [[cx, cy], [cx + lo, cy], [cx, cy + lo]];
        for (k, corner) in corners.iter().enumerate() {
            vertices[t * 3 + k].uv1 = *corner;
        }
    }
}

fn material_for(
    material: &ufbx::Material,
    base_dir: &Path,
    scene: &mut Scene,
    material_cache: &mut HashMap<u32, usize>,
    texture_cache: &mut HashMap<(u32, bool), usize>,
) -> usize {
    let id = material.element.element_id;
    if let Some(&index) = material_cache.get(&id) {
        return index;
    }

    let pbr = &material.pbr;
    let vec4 = |map: &ufbx::MaterialMap, fallback: [f32; 4]| {
        if map.has_value {
            [
                map.value_vec4.x as f32,
                map.value_vec4.y as f32,
                map.value_vec4.z as f32,
                map.value_vec4.w as f32,
            ]
        } else {
            fallback
        }
    };
    let real = |map: &ufbx::MaterialMap, fallback: f32| {
        if map.has_value {
            map.value_vec4.x as f32
        } else {
            fallback
        }
    };

    let mut load_tex = |scene: &mut Scene, map: &ufbx::MaterialMap, srgb: bool| -> Option<usize> {
        let texture = map.texture.as_ref()?;
        let key = (texture.element.element_id, srgb);
        if let Some(&index) = texture_cache.get(&key) {
            return Some(index);
        }
        match texture_data(texture, base_dir, srgb) {
            Ok(data) => {
                scene.textures.push(data);
                let index = scene.textures.len() - 1;
                texture_cache.insert(key, index);
                Some(index)
            }
            Err(e) => {
                tracing::warn!("skipping FBX texture: {e:#}");
                None
            }
        }
    };

    let albedo = load_tex(scene, &pbr.base_color, true);
    let normal = load_tex(scene, &pbr.normal_map, false);
    let emission_tex = load_tex(scene, &pbr.emission_color, true);

    let base = vec4(&pbr.base_color, [1.0; 4]);
    let base_factor = real(&pbr.base_factor, 1.0);
    let emission_color = vec4(&pbr.emission_color, [0.0; 4]);
    let emission_factor = real(&pbr.emission_factor, 1.0);
    let opacity = real(&pbr.opacity, 1.0);
    let has_emission = emission_tex.is_some()
        || (emission_factor > 0.0 && emission_color[..3].iter().any(|&c| c > 0.0));

    scene.materials.push(SceneMaterial {
        name: if material.element.name.is_empty() {
            format!("material {id}")
        } else {
            material.element.name.to_string()
        },
        params: MaterialParams {
            base_color: [
                base[0] * base_factor,
                base[1] * base_factor,
                base[2] * base_factor,
                opacity,
            ],
            metallic: real(&pbr.metalness, 0.0),
            roughness: real(&pbr.roughness, 0.7),
            emission_color: [emission_color[0], emission_color[1], emission_color[2]],
            emission_intensity: emission_factor,
            pbr_toon_blend: 0.0,
            ..MaterialParams::default()
        },
        features: MaterialFeatures {
            toon: false,
            normal_map: normal.is_some(),
            emission: has_emission,
            alpha_mode: if opacity < 1.0 {
                AlphaMode::Blend
            } else {
                AlphaMode::Opaque
            },
            double_sided: material.features.double_sided.enabled,
        },
        albedo,
        normal,
        orm: None,
        emission: emission_tex,
    });
    let index = scene.materials.len() - 1;
    material_cache.insert(id, index);
    index
}

fn texture_data(texture: &ufbx::Texture, base_dir: &Path, srgb: bool) -> Result<TextureData> {
    let img = if !texture.content.is_empty() {
        image::load_from_memory(&texture.content).context("decoding embedded FBX texture")?
    } else {
        let mut candidates = Vec::new();
        if !texture.absolute_filename.is_empty() {
            candidates.push(std::path::PathBuf::from(texture.absolute_filename.as_ref()));
        }
        if !texture.relative_filename.is_empty() {
            candidates.push(base_dir.join(texture.relative_filename.as_ref()));
        }
        if !texture.filename.is_empty() {
            candidates.push(base_dir.join(texture.filename.as_ref()));
        }
        let path = candidates
            .into_iter()
            .find(|p| p.exists())
            .context("FBX texture file not found on disk")?;
        image::open(&path).with_context(|| format!("loading texture {}", path.display()))?
    };
    let img = img.into_rgba8();
    Ok(TextureData {
        width: img.width(),
        height: img.height(),
        pixels: img.into_raw(),
        srgb,
        hdr: false,
    })
}

/// ufbx matrices are column-major 4x3 (rotation+translation).
fn matrix_to_mat4(m: &ufbx::Matrix) -> Mat4 {
    Mat4::from_cols_array(&[
        m.m00 as f32,
        m.m10 as f32,
        m.m20 as f32,
        0.0,
        m.m01 as f32,
        m.m11 as f32,
        m.m21 as f32,
        0.0,
        m.m02 as f32,
        m.m12 as f32,
        m.m22 as f32,
        0.0,
        m.m03 as f32,
        m.m13 as f32,
        m.m23 as f32,
        1.0,
    ])
}
