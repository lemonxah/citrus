//! Built-in test scene: generated meshes + materials exercising the
//! standard shader's phase-1 feature set. Used when no glTF path is given.

use citrus_render::{AlphaMode, MaterialFeatures, MaterialParams, MeshData, TextureData, Vertex};
use glam::{Mat4, Quat, Vec3};

use crate::scene_file::PrimitiveShape;
use crate::{Instance, Scene, SceneMaterial};

/// Generate the mesh for a creatable primitive shape.
pub fn primitive_mesh(shape: PrimitiveShape) -> MeshData {
    match shape {
        PrimitiveShape::Cube => cube(1.0),
        PrimitiveShape::Sphere => uv_sphere(0.5, 48, 24),
        PrimitiveShape::Capsule => capsule(0.25, 1.0, 32, 8),
        PrimitiveShape::Plane => plane(2.0, 1.0),
    }
}

/// Build a `MeshData`, using the primary (clean, non-overlapping) UVs as the
/// lightmap UV set too. Primitives need no separate unwrap.
fn mesh_with_lightmap(mut vertices: Vec<Vertex>, indices: Vec<u32>) -> MeshData {
    for v in &mut vertices {
        v.uv1 = v.uv;
    }
    MeshData { vertices, indices }
}

/// Capsule: cylinder of height `height` between two hemispheres of `radius`,
/// total height = height + 2*radius, centered at the origin.
fn capsule(radius: f32, height: f32, segments: u32, rings: u32) -> MeshData {
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    let half = height * 0.5;
    let total_rings = rings * 2 + 1; // top hemi, equator band, bottom hemi

    for ring in 0..=total_rings {
        // Map ring index to a latitude angle plus a cylinder offset.
        let (theta, y_offset) = if ring <= rings {
            // Top hemisphere: theta 0..PI/2
            (
                ring as f32 / rings as f32 * std::f32::consts::FRAC_PI_2,
                half,
            )
        } else {
            // Bottom hemisphere: theta PI/2..PI
            (
                std::f32::consts::FRAC_PI_2
                    + (ring - rings - 1) as f32 / rings as f32 * std::f32::consts::FRAC_PI_2,
                -half,
            )
        };
        let (sin_t, cos_t) = theta.sin_cos();
        for segment in 0..=segments {
            let u = segment as f32 / segments as f32;
            let phi = u * std::f32::consts::TAU;
            let (sin_p, cos_p) = phi.sin_cos();
            let normal = [sin_t * cos_p, cos_t, sin_t * sin_p];
            vertices.push(Vertex {
                position: [
                    normal[0] * radius,
                    normal[1] * radius + y_offset,
                    normal[2] * radius,
                ],
                normal,
                uv: [u, ring as f32 / total_rings as f32],
                tangent: [-sin_p, 0.0, cos_p, 1.0],
                ..Default::default()
            });
        }
    }
    let stride = segments + 1;
    for ring in 0..total_rings {
        for segment in 0..segments {
            let a = ring * stride + segment;
            let b = a + stride;
            indices.extend_from_slice(&[a, a + 1, b, b, a + 1, b + 1]);
        }
    }
    mesh_with_lightmap(vertices, indices)
}

