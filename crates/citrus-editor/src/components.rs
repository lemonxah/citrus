//! Editor-side component UI: the [`Inspect`] (inspector) and [`Gizmo`]
//! (viewport drawing) traits, the [`EditorComponents`] dispatch registry that
//! maps a component name to its inspector/gizmo, the built-in impls, and the
//! `components_ui` inspector list.
//!
//! The runtime component API (the `Component`/`TypedComponent` traits, the
//! registry, the component structs, `ComponentCtx`, `Transform`) lives in
//! `citrus-core` and is egui-free. This module is editor-only; nothing here
//! ships in a built game.

use std::collections::HashMap;

use citrus_core::{
    AudioListener, AudioRolloff, AudioSource, Bob, BodyKind, BoxCollider, CameraComponent,
    Component, ComponentRegistry, ControlMode, FluxVolume, FluxVrLight, LightComponent, LightKind,
    LightMode, LightProbeVolume, MeshCollider, ObjectId, ObjectRef, Pawn, ReflectionProbe,
    RigidBody, ShadowType,
    Spin,
    SpawnPoint, SphereCollider, Sync, TypedComponent, VolumeComponent,
    COLLISION_LAYERS,
};
use egui::collapsing_header::{CollapsingState, paint_default_icon};
use egui::{DragValue, Label, RichText, ScrollArea, Sense, Ui};

/// egui-memory key under which the Scene tree publishes the index of the object
/// currently being dragged (a `usize`, so its slot is shared across the
/// plugin/egui dylib boundary). `ObjectRef` drop targets read it.
pub const DRAG_OBJECT_KEY: &str = "citrus-drag-object";
/// egui-memory key holding the project-relative path of the file currently
/// dragged from the file browser (a `String`, shared across the dylib boundary;
/// same plugin-safe pattern as [`DRAG_OBJECT_KEY`]). Drop targets read it.
pub const DRAG_FILE_KEY: &str = "citrus-drag-file";

/// Context handed to a component's inspector: the scene's object list (for
/// `ObjectRef` pickers) plus a helper to draw the picker.
pub struct InspectCtx<'a> {
    /// (id, display name) of every object in the scene.
    pub objects: &'a [(ObjectId, String)],
}

impl InspectCtx<'_> {
    /// Draw an object-reference row as a drop target: drag an object from
    /// the Scene tree onto the box to set it; the ✕ clears it. Returns true if
    /// the reference changed.
    ///
    /// Deliberately avoids egui's drag-and-drop API: that goes through egui's
    /// `DragAndDrop` context plugin, which panics when called from a plugin's
    /// own (separately-linked) egui. Same `TypeId`-mismatch class as the
    /// label-selection crash. Instead the Scene tree publishes the dragged
    /// object index into egui memory as a `usize` (a std type, so its slot is
    /// shared across the dylib boundary), and we detect the drop with raw
    /// pointer state.
    pub fn object_ref(&self, ui: &mut Ui, label: &str, target: &mut ObjectRef) -> bool {
        let mut changed = false;
        let name = target
            .id()
            .and_then(|id| self.objects.iter().find(|(i, _)| *i == id))
            .map(|(_, n)| n.clone());
        let text = match &name {
            Some(n) => n.clone(),
            None if target.is_set() => "(missing)".to_owned(),
            None => "(drag an object here)".to_owned(),
        };
        let dragging: Option<usize> = ui.data(|d| d.get_temp(egui::Id::new(DRAG_OBJECT_KEY)));
        ui.horizontal(|ui| {
            ui.label(label);
            let inner = egui::Frame::group(ui.style())
                .inner_margin(egui::Margin::symmetric(6, 2))
                .show(ui, |ui| {
                    ui.set_min_width(120.0);
                    let mut t = RichText::new(text);
                    if name.is_none() {
                        t = t.weak();
                    }
                    ui.label(t);
                });
            let rect = inner.response.rect;
            let hovering = ui.rect_contains_pointer(rect);
            // Highlight while a drag is in flight and the pointer is over us.
            if dragging.is_some() && hovering {
                ui.painter().rect_stroke(
                    rect,
                    4.0,
                    egui::Stroke::new(2.0, ui.visuals().selection.stroke.color),
                    egui::StrokeKind::Inside,
                );
            }
            // Drop = pointer released over the box with a dragged object pending.
            if hovering
                && ui.input(|i| i.pointer.any_released())
                && let Some(idx) = dragging
                && let Some((id, _)) = self.objects.get(idx)
            {
                *target = ObjectRef::new(*id);
                changed = true;
            }
            if target.is_set() && ui.small_button("✕").on_hover_text("Clear").clicked() {
                *target = ObjectRef::NONE;
                changed = true;
            }
        });
        changed
    }

    /// An asset-path field that's also a drop target: drag a file from the
    /// file browser onto it to set `value` to that file's project-relative path.
    /// `exts` (lowercase, no dot) restricts which files are accepted; empty =
    /// any. Returns true if the value changed. Uses the same plugin-safe memory
    /// pattern as [`object_ref`] (the file browser publishes the dragged path
    /// into egui memory as a `String`).
    pub fn file_field(
        &self,
        ui: &mut Ui,
        label: &str,
        value: &mut String,
        hint: &str,
        exts: &[&str],
    ) -> bool {
        let mut changed = false;
        let dragging: Option<String> = ui.data(|d| d.get_temp(egui::Id::new(DRAG_FILE_KEY)));
        let droppable = dragging.as_ref().is_some_and(|p| {
            exts.is_empty()
                || std::path::Path::new(p)
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.to_ascii_lowercase())
                    .is_some_and(|e| exts.contains(&e.as_str()))
        });
        ui.horizontal(|ui| {
            ui.label(label);
            let edit = ui.add(
                egui::TextEdit::singleline(value)
                    .hint_text(hint)
                    .desired_width(f32::INFINITY),
            );
            changed |= edit.changed();
            // Highlight while a compatible file is dragged over the field.
            if droppable && ui.rect_contains_pointer(edit.rect) {
                ui.painter().rect_stroke(
                    edit.rect,
                    2.0,
                    egui::Stroke::new(2.0, ui.visuals().selection.stroke.color),
                    egui::StrokeKind::Inside,
                );
                if ui.input(|i| i.pointer.any_released())
                    && let Some(p) = &dragging
                {
                    *value = p.clone();
                    changed = true;
                }
            }
        });
        changed
    }
}

