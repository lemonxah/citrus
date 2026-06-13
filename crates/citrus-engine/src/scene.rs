//! Scene state: objects with TRS + provenance, GPU upload, material
//! management (imported, file-based, and edited), picking, and .scene
//! save/load.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use citrus_assets::{MaterialRef, ObjectSource, SceneEntry, SceneFile};
use citrus_editor::{AlphaModeModel, Component, ComponentCtx, ComponentRegistry, MaterialModel};
use citrus_render::{
    AlphaMode, DrawCmd, MaterialDesc, MaterialFeatures, MaterialHandle, MaterialParams, MeshHandle,
    Renderer, TextureHandle,
};
use glam::{Mat4, Quat, Vec3};

use crate::shaders::ShaderLibrary;

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
    /// When false the object is skipped at draw time (and its light, if any,
    /// stops contributing) but it stays in the scene.
    pub enabled: bool,
    /// Marks the object as non-moving so its geometry is included in the
    /// lighting bake (lightmaps + as a ray-trace occluder). Dynamic objects
    /// instead sample baked light probes.
    pub static_geometry: bool,
    /// Parent object index; transform is local to it.
    pub parent: Option<usize>,
    pub translation: Vec3,
    pub rotation: Quat,
    pub scale: Vec3,
    pub components: Vec<Box<dyn Component>>,
}

impl SceneObject {
    pub fn local_transform(&self) -> Mat4 {
        Mat4::from_scale_rotation_translation(self.scale, self.rotation, self.translation)
    }

    /// Serialize all components (undo snapshots, play-mode restore).
    pub fn save_components(&self) -> Vec<(String, String)> {
        self.components
            .iter()
            .map(|c| (c.type_name().to_owned(), c.save()))
            .collect()
    }

    /// Rebuild the component list from serialized state.
    pub fn load_components(&mut self, saved: &[(String, String)], registry: &ComponentRegistry) {
        self.components = saved
            .iter()
            .filter_map(|(kind, data)| registry.load(kind, data))
            .collect();
    }

    pub fn kind_label(&self) -> &'static str {
        match self.source {
            ObjectSource::Empty => "Empty",
            ObjectSource::Camera => "Camera",
            ObjectSource::Light => "Light",
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
    /// True for imported-model materials whose textures came embedded in the
    /// model file: they can't be expressed in a `.material` file (no paths),
    /// so scene saves keep them inline instead of materializing a file.
    pub embedded_textures: bool,
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
    /// Project-relative equirectangular skybox image (None = procedural sky).
    pub skybox: Option<String>,
    /// World lighting / environment (ambient + sun + skybox toggle).
    pub environment: citrus_assets::WorldEnvironment,
    /// Baked lighting result (None until a bake runs). Re-uploaded to the
    /// renderer for runtime sampling.
    pub baked: Option<BakedData>,
}

/// One probe volume's runtime metadata: where it sits and which SH range it
/// owns, so dynamic objects can trilinearly interpolate.
#[derive(Clone)]
pub struct ProbeVolumeMeta {
    /// World → volume-local (probe grid spans -size/2..+size/2 in local).
    pub world_to_local: Mat4,
    pub size: [f32; 3],
    pub counts: [usize; 3],
    /// First probe index (into `BakedData.probe_sh`) for this volume.
    pub sh_base: usize,
}

/// Baked lighting, kept on the scene and pushed to the renderer for runtime.
#[derive(Clone, Default)]
pub struct BakedData {
    /// Object index → lightmap layer in the renderer's lightmap array.
    pub object_lightmap: std::collections::HashMap<usize, usize>,
    pub lightmaps: Vec<citrus_render::BakedLightmap>,
    pub probe_volumes: Vec<ProbeVolumeMeta>,
    pub probe_sh: Vec<citrus_render::ProbeSh>,
}

/// Owned bake inputs gathered from the scene; `BakeInput` borrows from this.
pub struct BakeGather {
    pub instances: Vec<citrus_render::BakeInstance>,
    /// Object index per instance (parallel to `instances`).
    pub instance_objects: Vec<usize>,
    pub lights: Vec<citrus_render::BakeLight>,
    pub probes: Vec<Vec3>,
    pub probe_volumes: Vec<ProbeVolumeMeta>,
    pub sky_color: [f32; 3],
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
            skybox: None,
            environment: citrus_assets::WorldEnvironment::default(),
            baked: None,
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

