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
    Animator, JointKind, PortalMode, ScenePortal, NavAgent, NavMeshSurface, ParticleEmitter, PhysicsJoint, AudioListener, AudioRolloff, AudioSource, AcousticMaterial, AcousticPreset, AudioProbeVolume, Bob, BodyKind, BoxCollider, CameraComponent,
    CharacterController, Component, UiAlign, UiButton, UiCanvas, UiImage, UiPanel, UiRect, UiRenderMode,
    UiScaleMode, UiSlider, UiText, UiTextField, UiToggle, UiAction, UiFillDirection, UiPauseMenu, UiProgressBar, ComponentRegistry, ControlMode, FluxVolume, FluxVoxelLight, LightComponent, LightKind,
    LightMode, LightProbeVolume, LodComponent, LodLevel, MeshCollider, ObjectId, ObjectRef, Pawn,
    PlanarReflection, ReflectionProbe, RigidBody, ShadowType,
    Spin,
    SpawnPoint, SphereCollider, Sync, TypedComponent, VolumeComponent,
    COLLISION_LAYERS,
};
use egui::{DragValue, RichText, ScrollArea, Sense, Ui};
use egui_phosphor::regular as ph;

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
    /// Per-layer display names (len = NUM_LAYERS). Lets layer pickers (camera
    /// culling mask) show the SAME layer set + names as the Layers window and the
    /// object's layer dropdown, instead of a hardcoded count.
    pub layer_names: &'a [String],
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
            if target.is_set() && ui.small_button(ph::X).on_hover_text("Clear").clicked() {
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
        e.register::<PlanarReflection>();
        e.register::<FluxVoxelLight>();
        e.register::<FluxVolume>();
        e.register::<AudioSource>();
        e.register::<AudioListener>();
        e.register::<AcousticMaterial>();
        e.register::<AudioProbeVolume>();
        e.register::<BoxCollider>();
        e.register::<SphereCollider>();
        e.register::<MeshCollider>();
        e.register::<Spin>();
        e.register::<Bob>();
        e.register::<LodComponent>();
        e.register::<RigidBody>();
        e.register::<CharacterController>();
        e.register::<Animator>();
        e.register::<PhysicsJoint>();
        e.register::<NavMeshSurface>();
        e.register::<NavAgent>();
        e.register::<ParticleEmitter>();
        e.register::<ScenePortal>();
        e.register::<UiCanvas>();
        e.register::<UiRect>();
        e.register::<UiPanel>();
        e.register::<UiImage>();
        e.register::<UiText>();
        e.register::<UiButton>();
        e.register::<UiToggle>();
        e.register::<UiSlider>();
        e.register::<UiTextField>();
        e.register::<UiProgressBar>();
        e.register::<UiPauseMenu>();
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
    layer_names: &[String],
) -> ComponentsResponse {
    let mut response = ComponentsResponse::default();
    let inspect_ctx = InspectCtx { objects, layer_names };
    // Plugin components (cdylibs) statically link their own copy of egui, so a
    // selectable label drawn from a plugin's egui panics (mismatched TypeId for
    // egui's selection state). Disable selection for the whole inspector list.
    ui.style_mut().interaction.selectable_labels = false;
    for (index, component) in components.iter_mut().enumerate() {
        let id = ui.make_persistent_id(("citrus-component", index, component.type_name()));
        crate::widgets::divider(ui);
        let (icon, color) = component_icon(component.type_name());
        let (open, _) = crate::widgets::section_header(ui, id, Some((icon, color)), component.type_name(), None, |ui| {
            let kebab = crate::widgets::icon_button(ui, crate::icons::Icon::Kebab, "Component options", false);
            egui::Popup::menu(&kebab).show(|ui| {
                if ui.button("Remove component").clicked() {
                    response.remove = Some(index);
                    ui.close();
                }
            });
        });
        if open {
            egui::Frame::new()
                .inner_margin(egui::Margin { left: 8, right: 0, top: 0, bottom: 10 })
                .show(ui, |ui| {
                    response.changed |= editor.inspect(component.as_mut(), ui, &inspect_ctx);
                });
        }
    }
    crate::widgets::divider(ui);

    ui.add_space(12.0);
    let add = crate::widgets::dashed_button(ui, Some(crate::icons::Icon::Plus), "Add component");
    egui::Popup::menu(&add)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| {
            let search_id = ui.make_persistent_id("citrus-add-component-search");
            let mut search: String = ui.data(|d| d.get_temp(search_id).unwrap_or_default());
            let width = add.rect.width().max(240.0);
            ui.set_min_width(width);
            ui.set_max_width(width);
            let edit = crate::widgets::search_field(ui, &mut search, "Search components", width);
            if edit.changed() {
                ui.data_mut(|d| d.insert_temp(search_id, search.clone()));
            }
            let needle = search.to_lowercase();
            ui.add_space(4.0);
            ScrollArea::vertical().auto_shrink([false, true]).max_height(320.0).show(ui, |ui| {
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
                    let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, crate::theme::H_TOOLBAR), Sense::click());
                    if resp.hovered() {
                        ui.painter().rect_filled(rect, crate::theme::R_ROW, crate::theme::BG_BUTTON);
                    }
                    let (icon, color) = component_icon(name);
                    crate::icons::paint(
                        ui.painter(),
                        egui::Rect::from_center_size(egui::pos2(rect.left() + 16.0, rect.center().y), egui::vec2(14.0, 14.0)),
                        icon,
                        color,
                    );
                    ui.painter().text(
                        egui::pos2(rect.left() + 32.0, rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        name,
                        egui::FontId::proportional(crate::theme::SIZE_BODY),
                        if resp.hovered() { crate::theme::TEXT_STRONG } else { crate::theme::TEXT_SECONDARY },
                    );
                    if resp.clicked() {
                        response.add = Some(name);
                        ui.data_mut(|d| d.remove::<String>(search_id));
                        ui.close();
                    }
                }
                if !any {
                    ui.label(RichText::new("No matches").color(crate::theme::TEXT_DIM));
                }
            });
        });
    response
}

