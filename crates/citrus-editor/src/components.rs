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

/// Deferred engine request a component can make from a lifecycle hook. The
/// engine applies these after the update pass finishes (so the scene isn't
/// mutated mid-iteration). This is the first slice of the in-game API
/// (Features.md 2D); grow the enum as more world control is exposed.
#[derive(Debug, Clone)]
pub enum ComponentCommand {
    /// Switch to another scene (level change, menu -> game). Path is
    /// project-relative, e.g. "scenes/level2.scene". Replaces the current scene.
    LoadScene(String),
}

/// Per-frame update context handed to components in Play mode. Transform
/// fields are the owning object's local TRS.
pub struct ComponentCtx<'a> {
    pub dt: f32,
    /// Seconds since engine start.
    pub time: f32,
    pub translation: &'a mut Vec3,
    pub rotation: &'a mut Quat,
    pub scale: &'a mut Vec3,
    /// Deferred engine requests; drained and applied after the update pass.
    pub commands: &'a mut Vec<ComponentCommand>,
}

impl ComponentCtx<'_> {
    /// Queue a scene switch (level change, menu -> game). Project-relative
    /// path. Applied after the current update pass.
    pub fn load_scene(&mut self, path: impl Into<String>) {
        self.commands.push(ComponentCommand::LoadScene(path.into()));
    }
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
        registry.register::<AudioSource>();
        registry.register::<AudioListener>();
        registry.register::<BoxCollider>();
        registry.register::<SphereCollider>();
        registry.register::<MeshCollider>();
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
        // CloseOnClickOutside so clicking/typing in the search box doesn't
        // dismiss the menu (only a click outside, or picking a component,
        // closes it).
        egui::containers::menu::MenuButton::new("➕ Add Component")
            .config(
                egui::containers::menu::MenuConfig::new()
                    .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside),
            )
            .ui(ui, |ui| {
                // Search box + scrollable list: the registry grows large once
                // plugins and built-ins (lights, etc.) pile up.
                let search_id = ui.make_persistent_id("citrus-add-component-search");
                let mut search: String = ui.data(|d| d.get_temp(search_id).unwrap_or_default());

                // Fixed, user-resizable popup size (persisted): the list area
                // keeps a constant height and width so typing in the search box
                // never shrinks the window as matches drop. The grip below
                // adjusts this.
                let size_id = ui.make_persistent_id("citrus-add-component-size");
                let mut size: egui::Vec2 =
                    ui.data(|d| d.get_temp(size_id).unwrap_or(egui::vec2(240.0, 240.0)));
                ui.set_min_width(size.x);
                ui.set_max_width(size.x);

                let edit = ui.add(
                    egui::TextEdit::singleline(&mut search)
                        .hint_text("🔍 Search…")
                        .desired_width(f32::INFINITY),
                );
                if edit.changed() {
                    ui.data_mut(|d| d.insert_temp(search_id, search.clone()));
                }
                let needle = search.to_lowercase();
                ui.separator();
                // auto_shrink off + max_height = size.y -> the viewport stays a
                // constant height regardless of how many rows match.
                ScrollArea::vertical()
                    .auto_shrink([false; 2])
                    .max_height(size.y)
                    .show(ui, |ui| {
                        let mut any = false;
                        for name in registry.names() {
                            // Camera is intrinsic to camera objects — not
                            // something you add to arbitrary objects.
                            if name == "Camera" {
                                continue;
                            }
                            if !needle.is_empty() && !name.to_lowercase().contains(&needle) {
                                continue;
                            }
                            any = true;
                            // Full-width rows: easy hit target.
                            if ui
                                .add(egui::Button::new(name).min_size(egui::vec2(
                                    ui.available_width(),
                                    0.0,
                                )))
                                .clicked()
                            {
                                response.add = Some(name);
                                ui.data_mut(|d| d.remove::<String>(search_id));
                                ui.close();
                            }
                        }
                        if !any {
                            ui.label(RichText::new("No matches").weak());
                        }
                    });

                // Bottom-right resize grip: drag to change the popup size.
                let (grip_rect, grip) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), 12.0),
                    Sense::drag(),
                );
                let corner = grip_rect.right_bottom();
                let col = ui.visuals().weak_text_color();
                for i in 1..=3 {
                    let o = i as f32 * 3.0;
                    ui.painter().line_segment(
                        [corner - egui::vec2(o, 1.0), corner - egui::vec2(1.0, o)],
                        egui::Stroke::new(1.0, col),
                    );
                }
                if grip.dragged() {
                    let d = grip.drag_delta();
                    size.x = (size.x + d.x).clamp(180.0, 600.0);
                    size.y = (size.y + d.y).clamp(120.0, 600.0);
                    ui.data_mut(|d| d.insert_temp(size_id, size));
                }
                grip.on_hover_cursor(egui::CursorIcon::ResizeNwSe);
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

