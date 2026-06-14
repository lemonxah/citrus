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
    Component, ComponentRegistry, LightComponent, LightKind, LightMode, LightProbeVolume,
    MeshCollider, ObjectId, ObjectRef, RigidBody, ShadowType, Spin, SphereCollider, TypedComponent,
    VolumeComponent,
    COLLISION_LAYERS,
};
use egui::collapsing_header::{CollapsingState, paint_default_icon};
use egui::{DragValue, Label, RichText, ScrollArea, Sense, Ui};

/// egui-memory key under which the Scene tree publishes the index of the object
/// currently being dragged (a `usize`, so its slot is shared across the
/// plugin/egui dylib boundary). `ObjectRef` drop targets read it.
pub const DRAG_OBJECT_KEY: &str = "citrus-drag-object";

/// Context handed to a component's inspector: the scene's object list (for
/// `ObjectRef` pickers) plus a helper to draw the picker.
pub struct InspectCtx<'a> {
    /// (id, display name) of every object in the scene.
    pub objects: &'a [(ObjectId, String)],
}

impl InspectCtx<'_> {
    /// Draw an object-reference row as a **drop target**: drag an object from
    /// the Scene tree onto the box to set it; the ✕ clears it. Returns true if
    /// the reference changed.
    ///
    /// Deliberately avoids egui's drag-and-drop API: that goes through egui's
    /// `DragAndDrop` context plugin, which panics when called from a plugin's
    /// own (separately-linked) egui — same `TypeId`-mismatch class as the
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
}

/// Editor inspector for a component: draw widgets, return true if anything
/// changed. Implemented per component in the editor (or a plugin), keeping the
/// runtime `Component` egui-free.
pub trait Inspect {
    fn inspector_ui(&mut self, ui: &mut Ui, ctx: &InspectCtx) -> bool;
}

/// Context for drawing a component's editor viewport gizmo.
pub struct GizmoCtx<'a> {
    pub painter: &'a egui::Painter,
    /// World-space point -> screen position (None if behind the camera).
    pub world_to_screen: &'a dyn Fn(glam::Vec3) -> Option<egui::Pos2>,
    /// The owning object's world transform.
    pub world: glam::Mat4,
    /// True when the owning object is selected.
    pub selected: bool,
}

/// Editor viewport gizmo for a component. Default draws nothing; components
/// (and plugins) override to draw widgets when their object is selected.
pub trait Gizmo {
    fn draw_gizmo(&self, _ctx: &GizmoCtx) {}
}

type InspectFn = fn(&mut dyn Component, &mut Ui, &InspectCtx) -> bool;
type GizmoFn = fn(&dyn Component, &GizmoCtx);

/// Editor-side dispatch: component name -> inspector / gizmo. Built-ins
/// register in [`Self::with_builtins`]; plugins register through their
/// `citrus_register_editor` export.
#[derive(Default)]
pub struct EditorComponents {
    inspect: HashMap<&'static str, InspectFn>,
    gizmo: HashMap<&'static str, GizmoFn>,
}

impl EditorComponents {
    pub fn with_builtins() -> Self {
        let mut e = Self::default();
        e.register::<CameraComponent>();
        e.register::<LightComponent>();
        e.register::<LightProbeVolume>();
        e.register::<AudioSource>();
        e.register::<AudioListener>();
        e.register::<BoxCollider>();
        e.register::<SphereCollider>();
        e.register::<MeshCollider>();
        e.register::<Spin>();
        e.register::<Bob>();
        e.register::<RigidBody>();
        e.register::<VolumeComponent>();
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
        self.gizmo.insert(T::NAME, |c, ctx| {
            if let Some(t) = c.as_any().downcast_ref::<T>() {
                t.draw_gizmo(ctx);
            }
        });
    }

    /// Draw a component's inspector (no-op if unregistered).
    pub fn inspect(&self, component: &mut dyn Component, ui: &mut Ui, ctx: &InspectCtx) -> bool {
        match self.inspect.get(component.type_name()) {
            Some(f) => f(component, ui, ctx),
            None => false,
        }
    }

    /// Draw a component's viewport gizmo (no-op if unregistered).
    pub fn draw_gizmo(&self, component: &dyn Component, ctx: &GizmoCtx) {
        if let Some(f) = self.gizmo.get(component.type_name()) {
            f(component, ctx);
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
    fn inspector_ui(&mut self, ui: &mut Ui, _ctx: &InspectCtx) -> bool {
        let mut changed = false;
        property_row(ui, "Profile", &mut changed, |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.profile)
                    .hint_text("post/cinematic.postfx"),
            )
            .on_hover_text("Project-relative path to a .postfx profile")
        });
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
impl Gizmo for LightProbeVolume {}

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
impl Gizmo for AudioSource {}

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
impl Gizmo for BoxCollider {}

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
impl Gizmo for SphereCollider {}

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