/// Header icon and colour for a component type (editor spec §2.4).
pub fn component_icon(name: &str) -> (crate::icons::Icon, egui::Color32) {
    use crate::icons::Icon;
    use crate::theme;
    match name {
        "Light" => (Icon::LightPoint, theme::TYPE_LIGHT),
        "Camera" => (Icon::Camera, theme::TYPE_CAMERA),
        "UI Canvas" => (Icon::UiCanvas, theme::TYPE_UI),
        "UI Toggle" => (Icon::UiToggle, theme::TYPE_UI),
        "UI Text" | "UI Text Field" => (Icon::UiText, theme::TYPE_UI),
        n if n.starts_with("UI ") => (Icon::UiCanvas, theme::TYPE_UI),
        "Reflection Probe" | "Planar Reflection" => (Icon::ReflectionProbe, theme::TYPE_GI),
        "Flux Volume" | "Light Probe Volume" | "FluxVoxel Light" => (Icon::FluxVolume, theme::TYPE_GI),
        "Audio Source" | "Audio Listener" => (Icon::Audio, theme::TYPE_GROUP),
        "Animator" | "LOD Group" => (Icon::Model, theme::TYPE_MESH),
        _ => (Icon::Gear, theme::TYPE_GROUP),
    }
}

// ----------------------------------------------------------------- helpers

fn property_row(
    ui: &mut Ui,
    label: &str,
    changed: &mut bool,
    widget: impl FnOnce(&mut Ui) -> egui::Response,
) {
    crate::widgets::property_row(ui, label, |ui| {
        if widget(ui).changed() {
            *changed = true;
        }
    });
}

fn axis_row(ui: &mut Ui, label: &str, axis: &mut [f32; 3], changed: &mut bool) {
    crate::widgets::property_row(ui, label, |ui| {
        *changed |= crate::widgets::vec3_field(ui, axis, 2, 0.02);
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
    fn inspector_ui(&mut self, ui: &mut Ui, ctx: &InspectCtx) -> bool {
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
        // Layer culling mask (Unity-style): which layers this camera renders.
        // Layer names + the collision matrix are edited in Tools → Layers.
        ui.collapsing("Culling Mask", |ui| {
            ui.horizontal(|ui| {
                if ui.button("Everything").clicked() {
                    self.culling_mask = u32::MAX;
                    changed = true;
                }
                if ui.button("Nothing").clicked() {
                    self.culling_mask = 0;
                    changed = true;
                }
            });
            // Show ALL layers (same as the object dropdown + collision matrix), with
            // their real names. Scrollable so 32 rows don't blow out the inspector.
            let shown = ctx.layer_names.len().max(8) as u32;
            egui::ScrollArea::vertical()
                .id_salt("cam-cull-scroll")
                .max_height(220.0)
                .show(ui, |ui| {
            egui::Grid::new("camera-culling-mask").show(ui, |ui| {
                for l in 0u32..shown {
                    let bit = 1u32 << l;
                    let mut on = self.culling_mask & bit != 0;
                    let name = ctx
                        .layer_names
                        .get(l as usize)
                        .filter(|s| !s.is_empty())
                        .cloned()
                        .unwrap_or_else(|| format!("Layer {l}"));
                    if ui.checkbox(&mut on, name).changed() {
                        if on {
                            self.culling_mask |= bit;
                        } else {
                            self.culling_mask &= !bit;
                        }
                        changed = true;
                    }
                    if l % 2 == 1 {
                        ui.end_row();
                    }
                }
            });
            });
        });
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

impl Inspect for LodComponent {
    fn inspector_ui(&mut self, ui: &mut Ui, _ctx: &InspectCtx) -> bool {
        let mut changed = false;
        ui.label(
            egui::RichText::new("Swap to a cheaper mesh as the object recedes. Level 0 = nearest.")
                .small()
                .weak(),
        );
        let mut remove: Option<usize> = None;
        for (i, level) in self.levels.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                ui.label(format!("LOD {i}"));
                ui.label("≤");
                changed |= ui
                    .add(DragValue::new(&mut level.max_distance).speed(0.5).range(0.1..=10000.0).suffix(" m"))
                    .changed();
                changed |= ui
                    .add(egui::TextEdit::singleline(&mut level.model).hint_text("model path (blank = base mesh)"))
                    .changed();
                if ui.small_button(ph::X).clicked() {
                    remove = Some(i);
                }
            });
        }
        if let Some(i) = remove {
            self.levels.remove(i);
            changed = true;
        }
        ui.horizontal(|ui| {
            if ui.button("+ Add level").clicked() {
                let d = self.levels.last().map(|l| l.max_distance * 2.0).unwrap_or(15.0);
                self.levels.push(LodLevel { max_distance: d, model: String::new() });
                changed = true;
            }
        });
        property_row(ui, "Cull distance", &mut changed, |ui| {
            ui.add(DragValue::new(&mut self.cull_distance).speed(0.5).range(0.0..=10000.0).suffix(" m"))
                .on_hover_text("Beyond this the object is culled entirely (0 = never)")
        });
        // Keep levels sorted ascending so selection is correct.
        if changed {
            self.levels.sort_by(|a, b| a.max_distance.partial_cmp(&b.max_distance).unwrap());
        }
        changed
    }
}
impl Gizmo for LodComponent {}

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

impl Inspect for CharacterController {
    fn inspector_ui(&mut self, ui: &mut Ui, _ctx: &InspectCtx) -> bool {
        let mut changed = false;
        property_row(ui, "Radius", &mut changed, |ui| {
            ui.add(DragValue::new(&mut self.radius).speed(0.01).range(0.01..=10.0))
        });
        property_row(ui, "Height", &mut changed, |ui| {
            ui.add(DragValue::new(&mut self.height).speed(0.01).range(0.02..=20.0))
        });
        axis_row(ui, "Center", &mut self.center, &mut changed);
        property_row(ui, "Step height", &mut changed, |ui| {
            ui.add(DragValue::new(&mut self.step_height).speed(0.01).range(0.0..=2.0))
        });
        property_row(ui, "Max slope (deg)", &mut changed, |ui| {
            ui.add(DragValue::new(&mut self.max_slope).speed(0.5).range(0.0..=89.0))
        });
        property_row(ui, "Snap to ground", &mut changed, |ui| {
            ui.add(DragValue::new(&mut self.snap_to_ground).speed(0.01).range(0.0..=2.0))
        });
        changed
    }
}
impl Gizmo for CharacterController {}

fn range_row(ui: &mut Ui, label: &str, v: &mut [f32; 2], speed: f64, changed: &mut bool) {
    ui.horizontal(|ui| {
        ui.label(label);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            *changed |= ui.add(DragValue::new(&mut v[1]).speed(speed).prefix("max ")).changed();
            *changed |= ui.add(DragValue::new(&mut v[0]).speed(speed).prefix("min ")).changed();
        });
    });
}