/// How a spatial audio source quietens with distance between min and max.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum AudioRolloff {
    /// Volume falls off linearly from min_distance (1.0) to max_distance (0.0).
    Linear,
    /// Inverse-distance falloff (more natural; quiet tail).
    Logarithmic,
}

impl AudioRolloff {
    pub fn label(self) -> &'static str {
        match self {
            Self::Linear => "Linear",
            Self::Logarithmic => "Logarithmic",
        }
    }
    pub const ALL: [AudioRolloff; 2] = [Self::Linear, Self::Logarithmic];
}

/// A sound emitter. `spatial` sources attenuate with distance to the
/// AudioListener (min/max + rolloff); non-spatial (2D) sources play at a
/// constant volume regardless of position. Clips are project-relative
/// `.wav` / `.flac` / `.mp3` files. Playback is driven by the engine's audio
/// system in Play mode.
#[derive(Serialize, Deserialize)]
pub struct AudioSource {
    /// Project-relative path to a .wav / .flac / .mp3 clip.
    pub clip: String,
    pub play_on_start: bool,
    pub looping: bool,
    /// Base volume (0..=1, but higher is allowed for boosting).
    pub volume: f32,
    /// Playback speed / pitch multiplier (1.0 = original).
    pub pitch: f32,
    /// 3D (distance-attenuated) vs 2D (constant volume).
    pub spatial: bool,
    /// Within this distance the source is at full volume.
    pub min_distance: f32,
    /// Beyond this distance the source is silent.
    pub max_distance: f32,
    pub rolloff: AudioRolloff,
}

impl Default for AudioSource {
    fn default() -> Self {
        Self {
            clip: String::new(),
            play_on_start: true,
            looping: false,
            volume: 1.0,
            pitch: 1.0,
            spatial: true,
            min_distance: 1.0,
            max_distance: 20.0,
            rolloff: AudioRolloff::Logarithmic,
        }
    }
}

impl TypedComponent for AudioSource {
    const NAME: &'static str = "Audio Source";

    fn inspector_ui(&mut self, ui: &mut Ui) -> bool {
        let mut changed = false;
        ui.horizontal(|ui| {
            ui.label("Clip");
            changed |= ui
                .add(
                    egui::TextEdit::singleline(&mut self.clip)
                        .hint_text("audio/foo.wav")
                        .desired_width(f32::INFINITY),
                )
                .changed();
        });
        ui.label(
            RichText::new(".wav · .flac · .mp3 (project-relative)")
                .small()
                .weak(),
        );
        property_row(ui, "Play on Start", &mut changed, |ui| {
            ui.checkbox(&mut self.play_on_start, "")
        });
        property_row(ui, "Loop", &mut changed, |ui| {
            ui.checkbox(&mut self.looping, "")
        });
        property_row(ui, "Volume", &mut changed, |ui| {
            ui.add(egui::Slider::new(&mut self.volume, 0.0..=2.0))
        });
        property_row(ui, "Pitch", &mut changed, |ui| {
            ui.add(egui::Slider::new(&mut self.pitch, 0.25..=4.0))
        });
        property_row(ui, "Spatial (3D)", &mut changed, |ui| {
            ui.checkbox(&mut self.spatial, "")
        });
        if self.spatial {
            property_row(ui, "Min Distance", &mut changed, |ui| {
                ui.add(DragValue::new(&mut self.min_distance).speed(0.1).range(0.0..=10000.0))
            });
            property_row(ui, "Max Distance", &mut changed, |ui| {
                ui.add(DragValue::new(&mut self.max_distance).speed(0.5).range(0.0..=10000.0))
            });
            if self.max_distance < self.min_distance + 0.01 {
                self.max_distance = self.min_distance + 0.01;
            }
            ui.horizontal(|ui| {
                ui.label("Rolloff");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    egui::ComboBox::from_id_salt("citrus-audio-rolloff")
                        .selected_text(self.rolloff.label())
                        .show_ui(ui, |ui| {
                            for r in AudioRolloff::ALL {
                                changed |= ui
                                    .selectable_value(&mut self.rolloff, r, r.label())
                                    .changed();
                            }
                        });
                });
            });
        }
        changed
    }
}