pub fn test_scene() -> Scene {
    let meshes = vec![uv_sphere(0.5, 48, 24), cube(1.0), plane(10.0, 4.0)];
    const SPHERE: usize = 0;
    const CUBE: usize = 1;
    const PLANE: usize = 2;

    let textures = vec![checker_texture(256, 32)];
    const CHECKER: usize = 0;

    let materials = vec![
        SceneMaterial {
            name: "Toon Sphere".into(),
            params: MaterialParams {
                base_color: [0.45, 0.6, 1.0, 1.0],
                roughness: 0.6,
                toon_steps: 3.0,
                pbr_toon_blend: 1.0,
                ..Default::default()
            },
            features: MaterialFeatures {
                toon: true,
                ..Default::default()
            },
            albedo: None,
            normal: None,
            orm: None,
            emission: None,
        },
        SceneMaterial {
            name: "Brushed Metal".into(),
            params: MaterialParams {
                base_color: [0.95, 0.93, 0.88, 1.0],
                metallic: 1.0,
                roughness: 0.3,
                ..Default::default()
            },
            features: MaterialFeatures::default(),
            albedo: None,
            normal: None,
            orm: None,
            emission: None,
        },
        SceneMaterial {
            name: "Emissive Core".into(),
            params: MaterialParams {
                base_color: [0.05, 0.05, 0.08, 1.0],
                roughness: 0.9,
                emission_color: [0.1, 0.9, 0.8],
                emission_intensity: 4.0,
                ..Default::default()
            },
            features: MaterialFeatures {
                emission: true,
                ..Default::default()
            },
            albedo: None,
            normal: None,
            orm: None,
            emission: None,
        },
        SceneMaterial {
            name: "Checker Floor".into(),
            params: MaterialParams {
                roughness: 0.85,
                ..Default::default()
            },
            features: MaterialFeatures::default(),
            albedo: Some(CHECKER),
            normal: None,
            orm: None,
            emission: None,
        },
        SceneMaterial {
            name: "Glass Panel".into(),
            params: MaterialParams {
                base_color: [0.5, 0.75, 0.9, 0.35],
                roughness: 0.1,
                ..Default::default()
            },
            features: MaterialFeatures {
                alpha_mode: AlphaMode::Blend,
                double_sided: true,
                ..Default::default()
            },
            albedo: None,
            normal: None,
            orm: None,
            emission: None,
        },
    ];

    let instances = vec![
        Instance::single("Floor", PLANE, 3, Mat4::IDENTITY),
        Instance::single(
            "Toon Sphere",
            SPHERE,
            0,
            Mat4::from_translation(Vec3::new(-1.3, 0.5, 0.0)),
        ),
        Instance::single(
            "Metal Cube",
            CUBE,
            1,
            Mat4::from_rotation_translation(Quat::from_rotation_y(0.6), Vec3::new(1.3, 0.5, 0.0)),
        ),
        Instance::single(
            "Emissive Core",
            SPHERE,
            2,
            Mat4::from_scale_rotation_translation(
                Vec3::splat(0.6),
                Quat::IDENTITY,
                Vec3::new(0.0, 0.3, 1.4),
            ),
        ),
        Instance::single(
            "Glass Panel",
            CUBE,
            4,
            Mat4::from_scale_rotation_translation(
                Vec3::new(1.6, 1.0, 0.05),
                Quat::from_rotation_y(-0.4),
                Vec3::new(0.0, 0.6, -1.6),
            ),
        ),
    ];

    Scene {
        meshes,
        textures,
        materials,
        instances,
        skeletons: Vec::new(),
        animations: Vec::new(),
    }
}

fn checker_texture(size: u32, cell: u32) -> TextureData {
    let mut pixels = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let even = ((x / cell) + (y / cell)).is_multiple_of(2);
            let v = if even { 200 } else { 90 };
            pixels.extend_from_slice(&[v, v, v, 255]);
        }
    }
    TextureData {
        width: size,
        height: size,
        pixels,
        srgb: true,
        hdr: false,
    }
}

fn plane(size: f32, uv_tiles: f32) -> MeshData {
    let h = size * 0.5;
    let vertices = vec![
        Vertex {
            position: [-h, 0.0, -h],
            uv: [0.0, 0.0],
            ..Default::default()
        },
        Vertex {
            position: [h, 0.0, -h],
            uv: [uv_tiles, 0.0],
            ..Default::default()
        },
        Vertex {
            position: [h, 0.0, h],
            uv: [uv_tiles, uv_tiles],
            ..Default::default()
        },
        Vertex {
            position: [-h, 0.0, h],
            uv: [0.0, uv_tiles],
            ..Default::default()
        },
    ];
    mesh_with_lightmap(vertices, vec![0, 2, 1, 0, 3, 2])
}