impl Inspect for ParticleEmitter {
    fn inspector_ui(&mut self, ui: &mut Ui, _ctx: &InspectCtx) -> bool {
        let mut changed = false;
        property_row(ui, "Play on start", &mut changed, |ui| ui.checkbox(&mut self.play_on_start, ""));
        property_row(ui, "Looping", &mut changed, |ui| ui.checkbox(&mut self.looping, ""));
        if !self.looping {
            property_row(ui, "Duration (s)", &mut changed, |ui| ui.add(DragValue::new(&mut self.duration).speed(0.05).range(0.0..=600.0)));
        }
        property_row(ui, "Rate (/s)", &mut changed, |ui| ui.add(DragValue::new(&mut self.rate).speed(0.5).range(0.0..=10000.0)));
        property_row(ui, "Burst", &mut changed, |ui| ui.add(DragValue::new(&mut self.burst).range(0..=10000)));
        property_row(ui, "Max particles", &mut changed, |ui| ui.add(DragValue::new(&mut self.max_particles).range(1..=20000)));
        range_row(ui, "Lifetime (s)", &mut self.lifetime, 0.02, &mut changed);
        range_row(ui, "Speed", &mut self.speed, 0.05, &mut changed);
        axis_row(ui, "Direction", &mut self.direction, &mut changed);
        property_row(ui, "Cone angle", &mut changed, |ui| ui.add(DragValue::new(&mut self.cone_angle).speed(0.5).range(0.0..=180.0)));
        property_row(ui, "Spawn radius", &mut changed, |ui| ui.add(DragValue::new(&mut self.shape_radius).speed(0.01).range(0.0..=100.0)));
        property_row(ui, "Gravity", &mut changed, |ui| ui.add(DragValue::new(&mut self.gravity).speed(0.02)));
        property_row(ui, "Drag", &mut changed, |ui| ui.add(DragValue::new(&mut self.drag).speed(0.02).range(0.0..=50.0)));
        range_row(ui, "Size (start/end)", &mut self.size, 0.01, &mut changed);
        rgba_row(ui, "Color start", &mut self.color_start, &mut changed);
        rgba_row(ui, "Color end", &mut self.color_end, &mut changed);
        property_row(ui, "Texture", &mut changed, |ui| {
            ui.add(egui::TextEdit::singleline(&mut self.texture).hint_text("soft dot"))
        });
        property_row(ui, "World space", &mut changed, |ui| ui.checkbox(&mut self.world_space, ""));
        ui.horizontal(|ui| {
            if ui.button("Restart").clicked() {
                self.control.clear = true;
                self.control.play = true;
            }
            ui.label(egui::RichText::new(format!("{} alive", self.control.alive)).small().weak());
        });
        changed
    }
}
impl Gizmo for ParticleEmitter {}

impl Inspect for ScenePortal {
    fn inspector_ui(&mut self, ui: &mut Ui, ctx: &InspectCtx) -> bool {
        let mut changed = false;
        property_row(ui, "Scene", &mut changed, |ui| {
            ui.add(egui::TextEdit::singleline(&mut self.scene).hint_text("scenes/level2.scene"))
        });
        enum_row(ui, "Mode", "citrus-portal-mode", &mut self.mode, &PortalMode::ALL, PortalMode::label, &mut changed);
        changed |= ctx.object_ref(ui, "Activator", &mut self.activator);
        if !self.activator.is_set() {
            ui.label(egui::RichText::new("Unset: any object with a Pawn").small().weak());
        }
        if self.mode != PortalMode::Unload {
            property_row(ui, "Preload distance", &mut changed, |ui| {
                ui.add(DragValue::new(&mut self.preload_distance).speed(0.5).range(0.0..=10000.0))
            });
        }
        if self.mode == PortalMode::Switch {
            property_row(ui, "Keep activator", &mut changed, |ui| ui.checkbox(&mut self.keep_activator, ""));
        }
        property_row(ui, "One shot", &mut changed, |ui| ui.checkbox(&mut self.one_shot, ""));
        ui.label(
            egui::RichText::new("Needs a trigger collider on this object. Preloads while the activator approaches.")
                .small()
                .weak(),
        );
        changed
    }
}
impl Gizmo for ScenePortal {}

impl Inspect for NavMeshSurface {
    fn inspector_ui(&mut self, ui: &mut Ui, _ctx: &InspectCtx) -> bool {
        let mut changed = false;
        axis_row(ui, "Bounds size", &mut self.size, &mut changed);
        property_row(ui, "Cell size", &mut changed, |ui| ui.add(DragValue::new(&mut self.cell_size).speed(0.01).range(0.05..=4.0)));
        property_row(ui, "Agent radius", &mut changed, |ui| ui.add(DragValue::new(&mut self.agent_radius).speed(0.01).range(0.0..=10.0)));
        property_row(ui, "Agent height", &mut changed, |ui| ui.add(DragValue::new(&mut self.agent_height).speed(0.01).range(0.1..=20.0)));
        property_row(ui, "Max slope", &mut changed, |ui| ui.add(DragValue::new(&mut self.max_slope).speed(0.5).range(0.0..=89.0)));
        property_row(ui, "Step height", &mut changed, |ui| ui.add(DragValue::new(&mut self.step_height).speed(0.01).range(0.0..=5.0)));
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.show_preview, "Show navmesh");
            if ui.button("Rebake").clicked() {
                self.rebake_preview = true;
                self.show_preview = true;
            }
        });
        ui.label(
            egui::RichText::new("Baked from colliders when play starts; ctx.rebake_navmesh() at runtime")
                .small()
                .weak(),
        );
        changed
    }
}
impl Gizmo for NavMeshSurface {}

impl Inspect for NavAgent {
    fn inspector_ui(&mut self, ui: &mut Ui, _ctx: &InspectCtx) -> bool {
        let mut changed = false;
        property_row(ui, "Speed", &mut changed, |ui| ui.add(DragValue::new(&mut self.speed).speed(0.05).range(0.0..=100.0)));
        property_row(ui, "Turn speed", &mut changed, |ui| ui.add(DragValue::new(&mut self.turn_speed).speed(5.0).range(0.0..=5000.0)));
        property_row(ui, "Stopping distance", &mut changed, |ui| {
            ui.add(DragValue::new(&mut self.stopping_distance).speed(0.01).range(0.0..=50.0))
        });
        ui.label(egui::RichText::new("Gameplay: ctx.set_destination(agent, point)").small().weak());
        changed
    }
}
impl Gizmo for NavAgent {}

