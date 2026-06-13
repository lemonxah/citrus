//! Unity-style components: named behaviours that attach to scene objects.
//!
//! Implement [`TypedComponent`] (serde + Default + inspector UI) and register
//! it in the [`ComponentRegistry`]; the blanket impl provides the
//! object-safe [`Component`] the engine stores and drives. Components
//! serialize to RON strings (stored in .scene files and undo snapshots) and
//! run per-frame only while the editor is in Play mode. The plugin system
//! (TODO.md) will register custom components through the same registry.

use egui::collapsing_header::{CollapsingState, paint_default_icon};
use egui::{DragValue, Label, RichText, ScrollArea, Sense, Ui};
use glam::{Quat, Vec3};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Per-frame update context handed to components in Play mode. Transform
/// fields are the owning object's local TRS.
pub struct ComponentCtx<'a> {
    pub dt: f32,
    /// Seconds since engine start.
    pub time: f32,
    pub translation: &'a mut Vec3,
    pub rotation: &'a mut Quat,
    pub scale: &'a mut Vec3,
}

/// Object-safe component, as stored on scene objects. Implement
/// [`TypedComponent`] instead of this directly.
pub trait Component: 'static {
    fn type_name(&self) -> &'static str;
    /// Draw inspector widgets; returns true if anything changed.
    fn inspector_ui(&mut self, ui: &mut Ui) -> bool;
    /// Called once when Play mode starts.
    fn start(&mut self, ctx: &mut ComponentCtx);
    /// Per-frame behaviour (Play mode only).
    fn update(&mut self, ctx: &mut ComponentCtx);
    /// Runs after every component finished `update` this frame.
    fn late_update(&mut self, ctx: &mut ComponentCtx);
    /// Serialize to RON.
    fn save(&self) -> String;
    /// Downcasting for engine systems that read typed component data
    /// (e.g. the camera frustum widget).
    fn as_any(&self) -> &dyn std::any::Any;
    /// Mutable downcasting for engine systems that write typed component data
    /// (e.g. assigning stable camera IDs).
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;
}

/// Typed component interface; blanket-implements [`Component`]. All
/// lifecycle hooks default to no-ops — override the ones you need.
pub trait TypedComponent: Serialize + DeserializeOwned + Default + 'static {
    const NAME: &'static str;
    fn inspector_ui(&mut self, ui: &mut Ui) -> bool;
    /// Called once when Play mode starts.
    fn start(&mut self, _ctx: &mut ComponentCtx) {}
    /// Called every frame while playing.
    fn update(&mut self, _ctx: &mut ComponentCtx) {}
    /// Called every frame after all components ran `update`.
    fn late_update(&mut self, _ctx: &mut ComponentCtx) {}
}

impl<T: TypedComponent> Component for T {
    fn type_name(&self) -> &'static str {
        T::NAME
    }
    fn inspector_ui(&mut self, ui: &mut Ui) -> bool {
        TypedComponent::inspector_ui(self, ui)
    }
    fn start(&mut self, ctx: &mut ComponentCtx) {
        TypedComponent::start(self, ctx)
    }
    fn update(&mut self, ctx: &mut ComponentCtx) {
        TypedComponent::update(self, ctx)
    }
    fn late_update(&mut self, ctx: &mut ComponentCtx) {
        TypedComponent::late_update(self, ctx)
    }
    fn save(&self) -> String {
        ron::to_string(self).unwrap_or_default()
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

struct ComponentInfo {
    name: &'static str,
    create: fn() -> Box<dyn Component>,
    load: fn(&str) -> Result<Box<dyn Component>, ron::error::SpannedError>,
}

/// Name → component factory/loader. One registry per app; plugins extend it.
#[derive(Default)]
pub struct ComponentRegistry {
    entries: Vec<ComponentInfo>,
}

impl ComponentRegistry {
    pub fn with_builtins() -> Self {
        let mut registry = Self::default();
        registry.register::<CameraComponent>();
        registry.register::<LightComponent>();
        registry.register::<LightProbeVolume>();
        registry.register::<Spin>();
        registry.register::<Bob>();
        registry
    }

    /// Register a component type. Re-registering a name replaces the entry —
    /// that's how hot-reloaded plugin components supersede their old build.
    pub fn register<T: TypedComponent>(&mut self) {
        let info = ComponentInfo {
            name: T::NAME,
            create: || Box::new(T::default()),
            load: |data| Ok(Box::new(ron::from_str::<T>(data)?) as Box<dyn Component>),
        };
        match self.entries.iter_mut().find(|e| e.name == T::NAME) {
            Some(entry) => {
                tracing::debug!("component {:?} re-registered (reload)", T::NAME);
                *entry = info;
            }
            None => self.entries.push(info),
        }
    }

    pub fn names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.entries.iter().map(|e| e.name)
    }

    pub fn create(&self, name: &str) -> Option<Box<dyn Component>> {
        let entry = self.entries.iter().find(|e| e.name == name)?;
        Some((entry.create)())
    }

    /// Deserialize a saved component. Unknown names and broken data log a
    /// warning and return None so scene loads survive missing components.
    pub fn load(&self, name: &str, data: &str) -> Option<Box<dyn Component>> {
        let Some(entry) = self.entries.iter().find(|e| e.name == name) else {
            tracing::warn!("unknown component {name:?}; dropping it");
            return None;
        };
        match (entry.load)(data) {
            Ok(component) => Some(component),
            Err(e) => {
                tracing::warn!("loading component {name:?}: {e}");
                None
            }
        }
    }
}

