//! Scene state: objects with TRS + provenance, GPU upload, material
//! management (imported, file-based, and edited), picking, and .scene
//! save/load.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use glam::{Mat4, Quat, Vec3};
use citrus_editor::{AlphaModeModel, MaterialModel};
use citrus_render::{
    AlphaMode, DrawCmd, MaterialDesc, MaterialFeatures, MaterialHandle, MaterialParams,
    MeshHandle, Renderer, TextureHandle,
};
use citrus_assets::{MaterialRef, ObjectSource, SceneEntry, SceneFile};

/// Render data for mesh objects; empties and cameras have none.
#[derive(Clone, Copy)]
pub struct RenderInfo {
    /// Index into scene-local mesh arrays.
    pub mesh: usize,
    /// Index into `materials`.
    pub material: usize,
}

pub struct SceneObject {
    pub name: String,
    pub render: Option<RenderInfo>,
    pub source: ObjectSource,
    /// Parent object index; transform is local to it.
    pub parent: Option<usize>,
    pub translation: Vec3,
    pub rotation: Quat,
    pub scale: Vec3,
}

impl SceneObject {
    pub fn local_transform(&self) -> Mat4 {
        Mat4::from_scale_rotation_translation(self.scale, self.rotation, self.translation)
    }

    pub fn kind_label(&self) -> &'static str {
        match self.source {
            ObjectSource::Empty => "Empty",
            ObjectSource::Camera => "Camera",
            ObjectSource::Primitive { .. } => "Primitive",
            _ => "Mesh",
        }
    }
}

pub struct MaterialEntry {
    pub model: MaterialModel,
    pub default: MaterialModel,
    pub handle: MaterialHandle,
    /// Set when this material came from (or was saved to) a `.material` file.
    pub file: Option<PathBuf>,
}

#[derive(Clone, Copy)]
pub struct MeshInfo {
    pub vertices: u32,
    pub triangles: u32,
}

pub struct LoadedScene {
    /// Rebuilt every frame from renderable objects.
    pub draws: Vec<DrawCmd>,
    pub objects: Vec<SceneObject>,
    pub materials: Vec<MaterialEntry>,
    mesh_handles: Vec<MeshHandle>,
    mesh_infos: Vec<MeshInfo>,
    mesh_bounds: Vec<(Vec3, Vec3)>,
    primitive_meshes: HashMap<citrus_assets::PrimitiveShape, usize>,
    default_material: Option<usize>,
    /// model path -> base index of its meshes in the scene arrays
    /// (a model's primitives are appended contiguously).
    model_mesh_base: HashMap<PathBuf, usize>,
    material_file_cache: HashMap<PathBuf, usize>,
    texture_file_cache: HashMap<(PathBuf, bool), TextureHandle>,
}

impl LoadedScene {
    pub fn empty() -> Self {
        Self {
            draws: Vec::new(),
            objects: Vec::new(),
            materials: Vec::new(),
            mesh_handles: Vec::new(),
            mesh_infos: Vec::new(),
            mesh_bounds: Vec::new(),
            primitive_meshes: HashMap::new(),
            default_material: None,
            model_mesh_base: HashMap::new(),
            material_file_cache: HashMap::new(),
            texture_file_cache: HashMap::new(),
        }
    }

    pub fn mesh_info(&self, mesh: usize) -> MeshInfo {
        self.mesh_infos[mesh]
    }

    /// Center of a mesh's AABB in object space (the "Center" pivot).
    pub fn mesh_center_local(&self, mesh: usize) -> Vec3 {
        let (min, max) = self.mesh_bounds[mesh];
        (min + max) * 0.5
    }

    /// World transform of an object (walks the parent chain).
    pub fn world_transform(&self, index: usize) -> Mat4 {
        let mut m = self.objects[index].local_transform();
        let mut i = index;
        let mut guard = 0;
        while let Some(p) = self.objects[i].parent {
            if guard > 64 || p >= self.objects.len() {
                break;
            }
            m = self.objects[p].local_transform() * m;
            i = p;
            guard += 1;
        }
        m
    }