impl Inspect for PhysicsJoint {
    fn inspector_ui(&mut self, ui: &mut Ui, ctx: &InspectCtx) -> bool {
        let mut changed = false;
        enum_row(ui, "Type", "citrus-joint-kind", &mut self.kind, &JointKind::ALL, JointKind::label, &mut changed);
        changed |= ctx.object_ref(ui, "Connected body", &mut self.connected);
        if !self.connected.is_set() {
            ui.label(egui::RichText::new("Unset: attached to its start point in the world").small().weak());
        }
        axis_row(ui, "Anchor", &mut self.anchor, &mut changed);
        let k = self.kind;
        if matches!(k, JointKind::Hinge | JointKind::Slider) {
            axis_row(ui, "Axis", &mut self.axis, &mut changed);
            property_row(ui, "Limits", &mut changed, |ui| ui.checkbox(&mut self.limits_enabled, ""));
            if self.limits_enabled {
                let unit = if k == JointKind::Hinge { " deg" } else { " m" };
                ui.horizontal(|ui| {
                    ui.label("Range");
                    changed |= ui.add(DragValue::new(&mut self.limits[0]).speed(0.5).suffix(unit)).changed();
                    changed |= ui.add(DragValue::new(&mut self.limits[1]).speed(0.5).suffix(unit)).changed();
                });
            }
            property_row(ui, "Motor", &mut changed, |ui| ui.checkbox(&mut self.motor_enabled, ""));
            if self.motor_enabled {
                property_row(ui, "Motor speed", &mut changed, |ui| ui.add(DragValue::new(&mut self.motor_speed).speed(1.0)));
                property_row(ui, "Max force", &mut changed, |ui| {
                    ui.add(DragValue::new(&mut self.motor_max_force).speed(1.0).range(0.0..=1e7))
                });
            }
        }
        if matches!(k, JointKind::Spring | JointKind::Rope) {
            property_row(ui, "Rest length", &mut changed, |ui| {
                ui.add(DragValue::new(&mut self.rest_length).speed(0.01).range(0.0..=1000.0))
            });
            ui.label(egui::RichText::new("0 = distance at start").small().weak());
        }
        if k == JointKind::Spring {
            property_row(ui, "Stiffness", &mut changed, |ui| ui.add(DragValue::new(&mut self.stiffness).speed(0.5).range(0.0..=1e6)));
            property_row(ui, "Damping", &mut changed, |ui| ui.add(DragValue::new(&mut self.damping).speed(0.05).range(0.0..=1e4)));
        }
        property_row(ui, "Collide connected", &mut changed, |ui| ui.checkbox(&mut self.collide_connected, ""));
        ui.label(egui::RichText::new("Needs a collider + Rigid Body on this object").small().weak());
        changed
    }
}
impl Gizmo for PhysicsJoint {}

impl Inspect for Animator {
    fn inspector_ui(&mut self, ui: &mut Ui, _ctx: &InspectCtx) -> bool {
        let mut changed = false;
        let clips = self.state.clips.clone();
        let choices: Vec<String> = clips
            .iter()
            .cloned()
            .chain(self.blend_trees.iter().map(|t| t.name.clone()))
            .collect();
        property_row(ui, "Clip", &mut changed, |ui| {
            let shown = if self.clip.is_empty() { "(first clip)" } else { self.clip.as_str() };
            let mut resp = egui::ComboBox::from_id_salt("citrus-animator-clip")
                .selected_text(shown)
                .show_ui(ui, |ui| {
                    let mut clicked = ui.selectable_value(&mut self.clip, String::new(), "(first clip)").clicked();
                    for c in &choices {
                        clicked |= ui.selectable_value(&mut self.clip, c.clone(), c).clicked();
                    }
                    clicked
                });
            if resp.inner == Some(true) {
                resp.response.mark_changed();
            }
            resp.response
        });
        if clips.is_empty() {
            ui.label(egui::RichText::new("No clips found on this object's model").small().weak());
        }
        property_row(ui, "Speed", &mut changed, |ui| {
            ui.add(DragValue::new(&mut self.speed).speed(0.02).range(-10.0..=10.0))
        });
        property_row(ui, "Loop", &mut changed, |ui| ui.checkbox(&mut self.looping, ""));
        property_row(ui, "Play on start", &mut changed, |ui| ui.checkbox(&mut self.play_on_start, ""));
        property_row(ui, "Root motion", &mut changed, |ui| ui.checkbox(&mut self.root_motion, ""));
        property_row(ui, "GPU skinning", &mut changed, |ui| ui.checkbox(&mut self.gpu_skinning, ""));

        let clip_combo = |ui: &mut Ui, salt: String, value: &mut String, clips: &[String]| -> bool {
            let mut clicked = false;
            egui::ComboBox::from_id_salt(salt)
                .selected_text(if value.is_empty() { "(clip)" } else { value.as_str() })
                .show_ui(ui, |ui| {
                    for c in clips {
                        clicked |= ui.selectable_value(value, c.clone(), c).clicked();
                    }
                });
            clicked
        };

        egui::CollapsingHeader::new(format!("Blend trees ({})", self.blend_trees.len()))
            .id_salt("citrus-anim-trees")
            .show(ui, |ui| {
                let mut remove = None;
                for (t, tree) in self.blend_trees.iter_mut().enumerate() {
                    ui.separator();
                    property_row(ui, "Name", &mut changed, |ui| ui.text_edit_singleline(&mut tree.name));
                    property_row(ui, "Parameter", &mut changed, |ui| ui.text_edit_singleline(&mut tree.parameter));
                    property_row(ui, "Parameter Y (2D)", &mut changed, |ui| ui.text_edit_singleline(&mut tree.parameter_y));
                    let two_d = tree.is_2d();
                    let mut remove_motion = None;
                    for (m, motion) in tree.motions.iter_mut().enumerate() {
                        ui.horizontal(|ui| {
                            changed |= clip_combo(ui, format!("citrus-tree-{t}-{m}"), &mut motion.clip, &clips);
                            changed |= ui.add(DragValue::new(&mut motion.position[0]).speed(0.05)).changed();
                            if two_d {
                                changed |= ui.add(DragValue::new(&mut motion.position[1]).speed(0.05)).changed();
                            }
                            if ui.small_button("x").clicked() {
                                remove_motion = Some(m);
                            }
                        });
                    }
                    if let Some(m) = remove_motion {
                        tree.motions.remove(m);
                        changed = true;
                    }
                    ui.horizontal(|ui| {
                        if ui.button("Add motion").clicked() {
                            tree.motions.push(Default::default());
                            changed = true;
                        }
                        if ui.button("Remove tree").clicked() {
                            remove = Some(t);
                        }
                    });
                }
                if let Some(t) = remove {
                    self.blend_trees.remove(t);
                    changed = true;
                }
                if ui.button("Add blend tree").clicked() {
                    self.blend_trees.push(citrus_core::BlendTree {
                        name: format!("Blend {}", self.blend_trees.len() + 1),
                        parameter: "Speed".into(),
                        ..Default::default()
                    });
                    changed = true;
                }
            });

        egui::CollapsingHeader::new(format!("Events ({})", self.events.len()))
            .id_salt("citrus-anim-events")
            .show(ui, |ui| {
                let mut remove = None;
                for (k, e) in self.events.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        changed |= clip_combo(ui, format!("citrus-anim-ev-{k}"), &mut e.clip, &clips);
                        changed |= ui.add(DragValue::new(&mut e.time).speed(0.01).range(0.0..=1000.0).suffix(" s")).changed();
                        changed |= ui.add(egui::TextEdit::singleline(&mut e.name).desired_width(90.0)).changed();
                        if ui.small_button("x").clicked() {
                            remove = Some(k);
                        }
                    });
                }
                if let Some(k) = remove {
                    self.events.remove(k);
                    changed = true;
                }
                if ui.button("Add event").clicked() {
                    self.events.push(citrus_core::AnimEvent {
                        clip: clips.first().cloned().unwrap_or_default(),
                        time: 0.0,
                        name: "Event".into(),
                    });
                    changed = true;
                }
            });
        if let Some(cur) = &self.state.current {
            ui.label(
                egui::RichText::new(format!("Playing: {cur} ({:.2}s / {:.2}s)", self.state.time, self.state.duration))
                    .small()
                    .weak(),
            );
        }
        changed
    }
}
impl Gizmo for Animator {}