#[derive(Default)]
pub struct ComponentsResponse {
    pub changed: bool,
    /// Registry name picked from the Add Component menu.
    pub add: Option<&'static str>,
    /// Index of a component whose ✕ was clicked.
    pub remove: Option<usize>,
}

/// Component list + Add Component button (object inspector).
pub fn components_ui(
    ui: &mut Ui,
    components: &mut [Box<dyn Component>],
    registry: &ComponentRegistry,
) -> ComponentsResponse {
    let mut response = ComponentsResponse::default();
    for (index, component) in components.iter_mut().enumerate() {
        let id = ui.make_persistent_id(("citrus-component", index, component.type_name()));
        let mut state = CollapsingState::load_with_default_open(ui.ctx(), id, true);
        let header = ui.horizontal(|ui| {
            state.show_toggle_button(ui, paint_default_icon);
            let label = ui.add(
                Label::new(RichText::new(component.type_name()).strong()).sense(Sense::click()),
            );
            if label.clicked() {
                state.toggle(ui);
            }
            if label.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .small_button("✕")
                    .on_hover_text("Remove component")
                    .clicked()
                {
                    response.remove = Some(index);
                }
            });
        });
        state.show_body_indented(&header.response, ui, |ui| {
            response.changed |= component.inspector_ui(ui);
        });
        state.store(ui.ctx());
    }

    ui.add_space(4.0);
    ui.vertical_centered_justified(|ui| {
        ui.menu_button("➕ Add Component", |ui| {
            // Search box + scrollable list: the registry grows large once
            // plugins and built-ins (lights, etc.) pile up.
            let search_id = ui.make_persistent_id("citrus-add-component-search");
            let mut search: String = ui.data(|d| d.get_temp(search_id).unwrap_or_default());
            let edit = ui.add(
                egui::TextEdit::singleline(&mut search)
                    .hint_text("🔍 Search…")
                    .desired_width(180.0),
            );
            if edit.changed() {
                ui.data_mut(|d| d.insert_temp(search_id, search.clone()));
            }
            let needle = search.to_lowercase();
            ui.separator();
            ScrollArea::vertical().max_height(240.0).show(ui, |ui| {
                let mut any = false;
                for name in registry.names() {
                    if !needle.is_empty() && !name.to_lowercase().contains(&needle) {
                        continue;
                    }
                    any = true;
                    if ui.button(name).clicked() {
                        response.add = Some(name);
                        ui.data_mut(|d| d.remove::<String>(search_id));
                        ui.close();
                    }
                }
                if !any {
                    ui.label(RichText::new("No matches").weak());
                }
            });
        });
    });
    response
}

fn property_row(
    ui: &mut Ui,
    label: &str,
    changed: &mut bool,
    widget: impl FnOnce(&mut Ui) -> egui::Response,
) {
    ui.horizontal(|ui| {
        ui.label(label);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if widget(ui).changed() {
                *changed = true;
            }
        });
    });
}

fn axis_row(ui: &mut Ui, label: &str, axis: &mut [f32; 3], changed: &mut bool) {
    ui.horizontal(|ui| {
        ui.label(label);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // Right-to-left: add Z first so it reads X Y Z.
            for (name, value) in ["Z", "Y", "X"].iter().zip(axis.iter_mut().rev()) {
                *changed |= ui
                    .add_sized(
                        [48.0, 18.0],
                        DragValue::new(value).speed(0.02).max_decimals(2),
                    )
                    .changed();
                ui.label(RichText::new(*name).weak());
            }
        });
    });
}