    /// World-space bounds of an object: (center, radius). Used for F-focus.
    pub fn object_bounds(&self, index: usize) -> (Vec3, f32) {
        let object = &self.objects[index];
        let world = self.world_transform(index);
        match &object.render {
            Some(r) => {
                let (min, max) = self.mesh_bounds[r.mesh];
                let center = world.transform_point3((min + max) * 0.5);
                let scale = Vec3::new(
                    world.x_axis.length(),
                    world.y_axis.length(),
                    world.z_axis.length(),
                );
                let radius = ((max - min) * 0.5 * scale).length();
                (center, radius.max(0.05))
            }
            None => (world.w_axis.truncate(), 0.5),
        }
    }

    /// Re-parent `child`, preserving its world transform. Rejects cycles.
    pub fn set_parent(&mut self, child: usize, parent: Option<usize>) {
        if let Some(p) = parent {
            if p == child || p >= self.objects.len() {
                return;
            }
            // Reject if `child` is an ancestor of `p`.
            let mut i = p;
            let mut guard = 0;
            while let Some(pp) = self.objects[i].parent {
                if pp == child {
                    return;
                }
                i = pp;
                guard += 1;
                if guard > 64 {
                    return;
                }
            }
        }
        let world = self.world_transform(child);
        self.objects[child].parent = parent;
        let parent_world = parent.map_or(Mat4::IDENTITY, |p| self.world_transform(p));
        let local = parent_world.inverse() * world;
        let (scale, rotation, translation) = local.to_scale_rotation_translation();
        let o = &mut self.objects[child];
        o.translation = translation;
        o.rotation = rotation;
        o.scale = scale;
    }

    fn ensure_default_material(&mut self, renderer: &mut Renderer) -> Result<usize> {
        if let Some(i) = self.default_material {
            return Ok(i);
        }
        let desc = citrus_render::MaterialDesc {
            name: "Default".into(),
            params: MaterialParams::default(),
            features: MaterialFeatures::default(),
            albedo: None,
            normal: None,
            orm: None,
            emission: None,
            error: false,
        };
        let handle = renderer.create_material(&desc)?;
        let model = model_from_material(
            "Default",
            &MaterialParams::default(),
            &MaterialFeatures::default(),
            false,
        );
        self.materials.push(MaterialEntry {
            default: model.clone(),
            model,
            handle,
            file: None,
        });
        let index = self.materials.len() - 1;
        self.default_material = Some(index);
        Ok(index)
    }

    fn ensure_primitive_mesh(
        &mut self,
        renderer: &mut Renderer,
        shape: citrus_assets::PrimitiveShape,
    ) -> Result<usize> {
        if let Some(&i) = self.primitive_meshes.get(&shape) {
            return Ok(i);
        }
        let data = citrus_assets::primitive_mesh(shape);
        self.mesh_handles.push(renderer.upload_mesh(&data)?);
        self.mesh_infos.push(MeshInfo {
            vertices: data.vertices.len() as u32,
            triangles: data.indices.len() as u32 / 3,
        });
        let mut min = Vec3::splat(f32::INFINITY);
        let mut max = Vec3::splat(f32::NEG_INFINITY);
        for v in &data.vertices {
            min = min.min(Vec3::from(v.position));
            max = max.max(Vec3::from(v.position));
        }
        self.mesh_bounds.push((min, max));
        let index = self.mesh_handles.len() - 1;
        self.primitive_meshes.insert(shape, index);
        Ok(index)
    }

    /// Spawn an empty / camera / primitive. Returns the new object index.
    pub fn spawn(
        &mut self,
        renderer: &mut Renderer,
        source: ObjectSource,
        name: String,
        position: Vec3,
    ) -> Result<usize> {
        let render = match &source {
            ObjectSource::Primitive { shape } => {
                let mesh = self.ensure_primitive_mesh(renderer, *shape)?;
                let material = self.ensure_default_material(renderer)?;
                Some(RenderInfo { mesh, material })
            }
            _ => None,
        };
        self.objects.push(SceneObject {
            name,
            render,
            source,
            parent: None,
            translation: position,
            rotation: Quat::IDENTITY,
            scale: Vec3::ONE,
        });
        Ok(self.objects.len() - 1)
    }

