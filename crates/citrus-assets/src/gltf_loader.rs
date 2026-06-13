//! glTF 2.0 import into a [`Scene`].

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context as _, Result, bail};
use citrus_render::{AlphaMode, MaterialFeatures, MaterialParams, MeshData, Vertex};
use glam::Mat4;

use crate::{Instance, Scene, SceneMaterial};

pub fn load_gltf(path: impl AsRef<Path>) -> Result<Scene> {
    let path = path.as_ref();
    let (doc, buffers, images) =
        gltf::import(path).with_context(|| format!("importing glTF file {}", path.display()))?;

    let mut scene = Scene {
        meshes: Vec::new(),
        textures: Vec::new(),
        materials: Vec::new(),
        instances: Vec::new(),
    };
    // (image index, srgb) -> scene texture index. The same image can be
    // legally referenced as both color and data; cache per usage.
    let mut texture_cache: HashMap<(usize, bool), usize> = HashMap::new();

    let mut get_texture = |scene: &mut Scene, image_index: usize, srgb: bool| -> Result<usize> {
        if let Some(&index) = texture_cache.get(&(image_index, srgb)) {
            return Ok(index);
        }
        let data = rgba8_from_gltf(&images[image_index], srgb)?;
        scene.textures.push(data);
        let index = scene.textures.len() - 1;
        texture_cache.insert((image_index, srgb), index);
        Ok(index)
    };

    // Materials. Index i = glTF material i; one extra default at the end.
    for material in doc.materials() {
        let pbr = material.pbr_metallic_roughness();

        let albedo = pbr
            .base_color_texture()
            .map(|t| get_texture(&mut scene, t.texture().source().index(), true))
            .transpose()?;
        let orm_info = pbr.metallic_roughness_texture();
        let orm = orm_info
            .as_ref()
            .map(|t| get_texture(&mut scene, t.texture().source().index(), false))
            .transpose()?;
        let normal_info = material.normal_texture();
        let normal = normal_info
            .as_ref()
            .map(|t| get_texture(&mut scene, t.texture().source().index(), false))
            .transpose()?;
        let emission_tex = material
            .emissive_texture()
            .map(|t| get_texture(&mut scene, t.texture().source().index(), true))
            .transpose()?;

        // Occlusion is only honored when packed into the same image as
        // metallic-roughness (the common ORM layout); a separate occlusion
        // texture would need its own slot.
        let occlusion_strength = match (material.occlusion_texture(), &orm_info) {
            (Some(occ), Some(mr))
                if occ.texture().source().index() == mr.texture().source().index() =>
            {
                occ.strength()
            }
            _ => 0.0,
        };

        let emissive_factor = material.emissive_factor();
        let emissive_strength = material.emissive_strength().unwrap_or(1.0);
        let has_emission = emission_tex.is_some() || emissive_factor.iter().any(|&c| c > 0.0);

        let (alpha_mode, alpha_cutoff) = match material.alpha_mode() {
            gltf::material::AlphaMode::Opaque => (AlphaMode::Opaque, 0.5),
            gltf::material::AlphaMode::Mask => {
                (AlphaMode::Cutout, material.alpha_cutoff().unwrap_or(0.5))
            }
            gltf::material::AlphaMode::Blend => (AlphaMode::Blend, 0.5),
        };

        scene.materials.push(SceneMaterial {
            name: material
                .name()
                .map(str::to_owned)
                .unwrap_or_else(|| format!("material {}", material.index().unwrap_or(0))),
            params: MaterialParams {
                base_color: pbr.base_color_factor(),
                emission_color: emissive_factor,
                emission_intensity: emissive_strength,
                metallic: pbr.metallic_factor(),
                roughness: pbr.roughness_factor(),
                alpha_cutoff,
                normal_strength: normal_info.as_ref().map_or(1.0, |n| n.scale()),
                occlusion_strength,
                pbr_toon_blend: 0.0, // glTF imports as pure PBR; toon is opt-in
                ..MaterialParams::default()
            },
            features: MaterialFeatures {
                toon: false,
                normal_map: normal.is_some(),
                emission: has_emission,
                alpha_mode,
                double_sided: material.double_sided(),
            },
            albedo,
            normal,
            orm,
            emission: emission_tex,
        });
    }
    // Fallback for primitives without a material.
    scene.materials.push(SceneMaterial {
        name: "default".into(),
        params: MaterialParams::default(),
        features: MaterialFeatures::default(),
        albedo: None,
        normal: None,
        orm: None,
        emission: None,
    });
    let default_material = scene.materials.len() - 1;

    // Meshes: each glTF primitive becomes one MeshData.
    // (gltf mesh index, primitive index) -> (scene mesh, scene material)
    let mut primitive_map: HashMap<(usize, usize), (usize, usize)> = HashMap::new();
    for mesh in doc.meshes() {
        for primitive in mesh.primitives() {
            if primitive.mode() != gltf::mesh::Mode::Triangles {
                tracing::warn!(
                    mesh = mesh.name().unwrap_or("?"),
                    "skipping non-triangle primitive"
                );
                continue;
            }
            let reader = primitive.reader(|b| Some(&buffers[b.index()]));
            let Some(positions) = reader.read_positions() else {
                continue;
            };
            let positions: Vec<[f32; 3]> = positions.collect();
            let normals: Vec<[f32; 3]> = reader
                .read_normals()
                .map(|n| n.collect())
                .unwrap_or_else(|| vec![[0.0, 1.0, 0.0]; positions.len()]);
            let uvs: Vec<[f32; 2]> = reader
                .read_tex_coords(0)
                .map(|t| t.into_f32().collect())
                .unwrap_or_else(|| vec![[0.0, 0.0]; positions.len()]);
            // Second UV set = lightmap UVs; fall back to the primary set when
            // the model has no TEXCOORD_1.
            let uvs1: Vec<[f32; 2]> = reader
                .read_tex_coords(1)
                .map(|t| t.into_f32().collect())
                .unwrap_or_else(|| uvs.clone());
            let colors: Vec<[f32; 4]> = reader
                .read_colors(0)
                .map(|c| c.into_rgba_f32().collect())
                .unwrap_or_else(|| vec![[1.0; 4]; positions.len()]);
            let tangents: Vec<[f32; 4]> = reader
                .read_tangents()
                .map(|t| t.collect())
                .unwrap_or_else(|| vec![[1.0, 0.0, 0.0, 1.0]; positions.len()]);

            let vertices: Vec<Vertex> = (0..positions.len())
                .map(|i| Vertex {
                    position: positions[i],
                    normal: normals[i],
                    uv: uvs[i],
                    color: colors[i],
                    tangent: tangents[i],
                    uv1: uvs1[i],
                })
                .collect();
            let indices: Vec<u32> = reader
                .read_indices()
                .map(|i| i.into_u32().collect())
                .unwrap_or_else(|| (0..vertices.len() as u32).collect());

            scene.meshes.push(MeshData { vertices, indices });
            let material = primitive.material().index().unwrap_or(default_material);
            primitive_map.insert(
                (mesh.index(), primitive.index()),
                (scene.meshes.len() - 1, material),
            );
        }
    }

    // Instances from the node hierarchy.
    let gltf_scene = doc
        .default_scene()
        .or_else(|| doc.scenes().next())
        .context("glTF file contains no scenes")?;
    for node in gltf_scene.nodes() {
        visit_node(&node, Mat4::IDENTITY, &primitive_map, &mut scene.instances);
    }

    if scene.instances.is_empty() {
        bail!("glTF scene produced no renderable instances");
    }
    tracing::info!(
        meshes = scene.meshes.len(),
        materials = scene.materials.len(),
        textures = scene.textures.len(),
        instances = scene.instances.len(),
        "glTF scene loaded"
    );
    Ok(scene)
}