// ---------------------------------------------------------------- builtins

/// Camera properties (Unity-style). Attached automatically to camera
/// objects; the viewport draws a frustum widget from these values.
/// Post-process effect settings land here once a post pipeline exists.
#[derive(Serialize, Deserialize)]
pub struct CameraComponent {
    /// Stable per-camera identity, saved in the `.scene` so it survives
    /// reloads. The scene's "main" camera is the one with the smallest id.
    /// 0 means "not yet assigned"; the engine fills it in.
    #[serde(default)]
    pub id: u32,
    /// Vertical field of view, degrees.
    pub fov_deg: f32,
    pub near: f32,
    pub far: f32,
}

impl Default for CameraComponent {
    fn default() -> Self {
        Self {
            id: 0,
            fov_deg: 60.0,
            near: 0.1,
            far: 100.0,
        }
    }
}

impl TypedComponent for CameraComponent {
    const NAME: &'static str = "Camera";

    fn inspector_ui(&mut self, ui: &mut Ui) -> bool {
        let mut changed = false;
        property_row(ui, "Camera ID", &mut changed, |ui| {
            ui.add_enabled(false, DragValue::new(&mut self.id))
        });
        property_row(ui, "Field of View", &mut changed, |ui| {
            ui.add(egui::Slider::new(&mut self.fov_deg, 10.0..=170.0).suffix("°"))
        });
        property_row(ui, "Near", &mut changed, |ui| {
            ui.add(
                DragValue::new(&mut self.near)
                    .speed(0.01)
                    .range(0.001..=1000.0),
            )
        });
        property_row(ui, "Far", &mut changed, |ui| {
            ui.add(
                DragValue::new(&mut self.far)
                    .speed(0.5)
                    .range(0.01..=100000.0),
            )
        });
        if self.far <= self.near {
            self.far = self.near + 0.01;
        }
        ui.label(
            RichText::new("Post-processing effects arrive with the post pipeline")
                .small()
                .weak(),
        );
        changed
    }
}

/// What shape of light a [`LightComponent`] emits. Mirrors Unity's light
/// Type dropdown.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum LightKind {
    /// Parallel rays from an infinitely distant source (the sun); only the
    /// object's orientation matters, not its position.
    Directional,
    /// Omnidirectional point source that falls off with distance.
    Point,
    /// Cone of light from a point, attenuated by distance and cone angle.
    Spot,
}

impl LightKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Directional => "Directional",
            Self::Point => "Point",
            Self::Spot => "Spot",
        }
    }

    pub const ALL: [LightKind; 3] = [Self::Directional, Self::Point, Self::Spot];
}

/// Whether a light is recomputed every frame or baked into the scene's
/// lighting environment. Static lights can be precomputed (lightmaps / probes)
/// and skipped by the realtime renderer; mirrors Unity's light Mode.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum LightMode {
    /// Recomputed every frame; fully dynamic (moving lights, day/night).
    Realtime,
    /// Baked into the lighting environment; contributes nothing at runtime
    /// beyond the bake. Cheapest, but cannot change at runtime.
    Baked,
    /// Direct light is realtime, indirect/shadow contribution is baked.
    Mixed,
}

impl LightMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Realtime => "Realtime",
            Self::Baked => "Baked (static)",
            Self::Mixed => "Mixed",
        }
    }

    pub const ALL: [LightMode; 3] = [Self::Realtime, Self::Baked, Self::Mixed];
}

/// A scene light (Unity-style). The object's transform places/orients it;
/// directional lights use only the rotation, point lights only the position,
/// spot lights both. The engine gathers every `LightComponent` each frame
/// and feeds the renderer's multi-light path.
#[derive(Serialize, Deserialize)]
pub struct LightComponent {
    pub kind: LightKind,
    /// Realtime vs baked/static. Baked lights are skipped by the realtime
    /// light path once the lighting environment is baked.
    #[serde(default = "default_light_mode")]
    pub mode: LightMode,
    /// Linear RGB tint, 0..1.
    pub color: [f32; 3],
    /// Brightness multiplier.
    pub intensity: f32,
    /// Distance (meters) at which point/spot lights reach zero (ignored for
    /// directional).
    pub range: f32,
    /// Spot cone full angle at the outer edge, degrees.
    pub spot_angle: f32,
    /// 0 = hard cone edge, 1 = soft falloff that starts at the center.
    pub spot_blend: f32,
    /// Cast real-time shadows (depth map from the light's POV).
    #[serde(default = "default_true_light")]
    pub cast_shadows: bool,
    /// Depth-compare bias to fight shadow acne (in light clip space).
    #[serde(default = "default_shadow_bias")]
    pub shadow_bias: f32,
}