fn rgba_row(ui: &mut Ui, label: &str, c: &mut [f32; 4], changed: &mut bool) {
    property_row(ui, label, changed, |ui| crate::widgets::color_hex(ui, c));
}

fn vec2_row(ui: &mut Ui, label: &str, v: &mut [f32; 2], speed: f64, changed: &mut bool) {
    crate::widgets::property_row(ui, label, |ui| {
        let w = ((ui.available_width() - 4.0) / 2.0).max(40.0);
        ui.spacing_mut().item_spacing.x = 4.0;
        for (a, value) in v.iter_mut().enumerate() {
            *changed |= crate::widgets::axis_field(ui, a, value, 2, speed, w).changed();
        }
    });
}

fn enum_row<T: Copy + PartialEq>(
    ui: &mut Ui,
    label: &str,
    salt: &str,
    value: &mut T,
    all: &[T],
    name: fn(T) -> &'static str,
    changed: &mut bool,
) {
    property_row(ui, label, changed, |ui| {
        let mut resp = egui::ComboBox::from_id_salt(salt)
            .width(ui.available_width())
            .selected_text(name(*value))
            .show_ui(ui, |ui| {
                let mut clicked = false;
                for &k in all {
                    clicked |= ui.selectable_value(value, k, name(k)).clicked();
                }
                clicked
            });
        if resp.inner == Some(true) {
            resp.response.mark_changed();
        }
        resp.response
    });
}

impl Inspect for UiCanvas {
    fn inspector_ui(&mut self, ui: &mut Ui, _ctx: &InspectCtx) -> bool {
        let mut changed = false;
        enum_row(ui, "Render mode", "citrus-ui-render", &mut self.render_mode, &UiRenderMode::ALL, UiRenderMode::label, &mut changed);
        if self.render_mode == UiRenderMode::World {
            vec2_row(ui, "Size (m)", &mut self.world_size, 0.01, &mut changed);
            property_row(ui, "Pixels per meter", &mut changed, |ui| {
                ui.add(DragValue::new(&mut self.pixels_per_meter).speed(5.0).range(50.0..=4000.0))
            });
            let [w, h] = self.world_pixels();
            ui.label(
                egui::RichText::new(format!("{w} x {h} px panel in the object's XY plane, facing +Z"))
                    .small()
                    .weak(),
            );
        }
        enum_row(ui, "Scale mode", "citrus-ui-scale", &mut self.scale_mode, &UiScaleMode::ALL, UiScaleMode::label, &mut changed);
        if self.scale_mode == UiScaleMode::ScaleWithScreen {
            vec2_row(ui, "Reference size", &mut self.reference_size, 1.0, &mut changed);
            property_row(ui, "Match width/height", &mut changed, |ui| {
                ui.add(egui::Slider::new(&mut self.match_height, 0.0..=1.0))
            });
        }
        property_row(ui, "Sort order", &mut changed, |ui| ui.add(DragValue::new(&mut self.sort_order)));
        ui.label(
            egui::RichText::new("Children with UI components are laid out inside this canvas.")
                .small()
                .weak(),
        );
        changed
    }
}
impl Gizmo for UiCanvas {}

impl Inspect for UiRect {
    fn inspector_ui(&mut self, ui: &mut Ui, _ctx: &InspectCtx) -> bool {
        let mut changed = false;
        // Spec §8.3: 3x3 anchor preset grid (active anchor filled accent), plus
        // Stretch to fill the parent.
        crate::widgets::property_row(ui, "Anchor", |ui| {
            let cell = 18.0;
            let (rect, _) = ui.allocate_exact_size(egui::vec2(cell * 3.0 + 8.0, cell * 3.0 + 8.0), Sense::hover());
            let p = ui.painter();
            p.rect(rect, crate::theme::R_FIELD, crate::theme::BG_FIELD, egui::Stroke::new(1.0_f32, crate::theme::LINE_FIELD), egui::StrokeKind::Inside);
            for row in 0..3 {
                for col in 0..3 {
                    let a = [col as f32 * 0.5, row as f32 * 0.5];
                    let r = egui::Rect::from_min_size(
                        rect.min + egui::vec2(4.0 + col as f32 * cell, 4.0 + row as f32 * cell),
                        egui::vec2(cell, cell),
                    )
                    .shrink(3.0);
                    let id = ui.id().with(("citrus-anchor", row, col));
                    let resp = ui.interact(r, id, Sense::click());
                    let active = self.anchor_min == a && self.anchor_max == a;
                    let fill = if active {
                        crate::theme::accent()
                    } else if resp.hovered() {
                        crate::theme::TEXT_MUTED
                    } else {
                        crate::theme::BG_SEGMENT_ACTIVE
                    };
                    ui.painter().rect_filled(r, crate::theme::R_SWATCH, fill);
                    let name = UiRect::PRESETS.iter().find(|(_, x, y)| *x == a && *y == a).map_or("", |(n, _, _)| n);
                    if resp.on_hover_text(name).clicked() {
                        self.anchor_min = a;
                        self.anchor_max = a;
                        self.pivot = a;
                        changed = true;
                    }
                }
            }
            let stretch = self.anchor_min == [0.0, 0.0] && self.anchor_max == [1.0, 1.0];
            if crate::widgets::segment(ui, stretch, "Stretch").on_hover_text("Fill the parent").clicked() && !stretch {
                self.anchor_min = [0.0, 0.0];
                self.anchor_max = [1.0, 1.0];
                self.size = [0.0, 0.0];
                self.position = [0.0, 0.0];
                self.pivot = [0.5, 0.5];
                changed = true;
            }
        });
        vec2_row(ui, "Position", &mut self.position, 1.0, &mut changed);
        let stretch = self.anchor_min != self.anchor_max;
        vec2_row(ui, if stretch { "Size delta" } else { "Size" }, &mut self.size, 1.0, &mut changed);
        egui::CollapsingHeader::new("Anchors & pivot").id_salt("citrus-ui-rect-adv").show(ui, |ui| {
            vec2_row(ui, "Anchor min", &mut self.anchor_min, 0.01, &mut changed);
            vec2_row(ui, "Anchor max", &mut self.anchor_max, 0.01, &mut changed);
            vec2_row(ui, "Pivot", &mut self.pivot, 0.01, &mut changed);
        });
        changed
    }
}
impl Gizmo for UiRect {}