    /// Upload an imported asset scene, appending its content. `source_path`
    /// is recorded for .scene provenance (None = built-in test scene).
    pub fn add_asset_scene(
        &mut self,
        renderer: &mut Renderer,
        scene: &citrus_assets::Scene,
        source_path: Option<&Path>,
    ) -> Result<()> {
        let mesh_base = self.mesh_handles.len();
        if let Some(path) = source_path {
            self.model_mesh_base.insert(path.to_owned(), mesh_base);
        }

        let textures: Vec<_> = scene
            .textures
            .iter()
            .map(|t| renderer.upload_texture(t))
            .collect::<Result<_>>()?;
        for mesh in &scene.meshes {
            self.mesh_handles.push(renderer.upload_mesh(mesh)?);
            self.mesh_infos.push(MeshInfo {
                vertices: mesh.vertices.len() as u32,
                triangles: mesh.indices.len() as u32 / 3,
            });
            let mut min = Vec3::splat(f32::INFINITY);
            let mut max = Vec3::splat(f32::NEG_INFINITY);
            for v in &mesh.vertices {
                min = min.min(Vec3::from(v.position));
                max = max.max(Vec3::from(v.position));
            }
            self.mesh_bounds.push((min, max));
        }

        let material_base = self.materials.len();
        for material in &scene.materials {
            let desc = MaterialDesc {
                name: material.name.clone(),
                params: material.params,
                features: material.features,
                albedo: material.albedo.map(|i| textures[i]),
                normal: material.normal.map(|i| textures[i]),
                orm: material.orm.map(|i| textures[i]),
                emission: material.emission.map(|i| textures[i]),
                error: false,
            };
            let handle = renderer.create_material(&desc)?;
            let model = model_from_material(
                &material.name,
                &material.params,
                &material.features,
                material.normal.is_some(),
            );
            self.materials.push(MaterialEntry {
                default: model.clone(),
                model,
                handle,
                file: None,
            });
        }

        for instance in &scene.instances {
            let (scale, rotation, translation) =
                instance.transform.to_scale_rotation_translation();
            let mesh = mesh_base + instance.mesh;
            let material = material_base + instance.material;
            let source = match source_path {
                Some(path) => ObjectSource::Model {
                    path: path.to_string_lossy().into_owned(),
                    mesh: instance.mesh,
                },
                None => ObjectSource::Builtin {
                    mesh: instance.mesh,
                },
            };
            self.objects.push(SceneObject {
                name: instance.name.clone(),
                render: Some(RenderInfo { mesh, material }),
                source,
                parent: None,
                translation,
                rotation,
                scale,
            });
        }
        Ok(())
    }