fn default_light_mode() -> LightMode {
    LightMode::Realtime
}

fn default_true_light() -> bool {
    true
}

fn default_shadow_bias() -> f32 {
    0.002
}

impl Default for LightComponent {
    fn default() -> Self {
        Self {
            kind: LightKind::Directional,
            mode: LightMode::Realtime,
            color: [1.0, 0.98, 0.92],
            intensity: 3.0,
            range: 10.0,
            spot_angle: 45.0,
            spot_blend: 0.15,
            cast_shadows: true,
            shadow_bias: 0.002,
        }
    }
}

impl TypedComponent for LightComponent {
    const NAME: &'static str = "Light";

    fn inspector_ui(&mut self, ui: &mut Ui) -> bool {
        let mut changed = false;
        ui.horizontal(|ui| {
            ui.label("Type");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                egui::ComboBox::from_id_salt("citrus-light-kind")
                    .selected_text(self.kind.label())
                    .show_ui(ui, |ui| {
                        for kind in LightKind::ALL {
                            changed |= ui
                                .selectable_value(&mut self.kind, kind, kind.label())
                                .changed();
                        }
                    });
            });
        });
        ui.horizontal(|ui| {
            ui.label("Mode");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                egui::ComboBox::from_id_salt("citrus-light-mode")
                    .selected_text(self.mode.label())
                    .show_ui(ui, |ui| {
                        for mode in LightMode::ALL {
                            changed |= ui
                                .selectable_value(&mut self.mode, mode, mode.label())
                                .changed();
                        }
                    });
            });
        });
        property_row(ui, "Color", &mut changed, |ui| {
            ui.color_edit_button_rgb(&mut self.color)
        });
        property_row(ui, "Intensity", &mut changed, |ui| {
            ui.add(
                DragValue::new(&mut self.intensity)
                    .speed(0.05)
                    .range(0.0..=100.0),
            )
        });
        if self.kind != LightKind::Directional {
            property_row(ui, "Range", &mut changed, |ui| {
                ui.add(
                    DragValue::new(&mut self.range)
                        .speed(0.1)
                        .range(0.01..=10000.0),
                )
            });
        }
        if self.kind == LightKind::Spot {
            property_row(ui, "Spot Angle", &mut changed, |ui| {
                ui.add(egui::Slider::new(&mut self.spot_angle, 1.0..=179.0).suffix("°"))
            });
            property_row(ui, "Spot Blend", &mut changed, |ui| {
                ui.add(egui::Slider::new(&mut self.spot_blend, 0.0..=1.0))
            });
        }
        property_row(ui, "Cast Shadows", &mut changed, |ui| {
            ui.checkbox(&mut self.cast_shadows, "")
        });
        if self.cast_shadows {
            property_row(ui, "Shadow Bias", &mut changed, |ui| {
                ui.add(
                    DragValue::new(&mut self.shadow_bias)
                        .speed(0.0005)
                        .range(0.0..=0.1)
                        .max_decimals(4),
                )
            });
        }
        changed
    }
}

/// Rotate around a local axis at constant speed.
#[derive(Serialize, Deserialize)]
pub struct Spin {
    pub axis: [f32; 3],
    pub degrees_per_second: f32,
}

impl Default for Spin {
    fn default() -> Self {
        Self {
            axis: [0.0, 1.0, 0.0],
            degrees_per_second: 45.0,
        }
    }
}

impl TypedComponent for Spin {
    const NAME: &'static str = "Spin";

    fn inspector_ui(&mut self, ui: &mut Ui) -> bool {
        let mut changed = false;
        axis_row(ui, "Axis", &mut self.axis, &mut changed);
        property_row(ui, "Speed (°/s)", &mut changed, |ui| {
            ui.add(DragValue::new(&mut self.degrees_per_second).speed(1.0))
        });
        changed
    }

    fn update(&mut self, ctx: &mut ComponentCtx) {
        let axis = Vec3::from(self.axis).normalize_or(Vec3::Y);
        let step = Quat::from_axis_angle(axis, self.degrees_per_second.to_radians() * ctx.dt);
        *ctx.rotation = (step * *ctx.rotation).normalize();
    }
}