    /// Move `child` (and its subtree) under `new_parent`, inserted in the
    /// objects Vec immediately before `before` (None = append at the end of
    /// the new parent's children). Preserves world transform and remaps every
    /// positional parent index. Returns the old→new index map (empty if the
    /// move was rejected), so callers can fix up selection indices.
    pub fn reorder_object(
        &mut self,
        child: usize,
        new_parent: Option<usize>,
        before: Option<usize>,
    ) -> Vec<usize> {
        let len = self.objects.len();
        if child >= len {
            return Vec::new();
        }
        // Reject cycles: new_parent must not be `child` or a descendant of it.
        if let Some(p) = new_parent {
            if p >= len || p == child {
                return Vec::new();
            }
            let mut i = p;
            let mut guard = 0;
            while let Some(pp) = self.objects[i].parent {
                if pp == child {
                    return Vec::new();
                }
                i = pp;
                guard += 1;
                if guard > 64 {
                    break;
                }
            }
        }

        // Re-parent, preserving the world transform.
        let world = self.world_transform(child);
        self.objects[child].parent = new_parent;
        let parent_world = new_parent.map_or(Mat4::IDENTITY, |p| self.world_transform(p));
        let local = parent_world.inverse() * world;
        let (scale, rotation, translation) = local.to_scale_rotation_translation();
        {
            let o = &mut self.objects[child];
            o.translation = translation;
            o.rotation = rotation;
            o.scale = scale;
        }

        // The moving block = child's subtree in display (DFS pre-order).
        let moving = self.subtree_preorder(child);
        let mut in_moving = vec![false; len];
        for &m in &moving {
            in_moving[m] = true;
        }
        let rest: Vec<usize> = (0..len).filter(|i| !in_moving[*i]).collect();
        let insert_at = match before {
            Some(b) if b < len && !in_moving[b] => {
                rest.iter().position(|&i| i == b).unwrap_or(rest.len())
            }
            _ => rest.len(),
        };
        let mut new_order = Vec::with_capacity(len);
        new_order.extend_from_slice(&rest[..insert_at]);
        new_order.extend_from_slice(&moving);
        new_order.extend_from_slice(&rest[insert_at..]);

        let mut map = vec![0usize; len];
        for (ni, &oi) in new_order.iter().enumerate() {
            map[oi] = ni;
        }
        // Rebuild the Vec in the new order, then remap every parent index.
        let mut slots: Vec<Option<SceneObject>> = self.objects.drain(..).map(Some).collect();
        let mut rebuilt = Vec::with_capacity(len);
        for &oi in &new_order {
            rebuilt.push(slots[oi].take().unwrap());
        }
        self.objects = rebuilt;
        for o in &mut self.objects {
            o.parent = o.parent.map(|p| map[p]);
        }
        map
    }

    /// Indices of `root` and its descendants, in DFS pre-order (== display
    /// order, since children iterate in ascending index).
    fn subtree_preorder(&self, root: usize) -> Vec<usize> {
        let mut out = Vec::new();
        self.collect_subtree(root, &mut out);
        out
    }