fn visit_node(
    node: &gltf::Node<'_>,
    parent: Mat4,
    primitive_map: &HashMap<(usize, usize), (usize, usize)>,
    instances: &mut Vec<Instance>,
) {
    let transform = parent * Mat4::from_cols_array_2d(&node.transform().matrix());
    if let Some(mesh) = node.mesh() {
        let base_name = node
            .name()
            .or_else(|| mesh.name())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("node {}", node.index()));
        let multi = mesh.primitives().len() > 1;
        for primitive in mesh.primitives() {
            if let Some(&(mesh_index, material)) =
                primitive_map.get(&(mesh.index(), primitive.index()))
            {
                let name = if multi {
                    format!("{base_name} [{}]", primitive.index())
                } else {
                    base_name.clone()
                };
                instances.push(Instance {
                    name,
                    mesh: mesh_index,
                    material,
                    transform,
                });
            }
        }
    }
    for child in node.children() {
        visit_node(&child, transform, primitive_map, instances);
    }
}

fn rgba8_from_gltf(data: &gltf::image::Data, srgb: bool) -> Result<citrus_render::TextureData> {
    use gltf::image::Format;
    let pixel_count = (data.width * data.height) as usize;
    let pixels = match data.format {
        Format::R8G8B8A8 => data.pixels.clone(),
        Format::R8G8B8 => {
            let mut out = Vec::with_capacity(pixel_count * 4);
            for chunk in data.pixels.chunks_exact(3) {
                out.extend_from_slice(chunk);
                out.push(255);
            }
            out
        }
        Format::R8 => {
            let mut out = Vec::with_capacity(pixel_count * 4);
            for &v in &data.pixels {
                out.extend_from_slice(&[v, v, v, 255]);
            }
            out
        }
        Format::R8G8 => {
            let mut out = Vec::with_capacity(pixel_count * 4);
            for chunk in data.pixels.chunks_exact(2) {
                out.extend_from_slice(&[chunk[0], chunk[1], 0, 255]);
            }
            out
        }
        other => bail!("unsupported glTF image format {other:?} (16-bit formats land in M3)"),
    };
    Ok(citrus_render::TextureData {
        width: data.width,
        height: data.height,
        pixels,
        srgb,
    })
}