/// Sine-wave hover along an axis.
#[derive(Serialize, Deserialize)]
pub struct Bob {
    pub axis: [f32; 3],
    pub amplitude: f32,
    /// Cycles per second.
    pub frequency: f32,
    /// Offset currently applied to the translation, so each frame can apply
    /// only the delta (keeps the authored position the wave's center).
    #[serde(skip)]
    applied: Vec3,
}

impl Default for Bob {
    fn default() -> Self {
        Self {
            axis: [0.0, 1.0, 0.0],
            amplitude: 0.5,
            frequency: 0.5,
            applied: Vec3::ZERO,
        }
    }
}

impl TypedComponent for Bob {
    const NAME: &'static str = "Bob";

    fn inspector_ui(&mut self, ui: &mut Ui) -> bool {
        let mut changed = false;
        axis_row(ui, "Axis", &mut self.axis, &mut changed);
        property_row(ui, "Amplitude", &mut changed, |ui| {
            ui.add(DragValue::new(&mut self.amplitude).speed(0.02))
        });
        property_row(ui, "Frequency (Hz)", &mut changed, |ui| {
            ui.add(DragValue::new(&mut self.frequency).speed(0.02))
        });
        changed
    }

    fn update(&mut self, ctx: &mut ComponentCtx) {
        let phase = ctx.time * self.frequency * std::f32::consts::TAU;
        let offset = Vec3::from(self.axis) * (self.amplitude * phase.sin());
        *ctx.translation += offset - self.applied;
        self.applied = offset;
    }
}

/// A box volume seeded with a regular grid of irradiance light probes. The
/// owning object's transform places/orients the box; `size` is its full extent
/// in local meters and `density` the number of probes per meter along each
/// axis. The bake samples incoming light at every grid point and stores SH
/// coefficients, which dynamic (non-static) objects interpolate at runtime.
/// Mirrors Unity's Light Probe Group, but grid-generated from a volume.
#[derive(Serialize, Deserialize)]
pub struct LightProbeVolume {
    /// Full box size in local meters (the grid spans -size/2 .. +size/2).
    pub size: [f32; 3],
    /// Probes per meter along each axis; the grid count is `size * density`,
    /// clamped to at least 2 per axis so trilinear interpolation has corners.
    pub density: f32,
}

impl Default for LightProbeVolume {
    fn default() -> Self {
        Self {
            size: [4.0, 3.0, 4.0],
            density: 1.0,
        }
    }
}

impl LightProbeVolume {
    /// Probe count along each axis (≥ 2 so a cell always has 8 corners).
    pub fn counts(&self) -> [usize; 3] {
        let mut out = [2usize; 3];
        for (i, c) in out.iter_mut().enumerate() {
            let n = (self.size[i].max(0.0) * self.density).round() as i64 + 1;
            *c = n.clamp(2, 256) as usize;
        }
        out
    }

    /// Total number of probes the volume resolves to.
    pub fn probe_count(&self) -> usize {
        let [x, y, z] = self.counts();
        x * y * z
    }

    /// Probe positions in the object's local space, ordered x-fastest then y
    /// then z (matches `counts()` for trilinear indexing).
    pub fn local_positions(&self) -> Vec<Vec3> {
        let [nx, ny, nz] = self.counts();
        let half = Vec3::from(self.size) * 0.5;
        let step = |n: usize, axis: usize| {
            if n > 1 {
                self.size[axis] / (n - 1) as f32
            } else {
                0.0
            }
        };
        let (sx, sy, sz) = (step(nx, 0), step(ny, 1), step(nz, 2));
        let mut out = Vec::with_capacity(nx * ny * nz);
        for iz in 0..nz {
            for iy in 0..ny {
                for ix in 0..nx {
                    out.push(Vec3::new(
                        -half.x + sx * ix as f32,
                        -half.y + sy * iy as f32,
                        -half.z + sz * iz as f32,
                    ));
                }
            }
        }
        out
    }
}

impl TypedComponent for LightProbeVolume {
    const NAME: &'static str = "Light Probe Volume";

    fn inspector_ui(&mut self, ui: &mut Ui) -> bool {
        let mut changed = false;
        axis_row(ui, "Size", &mut self.size, &mut changed);
        property_row(ui, "Density (/m)", &mut changed, |ui| {
            ui.add(
                DragValue::new(&mut self.density)
                    .speed(0.05)
                    .range(0.05..=16.0),
            )
        });
        let [x, y, z] = self.counts();
        ui.horizontal(|ui| {
            ui.label("Probes");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(format!("{x} × {y} × {z} = {}", x * y * z)).weak());
            });
        });
        changed
    }
}