impl Inspect for UiPanel {
    fn inspector_ui(&mut self, ui: &mut Ui, _ctx: &InspectCtx) -> bool {
        let mut changed = false;
        rgba_row(ui, "Color", &mut self.color, &mut changed);
        property_row(ui, "Corner radius", &mut changed, |ui| {
            ui.add(DragValue::new(&mut self.corner_radius).speed(0.2).range(0.0..=200.0))
        });
        property_row(ui, "Border width", &mut changed, |ui| {
            ui.add(DragValue::new(&mut self.border_width).speed(0.1).range(0.0..=50.0))
        });
        if self.border_width > 0.0 {
            rgba_row(ui, "Border color", &mut self.border_color, &mut changed);
        }
        changed
    }
}
impl Gizmo for UiPanel {}

impl Inspect for UiImage {
    fn inspector_ui(&mut self, ui: &mut Ui, _ctx: &InspectCtx) -> bool {
        let mut changed = false;
        property_row(ui, "Texture", &mut changed, |ui| {
            ui.add(egui::TextEdit::singleline(&mut self.texture).hint_text("textures/icon.png"))
        });
        rgba_row(ui, "Tint", &mut self.tint, &mut changed);
        property_row(ui, "Preserve aspect", &mut changed, |ui| ui.checkbox(&mut self.preserve_aspect, ""));
        changed
    }
}
impl Gizmo for UiImage {}

impl Inspect for UiText {
    fn inspector_ui(&mut self, ui: &mut Ui, _ctx: &InspectCtx) -> bool {
        let mut changed = false;
        ui.label("Text");
        if ui
            .add(egui::TextEdit::multiline(&mut self.text).desired_rows(2).desired_width(f32::INFINITY))
            .changed()
        {
            changed = true;
        }
        property_row(ui, "Size", &mut changed, |ui| {
            ui.add(DragValue::new(&mut self.size).speed(0.2).range(1.0..=500.0))
        });
        rgba_row(ui, "Color", &mut self.color, &mut changed);
        enum_row(ui, "Align X", "citrus-ui-ax", &mut self.align_x, &UiAlign::ALL, UiAlign::label, &mut changed);
        enum_row(ui, "Align Y", "citrus-ui-ay", &mut self.align_y, &UiAlign::ALL, UiAlign::label, &mut changed);
        property_row(ui, "Wrap", &mut changed, |ui| ui.checkbox(&mut self.wrap, ""));
        property_row(ui, "Font", &mut changed, |ui| {
            ui.add(egui::TextEdit::singleline(&mut self.font).hint_text("built-in"))
        });
        changed
    }
}
impl Gizmo for UiText {}

impl Inspect for UiButton {
    fn inspector_ui(&mut self, ui: &mut Ui, _ctx: &InspectCtx) -> bool {
        let mut changed = false;
        property_row(ui, "Interactable", &mut changed, |ui| ui.checkbox(&mut self.interactable, ""));
        rgba_row(ui, "Normal", &mut self.normal, &mut changed);
        rgba_row(ui, "Hovered", &mut self.hovered, &mut changed);
        rgba_row(ui, "Pressed", &mut self.pressed, &mut changed);
        rgba_row(ui, "Disabled", &mut self.disabled, &mut changed);
        ui.separator();
        ui.label("On click");
        let mut remove = None;
        for (k, action) in self.on_click.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt(("citrus-ui-action", k))
                    .selected_text(action.label())
                    .width(110.0)
                    .show_ui(ui, |ui| {
                        for kind in UiAction::kinds() {
                            let same = std::mem::discriminant(&kind) == std::mem::discriminant(action);
                            if ui.selectable_label(same, kind.label()).clicked() && !same {
                                *action = action.with_kind_of(&kind);
                                changed = true;
                            }
                        }
                    });
                let hint = if matches!(action, UiAction::LoadScene(_)) { "scenes/level.scene" } else { "object name" };
                if let Some(target) = action.target_mut() {
                    let w = (ui.available_width() - 28.0).max(40.0);
                    changed |= ui.add(egui::TextEdit::singleline(target).hint_text(hint).desired_width(w)).changed();
                }
                if ui.small_button(ph::X).on_hover_text("Remove action").clicked() {
                    remove = Some(k);
                }
            });
        }
        if let Some(k) = remove {
            self.on_click.remove(k);
            changed = true;
        }
        if ui.button(format!("{} Add action", ph::PLUS)).clicked() {
            self.on_click.push(UiAction::Show(String::new()));
            changed = true;
        }
        ui.label(egui::RichText::new("Code: ctx.ui_clicked(button_ref)").small().weak());
        changed
    }
}
impl Gizmo for UiButton {}

impl Inspect for UiToggle {
    fn inspector_ui(&mut self, ui: &mut Ui, _ctx: &InspectCtx) -> bool {
        let mut changed = false;
        property_row(ui, "On", &mut changed, |ui| ui.checkbox(&mut self.on, ""));
        property_row(ui, "Interactable", &mut changed, |ui| ui.checkbox(&mut self.interactable, ""));
        rgba_row(ui, "Box", &mut self.box_color, &mut changed);
        rgba_row(ui, "Check", &mut self.check_color, &mut changed);
        changed
    }
}
impl Gizmo for UiToggle {}