/// Marks the object whose transform is the "ears" for spatial audio (usually
/// the main camera). The first active listener wins; if none exists the
/// engine falls back to the editor camera.
#[derive(Serialize, Deserialize, Default)]
pub struct AudioListener {}

impl TypedComponent for AudioListener {
    const NAME: &'static str = "Audio Listener";

    fn inspector_ui(&mut self, ui: &mut Ui) -> bool {
        ui.label(
            RichText::new("Ears for spatial audio (put this on the main camera).")
                .small()
                .weak(),
        );
        false
    }
}

// ----------------------------------------------------------- colliders

/// Number of collision layers (mirrors the layer-collision matrix that the
/// physics engine consults; see the physics TODO).
pub const COLLISION_LAYERS: u32 = 32;

/// Shared collider inspector rows: trigger flag + collision layer.
fn collider_common_ui(ui: &mut Ui, is_trigger: &mut bool, layer: &mut u32, changed: &mut bool) {
    property_row(ui, "Is Trigger", changed, |ui| {
        ui.checkbox(is_trigger, "")
            .on_hover_text("Detect overlaps without blocking (no collision response)")
    });
    property_row(ui, "Layer", changed, |ui| {
        let r = ui.add(egui::DragValue::new(layer).range(0..=COLLISION_LAYERS - 1));
        *layer = (*layer).min(COLLISION_LAYERS - 1);
        r
    });
}

/// Axis-aligned (in object space) box collision zone. `center` offsets it from
/// the object origin; `size` is the full extent. No physics solver yet — this
/// is authoring data the physics engine (TODO) will consume.
#[derive(Serialize, Deserialize)]
pub struct BoxCollider {
    pub center: [f32; 3],
    pub size: [f32; 3],
    pub is_trigger: bool,
    pub layer: u32,
}

impl Default for BoxCollider {
    fn default() -> Self {
        Self {
            center: [0.0; 3],
            size: [1.0; 3],
            is_trigger: false,
            layer: 0,
        }
    }
}

impl TypedComponent for BoxCollider {
    const NAME: &'static str = "Box Collider";

    fn inspector_ui(&mut self, ui: &mut Ui) -> bool {
        let mut changed = false;
        axis_row(ui, "Center", &mut self.center, &mut changed);
        axis_row(ui, "Size", &mut self.size, &mut changed);
        for s in &mut self.size {
            *s = s.max(0.01);
        }
        collider_common_ui(ui, &mut self.is_trigger, &mut self.layer, &mut changed);
        changed
    }
}

/// Sphere collision zone (center offset + radius).
#[derive(Serialize, Deserialize)]
pub struct SphereCollider {
    pub center: [f32; 3],
    pub radius: f32,
    pub is_trigger: bool,
    pub layer: u32,
}

impl Default for SphereCollider {
    fn default() -> Self {
        Self {
            center: [0.0; 3],
            radius: 0.5,
            is_trigger: false,
            layer: 0,
        }
    }
}

impl TypedComponent for SphereCollider {
    const NAME: &'static str = "Sphere Collider";

    fn inspector_ui(&mut self, ui: &mut Ui) -> bool {
        let mut changed = false;
        axis_row(ui, "Center", &mut self.center, &mut changed);
        property_row(ui, "Radius", &mut changed, |ui| {
            ui.add(DragValue::new(&mut self.radius).speed(0.05).range(0.01..=10000.0))
        });
        collider_common_ui(ui, &mut self.is_trigger, &mut self.layer, &mut changed);
        changed
    }
}

/// Collision from the object's own mesh. `convex` builds a convex hull (needed
/// for dynamic bodies); otherwise the exact triangle mesh is used (static
/// only). Follows the mesh, so there's no editable size.
#[derive(Serialize, Deserialize)]
pub struct MeshCollider {
    pub convex: bool,
    pub is_trigger: bool,
    pub layer: u32,
}

impl Default for MeshCollider {
    fn default() -> Self {
        Self {
            convex: false,
            is_trigger: false,
            layer: 0,
        }
    }
}

impl TypedComponent for MeshCollider {
    const NAME: &'static str = "Mesh Collider";

    fn inspector_ui(&mut self, ui: &mut Ui) -> bool {
        let mut changed = false;
        property_row(ui, "Convex", &mut changed, |ui| {
            ui.checkbox(&mut self.convex, "")
                .on_hover_text("Convex hull (required for dynamic bodies); off = exact triangles (static only)")
        });
        collider_common_ui(ui, &mut self.is_trigger, &mut self.layer, &mut changed);
        ui.label(
            RichText::new("Uses this object's mesh as the collision shape.")
                .small()
                .weak(),
        );
        changed
    }
}