/// Editor inspector for a component: draw widgets, return true if anything
/// changed. Implemented per component in the editor (or a plugin), keeping the
/// runtime `Component` egui-free.
pub trait Inspect {
    fn inspector_ui(&mut self, ui: &mut Ui, ctx: &InspectCtx) -> bool;
}

/// A viewport gizmo a component declares. The editor owns drawing + interaction
/// for each kind (wireframe box with resize handles, an interior point cloud, a
/// range sphere); a component just lists which it wants. Coordinates are in the
/// owning object's LOCAL space (the editor applies the object world transform).
#[derive(Clone)]
pub enum GizmoSpec {
    /// Resizable box. `object_anchored` = box is centered on the object origin
    /// (resize moves the OBJECT to keep the opposite face fixed); otherwise it
    /// has its own local `center` (resize moves that center, e.g. a collider).
    /// `blend` draws a dimmer inner shell (reflection-probe blend distance; 0 = none).
    Box {
        center: glam::Vec3,
        half_extents: glam::Vec3,
        object_anchored: bool,
        blend: f32,
        color: egui::Color32,
    },
    /// A cloud of interior points (probe grid), display-only.
    Points {
        positions: Vec<glam::Vec3>,
        color: egui::Color32,
    },
    /// A resizable range sphere (radius handle on the camera-right side).
    Range {
        center: glam::Vec3,
        radius: f32,
        color: egui::Color32,
    },
}

/// The result of dragging a gizmo handle, applied back to the component via
/// [`Gizmo::apply_gizmo_edit`]. `index` is the position in the component's
/// `gizmos()` list. Values are in the object's LOCAL space.
#[derive(Clone, Copy, Debug)]
pub enum GizmoEdit {
    /// Box resize: new half-extents (+ new local center for non-object-anchored
    /// boxes; object-anchored boxes get `center` unchanged and the editor moves
    /// the object instead).
    Box {
        index: usize,
        half_extents: glam::Vec3,
        center: glam::Vec3,
    },
    /// Range-sphere resize: new radius.
    Range { index: usize, radius: f32 },
}

/// Editor viewport gizmos for a component. Default = none. A component lists the
/// gizmos it wants via [`gizmos`](Gizmo::gizmos) and writes back resize edits via
/// [`apply_gizmo_edit`](Gizmo::apply_gizmo_edit). Plugins implement this to get
/// the same handles as the built-ins.
pub trait Gizmo {
    fn gizmos(&self) -> Vec<GizmoSpec> {
        Vec::new()
    }
    fn apply_gizmo_edit(&mut self, _edit: GizmoEdit) {}
}