    /// Load (or fetch cached) a `.material` file as a material entry.
    /// Returns the material index. Broken files yield an error-shader
    /// material so the problem is visible in the viewport.
    pub fn material_from_file(
        &mut self,
        renderer: &mut Renderer,
        path: &Path,
        project_root: &Path,
    ) -> usize {
        if let Some(&index) = self.material_file_cache.get(path) {
            return index;
        }
        let index = match self.try_load_material_file(renderer, path, project_root) {
            Ok(index) => index,
            Err(e) => {
                tracing::error!("loading material {}: {e:#}", path.display());
                let name = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "broken".into());
                let desc = MaterialDesc {
                    name: format!("{name} (error)"),
                    params: MaterialParams::default(),
                    features: MaterialFeatures::default(),
                    albedo: None,
                    normal: None,
                    orm: None,
                    emission: None,
                    error: true,
                };
                let handle = renderer.create_material(&desc).expect("material pool full");
                let model = model_from_material(
                    &format!("{name} (error)"),
                    &MaterialParams::default(),
                    &MaterialFeatures::default(),
                    false,
                );
                self.materials.push(MaterialEntry {
                    default: model.clone(),
                    model,
                    handle,
                    file: Some(path.to_owned()),
                });
                self.materials.len() - 1
            }
        };
        self.material_file_cache.insert(path.to_owned(), index);
        index
    }

    fn try_load_material_file(
        &mut self,
        renderer: &mut Renderer,
        path: &Path,
        project_root: &Path,
    ) -> Result<usize> {
        let file = citrus_assets::load_material_file(path)?;
        let mut load_tex = |slot: &Option<PathBuf>, srgb: bool| -> Result<Option<TextureHandle>> {
            let Some(rel) = slot else { return Ok(None) };
            let abs = project_root.join(rel);
            if let Some(&handle) = self.texture_file_cache.get(&(abs.clone(), srgb)) {
                return Ok(Some(handle));
            }
            let data = citrus_assets::load_texture_file(&abs, srgb)?;
            let handle = renderer.upload_texture(&data)?;
            self.texture_file_cache.insert((abs, srgb), handle);
            Ok(Some(handle))
        };
        let albedo = load_tex(&file.textures.albedo, true)?;
        let normal = load_tex(&file.textures.normal, false)?;
        let orm = load_tex(&file.textures.orm, false)?;
        let emission = load_tex(&file.textures.emission, true)?;

        let desc = MaterialDesc {
            name: file.name.clone(),
            params: file.params,
            features: file.features,
            albedo,
            normal,
            orm,
            emission,
            error: false,
        };
        let handle = renderer.create_material(&desc)?;
        let mut model =
            model_from_material(&file.name, &file.params, &file.features, normal.is_some());
        model.shader = file.shader.clone();
        self.materials.push(MaterialEntry {
            default: model.clone(),
            model,
            handle,
            file: Some(path.to_owned()),
        });
        Ok(self.materials.len() - 1)
    }

    /// Assign a `.material` file to an object's slot.
    pub fn assign_material(
        &mut self,
        renderer: &mut Renderer,
        object: usize,
        path: &Path,
        project_root: &Path,
    ) {
        let material = self.material_from_file(renderer, path, project_root);
        if let Some(render) = &mut self.objects[object].render {
            render.material = material;
        }
    }

    /// Push one material's inspector model into the renderer.
    pub fn apply_material(&mut self, renderer: &mut Renderer, index: usize) {
        let entry = &self.materials[index];
        let m = &entry.model;
        let params = renderer.material_params_mut(entry.handle);
        params.base_color = m.base_color;
        params.metallic = m.metallic;
        params.roughness = m.roughness;
        params.occlusion_strength = m.occlusion_strength;
        params.toon_steps = m.toon_steps;
        params.pbr_toon_blend = m.pbr_toon_blend;
        params.emission_color = m.emission_color;
        params.emission_intensity = m.emission_intensity;
        params.alpha_cutoff = m.alpha_cutoff;
        params.normal_strength = m.normal_strength;
        renderer.set_material_features(entry.handle, features_from_model(m));
        // Unknown shader → error swirl until custom shaders exist.
        renderer.set_material_error(
            entry.handle,
            !citrus_editor::SHADER_REGISTRY.contains(&m.shader.as_str()),
        );
    }

    /// Sync all draw transforms from object TRS (cheap; runs every frame).
    pub fn sync_draws(&mut self, selected: Option<usize>, highlight: f32) {
        self.draws.clear();
        for i in 0..self.objects.len() {
            let Some(render) = self.objects[i].render else {
                continue;
            };
            self.draws.push(DrawCmd {
                mesh: self.mesh_handles[render.mesh],
                material: self.materials[render.material].handle,
                transform: self.world_transform(i),
                highlight: if selected == Some(i) { highlight } else { 0.0 },
            });
        }
    }

    /// Ray-pick the closest object (ray vs object-space AABB).
    pub fn pick(&self, origin: Vec3, dir: Vec3) -> Option<usize> {
        let mut best: Option<(usize, f32)> = None;
        for (i, object) in self.objects.iter().enumerate() {
            let Some(render) = &object.render else {
                continue;
            };
            let world = self.world_transform(i);
            let inv = world.inverse();
            let local_origin = inv.transform_point3(origin);
            let local_dir = inv.transform_vector3(dir);
            let (min, max) = self.mesh_bounds[render.mesh];
            if let Some(t_local) = ray_aabb(local_origin, local_dir, min, max) {
                let hit_world = world.transform_point3(local_origin + local_dir * t_local);
                let t_world = (hit_world - origin).length();
                if best.is_none_or(|(_, t)| t_world < t) {
                    best = Some((i, t_world));
                }
            }
        }
        best.map(|(i, _)| i)
    }

    /// Serialize the current scene to a SceneFile.
    pub fn to_scene_file(&self, project_root: &Path) -> SceneFile {
        let entries = self
            .objects
            .iter()
            .map(|object| {
                let material = match &object.render {
                    Some(render) => {
                        let entry = &self.materials[render.material];
                        match &entry.file {
                            Some(path) => MaterialRef::File(relative_to(path, project_root)),
                            None => {
                                let (params, features) = material_from_model(&entry.model);
                                MaterialRef::Inline { params, features }
                            }
                        }
                    }
                    None => MaterialRef::Inline {
                        params: MaterialParams::default(),
                        features: MaterialFeatures::default(),
                    },
                };
                SceneEntry {
                    name: object.name.clone(),
                    source: object.source.clone(),
                    material,
                    parent: object.parent,
                    translation: object.translation.to_array(),
                    rotation: object.rotation.to_array(),
                    scale: object.scale.to_array(),
                }
            })
            .collect();
        SceneFile { entries }
    }

    /// Rebuild the whole scene from a SceneFile. The renderer's scene
    /// resources must have been reset by the caller.
    pub fn load_scene_file(
        renderer: &mut Renderer,
        file: &SceneFile,
        project_root: &Path,
    ) -> Result<Self> {
        let mut scene = Self::empty();

        // Import each referenced model (and the builtin set) once.
        let mut model_object_template: HashMap<String, Vec<usize>> = HashMap::new();
        let mut needs_builtin = false;
        for entry in &file.entries {
            match &entry.source {
                ObjectSource::Model { path, .. } => {
                    model_object_template.entry(path.clone()).or_default();
                }
                ObjectSource::Builtin { .. } => needs_builtin = true,
                ObjectSource::Primitive { .. } | ObjectSource::Empty | ObjectSource::Camera => {}
            }
        }

        // Builtin meshes/materials come from the test scene; imported models
        // bring their own materials for Inline overrides.
        let mut builtin_template: Option<(usize, Vec<usize>)> = None; // (mesh base, material per mesh... )
        if needs_builtin {
            let test = citrus_assets::test_scene();
            let mesh_base = scene.mesh_handles.len();
            scene.add_asset_scene(renderer, &test, None)?;
            // Remove the template objects; entries recreate placements.
            let template_materials: Vec<usize> =
                scene.objects.iter().filter_map(|o| o.render.map(|r| r.material)).collect();
            scene.objects.clear();
            scene.draws.clear();
            builtin_template = Some((mesh_base, template_materials));
        }

        let mut model_info: HashMap<String, (usize, Vec<usize>)> = HashMap::new();
        for path in model_object_template.keys() {
            let abs = project_root.join(path);
            let asset = citrus_assets::load_model(&abs)
                .with_context(|| format!("importing {path} for scene"))?;
            let mesh_base = scene.mesh_handles.len();
            let object_start = scene.objects.len();
            scene.add_asset_scene(renderer, &asset, Some(Path::new(path)))?;
            // Template: per model-local mesh index → material index.
            let mut per_mesh_material = vec![0usize; asset.meshes.len()];
            for object in &scene.objects[object_start..] {
                if let Some(render) = &object.render {
                    per_mesh_material[render.mesh - mesh_base] = render.material;
                }
            }
            scene.objects.truncate(object_start);
            scene.draws.truncate(object_start);
            model_info.insert(path.clone(), (mesh_base, per_mesh_material));
        }

        for entry in &file.entries {
            // (mesh, template material) for sources that render.
            let mesh_material = match &entry.source {
                ObjectSource::Model { path, mesh } => {
                    let (base, materials) = model_info
                        .get(path)
                        .context("scene references a model that failed to load")?;
                    let local = (*mesh).min(materials.len().saturating_sub(1));
                    Some((base + local, materials[local]))
                }
                ObjectSource::Builtin { mesh } => {
                    let (base, materials) = builtin_template
                        .as_ref()
                        .context("scene references builtin meshes but none loaded")?;
                    // Builtin material template is per-object; use mesh index
                    // clamped into the material list as a fallback.
                    let local = (*mesh).min(2);
                    let material = materials.get(local).copied().unwrap_or(0);
                    Some((base + local, material))
                }
                ObjectSource::Primitive { shape } => {
                    let mesh = scene.ensure_primitive_mesh(renderer, *shape)?;
                    let material = scene.ensure_default_material(renderer)?;
                    Some((mesh, material))
                }
                ObjectSource::Empty | ObjectSource::Camera => None,
            };

            let render = match mesh_material {
                Some((mesh, default_material)) => {
                    let material = match &entry.material {
                        MaterialRef::File(path) => {
                            let abs = project_root.join(path);
                            scene.material_from_file(renderer, &abs, project_root)
                        }
                        MaterialRef::Inline { params, features } => {
                            // Apply the stored params over the imported
                            // material's textures by editing its model.
                            let entry_ref = &mut scene.materials[default_material];
                            let has_normal = entry_ref.model.has_normal_texture;
                            let name = entry_ref.model.name.clone();
                            entry_ref.model =
                                model_from_material(&name, params, features, has_normal);
                            default_material
                        }
                    };
                    Some(RenderInfo { mesh, material })
                }
                None => None,
            };

            scene.objects.push(SceneObject {
                name: entry.name.clone(),
                render,
                source: entry.source.clone(),
                parent: None, // applied below once all objects exist
                translation: Vec3::from(entry.translation),
                rotation: Quat::from_array(entry.rotation),
                scale: Vec3::from(entry.scale),
            });
        }

        // Parent links (entry order == object order in a fresh scene).
        for (i, entry) in file.entries.iter().enumerate() {
            if let Some(parent) = entry.parent {
                if parent < scene.objects.len() && parent != i {
                    scene.objects[i].parent = Some(parent);
                }
            }
        }

        // Push all material models (incl. inline overrides) to the renderer.
        for i in 0..scene.materials.len() {
            scene.apply_material(renderer, i);
        }
        Ok(scene)
    }
}