impl Inspect for UiSlider {
    fn inspector_ui(&mut self, ui: &mut Ui, _ctx: &InspectCtx) -> bool {
        let mut changed = false;
        property_row(ui, "Min", &mut changed, |ui| ui.add(DragValue::new(&mut self.min).speed(0.1)));
        property_row(ui, "Max", &mut changed, |ui| ui.add(DragValue::new(&mut self.max).speed(0.1)));
        let (lo, hi) = (self.min.min(self.max), self.max.max(self.min));
        property_row(ui, "Value", &mut changed, |ui| ui.add(egui::Slider::new(&mut self.value, lo..=hi)));
        property_row(ui, "Whole numbers", &mut changed, |ui| ui.checkbox(&mut self.whole_numbers, ""));
        property_row(ui, "Interactable", &mut changed, |ui| ui.checkbox(&mut self.interactable, ""));
        rgba_row(ui, "Track", &mut self.track_color, &mut changed);
        rgba_row(ui, "Fill", &mut self.fill_color, &mut changed);
        rgba_row(ui, "Handle", &mut self.handle_color, &mut changed);
        property_row(ui, "Audio bus", &mut changed, |ui| {
            ui.add(egui::TextEdit::singleline(&mut self.audio_bus).hint_text("none (e.g. Music)"))
                .on_hover_text("Drives this Audio Mixer bus's volume (0..1 along the slider)")
        });
        changed
    }
}
impl Gizmo for UiSlider {}

impl Inspect for UiProgressBar {
    fn inspector_ui(&mut self, ui: &mut Ui, _ctx: &InspectCtx) -> bool {
        let mut changed = false;
        property_row(ui, "Value", &mut changed, |ui| ui.add(egui::Slider::new(&mut self.value, 0.0..=1.0)));
        enum_row(ui, "Direction", "citrus-ui-fill", &mut self.direction, &UiFillDirection::ALL, UiFillDirection::label, &mut changed);
        rgba_row(ui, "Background", &mut self.background, &mut changed);
        rgba_row(ui, "Fill", &mut self.fill, &mut changed);
        property_row(ui, "Corner radius", &mut changed, |ui| {
            ui.add(DragValue::new(&mut self.corner_radius).speed(0.2).range(0.0..=200.0))
        });
        ui.label(egui::RichText::new("Gameplay: ctx.modify::<UiProgressBar>(bar, |b| b.value = hp)").small().weak());
        changed
    }
}
impl Gizmo for UiProgressBar {}

impl Inspect for UiPauseMenu {
    fn inspector_ui(&mut self, ui: &mut Ui, _ctx: &InspectCtx) -> bool {
        let mut changed = false;
        property_row(ui, "Panel", &mut changed, |ui| {
            ui.add(egui::TextEdit::singleline(&mut self.panel).hint_text("object name"))
        });
        property_row(ui, "Pause time", &mut changed, |ui| ui.checkbox(&mut self.pause_time, ""));
        property_row(ui, "Lock cursor in game", &mut changed, |ui| ui.checkbox(&mut self.lock_cursor_in_game, ""));
        ui.label(
            egui::RichText::new("Escape / gamepad back toggles the panel. Resume button: Hide <panel>.")
                .small()
                .weak(),
        );
        changed
    }
}
impl Gizmo for UiPauseMenu {}

impl Inspect for UiTextField {
    fn inspector_ui(&mut self, ui: &mut Ui, _ctx: &InspectCtx) -> bool {
        let mut changed = false;
        property_row(ui, "Text", &mut changed, |ui| ui.text_edit_singleline(&mut self.text));
        property_row(ui, "Placeholder", &mut changed, |ui| ui.text_edit_singleline(&mut self.placeholder));
        property_row(ui, "Max length", &mut changed, |ui| ui.add(DragValue::new(&mut self.max_length).range(0..=10000)));
        property_row(ui, "Size", &mut changed, |ui| ui.add(DragValue::new(&mut self.size).speed(0.2).range(1.0..=300.0)));
        rgba_row(ui, "Text color", &mut self.text_color, &mut changed);
        rgba_row(ui, "Placeholder color", &mut self.placeholder_color, &mut changed);
        property_row(ui, "Password", &mut changed, |ui| ui.checkbox(&mut self.password, ""));
        property_row(ui, "Interactable", &mut changed, |ui| ui.checkbox(&mut self.interactable, ""));
        ui.label(egui::RichText::new("Gameplay: ctx.ui_submitted(field) on Enter").small().weak());
        changed
    }
}
impl Gizmo for UiTextField {}

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
        // Unity's box offset: the box aligns to the room while the OBJECT stays
        // where reflections should look right from (eye height).
        axis_row(ui, "Box Offset", &mut self.box_offset, &mut changed);
        property_row(ui, "Lock box in world", &mut changed, |ui| {
            ui.checkbox(&mut self.lock_box, "")
                .on_hover_text(
                    "Freeze the box in world space: moving the probe object then only \
                     repositions the capture point inside the box (Box Offset auto-compensates). \
                     Align the box to the room first, then enable this and place the capture \
                     point freely.",
                )
        });
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
            .button(format!("{} Bake This Probe", ph::SPARKLE))
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
impl Inspect for PlanarReflection {
    fn inspector_ui(&mut self, ui: &mut Ui, _ctx: &InspectCtx) -> bool {
        let mut changed = false;
        property_row(ui, "Max roughness", &mut changed, |ui| {
            ui.add(egui::Slider::new(&mut self.max_roughness, 0.05..=0.95))
        });
        ui.label(
            RichText::new(
                "True mirror: the scene re-renders every frame from the camera \
                 mirrored about this object's local +Y (up) plane — put it on a floor \
                 or a wall-mirror quad. Fades back to the reflection probe above the \
                 roughness cutoff. Each active surface costs a scene pass (max 4).",
            )
            .small()
            .weak(),
        );
        changed
    }
}
impl Gizmo for PlanarReflection {}