fn cube(size: f32) -> MeshData {
    let h = size * 0.5;
    // (normal, tangent, four corners)
    type CubeFace = ([f32; 3], [f32; 4], [[f32; 3]; 4]);
    let faces: [CubeFace; 6] = [
        (
            [0.0, 0.0, 1.0],
            [1.0, 0.0, 0.0, 1.0],
            [[-h, -h, h], [h, -h, h], [h, h, h], [-h, h, h]],
        ),
        (
            [0.0, 0.0, -1.0],
            [-1.0, 0.0, 0.0, 1.0],
            [[h, -h, -h], [-h, -h, -h], [-h, h, -h], [h, h, -h]],
        ),
        (
            [1.0, 0.0, 0.0],
            [0.0, 0.0, -1.0, 1.0],
            [[h, -h, h], [h, -h, -h], [h, h, -h], [h, h, h]],
        ),
        (
            [-1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 1.0],
            [[-h, -h, -h], [-h, -h, h], [-h, h, h], [-h, h, -h]],
        ),
        (
            [0.0, 1.0, 0.0],
            [1.0, 0.0, 0.0, 1.0],
            [[-h, h, h], [h, h, h], [h, h, -h], [-h, h, -h]],
        ),
        (
            [0.0, -1.0, 0.0],
            [1.0, 0.0, 0.0, 1.0],
            [[-h, -h, -h], [h, -h, -h], [h, -h, h], [-h, -h, h]],
        ),
    ];
    let mut vertices = Vec::with_capacity(24);
    let mut indices = Vec::with_capacity(36);
    // uv0 is per-face 0..1 (texture mapping, tiles/overlaps across faces); uv1
    // is a non-overlapping lightmap atlas: the 6 faces packed into a 3x2 grid,
    // each face inset by a gutter so the bake/bilinear filter don't bleed
    // between faces. (uv0 alone can't be a lightmap: all faces would share the
    // same 0..1 region and fight over the same texels.)
    const M: f32 = 0.06; // per-cell inset (gutter): bigger = wider inter-face gap
                         // so bilinear filtering doesn't bleed between faces.
    for (f, (normal, tangent, corners)) in faces.into_iter().enumerate() {
        let base = vertices.len() as u32;
        let col = (f % 3) as f32;
        let row = (f / 3) as f32;
        for (i, position) in corners.into_iter().enumerate() {
            let uv = [[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]][i];
            let uv1 = [
                (col + M + uv[0] * (1.0 - 2.0 * M)) / 3.0,
                (row + M + uv[1] * (1.0 - 2.0 * M)) / 2.0,
            ];
            vertices.push(Vertex {
                position,
                normal,
                uv,
                uv1,
                tangent,
                ..Default::default()
            });
        }
        indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    MeshData { vertices, indices }
}

fn uv_sphere(radius: f32, segments: u32, rings: u32) -> MeshData {
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    for ring in 0..=rings {
        let v = ring as f32 / rings as f32;
        let theta = v * std::f32::consts::PI;
        let (sin_t, cos_t) = theta.sin_cos();
        for segment in 0..=segments {
            let u = segment as f32 / segments as f32;
            let phi = u * std::f32::consts::TAU;
            let (sin_p, cos_p) = phi.sin_cos();
            let normal = [sin_t * cos_p, cos_t, sin_t * sin_p];
            vertices.push(Vertex {
                position: [normal[0] * radius, normal[1] * radius, normal[2] * radius],
                normal,
                uv: [u, v],
                // d(position)/du: tangent along longitude.
                tangent: [-sin_p, 0.0, cos_p, 1.0],
                ..Default::default()
            });
        }
    }
    let stride = segments + 1;
    for ring in 0..rings {
        for segment in 0..segments {
            let a = ring * stride + segment;
            let b = a + stride;
            indices.extend_from_slice(&[a, a + 1, b, b, a + 1, b + 1]);
        }
    }
    mesh_with_lightmap(vertices, indices)
}