pub fn model_from_material(
    name: &str,
    p: &MaterialParams,
    f: &MaterialFeatures,
    has_normal_texture: bool,
) -> MaterialModel {
    MaterialModel {
        name: name.to_owned(),
        shader: "standard".into(),
        base_color: p.base_color,
        metallic: p.metallic,
        roughness: p.roughness,
        occlusion_strength: p.occlusion_strength,
        toon_enabled: f.toon,
        toon_steps: p.toon_steps,
        pbr_toon_blend: p.pbr_toon_blend,
        emission_enabled: f.emission,
        emission_color: p.emission_color,
        emission_intensity: p.emission_intensity,
        alpha_mode: match f.alpha_mode {
            AlphaMode::Opaque => AlphaModeModel::Opaque,
            AlphaMode::Cutout => AlphaModeModel::Cutout,
            AlphaMode::Blend => AlphaModeModel::Blend,
        },
        alpha_cutoff: p.alpha_cutoff,
        has_normal_texture,
        normal_map_enabled: f.normal_map,
        normal_strength: p.normal_strength,
        double_sided: f.double_sided,
    }
}

pub fn material_from_model(m: &MaterialModel) -> (MaterialParams, MaterialFeatures) {
    (
        MaterialParams {
            base_color: m.base_color,
            metallic: m.metallic,
            roughness: m.roughness,
            occlusion_strength: m.occlusion_strength,
            toon_steps: m.toon_steps,
            pbr_toon_blend: m.pbr_toon_blend,
            emission_color: m.emission_color,
            emission_intensity: m.emission_intensity,
            alpha_cutoff: m.alpha_cutoff,
            normal_strength: m.normal_strength,
        },
        features_from_model(m),
    )
}

pub fn features_from_model(m: &MaterialModel) -> MaterialFeatures {
    MaterialFeatures {
        toon: m.toon_enabled,
        normal_map: m.normal_map_enabled,
        emission: m.emission_enabled,
        alpha_mode: match m.alpha_mode {
            AlphaModeModel::Opaque => AlphaMode::Opaque,
            AlphaModeModel::Cutout => AlphaMode::Cutout,
            AlphaModeModel::Blend => AlphaMode::Blend,
        },
        double_sided: m.double_sided,
    }
}

pub fn relative_to(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

fn ray_aabb(origin: Vec3, dir: Vec3, min: Vec3, max: Vec3) -> Option<f32> {
    let inv_dir = dir.recip();
    let t1 = (min - origin) * inv_dir;
    let t2 = (max - origin) * inv_dir;
    let t_min = t1.min(t2).max_element();
    let t_max = t1.max(t2).min_element();
    (t_max >= t_min.max(0.0)).then_some(t_min.max(0.0))
}