/// Probe/voxel point cloud for a `Points` gizmo, capped for display: strided
/// down to a budget so a DENSE volume still shows a representative grid instead
/// of nothing (the old hard cap just hid all points past ~4096). `None` only for
/// an empty or absurdly large grid (generation guard). `positions` is generated
/// lazily so the guard avoids building a huge vec.
fn display_points(count: usize, positions: impl FnOnce() -> Vec<glam::Vec3>) -> Option<Vec<glam::Vec3>> {
    const GEN_CAP: usize = 300_000; // don't even build a vec larger than this
    const BUDGET: usize = 16_384; // max points actually drawn
    if count == 0 || count > GEN_CAP {
        return None;
    }
    let mut p = positions();
    if p.len() > BUDGET {
        let stride = p.len().div_ceil(BUDGET);
        p = p.into_iter().step_by(stride).collect();
    }
    Some(p)
}

type InspectFn = fn(&mut dyn Component, &mut Ui, &InspectCtx) -> bool;
type GizmosFn = fn(&dyn Component) -> Vec<GizmoSpec>;
type GizmoEditFn = fn(&mut dyn Component, GizmoEdit);

/// Editor-side dispatch: component name -> inspector / gizmo. Built-ins
/// register in [`Self::with_builtins`]; plugins register through their
/// `citrus_register_editor` export.
#[derive(Default)]
pub struct EditorComponents {
    inspect: HashMap<&'static str, InspectFn>,
    gizmos: HashMap<&'static str, GizmosFn>,
    gizmo_edit: HashMap<&'static str, GizmoEditFn>,
}

impl EditorComponents {
    pub fn with_builtins() -> Self {
        let mut e = Self::default();
        e.register::<CameraComponent>();
        e.register::<LightComponent>();
        e.register::<LightProbeVolume>();
        e.register::<ReflectionProbe>();
        e.register::<FluxVrLight>();
        e.register::<FluxVolume>();
        e.register::<AudioSource>();
        e.register::<AudioListener>();
        e.register::<BoxCollider>();
        e.register::<SphereCollider>();
        e.register::<MeshCollider>();
        e.register::<Spin>();
        e.register::<Bob>();
        e.register::<RigidBody>();
        e.register::<VolumeComponent>();
        e.register::<Pawn>();
        e.register::<SpawnPoint>();
        e.register::<Sync>();
        e
    }

    /// Register a component's editor traits. Re-registering replaces (plugin
    /// hot-reload). Requires the runtime [`TypedComponent`] (for the name) plus
    /// the editor [`Inspect`] + [`Gizmo`] impls.
    pub fn register<T: TypedComponent + Inspect + Gizmo>(&mut self) {
        self.inspect.insert(T::NAME, |c, ui, ictx| {
            c.as_any_mut()
                .downcast_mut::<T>()
                .is_some_and(|t| t.inspector_ui(ui, ictx))
        });
        self.gizmos.insert(T::NAME, |c| {
            c.as_any()
                .downcast_ref::<T>()
                .map(|t| t.gizmos())
                .unwrap_or_default()
        });
        self.gizmo_edit.insert(T::NAME, |c, edit| {
            if let Some(t) = c.as_any_mut().downcast_mut::<T>() {
                t.apply_gizmo_edit(edit);
            }
        });
    }

    /// A component's declared viewport gizmos (empty if unregistered/none).
    pub fn gizmos(&self, component: &dyn Component) -> Vec<GizmoSpec> {
        match self.gizmos.get(component.type_name()) {
            Some(f) => f(component),
            None => Vec::new(),
        }
    }

    /// Apply a gizmo resize edit back to a component.
    pub fn apply_gizmo_edit(&self, component: &mut dyn Component, edit: GizmoEdit) {
        if let Some(f) = self.gizmo_edit.get(component.type_name()) {
            f(component, edit);
        }
    }

    /// Draw a component's inspector (no-op if unregistered).
    pub fn inspect(&self, component: &mut dyn Component, ui: &mut Ui, ctx: &InspectCtx) -> bool {
        match self.inspect.get(component.type_name()) {
            Some(f) => f(component, ui, ctx),
            None => false,
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

/// Component list + Add Component button (object inspector). Inspectors are
/// dispatched through `editor` so the runtime `Component` stays egui-free.
pub fn components_ui(
    ui: &mut Ui,
    components: &mut [Box<dyn Component>],
    registry: &ComponentRegistry,
    editor: &EditorComponents,
    objects: &[(ObjectId, String)],
) -> ComponentsResponse {
    let mut response = ComponentsResponse::default();
    let inspect_ctx = InspectCtx { objects };
    // Plugin components (cdylibs) statically link their own copy of egui, so a
    // selectable label drawn from a plugin's egui panics (mismatched TypeId for
    // egui's selection state). Disable selection for the whole inspector list.
    ui.style_mut().interaction.selectable_labels = false;
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
            response.changed |= editor.inspect(component.as_mut(), ui, &inspect_ctx);
        });
        state.store(ui.ctx());
    }

