//! FBX import via ufbx.
//!
//! Geometry is converted to y-up meters by ufbx itself; meshes are split per
//! material part (mirroring the glTF loader's one-mesh-per-primitive model),
//! and PBR factors plus base color / normal / emission textures are mapped
//! onto the standard shader. Roughness/metalness *textures* are not packed
//! into ORM yet (factors only) — tracked in TODO.md.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context as _, Result, anyhow, bail};
use glam::Mat4;
use citrus_render::{
    AlphaMode, MaterialFeatures, MaterialParams, MeshData, TextureData, Vertex,
};

use crate::{Instance, Scene, SceneMaterial};

pub fn load_fbx(path: impl AsRef<Path>) -> Result<Scene> {
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
    let fbx = ufbx::load_file(
        path.to_str().context("non-UTF8 path")?,
        opts,
    )
    .map_err(|e| anyhow!("loading FBX {}: {}", path.display(), e.description.as_ref()))?;

    let base_dir = path.parent().unwrap_or(Path::new("."));
    let mut scene = Scene {
        meshes: Vec::new(),
        textures: Vec::new(),
        materials: Vec::new(),
        instances: Vec::new(),
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
        let transform = matrix_to_mat4(&node.geometry_to_world);
        let node_name = if node.element.name.is_empty() {
            format!("node {}", node.element.element_id)
        } else {
            node.element.name.to_string()
        };

        // Parts: one per material, or the whole mesh when unpartitioned.
        let part_count = mesh.material_parts.len().max(1);
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
            let name = if part_count > 1 {
                format!("{node_name} [{part_index}]")
            } else {
                node_name.clone()
            };
            scene.instances.push(Instance {
                name,
                mesh: entry.0,
                material: entry.1,
                transform,
            });
        }
    }

    if scene.instances.is_empty() {
        bail!("FBX file {} produced no renderable meshes", path.display());
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

/// Convert one material part of a ufbx mesh into MeshData (un-indexed:
/// one vertex per triangle corner; index dedup is a later optimization).
fn convert_part(mesh: &ufbx::Mesh, part_index: usize) -> Result<Option<MeshData>> {
    let mut vertices: Vec<Vertex> = Vec::new();
    let mut tri_indices = vec![0u32; mesh.max_face_triangles.max(1) * 3];

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
            vertices.push(Vertex {
                position: [p.x as f32, p.y as f32, p.z as f32],
                normal: n,
                uv,
                color,
                tangent,
            });
        }
    }

    if vertices.is_empty() {
        return Ok(None);
    }
    let indices = (0..vertices.len() as u32).collect();
    Ok(Some(MeshData { vertices, indices }))
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