impl Gizmo for ReflectionProbe {
    fn gizmos(&self) -> Vec<GizmoSpec> {
        vec![GizmoSpec::Box {
            center: glam::Vec3::from(self.box_offset),
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

impl Inspect for FluxVoxelLight {
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
impl Gizmo for FluxVoxelLight {}

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
        property_row(ui, "Mixer bus", &mut changed, |ui| {
            ui.add(egui::TextEdit::singleline(&mut self.bus).desired_width(110.0))
        });
        ui.label(
            RichText::new(".wav · .flac · .mp3 · .ogg (project-relative)")
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
            property_row(ui, "HRTF (binaural)", &mut changed, |ui| {
                ui.checkbox(&mut self.hrtf, "").on_hover_text("Off: plain stereo panning")
            });
            property_row(ui, "Air absorption", &mut changed, |ui| ui.checkbox(&mut self.air_absorption, ""));
            property_row(ui, "Occlusion", &mut changed, |ui| {
                ui.checkbox(&mut self.occlusion, "")
                    .on_hover_text("Static geometry between source and listener muffles it")
            });
            property_row(ui, "Directivity", &mut changed, |ui| {
                ui.add(egui::Slider::new(&mut self.directivity, 0.0..=1.0))
                    .on_hover_text("0 omnidirectional, 0.5 cardioid, 1 dipole along the object's forward (-Z)")
            });
            if self.directivity > 0.0 {
                property_row(ui, "Sharpness", &mut changed, |ui| {
                    ui.add(egui::Slider::new(&mut self.directivity_sharpness, 0.0..=8.0))
                });
            }
            property_row(ui, "Reflections", &mut changed, |ui| {
                ui.checkbox(&mut self.reflections, "")
                    .on_hover_text("Simulate this source's own reflections; off uses the listener reverb")
            });
            property_row(ui, "Pathing", &mut changed, |ui| {
                ui.checkbox(&mut self.pathing, "")
                    .on_hover_text("Sound bends around corners (needs a baked Audio Probe Volume)")
            });
            property_row(ui, "Via ambisonics", &mut changed, |ui| {
                ui.checkbox(&mut self.spatialize_via_ambisonics, "")
                    .on_hover_text("Encode to ambisonics and decode instead of direct HRTF")
            });
        }
        const ORDERS: [u8; 4] = [0, 1, 2, 3];
        enum_row(ui, "Ambisonic clip", "citrus-audio-ambi", &mut self.ambisonic_order, &ORDERS, |o| match o {
            0 => "No",
            1 => "1st order",
            2 => "2nd order",
            _ => "3rd order",
        }, &mut changed);
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

impl Inspect for AcousticMaterial {
    fn inspector_ui(&mut self, ui: &mut Ui, _ctx: &InspectCtx) -> bool {
        let mut changed = false;
        let before = self.preset;
        enum_row(ui, "Preset", "citrus-acoustic-preset", &mut self.preset, &AcousticPreset::ALL, AcousticPreset::label, &mut changed);
        if self.preset != before
            && let Some((a, sc, t)) = before.values()
            && self.preset == AcousticPreset::Custom
        {
            // Start a custom material from the preset it replaces.
            (self.absorption, self.scattering, self.transmission) = (a, sc, t);
        }
        let custom = self.preset == AcousticPreset::Custom;
        let (mut a, mut sc, mut t) = self.values();
        ui.add_enabled_ui(custom, |ui| {
            for (label, band) in [("Absorption", &mut a), ("Transmission", &mut t)] {
                property_row(ui, label, &mut changed, |ui| {
                    let mut any = false;
                    let mut resp = ui.horizontal(|ui| {
                        for v in band.iter_mut() {
                            any |= ui.add(DragValue::new(v).speed(0.005).range(0.0..=1.0).max_decimals(3)).changed();
                        }
                    }).response;
                    if any {
                        resp.mark_changed();
                    }
                    resp
                });
            }
            property_row(ui, "Scattering", &mut changed, |ui| ui.add(egui::Slider::new(&mut sc, 0.0..=1.0)));
        });
        if custom {
            (self.absorption, self.scattering, self.transmission) = (a, sc, t);
        }
        ui.label(RichText::new("Bands: low / mid / high").small().weak());
        property_row(ui, "Moving occluder", &mut changed, |ui| {
            ui.checkbox(&mut self.dynamic, "")
                .on_hover_text("Occlude and reflect even when not Static (follows the object)")
        });
        changed
    }
}
impl Gizmo for AcousticMaterial {}

impl Inspect for AudioProbeVolume {
    fn inspector_ui(&mut self, ui: &mut Ui, _ctx: &InspectCtx) -> bool {
        let mut changed = false;
        axis_row(ui, "Size", &mut self.size, &mut changed);
        property_row(ui, "Spacing", &mut changed, |ui| {
            ui.add(DragValue::new(&mut self.spacing).speed(0.05).range(0.25..=50.0).suffix(" m"))
        });
        property_row(ui, "Height", &mut changed, |ui| {
            ui.add(DragValue::new(&mut self.height).speed(0.05).range(0.0..=20.0).suffix(" m"))
        });
        property_row(ui, "Bake reverb", &mut changed, |ui| ui.checkbox(&mut self.reverb, ""));
        property_row(ui, "Bake pathing", &mut changed, |ui| ui.checkbox(&mut self.pathing, ""));
        ui.label(RichText::new("Probes sit on floor geometry inside the box. Tools > Bake Audio.").small().weak());
        changed
    }
}
impl Gizmo for AudioProbeVolume {
    fn gizmos(&self) -> Vec<GizmoSpec> {
        let half = glam::Vec3::from(self.size) * 0.5;
        let mut g = vec![GizmoSpec::Box {
            center: glam::Vec3::ZERO,
            half_extents: half,
            object_anchored: true,
            blend: 0.0,
            color: crate::theme::TYPE_GI,
        }];
        // Preview of the floor grid (the bake snaps probes to real floors).
        let step = self.spacing.max(0.25);
        let (nx, nz) = (((half.x * 2.0) / step).floor() as usize + 1, ((half.z * 2.0) / step).floor() as usize + 1);
        if let Some(positions) = display_points(nx * nz, || {
            let mut p = Vec::with_capacity(nx * nz);
            for ix in 0..nx {
                for iz in 0..nz {
                    p.push(glam::Vec3::new(-half.x + ix as f32 * step, -half.y + self.height, -half.z + iz as f32 * step));
                }
            }
            p
        }) {
            g.push(GizmoSpec::Points { positions, color: crate::theme::with_alpha(crate::theme::TYPE_GI, 0.7) });
        }
        g
    }
    fn apply_gizmo_edit(&mut self, edit: GizmoEdit) {
        if let GizmoEdit::Box { half_extents, .. } = edit {
            self.size = (half_extents * 2.0).to_array();
        }
    }
}