    fn collect_subtree(&self, root: usize, out: &mut Vec<usize>) {
        out.push(root);
        for i in 0..self.objects.len() {
            if self.objects[i].parent == Some(root) {
                self.collect_subtree(i, out);
            }
        }
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
            embedded_textures: false,
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
            enabled: true,
            static_geometry: false,
            parent: None,
            translation: position,
            rotation: Quat::IDENTITY,
            scale: Vec3::ONE,
            components: Vec::new(),
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
                embedded_textures: material.albedo.is_some()
                    || material.normal.is_some()
                    || material.orm.is_some()
                    || material.emission.is_some(),
            });
        }

        for instance in &scene.instances {
            let (scale, rotation, translation) = instance.transform.to_scale_rotation_translation();
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
                enabled: true,
                static_geometry: false,
                parent: None,
                translation,
                rotation,
                scale,
                components: Vec::new(),
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
        shaders: &mut ShaderLibrary,
        path: &Path,
        project_root: &Path,
    ) -> usize {
        if let Some(&index) = self.material_file_cache.get(path) {
            return index;
        }
        let index = match self.try_load_material_file(renderer, shaders, path, project_root) {
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
                    embedded_textures: false,
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
        shaders: &mut ShaderLibrary,
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
        if let Some(q) = file.render_queue {
            model.render_queue = q;
        }
        if file.shader != "standard" {
            let entry = shaders.resolve(renderer, project_root, &file.shader);
            if let Some(source) = &entry.source {
                model.custom_values = source.pack(&file.custom).to_vec();
            }
        }
        self.materials.push(MaterialEntry {
            default: model.clone(),
            model,
            handle,
            file: Some(path.to_owned()),
            embedded_textures: false,
        });
        let index = self.materials.len() - 1;
        self.apply_material(renderer, shaders, project_root, index);
        Ok(index)
    }

    /// Assign a `.material` file to an object's slot.
    pub fn assign_material(
        &mut self,
        renderer: &mut Renderer,
        shaders: &mut ShaderLibrary,
        object: usize,
        path: &Path,
        project_root: &Path,
    ) {
        let material = self.material_from_file(renderer, shaders, path, project_root);
        if let Some(render) = &mut self.objects[object].render {
            render.material = material;
        }
    }

    /// Push one material's inspector model into the renderer, resolving its
    /// shader (compiling custom shaders on first use).
    pub fn apply_material(
        &mut self,
        renderer: &mut Renderer,
        shaders: &mut ShaderLibrary,
        project_root: &Path,
        index: usize,
    ) {
        let entry = &self.materials[index];
        let handle = entry.handle;
        let m = &entry.model;
        let params = renderer.material_params_mut(handle);
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
        renderer.set_material_features(handle, features_from_model(m));
        renderer.set_material_render_queue(handle, m.render_queue);

        if m.shader == "standard" {
            renderer.set_material_shader(handle, None);
            renderer.set_material_error(handle, false);
            return;
        }
        let shader = m.shader.clone();
        let shader_entry = shaders.resolve(renderer, project_root, &shader);
        match shader_entry.id {
            Some(id) => {
                let defaults = shader_entry.defaults();
                let model = &mut self.materials[index].model;
                if model.custom_values.len() != defaults.len() {
                    model.custom_values = defaults;
                }
                let mut data = [0.0f32; 16];
                data.copy_from_slice(&model.custom_values);
                renderer.set_material_shader(handle, Some(id));
                renderer.set_material_custom_data(handle, data);
                renderer.set_material_error(handle, false);
            }
            // Broken/missing shader → error swirl.
            None => renderer.set_material_error(handle, true),
        }
    }

    /// Link a material to a `.material` file it was saved to (auto-save of
    /// previously file-less materials).
    pub fn set_material_file(&mut self, index: usize, path: PathBuf) {
        self.material_file_cache.insert(path.clone(), index);
        self.materials[index].file = Some(path);
    }

    /// Re-apply every material that uses one of `changed` shaders (hot
    /// reload).
    pub fn reapply_materials_using(
        &mut self,
        renderer: &mut Renderer,
        shaders: &mut ShaderLibrary,
        project_root: &Path,
        changed: &[String],
    ) {
        for index in 0..self.materials.len() {
            if changed.contains(&self.materials[index].model.shader) {
                self.apply_material(renderer, shaders, project_root, index);
            }
        }
    }

    /// Make sure every camera object carries a Camera component (spawned
    /// and legacy-scene cameras alike).
    pub fn ensure_camera_components(&mut self, registry: &ComponentRegistry) {
        for object in &mut self.objects {
            if matches!(object.source, ObjectSource::Camera)
                && !object.components.iter().any(|c| c.type_name() == "Camera")
                && let Some(camera) = registry.create("Camera")
            {
                object.components.push(camera);
            }
        }
    }

    /// Make sure every light object carries a Light component (a default
    /// directional one for legacy/edited scenes that lost it).
    pub fn ensure_light_components(&mut self, registry: &ComponentRegistry) {
        for object in &mut self.objects {
            if matches!(object.source, ObjectSource::Light)
                && !object.components.iter().any(|c| c.type_name() == "Light")
                && let Some(light) = registry.create("Light")
            {
                object.components.push(light);
            }
        }
    }

    /// Assign a stable, unique id to every camera that doesn't have one yet
    /// (id 0 = unassigned). New ids continue past the current maximum, so the
    /// oldest camera keeps the smallest id (and stays "main") across spawns
    /// and reloads. Returns true if any id was assigned (so the caller can
    /// persist the change).
    pub fn ensure_camera_ids(&mut self) -> bool {
        use citrus_editor::CameraComponent;
        let mut max_id = 0u32;
        for object in &self.objects {
            for c in &object.components {
                if let Some(cam) = c.as_any().downcast_ref::<CameraComponent>() {
                    max_id = max_id.max(cam.id);
                }
            }
        }
        let mut changed = false;
        for object in &mut self.objects {
            for c in &mut object.components {
                if let Some(cam) = c.as_any_mut().downcast_mut::<CameraComponent>()
                    && cam.id == 0
                {
                    max_id += 1;
                    cam.id = max_id;
                    changed = true;
                }
            }
        }
        changed
    }

    /// Object index of the "main" camera: the one with the smallest camera id.
    /// Run [`ensure_camera_ids`](Self::ensure_camera_ids) first so every camera
    /// has an id.
    pub fn main_camera(&self) -> Option<usize> {
        use citrus_editor::CameraComponent;
        let mut best: Option<(usize, u32)> = None;
        for (i, object) in self.objects.iter().enumerate() {
            if let Some(cam) = object
                .components
                .iter()
                .find_map(|c| c.as_any().downcast_ref::<CameraComponent>())
                && best.is_none_or(|(_, id)| cam.id < id)
            {
                best = Some((i, cam.id));
            }
        }
        best.map(|(i, _)| i)
    }

    /// View, projection, and world position of the main camera for the given
    /// aspect ratio (cameras look down -Z, glTF convention).
    pub fn main_camera_view_proj(&self, aspect: f32) -> Option<(Mat4, Mat4, Vec3)> {
        use citrus_editor::CameraComponent;
        let i = self.main_camera()?;
        let cam = self.objects[i]
            .components
            .iter()
            .find_map(|c| c.as_any().downcast_ref::<CameraComponent>())?;
        let world = self.world_transform(i);
        let (_, rotation, position) = world.to_scale_rotation_translation();
        let forward = rotation * Vec3::NEG_Z;
        let up = rotation * Vec3::Y;
        let view = Mat4::look_to_rh(position, forward, up);
        let near = cam.near.max(0.001);
        let far = cam.far.max(near + 0.001);
        let proj = Mat4::perspective_rh(cam.fov_deg.to_radians(), aspect.max(0.01), near, far);
        Some((view, proj, position))
    }

    /// Call one lifecycle hook on every component. Deferred engine requests
    /// (e.g. load-scene) are appended to `commands` for the caller to drain.
    fn each_component(
        &mut self,
        dt: f32,
        time: f32,
        commands: &mut Vec<citrus_editor::ComponentCommand>,
        mut call: impl FnMut(&mut Box<dyn citrus_editor::Component>, &mut ComponentCtx),
    ) {
        for object in &mut self.objects {
            let SceneObject {
                components,
                translation,
                rotation,
                scale,
                ..
            } = object;
            for component in components.iter_mut() {
                call(
                    component,
                    &mut ComponentCtx {
                        dt,
                        time,
                        translation,
                        rotation,
                        scale,
                        commands,
                    },
                );
            }
        }
    }

    /// Play started: every component's `start` hook.
    pub fn start_components(
        &mut self,
        time: f32,
        commands: &mut Vec<citrus_editor::ComponentCommand>,
    ) {
        self.each_component(0.0, time, commands, |c, ctx| c.start(ctx));
    }

    /// Run all components for one frame (Play mode): all `update`s, then
    /// all `late_update`s.
    pub fn update_components(
        &mut self,
        dt: f32,
        time: f32,
        commands: &mut Vec<citrus_editor::ComponentCommand>,
    ) {
        self.each_component(dt, time, commands, |c, ctx| c.update(ctx));
        self.each_component(dt, time, commands, |c, ctx| c.late_update(ctx));
    }

    /// True if the object and all of its ancestors are enabled (a disabled
    /// parent hides its whole subtree).
    pub fn is_active(&self, index: usize) -> bool {
        let mut i = index;
        let mut guard = 0;
        loop {
            if !self.objects[i].enabled {
                return false;
            }
            match self.objects[i].parent {
                Some(p) if p < self.objects.len() && guard < 64 => {
                    i = p;
                    guard += 1;
                }
                _ => return true,
            }
        }
    }

    /// Collect every `Light` component into renderer light instances, reading
    /// world position/orientation from each object's transform. Baked lights
    /// are included for now (the bake path isn't built yet), so scenes don't
    /// go dark.
    pub fn gather_lights(&self) -> Vec<citrus_render::LightInstance> {
        use citrus_editor::{LightComponent, LightKind};
        let mut lights = Vec::new();
        for i in 0..self.objects.len() {
            if !self.is_active(i) {
                continue;
            }
            let Some(light) = self.objects[i]
                .components
                .iter()
                .find_map(|c| c.as_any().downcast_ref::<LightComponent>())
            else {
                continue;
            };
            let world = self.world_transform(i);
            let (_, rotation, position) = world.to_scale_rotation_translation();
            // Forward (-Z) is the light's travel direction.
            let direction = rotation * Vec3::NEG_Z;
            let kind = match light.kind {
                LightKind::Directional => citrus_render::LightKind::Directional,
                LightKind::Point => citrus_render::LightKind::Point,
                LightKind::Spot => citrus_render::LightKind::Spot,
            };
            lights.push(citrus_render::LightInstance {
                kind,
                position,
                direction,
                color: light.color,
                intensity: light.intensity,
                range: light.range,
                spot_inner_deg: light.spot_angle * (1.0 - light.spot_blend),
                spot_outer_deg: light.spot_angle,
                cast_shadows: light.cast_shadows,
                shadow_bias: light.shadow_bias,
            });
        }
        lights
    }

    /// Gather everything the GPU lighting bake needs: static-geometry
    /// instances (with per-object lightmap resolution from texel density),
    /// Baked-mode lights, and probe-volume grid points. See [`BakeGather`].
    pub fn gather_bake(&self) -> BakeGather {
        use citrus_editor::{LightComponent, LightKind, LightMode, LightProbeVolume};

        let settings = self.environment.bake;
        let mut instances = Vec::new();
        let mut instance_objects = Vec::new();

        for i in 0..self.objects.len() {
            let obj = &self.objects[i];
            if !obj.static_geometry || !self.is_active(i) {
                continue;
            }
            let Some(render) = obj.render else { continue };
            let world = self.world_transform(i);
            let (min, max) = self.mesh_bounds[render.mesh];
            let scale = world.to_scale_rotation_translation().0.abs();
            let extent = (max - min) * scale;
            let max_extent = extent.x.max(extent.y).max(extent.z).max(0.01);
            let size = (settings.texel_density * max_extent).round() as u32;
            let lightmap_size = size.clamp(16, settings.max_lightmap.max(16));

            let model = &self.materials[render.material].model;
            let emission = if model.emission_enabled {
                [
                    model.emission_color[0] * model.emission_intensity,
                    model.emission_color[1] * model.emission_intensity,
                    model.emission_color[2] * model.emission_intensity,
                ]
            } else {
                [0.0; 3]
            };
            instances.push(citrus_render::BakeInstance {
                mesh: self.mesh_handles[render.mesh],
                transform: world,
                lightmap_size,
                albedo: [model.base_color[0], model.base_color[1], model.base_color[2]],
                emission,
            });
            instance_objects.push(i);
        }

        // Baked-mode lights only: Mixed/Realtime stay in the realtime path so
        // their direct term isn't double-counted (Unity/Unreal convention).
        let mut lights = Vec::new();
        for i in 0..self.objects.len() {
            if !self.is_active(i) {
                continue;
            }
            let Some(light) = self.objects[i]
                .components
                .iter()
                .find_map(|c| c.as_any().downcast_ref::<LightComponent>())
            else {
                continue;
            };
            if light.mode != LightMode::Baked {
                continue;
            }
            let world = self.world_transform(i);
            let (_, rotation, position) = world.to_scale_rotation_translation();
            let direction = rotation * Vec3::NEG_Z;
            let kind = match light.kind {
                LightKind::Directional => citrus_render::LightKind::Directional,
                LightKind::Point => citrus_render::LightKind::Point,
                LightKind::Spot => citrus_render::LightKind::Spot,
            };
            lights.push(citrus_render::BakeLight {
                kind,
                position,
                direction,
                color: [
                    light.color[0] * light.intensity,
                    light.color[1] * light.intensity,
                    light.color[2] * light.intensity,
                ],
                range: light.range,
                spot_inner_deg: light.spot_angle * (1.0 - light.spot_blend),
                spot_outer_deg: light.spot_angle,
            });
        }

        // Probe volumes → flattened world-space probe points + per-volume
        // metadata for runtime trilinear lookup.
        let mut probes = Vec::new();
        let mut probe_volumes = Vec::new();
        for i in 0..self.objects.len() {
            if !self.is_active(i) {
                continue;
            }
            let Some(vol) = self.objects[i]
                .components
                .iter()
                .find_map(|c| c.as_any().downcast_ref::<LightProbeVolume>())
            else {
                continue;
            };
            let world = self.world_transform(i);
            let sh_base = probes.len();
            for local in vol.local_positions() {
                probes.push(world.transform_point3(local));
            }
            probe_volumes.push(ProbeVolumeMeta {
                world_to_local: world.inverse(),
                size: vol.size,
                counts: vol.counts(),
                sh_base,
            });
        }

        let amb = self.environment.ambient;
        let ai = self.environment.ambient_intensity;
        BakeGather {
            instances,
            instance_objects,
            lights,
            probes,
            probe_volumes,
            sky_color: [amb[0] * ai, amb[1] * ai, amb[2] * ai],
        }
    }

    /// Object-space AABB (min, max) of an object's render mesh, for the mesh
    /// collider wireframe.
    pub fn render_mesh_bounds(&self, index: usize) -> Option<(Vec3, Vec3)> {
        self.objects[index]
            .render
            .map(|r| self.mesh_bounds[r.mesh])
    }

    /// Flatten every active `AudioSource` into a per-frame cue list (with
    /// world position) for the audio engine.
    pub fn gather_audio(&self) -> Vec<crate::audio::AudioCue> {
        use citrus_editor::AudioSource;
        let mut cues = Vec::new();
        for i in 0..self.objects.len() {
            if !self.is_active(i) {
                continue;
            }
            if let Some(src) = self.objects[i]
                .components
                .iter()
                .find_map(|c| c.as_any().downcast_ref::<AudioSource>())
            {
                let pos = self.world_transform(i).w_axis.truncate();
                cues.push(crate::audio::AudioCue::from_source(i, src, pos));
            }
        }
        cues
    }

    /// World position of the first active `AudioListener` (the spatial "ears").
    pub fn audio_listener(&self) -> Option<Vec3> {
        use citrus_editor::AudioListener;
        for i in 0..self.objects.len() {
            if !self.is_active(i) {
                continue;
            }
            if self.objects[i]
                .components
                .iter()
                .any(|c| c.as_any().downcast_ref::<AudioListener>().is_some())
            {
                return Some(self.world_transform(i).w_axis.truncate());
            }
        }
        None
    }

    /// Sync all draw transforms from object TRS (cheap; runs every frame).
    pub fn sync_draws(&mut self, selected: Option<usize>, highlight: f32) {
        self.draws.clear();
        for i in 0..self.objects.len() {
            let Some(render) = self.objects[i].render else {
                continue;
            };
            if !self.is_active(i) {
                continue;
            }
            self.draws.push(DrawCmd {
                mesh: self.mesh_handles[render.mesh],
                material: self.materials[render.material].handle,
                transform: self.world_transform(i),
                highlight: if selected == Some(i) { highlight } else { 0.0 },
                mesh_center: self.mesh_center_local(render.mesh),
            });
        }
    }

    /// Delete an object and its whole subtree, remapping the surviving
    /// objects' parent indices (objects are a positional Vec). Meshes and
    /// materials are left in place (slot GC is a separate concern).
    pub fn remove_object(&mut self, index: usize) {
        if index >= self.objects.len() {
            return;
        }
        // Mark the object and every descendant for removal.
        let mut remove = vec![false; self.objects.len()];
        remove[index] = true;
        // Children always have a higher index than... not guaranteed, so
        // iterate to a fixpoint over the parent links.
        let mut changed = true;
        while changed {
            changed = false;
            for i in 0..self.objects.len() {
                if remove[i] {
                    continue;
                }
                if let Some(p) = self.objects[i].parent
                    && p < remove.len()
                    && remove[p]
                {
                    remove[i] = true;
                    changed = true;
                }
            }
        }
        // old index -> new index for survivors.
        let mut remap = vec![None; self.objects.len()];
        let mut next = 0usize;
        for (i, slot) in remap.iter_mut().enumerate() {
            if !remove[i] {
                *slot = Some(next);
                next += 1;
            }
        }
        let mut i = 0;
        self.objects.retain(|_| {
            let keep = !remove[i];
            i += 1;
            keep
        });
        for object in &mut self.objects {
            object.parent = object.parent.and_then(|p| remap.get(p).copied().flatten());
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
    pub fn to_scene_file(&self, project_root: &Path, shaders: &ShaderLibrary) -> SceneFile {
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
                                let custom = shaders
                                    .get(&entry.model.shader)
                                    .and_then(|e| e.source.as_ref())
                                    .map(|s| s.unpack(&entry.model.custom_values))
                                    .unwrap_or_default();
                                MaterialRef::Inline {
                                    params,
                                    features,
                                    shader: entry.model.shader.clone(),
                                    custom,
                                    render_queue: Some(entry.model.render_queue),
                                }
                            }
                        }
                    }
                    None => MaterialRef::Inline {
                        params: MaterialParams::default(),
                        features: MaterialFeatures::default(),
                        shader: "standard".into(),
                        custom: Default::default(),
                        render_queue: None,
                    },
                };
                SceneEntry {
                    name: object.name.clone(),
                    source: object.source.clone(),
                    enabled: object.enabled,
                    static_geometry: object.static_geometry,
                    material,
                    parent: object.parent,
                    components: object
                        .components
                        .iter()
                        .map(|c| citrus_assets::ComponentData {
                            kind: c.type_name().to_owned(),
                            data: c.save(),
                        })
                        .collect(),
                    translation: object.translation.to_array(),
                    rotation: object.rotation.to_array(),
                    scale: object.scale.to_array(),
                }
            })
            .collect();
        SceneFile {
            entries,
            skybox: self.skybox.clone(),
            environment: self.environment.clone(),
        }
    }

    /// Rebuild the whole scene from a SceneFile. The renderer's scene
    /// resources must have been reset by the caller.
    pub fn load_scene_file(
        renderer: &mut Renderer,
        file: &SceneFile,
        project_root: &Path,
        registry: &ComponentRegistry,
        shaders: &mut ShaderLibrary,
    ) -> Result<Self> {
        let mut scene = Self::empty();
        scene.skybox = file.skybox.clone();
        scene.environment = file.environment.clone();

        // Import each referenced model (and the builtin set) once.
        let mut model_object_template: HashMap<String, Vec<usize>> = HashMap::new();
        let mut needs_builtin = false;
        for entry in &file.entries {
            match &entry.source {
                ObjectSource::Model { path, .. } => {
                    model_object_template.entry(path.clone()).or_default();
                }
                ObjectSource::Builtin { .. } => needs_builtin = true,
                ObjectSource::Primitive { .. }
                | ObjectSource::Empty
                | ObjectSource::Camera
                | ObjectSource::Light => {}
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
            let template_materials: Vec<usize> = scene
                .objects
                .iter()
                .filter_map(|o| o.render.map(|r| r.material))
                .collect();
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
                ObjectSource::Empty | ObjectSource::Camera | ObjectSource::Light => None,
            };

            let render = match mesh_material {
                Some((mesh, default_material)) => {
                    let material = match &entry.material {
                        MaterialRef::File(path) => {
                            let abs = project_root.join(path);
                            scene.material_from_file(renderer, shaders, &abs, project_root)
                        }
                        MaterialRef::Inline {
                            params,
                            features,
                            shader,
                            custom,
                            render_queue,
                        } => {
                            // Apply the stored params over the imported
                            // material's textures by editing its model.
                            let entry_ref = &mut scene.materials[default_material];
                            let has_normal = entry_ref.model.has_normal_texture;
                            let name = entry_ref.model.name.clone();
                            let mut model =
                                model_from_material(&name, params, features, has_normal);
                            model.shader = shader.clone();
                            if let Some(q) = render_queue {
                                model.render_queue = *q;
                            }
                            if shader != "standard" {
                                let shader_entry = shaders.resolve(renderer, project_root, shader);
                                if let Some(source) = &shader_entry.source {
                                    model.custom_values = source.pack(custom).to_vec();
                                }
                            }
                            scene.materials[default_material].model = model;
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
                enabled: entry.enabled,
                static_geometry: entry.static_geometry,
                parent: None, // applied below once all objects exist
                translation: Vec3::from(entry.translation),
                rotation: Quat::from_array(entry.rotation),
                scale: Vec3::from(entry.scale),
                components: entry
                    .components
                    .iter()
                    .filter_map(|c| registry.load(&c.kind, &c.data))
                    .collect(),
            });
        }

        // Parent links (entry order == object order in a fresh scene).
        for (i, entry) in file.entries.iter().enumerate() {
            if let Some(parent) = entry.parent
                && parent < scene.objects.len()
                && parent != i
            {
                scene.objects[i].parent = Some(parent);
            }
        }

        // Push all material models (incl. inline overrides) to the renderer.
        for i in 0..scene.materials.len() {
            scene.apply_material(renderer, shaders, project_root, i);
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
        custom_values: Vec::new(),
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
        render_queue: (match f.alpha_mode {
            AlphaMode::Opaque => AlphaModeModel::Opaque,
            AlphaMode::Cutout => AlphaModeModel::Cutout,
            AlphaMode::Blend => AlphaModeModel::Blend,
        })
        .default_render_queue(),
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