    ui.add_space(4.0);
    ui.vertical_centered_justified(|ui| {
        egui::containers::menu::MenuButton::new("➕ Add Component")
            .config(
                egui::containers::menu::MenuConfig::new()
                    .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside),
            )
            .ui(ui, |ui| {
                let search_id = ui.make_persistent_id("citrus-add-component-search");
                let mut search: String = ui.data(|d| d.get_temp(search_id).unwrap_or_default());

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
                ScrollArea::vertical()
                    .auto_shrink([false; 2])
                    .max_height(size.y)
                    .show(ui, |ui| {
                        let mut any = false;
                        for name in registry.names() {
                            // Camera is intrinsic to camera objects.
                            if name == "Camera" {
                                continue;
                            }
                            if !needle.is_empty() && !name.to_lowercase().contains(&needle) {
                                continue;
                            }
                            any = true;
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

                let (grip_rect, grip) =
                    ui.allocate_exact_size(egui::vec2(ui.available_width(), 12.0), Sense::drag());
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

// ----------------------------------------------------------------- helpers

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

// ----------------------------------------------------------- builtin inspect

impl Inspect for CameraComponent {
    fn inspector_ui(&mut self, ui: &mut Ui, _ctx: &InspectCtx) -> bool {
        let mut changed = false;
        property_row(ui, "Camera ID", &mut changed, |ui| {
            ui.add_enabled(false, DragValue::new(&mut self.id))
        });
        property_row(ui, "Field of View", &mut changed, |ui| {
            ui.add(egui::Slider::new(&mut self.fov_deg, 10.0..=170.0).suffix("°"))
        });
        property_row(ui, "Near", &mut changed, |ui| {
            ui.add(DragValue::new(&mut self.near).speed(0.01).range(0.001..=1000.0))
        });
        property_row(ui, "Far", &mut changed, |ui| {
            ui.add(DragValue::new(&mut self.far).speed(0.5).range(0.01..=100000.0))
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
impl Gizmo for CameraComponent {}

impl Inspect for LightComponent {
    fn inspector_ui(&mut self, ui: &mut Ui, _ctx: &InspectCtx) -> bool {
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
            ui.add(DragValue::new(&mut self.intensity).speed(0.05).range(0.0..=100.0))
        });
        let (lm, unit) = self.approx_photometric();
        ui.horizontal(|ui| {
            ui.label("");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(format!("≈ {lm:.0} {unit}")).weak().small())
                    .on_hover_text("Approximate photometric output (display only)");
            });
        });
        if self.kind != LightKind::Directional {
            property_row(ui, "Range", &mut changed, |ui| {
                ui.add(DragValue::new(&mut self.range).speed(0.1).range(0.01..=10000.0))
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
        ui.horizontal(|ui| {
            ui.label("Shadows");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                egui::ComboBox::from_id_salt("citrus-light-shadow")
                    .selected_text(self.shadow_type.label())
                    .show_ui(ui, |ui| {
                        for st in ShadowType::ALL {
                            changed |= ui
                                .selectable_value(&mut self.shadow_type, st, st.label())
                                .changed();
                        }
                    });
            });
        });
        if self.shadow_type.casts() {
            property_row(ui, "Shadow Bias", &mut changed, |ui| {
                ui.add(
                    DragValue::new(&mut self.shadow_bias)
                        .speed(0.0005)
                        .range(0.0..=0.1)
                        .max_decimals(4),
                )
            });
            property_row(ui, "Radius (soft)", &mut changed, |ui| {
                ui.add(DragValue::new(&mut self.radius).speed(0.02).range(0.0..=5.0))
                    .on_hover_text(
                        "Light source size for baked soft shadows — larger = softer \
                         penumbra (smooths jagged shadows at low texel density). 0 = hard.",
                    )
            });
        }
        changed
    }
}
impl Gizmo for LightComponent {}

impl Inspect for VolumeComponent {
    fn inspector_ui(&mut self, ui: &mut Ui, ctx: &InspectCtx) -> bool {
        let mut changed = false;
        // Drag a .postfx from the file browser onto this field to set it.
        changed |= ctx.file_field(
            ui,
            "Profile",
            &mut self.profile,
            "drag a .postfx here",
            &["postfx"],
        );
        property_row(ui, "Global", &mut changed, |ui| {
            ui.checkbox(&mut self.global, "")
                .on_hover_text("Affects the whole scene (else only within the box bounds)")
        });
        property_row(ui, "Priority", &mut changed, |ui| {
            ui.add(DragValue::new(&mut self.priority).speed(0.1))
                .on_hover_text("Higher blends last / wins ties")
        });
        property_row(ui, "Weight", &mut changed, |ui| {
            ui.add(egui::Slider::new(&mut self.weight, 0.0..=1.0))
        });
        if !self.global {
            property_row(ui, "Blend Distance", &mut changed, |ui| {
                ui.add(DragValue::new(&mut self.blend_distance).speed(0.05).range(0.0..=100.0))
                    .on_hover_text("World-units the effect fades in over, approaching the box")
            });
            axis_row(ui, "Half Extents", &mut self.half_extents, &mut changed);
        }
        changed
    }
}
impl Gizmo for VolumeComponent {}

impl Inspect for Spin {
    fn inspector_ui(&mut self, ui: &mut Ui, _ctx: &InspectCtx) -> bool {
        let mut changed = false;
        axis_row(ui, "Axis", &mut self.axis, &mut changed);
        property_row(ui, "Speed (°/s)", &mut changed, |ui| {
            ui.add(DragValue::new(&mut self.degrees_per_second).speed(1.0))
        });
        changed
    }
}
impl Gizmo for Spin {}

impl Inspect for Bob {
    fn inspector_ui(&mut self, ui: &mut Ui, _ctx: &InspectCtx) -> bool {
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
}
impl Gizmo for Bob {}

impl Inspect for RigidBody {
    fn inspector_ui(&mut self, ui: &mut Ui, _ctx: &InspectCtx) -> bool {
        let mut changed = false;
        property_row(ui, "Type", &mut changed, |ui| {
            let mut resp = egui::ComboBox::from_id_salt("citrus-body-kind")
                .selected_text(self.kind.label())
                .show_ui(ui, |ui| {
                    let mut clicked = false;
                    for k in BodyKind::ALL {
                        clicked |= ui
                            .selectable_value(&mut self.kind, k, k.label())
                            .clicked();
                    }
                    clicked
                });
            if resp.inner == Some(true) {
                resp.response.mark_changed();
            }
            resp.response
        });
        if self.kind == BodyKind::Dynamic {
            property_row(ui, "Mass (kg)", &mut changed, |ui| {
                ui.add(DragValue::new(&mut self.mass).speed(0.1).range(0.0..=1e6))
            });
            property_row(ui, "Gravity scale", &mut changed, |ui| {
                ui.add(DragValue::new(&mut self.gravity_scale).speed(0.05))
            });
        }
        property_row(ui, "Restitution", &mut changed, |ui| {
            ui.add(DragValue::new(&mut self.restitution).speed(0.02).range(0.0..=1.0))
        });
        property_row(ui, "Friction", &mut changed, |ui| {
            ui.add(DragValue::new(&mut self.friction).speed(0.02).range(0.0..=2.0))
        });
        changed
    }
}
impl Gizmo for RigidBody {}

impl Inspect for LightProbeVolume {
    fn inspector_ui(&mut self, ui: &mut Ui, _ctx: &InspectCtx) -> bool {
        let mut changed = false;
        axis_row(ui, "Size", &mut self.size, &mut changed);
        property_row(ui, "Density (/m)", &mut changed, |ui| {
            ui.add(DragValue::new(&mut self.density).speed(0.05).range(0.05..=16.0))
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
impl Gizmo for LightProbeVolume {
    fn gizmos(&self) -> Vec<GizmoSpec> {
        let mut g = vec![GizmoSpec::Box {
            center: glam::Vec3::ZERO,
            half_extents: glam::Vec3::from(self.size) * 0.5,
            object_anchored: true,
            blend: 0.0,
            color: egui::Color32::from_rgb(120, 200, 255),
        }];
        if let Some(positions) = display_points(self.probe_count(), || self.local_positions()) {
            g.push(GizmoSpec::Points {
                positions,
                color: egui::Color32::from_rgb(180, 225, 255),
            });
        }
        g
    }
    fn apply_gizmo_edit(&mut self, edit: GizmoEdit) {
        if let GizmoEdit::Box { half_extents, .. } = edit {
            self.size = (half_extents * 2.0).to_array();
        }
    }
}

impl Inspect for ReflectionProbe {
    fn inspector_ui(&mut self, ui: &mut Ui, _ctx: &InspectCtx) -> bool {
        let mut changed = false;
        axis_row(ui, "Size", &mut self.size, &mut changed);
        ui.horizontal(|ui| {
            ui.label("Resolution");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let res_label = |r: u32| match r {
                    8192 => "8K".to_string(),
                    4096 => "4K".to_string(),
                    2048 => "2K".to_string(),
                    1024 => "1K".to_string(),
                    other => format!("{other}"),
                };
                egui::ComboBox::from_id_salt("citrus-refl-probe-res")
                    .selected_text(res_label(self.resolution))
                    .show_ui(ui, |ui| {
                        for r in [8192u32, 4096, 2048, 1024, 512, 256, 128, 64, 32] {
                            changed |= ui
                                .selectable_value(&mut self.resolution, r, res_label(r))
                                .changed();
                        }
                    });
            });
        });
        if self.resolution >= 4096 {
            ui.label(
                RichText::new("High resolutions cost a lot of VRAM and bake time (8K ≈ GBs).")
                    .small()
                    .color(egui::Color32::from_rgb(220, 170, 110)),
            );
        }
        ui.horizontal(|ui| {
            ui.label("Mode");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                use citrus_core::ReflectionProbeMode as M;
                egui::ComboBox::from_id_salt("citrus-refl-probe-mode")
                    .selected_text(match self.mode {
                        M::Realtime => "Realtime",
                        M::Baked => "Baked",
                    })
                    .show_ui(ui, |ui| {
                        changed |= ui
                            .selectable_value(&mut self.mode, M::Realtime, "Realtime")
                            .changed();
                        changed |= ui
                            .selectable_value(&mut self.mode, M::Baked, "Baked")
                            .changed();
                    });
            });
        });
        if ui
            .button("✨ Bake This Probe")
            .on_hover_text("Capture this probe's cubemap from the current scene and save it as a .reflprobe sidecar")
            .clicked()
        {
            self.bake_now = true;
            // Not a serialized edit, but return changed so the engine's per-frame
            // poll runs this frame.
            changed = true;
        }
        property_row(ui, "Intensity", &mut changed, |ui| {
            ui.add(egui::Slider::new(&mut self.intensity, 0.0..=2.0))
        });
        property_row(ui, "Blend distance", &mut changed, |ui| {
            ui.add(egui::Slider::new(&mut self.blend_distance, 0.0..=10.0).suffix(" m"))
        });
        property_row(ui, "Importance", &mut changed, |ui| {
            ui.add(egui::DragValue::new(&mut self.importance).range(-8..=8))
        });
        property_row(ui, "Box projection", &mut changed, |ui| {
            ui.checkbox(&mut self.box_projection, "")
        });
        ui.label(
            RichText::new(
                "Box-projected cubemap reflection. Blend distance fades the probe to the \
                 skybox (and to overlapping probes) inside the box edge; importance breaks \
                 overlap ties. Recapture/bake from FluxBaker after editing the scene.",
            )
            .small()
            .weak(),
        );
        changed
    }
}
impl Gizmo for ReflectionProbe {
    fn gizmos(&self) -> Vec<GizmoSpec> {
        vec![GizmoSpec::Box {
            center: glam::Vec3::ZERO,
            half_extents: glam::Vec3::from(self.size) * 0.5,
            object_anchored: true,
            blend: self.blend_distance.max(0.0),
            color: egui::Color32::from_rgb(150, 220, 255),
        }]
    }
    fn apply_gizmo_edit(&mut self, edit: GizmoEdit) {
        if let GizmoEdit::Box { half_extents, .. } = edit {
            self.size = (half_extents * 2.0).to_array();
        }
    }
}

impl Inspect for FluxVrLight {
    fn inspector_ui(&mut self, ui: &mut Ui, _ctx: &InspectCtx) -> bool {
        let mut changed = false;
        ui.horizontal(|ui| {
            ui.label("Color");
            changed |= ui.color_edit_button_rgb(&mut self.color).changed();
        });
        property_row(ui, "Intensity", &mut changed, |ui| {
            ui.add(DragValue::new(&mut self.intensity).speed(0.05).range(0.0..=64.0))
        });
        property_row(ui, "Range (m)", &mut changed, |ui| {
            ui.add(DragValue::new(&mut self.range).speed(0.1).range(0.1..=256.0))
        });
        changed
    }
}
impl Gizmo for FluxVrLight {}

impl Inspect for FluxVolume {
    fn inspector_ui(&mut self, ui: &mut Ui, _ctx: &InspectCtx) -> bool {
        let mut changed = false;
        axis_row(ui, "Size", &mut self.size, &mut changed);
        property_row(ui, "Density (/m)", &mut changed, |ui| {
            ui.add(DragValue::new(&mut self.density).speed(0.05).range(0.05..=8.0))
        });
        let [x, y, z] = self.counts();
        ui.horizontal(|ui| {
            ui.label("Voxels");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(format!("{x} × {y} × {z} = {}", x * y * z)).weak());
            });
        });
        changed
    }
}
impl Gizmo for FluxVolume {
    fn gizmos(&self) -> Vec<GizmoSpec> {
        let mut g = vec![GizmoSpec::Box {
            center: glam::Vec3::ZERO,
            half_extents: glam::Vec3::from(self.size) * 0.5,
            object_anchored: true,
            blend: 0.0,
            color: egui::Color32::from_rgb(170, 140, 255),
        }];
        if let Some(positions) = display_points(self.probe_count(), || self.local_positions()) {
            g.push(GizmoSpec::Points {
                positions,
                color: egui::Color32::from_rgb(200, 170, 255),
            });
        }
        g
    }
    fn apply_gizmo_edit(&mut self, edit: GizmoEdit) {
        if let GizmoEdit::Box { half_extents, .. } = edit {
            self.size = (half_extents * 2.0).to_array();
        }
    }
}

impl Inspect for AudioSource {
    fn inspector_ui(&mut self, ui: &mut Ui, _ctx: &InspectCtx) -> bool {
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
impl Gizmo for AudioSource {
    fn gizmos(&self) -> Vec<GizmoSpec> {
        vec![
            GizmoSpec::Range {
                center: glam::Vec3::ZERO,
                radius: self.min_distance,
                color: egui::Color32::from_rgb(120, 200, 255),
            },
            GizmoSpec::Range {
                center: glam::Vec3::ZERO,
                radius: self.max_distance,
                color: egui::Color32::from_rgb(90, 140, 200),
            },
        ]
    }
    fn apply_gizmo_edit(&mut self, edit: GizmoEdit) {
        if let GizmoEdit::Range { index, radius } = edit {
            let r = radius.max(0.0);
            match index {
                0 => self.min_distance = r.min(self.max_distance),
                1 => self.max_distance = r.max(self.min_distance),
                _ => {}
            }
        }
    }
}

impl Inspect for AudioListener {
    fn inspector_ui(&mut self, ui: &mut Ui, _ctx: &InspectCtx) -> bool {
        ui.label(
            RichText::new("Ears for spatial audio (put this on the main camera).")
                .small()
                .weak(),
        );
        false
    }
}
impl Gizmo for AudioListener {}

impl Inspect for BoxCollider {
    fn inspector_ui(&mut self, ui: &mut Ui, _ctx: &InspectCtx) -> bool {
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
impl Gizmo for BoxCollider {
    fn gizmos(&self) -> Vec<GizmoSpec> {
        vec![GizmoSpec::Box {
            center: glam::Vec3::from(self.center),
            half_extents: glam::Vec3::from(self.size) * 0.5,
            object_anchored: false,
            blend: 0.0,
            color: egui::Color32::from_rgb(240, 220, 70),
        }]
    }
    fn apply_gizmo_edit(&mut self, edit: GizmoEdit) {
        if let GizmoEdit::Box {
            half_extents,
            center,
            ..
        } = edit
        {
            self.size = (half_extents * 2.0).to_array();
            self.center = center.to_array();
        }
    }
}

impl Inspect for SphereCollider {
    fn inspector_ui(&mut self, ui: &mut Ui, _ctx: &InspectCtx) -> bool {
        let mut changed = false;
        axis_row(ui, "Center", &mut self.center, &mut changed);
        property_row(ui, "Radius", &mut changed, |ui| {
            ui.add(DragValue::new(&mut self.radius).speed(0.05).range(0.01..=10000.0))
        });
        collider_common_ui(ui, &mut self.is_trigger, &mut self.layer, &mut changed);
        changed
    }
}
impl Gizmo for SphereCollider {
    fn gizmos(&self) -> Vec<GizmoSpec> {
        vec![GizmoSpec::Range {
            center: glam::Vec3::from(self.center),
            radius: self.radius,
            color: egui::Color32::from_rgb(240, 220, 70),
        }]
    }
    fn apply_gizmo_edit(&mut self, edit: GizmoEdit) {
        if let GizmoEdit::Range { radius, .. } = edit {
            self.radius = radius.max(0.0);
        }
    }
}

impl Inspect for MeshCollider {
    fn inspector_ui(&mut self, ui: &mut Ui, _ctx: &InspectCtx) -> bool {
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
impl Gizmo for MeshCollider {}

impl Inspect for Pawn {
    fn inspector_ui(&mut self, ui: &mut Ui, ctx: &InspectCtx) -> bool {
        let mut changed = false;
        ui.horizontal(|ui| {
            ui.label("Mode");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                egui::ComboBox::from_id_salt("citrus-pawn-mode")
                    .selected_text(self.mode.label())
                    .show_ui(ui, |ui| {
                        for m in ControlMode::ALL {
                            changed |= ui.selectable_value(&mut self.mode, m, m.label()).changed();
                        }
                    });
            });
        });
        property_row(ui, "Possessed", &mut changed, |ui| {
            ui.checkbox(&mut self.possessed, "")
                .on_hover_text("The local player controls this pawn (receives input)")
        });
        changed |= ctx.object_ref(ui, "Camera", &mut self.camera);
        property_row(ui, "Spawn Tag", &mut changed, |ui| {
            ui.text_edit_singleline(&mut self.spawn_tag)
                .on_hover_text("Teleport to a SpawnPoint with this tag on Play start")
        });
        property_row(ui, "Move Speed", &mut changed, |ui| {
            ui.add(DragValue::new(&mut self.move_speed).speed(0.1).range(0.0..=100.0))
        });
        property_row(ui, "Accel", &mut changed, |ui| {
            ui.add(DragValue::new(&mut self.accel).speed(0.5).range(0.1..=500.0))
        });
        property_row(ui, "Decel", &mut changed, |ui| {
            ui.add(DragValue::new(&mut self.decel).speed(0.5).range(0.1..=500.0))
        });
        property_row(ui, "Jump Power", &mut changed, |ui| {
            ui.add(DragValue::new(&mut self.jump_power).speed(0.1).range(0.0..=50.0))
        });
        property_row(ui, "Gravity", &mut changed, |ui| {
            ui.add(DragValue::new(&mut self.gravity).speed(0.1).range(0.0..=100.0))
        });
        property_row(ui, "Look Sensitivity", &mut changed, |ui| {
            ui.add(DragValue::new(&mut self.look_sensitivity).speed(0.005).range(0.0..=5.0))
        });
        property_row(ui, "Eye Height", &mut changed, |ui| {
            ui.add(DragValue::new(&mut self.eye_height).speed(0.05).range(0.0..=10.0))
        });
        property_row(ui, "Arm Length", &mut changed, |ui| {
            ui.add(DragValue::new(&mut self.arm_length).speed(0.1).range(0.0..=50.0))
        });
        changed
    }
}
impl Gizmo for Pawn {}

impl Inspect for SpawnPoint {
    fn inspector_ui(&mut self, ui: &mut Ui, _ctx: &InspectCtx) -> bool {
        let mut changed = false;
        property_row(ui, "Tag", &mut changed, |ui| {
            ui.text_edit_singleline(&mut self.tag)
                .on_hover_text("Group: \"player\", \"npc\", a team name…")
        });
        property_row(ui, "Index", &mut changed, |ui| {
            ui.add(DragValue::new(&mut self.index).range(0..=4096))
        });
        changed
    }
}
impl Gizmo for SpawnPoint {}

impl Inspect for Sync {
    fn inspector_ui(&mut self, ui: &mut Ui, _ctx: &InspectCtx) -> bool {
        let mut changed = false;
        property_row(ui, "Grabbable", &mut changed, |ui| {
            ui.checkbox(&mut self.grabbable, "")
                .on_hover_text("Any peer can take ownership by pressing the grab action")
        });
        property_row(ui, "Grab Action", &mut changed, |ui| {
            ui.text_edit_singleline(&mut self.grab_action)
        });
        property_row(ui, "Smoothing", &mut changed, |ui| {
            ui.add(DragValue::new(&mut self.smoothing).speed(0.5).range(0.0..=60.0))
                .on_hover_text("Remote-update lerp rate (0 = snap)")
        });
        ui.label(
            RichText::new("Replicates this object's transform; the owner sends, others receive.")
                .small()
                .weak(),
        );
        changed
    }
}
impl Gizmo for Sync {}
