//! Inspectors and viewport gizmos for the built-in gameplay kit components
//! (citrus-core `kit`): controllers, weapons, health, HUD bindings,
//! click-to-move, RTS camera / selection and the VR rig.

use citrus_core::{
    ClickToMove, ConflictPolicy, ControlsMenu, DeathAction, FirstPersonController, FollowCamera, Grabbable, Health, HitscanWeapon, HudBinding, HudValue,
    Interactable, ResourceAmount, ResourceStore, RtsCamera, RtsSelection, Selectable, Swimmer, TargetDummy, ThirdPersonController,
    VrAvatar, VrLocomotion, VrPlayer, WeaponTrigger,
};
use egui::{DragValue, Ui};
use glam::Vec3;

use crate::components::{EditorComponents, Gizmo, GizmoEdit, GizmoSpec, Inspect, InspectCtx};
use crate::theme;
use crate::widgets;

pub(crate) fn register(e: &mut EditorComponents) {
    e.register::<Health>();
    e.register::<HitscanWeapon>();
    e.register::<TargetDummy>();
    e.register::<FirstPersonController>();
    e.register::<Swimmer>();
    e.register::<ThirdPersonController>();
    e.register::<FollowCamera>();
    e.register::<ClickToMove>();
    e.register::<Interactable>();
    e.register::<RtsCamera>();
    e.register::<RtsSelection>();
    e.register::<Selectable>();
    e.register::<ResourceStore>();
    e.register::<HudBinding>();
    e.register::<VrPlayer>();
    e.register::<Grabbable>();
    e.register::<VrAvatar>();
    e.register::<ControlsMenu>();
}

/// Header icon for kit components (None = not a kit component).
pub(crate) fn icon(name: &str) -> Option<(crate::icons::Icon, egui::Color32)> {
    use crate::icons::Icon;
    let pick = match name {
        "Health" | "Target Dummy" => (Icon::Target, theme::STATUS_ERROR),
        "Hitscan Weapon" => (Icon::Target, theme::TYPE_LIGHT),
        "First Person Controller" | "Third Person Controller" | "Follow Camera" | "RTS Camera" => {
            (Icon::Camera, theme::TYPE_CAMERA)
        }
        "Click To Move" | "RTS Selection" | "Selectable" => (Icon::ToolSelect, theme::TYPE_MESH),
        "Interactable" => (Icon::ToolSelect, theme::TYPE_LIGHT),
        "Resource Store" => (Icon::Stats, theme::TYPE_GROUP),
        "HUD Binding" | "Controls Menu" => (Icon::UiText, theme::TYPE_UI),
        "VR Player" | "VR Avatar" | "Grabbable" => (Icon::Gizmo, theme::TYPE_GI),
        _ => return None,
    };
    Some(pick)
}

// ------------------------------------------------------------------ rows

fn drag(ui: &mut Ui, label: &str, v: &mut f32, speed: f64, range: std::ops::RangeInclusive<f32>, changed: &mut bool) {
    widgets::property_row(ui, label, |ui| *changed |= ui.add(DragValue::new(v).speed(speed).range(range)).changed());
}

fn uint(ui: &mut Ui, label: &str, v: &mut u32, max: u32, changed: &mut bool) {
    widgets::property_row(ui, label, |ui| *changed |= ui.add(DragValue::new(v).range(0..=max)).changed());
}

fn check(ui: &mut Ui, label: &str, v: &mut bool, changed: &mut bool) {
    widgets::property_row(ui, label, |ui| *changed |= ui.checkbox(v, "").changed());
}

fn text(ui: &mut Ui, label: &str, v: &mut String, changed: &mut bool) {
    widgets::property_row(ui, label, |ui| *changed |= ui.text_edit_singleline(v).changed());
}

fn vec3(ui: &mut Ui, label: &str, v: &mut [f32; 3], changed: &mut bool) {
    widgets::property_row(ui, label, |ui| *changed |= widgets::vec3_field(ui, v, 2, 0.02));
}

fn combo<T: Copy + PartialEq>(ui: &mut Ui, id: &str, label: &str, value: &mut T, options: &[(T, &str)], changed: &mut bool) {
    let current = options.iter().find(|(v, _)| v == value).map(|(_, l)| *l).unwrap_or("");
    widgets::property_row(ui, label, |ui| {
        egui::ComboBox::from_id_salt(id).selected_text(current).show_ui(ui, |ui| {
            for (v, l) in options {
                *changed |= ui.selectable_value(value, *v, *l).changed();
            }
        });
    });
}

/// Collapsible group of input action names.
fn actions(ui: &mut Ui, id: &str, fields: &mut [(&str, &mut String)], changed: &mut bool) {
    egui::CollapsingHeader::new("Input actions").id_salt(id).show(ui, |ui| {
        for (label, v) in fields.iter_mut() {
            text(ui, label, v, changed);
        }
    });
}

fn note(ui: &mut Ui, s: &str) {
    ui.label(egui::RichText::new(s).size(theme::SIZE_META).color(theme::TEXT_DIM));
}

fn ring(center: Vec3, r: f32) -> Vec<(Vec3, Vec3)> {
    let n = 32;
    (0..n)
        .map(|k| {
            let a = k as f32 / n as f32 * std::f32::consts::TAU;
            let b = (k + 1) as f32 / n as f32 * std::f32::consts::TAU;
            (center + Vec3::new(a.cos(), 0.0, a.sin()) * r, center + Vec3::new(b.cos(), 0.0, b.sin()) * r)
        })
        .collect()
}

const GIZMO_GREEN: egui::Color32 = theme::STATUS_OK;
const GIZMO_BLUE: egui::Color32 = theme::STATUS_INFO;
const GIZMO_WARM: egui::Color32 = theme::TYPE_LIGHT;

// ------------------------------------------------------------------ combat

impl Inspect for Health {
    fn inspector_ui(&mut self, ui: &mut Ui, _ctx: &InspectCtx) -> bool {
        let mut c = false;
        drag(ui, "Max", &mut self.max, 1.0, 1.0..=100_000.0, &mut c);
        drag(ui, "Current", &mut self.current, 1.0, 0.0..=self.max.max(1.0), &mut c);
        drag(ui, "Regen / sec", &mut self.regen_per_sec, 0.1, 0.0..=10_000.0, &mut c);
        drag(ui, "Regen delay", &mut self.regen_delay, 0.05, 0.0..=60.0, &mut c);
        check(ui, "Invulnerable", &mut self.invulnerable, &mut c);
        let opts: Vec<(DeathAction, &str)> = DeathAction::ALL.iter().map(|a| (*a, a.label())).collect();
        combo(ui, "citrus-health-death", "On death", &mut self.on_death, &opts, &mut c);
        c
    }
}
impl Gizmo for Health {}

impl Inspect for HitscanWeapon {
    fn inspector_ui(&mut self, ui: &mut Ui, ctx: &InspectCtx) -> bool {
        let mut c = false;
        let opts: Vec<(WeaponTrigger, &str)> = WeaponTrigger::ALL.iter().map(|t| (*t, t.label())).collect();
        combo(ui, "citrus-weapon-trigger", "Trigger", &mut self.trigger, &opts, &mut c);
        check(ui, "Automatic", &mut self.automatic, &mut c);
        drag(ui, "Fire rate", &mut self.fire_rate, 0.1, 0.1..=100.0, &mut c);
        drag(ui, "Damage", &mut self.damage, 0.5, 0.0..=100_000.0, &mut c);
        drag(ui, "Range", &mut self.range, 1.0, 0.1..=10_000.0, &mut c);
        drag(ui, "Impact force", &mut self.impact_force, 0.1, 0.0..=1000.0, &mut c);
        drag(ui, "Spread (deg)", &mut self.spread_deg, 0.05, 0.0..=45.0, &mut c);
        widgets::eyebrow(ui, "Ammo");
        uint(ui, "Magazine", &mut self.magazine_size, 10_000, &mut c);
        uint(ui, "Loaded", &mut self.ammo, self.magazine_size, &mut c);
        uint(ui, "Reserve", &mut self.reserve, 1_000_000, &mut c);
        check(ui, "Infinite reserve", &mut self.infinite_reserve, &mut c);
        drag(ui, "Reload time", &mut self.reload_time, 0.05, 0.0..=30.0, &mut c);
        widgets::eyebrow(ui, "References");
        c |= ctx.object_ref(ui, "Aim from", &mut self.aim_from);
        c |= ctx.object_ref(ui, "Muzzle", &mut self.muzzle);
        uint(ui, "Muzzle burst", &mut self.muzzle_burst, 1000, &mut c);
        c |= ctx.object_ref(ui, "Hit effect", &mut self.hit_effect);
        uint(ui, "Hit burst", &mut self.hit_burst, 1000, &mut c);
        c |= ctx.file_field(ui, "Fire sound", &mut self.fire_sound, "audio/shot.wav", &["wav", "ogg", "mp3", "flac"]);
        c |= ctx.file_field(ui, "Hit sound", &mut self.hit_sound, "audio/hit.wav", &["wav", "ogg", "mp3", "flac"]);
        let mut fields = [("Fire", &mut self.fire_action), ("Reload", &mut self.reload_action)];
        actions(ui, "citrus-weapon-actions", &mut fields, &mut c);
        c
    }
}
impl Gizmo for HitscanWeapon {
    fn gizmos(&self) -> Vec<GizmoSpec> {
        let len = self.range.min(8.0);
        vec![GizmoSpec::Lines { segments: vec![(Vec3::ZERO, Vec3::NEG_Z * len)], color: GIZMO_WARM, world: false }]
    }
}

impl Inspect for TargetDummy {
    fn inspector_ui(&mut self, ui: &mut Ui, _ctx: &InspectCtx) -> bool {
        let mut c = false;
        drag(ui, "Respawn delay", &mut self.respawn_delay, 0.05, 0.0..=120.0, &mut c);
        drag(ui, "Fall speed", &mut self.fall_speed, 5.0, 1.0..=3600.0, &mut c);
        uint(ui, "Points", &mut self.points, 100_000, &mut c);
        note(ui, "Needs a Health on the same object.");
        c
    }
}
impl Gizmo for TargetDummy {}

// ------------------------------------------------------------------ controllers

impl Inspect for Swimmer {
    fn inspector_ui(&mut self, ui: &mut Ui, _ctx: &InspectCtx) -> bool {
        let mut c = false;
        widgets::info_note(
            ui,
            "Tunes how the First / Third Person controllers swim: in water deeper than Swim depth they float with the head out, stroke where they look (dive by looking down; Jump rises, Crouch sinks), hold their breath under and climb out over an edge they swim into.",
        );
        drag(ui, "Swim depth (x height)", &mut self.swim_depth, 0.01, 0.2..=1.5, &mut c);
        drag(ui, "Swim speed", &mut self.swim_speed, 0.05, 0.0..=20.0, &mut c);
        drag(ui, "Sprint speed", &mut self.sprint_speed, 0.05, 0.0..=20.0, &mut c);
        drag(ui, "Stroke acceleration", &mut self.stroke_accel, 0.05, 0.1..=50.0, &mut c);
        drag(ui, "Water drag", &mut self.water_drag, 0.05, 0.0..=50.0, &mut c);
        drag(ui, "Float height (m)", &mut self.float_height, 0.01, -1.0..=1.0, &mut c);
        drag(ui, "Breath (s)", &mut self.breath, 0.5, 0.0..=600.0, &mut c);
        drag(ui, "Climb height (m)", &mut self.climb_height, 0.01, 0.0..=3.0, &mut c);
        drag(ui, "Climb time (s)", &mut self.climb_time, 0.01, 0.05..=5.0, &mut c);
        c
    }
}

impl Gizmo for Swimmer {}

impl Inspect for FirstPersonController {
    fn inspector_ui(&mut self, ui: &mut Ui, ctx: &InspectCtx) -> bool {
        let mut c = false;
        c |= ctx.object_ref(ui, "Camera", &mut self.camera);
        drag(ui, "Walk speed", &mut self.walk_speed, 0.05, 0.0..=100.0, &mut c);
        drag(ui, "Sprint speed", &mut self.sprint_speed, 0.05, 0.0..=100.0, &mut c);
        drag(ui, "Crouch speed", &mut self.crouch_speed, 0.05, 0.0..=100.0, &mut c);
        drag(ui, "Acceleration", &mut self.acceleration, 0.1, 0.1..=200.0, &mut c);
        drag(ui, "Air control", &mut self.air_control, 0.01, 0.0..=1.0, &mut c);
        drag(ui, "Jump height", &mut self.jump_height, 0.02, 0.0..=20.0, &mut c);
        drag(ui, "Gravity", &mut self.gravity, 0.1, 0.0..=200.0, &mut c);
        drag(ui, "Look sensitivity", &mut self.look_sensitivity, 0.005, 0.0..=5.0, &mut c);
        drag(ui, "Eye height", &mut self.eye_height, 0.01, 0.0..=10.0, &mut c);
        drag(ui, "Crouch eye height", &mut self.crouch_eye_height, 0.01, 0.0..=10.0, &mut c);
        drag(ui, "Crouch capsule height", &mut self.crouch_height, 0.01, 0.0..=10.0, &mut c);
        check(ui, "Crouch toggles", &mut self.crouch_toggle, &mut c);
        drag(ui, "Min pitch", &mut self.min_pitch, 0.5, -89.0..=0.0, &mut c);
        drag(ui, "Max pitch", &mut self.max_pitch, 0.5, 0.0..=89.0, &mut c);
        check(ui, "Lock cursor", &mut self.lock_cursor, &mut c);
        let mut fields = [
            ("Move", &mut self.move_action),
            ("Look", &mut self.look_action),
            ("Jump", &mut self.jump_action),
            ("Sprint", &mut self.sprint_action),
            ("Crouch", &mut self.crouch_action),
        ];
        actions(ui, "citrus-fps-actions", &mut fields, &mut c);
        c
    }
}
impl Gizmo for FirstPersonController {
    /// Eye and crouch-eye heights.
    fn gizmos(&self) -> Vec<GizmoSpec> {
        let mut s = vec![(Vec3::ZERO, Vec3::Y * self.eye_height)];
        s.extend(ring(Vec3::Y * self.eye_height, 0.15));
        s.extend(ring(Vec3::Y * self.crouch_eye_height, 0.1));
        vec![GizmoSpec::Lines { segments: s, color: GIZMO_BLUE, world: false }]
    }
}

impl Inspect for ThirdPersonController {
    fn inspector_ui(&mut self, ui: &mut Ui, ctx: &InspectCtx) -> bool {
        let mut c = false;
        c |= ctx.object_ref(ui, "Camera", &mut self.camera);
        note(ui, "The camera must be a root object.");
        drag(ui, "Walk speed", &mut self.walk_speed, 0.05, 0.0..=100.0, &mut c);
        drag(ui, "Sprint speed", &mut self.sprint_speed, 0.05, 0.0..=100.0, &mut c);
        drag(ui, "Aim speed", &mut self.aim_speed, 0.05, 0.0..=100.0, &mut c);
        drag(ui, "Acceleration", &mut self.acceleration, 0.1, 0.1..=200.0, &mut c);
        drag(ui, "Turn speed", &mut self.turn_speed, 5.0, 0.0..=5000.0, &mut c);
        drag(ui, "Jump height", &mut self.jump_height, 0.02, 0.0..=20.0, &mut c);
        drag(ui, "Gravity", &mut self.gravity, 0.1, 0.0..=200.0, &mut c);
        drag(ui, "Look sensitivity", &mut self.look_sensitivity, 0.005, 0.0..=5.0, &mut c);
        drag(ui, "Min pitch", &mut self.min_pitch, 0.5, -89.0..=0.0, &mut c);
        drag(ui, "Max pitch", &mut self.max_pitch, 0.5, 0.0..=89.0, &mut c);
        widgets::eyebrow(ui, "Camera boom");
        drag(ui, "Pivot height", &mut self.pivot_height, 0.01, 0.0..=10.0, &mut c);
        drag(ui, "Distance", &mut self.distance, 0.05, 0.0..=50.0, &mut c);
        drag(ui, "Aim distance", &mut self.aim_distance, 0.05, 0.0..=50.0, &mut c);
        drag(ui, "Shoulder", &mut self.shoulder_offset, 0.01, -5.0..=5.0, &mut c);
        drag(ui, "Aim shoulder", &mut self.aim_shoulder_offset, 0.01, -5.0..=5.0, &mut c);
        drag(ui, "Camera radius", &mut self.camera_radius, 0.01, 0.0..=2.0, &mut c);
        drag(ui, "Zoom rate", &mut self.zoom_rate, 0.1, 0.1..=100.0, &mut c);
        check(ui, "Lock cursor", &mut self.lock_cursor, &mut c);
        let mut fields = [
            ("Move", &mut self.move_action),
            ("Look", &mut self.look_action),
            ("Jump", &mut self.jump_action),
            ("Sprint", &mut self.sprint_action),
            ("Aim", &mut self.aim_action),
        ];
        actions(ui, "citrus-tps-actions", &mut fields, &mut c);
        c
    }
}
impl Gizmo for ThirdPersonController {
    /// The boom from the pivot to the hip and aim camera positions.
    fn gizmos(&self) -> Vec<GizmoSpec> {
        let pivot = Vec3::Y * self.pivot_height;
        let hip = pivot + Vec3::new(self.shoulder_offset, 0.0, self.distance);
        let aim = pivot + Vec3::new(self.aim_shoulder_offset, 0.0, self.aim_distance);
        vec![
            GizmoSpec::Lines { segments: vec![(Vec3::ZERO, pivot), (pivot, hip)], color: GIZMO_BLUE, world: false },
            GizmoSpec::Lines { segments: vec![(pivot, aim)], color: GIZMO_WARM, world: false },
        ]
    }
}

impl Inspect for FollowCamera {
    fn inspector_ui(&mut self, ui: &mut Ui, ctx: &InspectCtx) -> bool {
        let mut c = false;
        c |= ctx.object_ref(ui, "Target", &mut self.target);
        vec3(ui, "Offset", &mut self.offset, &mut c);
        vec3(ui, "Look offset", &mut self.look_offset, &mut c);
        drag(ui, "Smoothing", &mut self.smoothing, 0.1, 0.0..=100.0, &mut c);
        text(ui, "Zoom action", &mut self.zoom_action, &mut c);
        drag(ui, "Zoom step", &mut self.zoom_step, 0.01, 0.0..=1.0, &mut c);
        drag(ui, "Min zoom", &mut self.min_zoom, 0.01, 0.05..=10.0, &mut c);
        drag(ui, "Max zoom", &mut self.max_zoom, 0.01, 0.05..=10.0, &mut c);
        c
    }
}
impl Gizmo for FollowCamera {}

// ------------------------------------------------------------------ top-down

impl Inspect for ClickToMove {
    fn inspector_ui(&mut self, ui: &mut Ui, ctx: &InspectCtx) -> bool {
        let mut c = false;
        text(ui, "Click action", &mut self.click_action, &mut c);
        check(ui, "Hold to move", &mut self.hold_to_move, &mut c);
        c |= ctx.object_ref(ui, "Marker", &mut self.marker);
        drag(ui, "Max ray", &mut self.max_ray_distance, 1.0, 1.0..=10_000.0, &mut c);
        note(ui, "Needs a Nav Agent here and a NavMesh Surface in the scene.");
        c
    }
}
impl Gizmo for ClickToMove {}

impl Inspect for Interactable {
    fn inspector_ui(&mut self, ui: &mut Ui, ctx: &InspectCtx) -> bool {
        let mut c = false;
        text(ui, "Prompt", &mut self.prompt, &mut c);
        drag(ui, "Range", &mut self.range, 0.05, 0.1..=100.0, &mut c);
        c |= ctx.object_ref(ui, "Toggle", &mut self.toggle);
        check(ui, "Rebake navmesh", &mut self.rebake_navmesh, &mut c);
        c |= ctx.file_field(ui, "Sound", &mut self.sound, "audio/click.wav", &["wav", "ogg", "mp3", "flac"]);
        drag(ui, "Cooldown", &mut self.cooldown, 0.05, 0.0..=60.0, &mut c);
        check(ui, "Single use", &mut self.single_use, &mut c);
        c
    }
}
impl Gizmo for Interactable {
    fn gizmos(&self) -> Vec<GizmoSpec> {
        vec![GizmoSpec::Range { center: Vec3::ZERO, radius: self.range, color: GIZMO_WARM }]
    }
    fn apply_gizmo_edit(&mut self, edit: GizmoEdit) {
        if let GizmoEdit::Range { radius, .. } = edit {
            self.range = radius.max(0.1);
        }
    }
}

// ------------------------------------------------------------------ strategy

impl Inspect for RtsCamera {
    fn inspector_ui(&mut self, ui: &mut Ui, _ctx: &InspectCtx) -> bool {
        let mut c = false;
        drag(ui, "Yaw", &mut self.yaw, 0.5, -360.0..=360.0, &mut c);
        drag(ui, "Pitch", &mut self.pitch, 0.5, 5.0..=89.0, &mut c);
        drag(ui, "Distance", &mut self.distance, 0.1, 0.5..=1000.0, &mut c);
        drag(ui, "Min distance", &mut self.min_distance, 0.1, 0.5..=1000.0, &mut c);
        drag(ui, "Max distance", &mut self.max_distance, 0.1, 0.5..=1000.0, &mut c);
        drag(ui, "Pan speed", &mut self.pan_speed, 0.1, 0.0..=500.0, &mut c);
        check(ui, "Edge pan", &mut self.edge_pan, &mut c);
        drag(ui, "Edge margin (px)", &mut self.edge_margin, 0.5, 0.0..=200.0, &mut c);
        drag(ui, "Zoom step", &mut self.zoom_step, 0.05, 0.0..=100.0, &mut c);
        drag(ui, "Rotate speed", &mut self.rotate_speed, 1.0, 0.0..=1000.0, &mut c);
        drag(ui, "Smoothing", &mut self.smoothing, 0.1, 0.0..=100.0, &mut c);
        widgets::property_row(ui, "Bounds min", |ui| {
            c |= ui.add(DragValue::new(&mut self.bounds_min[0]).speed(0.5).prefix("x ")).changed();
            c |= ui.add(DragValue::new(&mut self.bounds_min[1]).speed(0.5).prefix("z ")).changed();
        });
        widgets::property_row(ui, "Bounds max", |ui| {
            c |= ui.add(DragValue::new(&mut self.bounds_max[0]).speed(0.5).prefix("x ")).changed();
            c |= ui.add(DragValue::new(&mut self.bounds_max[1]).speed(0.5).prefix("z ")).changed();
        });
        let mut fields =
            [("Move", &mut self.move_action), ("Zoom", &mut self.zoom_action), ("Rotate", &mut self.rotate_action)];
        actions(ui, "citrus-rts-camera-actions", &mut fields, &mut c);
        c
    }
}
impl Gizmo for RtsCamera {
    /// The pan bounds on the ground (world space).
    fn gizmos(&self) -> Vec<GizmoSpec> {
        let (a, b) = (self.bounds_min, self.bounds_max);
        let p = |x: f32, z: f32| Vec3::new(x, 0.05, z);
        let segments = vec![
            (p(a[0], a[1]), p(b[0], a[1])),
            (p(b[0], a[1]), p(b[0], b[1])),
            (p(b[0], b[1]), p(a[0], b[1])),
            (p(a[0], b[1]), p(a[0], a[1])),
        ];
        vec![GizmoSpec::Lines { segments, color: GIZMO_BLUE, world: true }]
    }
}

impl Inspect for RtsSelection {
    fn inspector_ui(&mut self, ui: &mut Ui, ctx: &InspectCtx) -> bool {
        let mut c = false;
        c |= ctx.object_ref(ui, "Box visual", &mut self.box_visual);
        drag(ui, "UI ref. height", &mut self.ui_reference_height, 1.0, 1.0..=8192.0, &mut c);
        drag(ui, "Drag threshold", &mut self.drag_threshold, 0.5, 0.0..=100.0, &mut c);
        drag(ui, "Formation spacing", &mut self.formation_spacing, 0.05, 0.1..=50.0, &mut c);
        c |= ctx.object_ref(ui, "Move marker", &mut self.move_marker);
        uint(ui, "Team", &mut self.team, 1000, &mut c);
        let mut fields = [
            ("Select", &mut self.select_action),
            ("Command", &mut self.command_action),
            ("Add to selection", &mut self.add_action),
        ];
        actions(ui, "citrus-rts-selection-actions", &mut fields, &mut c);
        c
    }
}
impl Gizmo for RtsSelection {}

impl Inspect for Selectable {
    fn inspector_ui(&mut self, ui: &mut Ui, ctx: &InspectCtx) -> bool {
        let mut c = false;
        c |= ctx.object_ref(ui, "Ring", &mut self.ring);
        uint(ui, "Team", &mut self.team, 1000, &mut c);
        c
    }
}
impl Gizmo for Selectable {
    fn gizmos(&self) -> Vec<GizmoSpec> {
        vec![GizmoSpec::Lines { segments: ring(Vec3::Y * 0.05, 0.6), color: GIZMO_GREEN, world: false }]
    }
}

impl Inspect for ResourceStore {
    fn inspector_ui(&mut self, ui: &mut Ui, _ctx: &InspectCtx) -> bool {
        let mut c = false;
        let mut remove = None;
        for (i, r) in self.resources.iter_mut().enumerate() {
            ui.push_id(i, |ui| {
                ui.horizontal(|ui| {
                    widgets::eyebrow(ui, if r.name.is_empty() { "Resource" } else { &r.name });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if widgets::icon_button(ui, crate::icons::Icon::Trash, "Remove resource", false).clicked() {
                            remove = Some(i);
                        }
                    });
                });
                text(ui, "Name", &mut r.name, &mut c);
                drag(ui, "Amount", &mut r.amount, 1.0, -1e9..=1e9, &mut c);
                drag(ui, "Income / sec", &mut r.income_per_sec, 0.1, -1e6..=1e6, &mut c);
                drag(ui, "Capacity", &mut r.capacity, 1.0, 0.0..=1e9, &mut c);
            });
        }
        if let Some(i) = remove {
            self.resources.remove(i);
            c = true;
        }
        if widgets::dashed_button(ui, Some(crate::icons::Icon::Plus), "Add resource").clicked() {
            self.resources.push(ResourceAmount::default());
            c = true;
        }
        c
    }
}
impl Gizmo for ResourceStore {}

impl Inspect for HudBinding {
    fn inspector_ui(&mut self, ui: &mut Ui, ctx: &InspectCtx) -> bool {
        let mut c = false;
        c |= ctx.object_ref(ui, "Source", &mut self.source);
        widgets::property_row(ui, "Value", |ui| {
            egui::ComboBox::from_id_salt("citrus-hud-value").selected_text(self.value.label()).show_ui(ui, |ui| {
                for kind in HudValue::kinds() {
                    let same = std::mem::discriminant(&kind) == std::mem::discriminant(&self.value);
                    if ui.selectable_label(same, kind.label()).clicked() && !same {
                        self.value = kind;
                        c = true;
                    }
                }
            });
        });
        if let HudValue::Resource(name) = &mut self.value {
            text(ui, "Resource", name, &mut c);
        }
        text(ui, "Format", &mut self.format, &mut c);
        if self.value == HudValue::Ammo {
            text(ui, "Reloading text", &mut self.reloading_text, &mut c);
        }
        note(ui, "Writes into this object's UI Text ({value}, {max}) and UI Progress Bar.");
        c
    }
}
impl Gizmo for HudBinding {}

// ------------------------------------------------------------------ controls menu

impl Inspect for ControlsMenu {
    fn inspector_ui(&mut self, ui: &mut Ui, _ctx: &InspectCtx) -> bool {
        let mut c = false;
        text(ui, "Name prefix", &mut self.prefix, &mut c);
        uint(ui, "Rows", &mut self.rows, 32, &mut c);
        uint(ui, "Bindings per row", &mut self.columns, 2, &mut c);
        let opts: Vec<(ConflictPolicy, &str)> = ConflictPolicy::ALL.iter().map(|p| (*p, p.label())).collect();
        combo(ui, "citrus-controls-conflicts", "On conflict", &mut self.conflicts, &opts, &mut c);
        widgets::eyebrow(ui, "Hidden actions");
        let mut remove = None;
        for (i, name) in self.hidden_actions.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                c |= ui.text_edit_singleline(name).changed();
                if ui.small_button(egui_phosphor::regular::X).on_hover_text("Show this action again").clicked() {
                    remove = Some(i);
                }
            });
        }
        if let Some(i) = remove {
            self.hidden_actions.remove(i);
            c = true;
        }
        if ui.small_button("Hide an action").clicked() {
            self.hidden_actions.push("Look".into());
            c = true;
        }
        note(
            ui,
            "Fills the rows named \"<prefix> Row N ...\" (see the Controls UI template) with the shown scheme's actions. Players click a binding, then press the new input; remaps save to their input.ron.",
        );
        c
    }
}
impl Gizmo for ControlsMenu {}

// ------------------------------------------------------------------ VR

impl Inspect for VrPlayer {
    fn inspector_ui(&mut self, ui: &mut Ui, ctx: &InspectCtx) -> bool {
        let mut c = false;
        c |= ctx.object_ref(ui, "Head", &mut self.head);
        c |= ctx.object_ref(ui, "Left hand", &mut self.left_hand);
        c |= ctx.object_ref(ui, "Right hand", &mut self.right_hand);
        let opts: Vec<(VrLocomotion, &str)> = VrLocomotion::ALL.iter().map(|l| (*l, l.label())).collect();
        combo(ui, "citrus-vr-locomotion", "Locomotion", &mut self.locomotion, &opts, &mut c);
        drag(ui, "Move speed", &mut self.move_speed, 0.05, 0.0..=50.0, &mut c);
        drag(ui, "Snap turn (deg)", &mut self.snap_turn, 1.0, 0.0..=180.0, &mut c);
        drag(ui, "Smooth turn speed", &mut self.smooth_turn_speed, 1.0, 0.0..=720.0, &mut c);
        c |= ctx.object_ref(ui, "Teleport marker", &mut self.teleport_marker);
        drag(ui, "Teleport speed", &mut self.teleport_speed, 0.1, 1.0..=50.0, &mut c);
        drag(ui, "Grab radius", &mut self.grab_radius, 0.01, 0.0..=2.0, &mut c);
        text(ui, "Spawn tag", &mut self.spawn_tag, &mut c);
        widgets::eyebrow(ui, "Desktop fallback");
        check(ui, "Enabled", &mut self.desktop_fallback, &mut c);
        drag(ui, "Eye height", &mut self.eye_height, 0.01, 0.0..=5.0, &mut c);
        drag(ui, "Look sensitivity", &mut self.look_sensitivity, 0.005, 0.0..=5.0, &mut c);
        let mut fields = [
            ("Move", &mut self.move_action),
            ("Look", &mut self.look_action),
            ("Grab", &mut self.grab_action),
            ("Fire", &mut self.fire_action),
            ("Teleport", &mut self.teleport_action),
        ];
        actions(ui, "citrus-vr-actions", &mut fields, &mut c);
        widgets::eyebrow(ui, "VR controller actions");
        let mut fields = [
            ("Turn", &mut self.turn_action),
            ("Grab left", &mut self.grab_left_action),
            ("Grab right", &mut self.grab_right_action),
            ("Use left", &mut self.use_left_action),
            ("Use right", &mut self.use_right_action),
        ];
        actions(ui, "citrus-vr-controller-actions", &mut fields, &mut c);
        note(ui, "Move and Teleport are shared with the desktop fallback. Bind these in Project Settings > Input (VR Controllers scheme).");
        c
    }
}
impl Gizmo for VrPlayer {
    /// A 2 x 2 m play-space square on the floor plus the desktop eye height.
    fn gizmos(&self) -> Vec<GizmoSpec> {
        let p = |x: f32, z: f32| Vec3::new(x, 0.02, z);
        let mut s = vec![(p(-1.0, -1.0), p(1.0, -1.0)), (p(1.0, -1.0), p(1.0, 1.0)), (p(1.0, 1.0), p(-1.0, 1.0)), (p(-1.0, 1.0), p(-1.0, -1.0))];
        s.push((Vec3::ZERO, Vec3::Y * self.eye_height));
        s.push((Vec3::Y * self.eye_height, Vec3::new(0.0, self.eye_height, -0.4)));
        vec![GizmoSpec::Lines { segments: s, color: GIZMO_BLUE, world: false }]
    }
}

impl Inspect for Grabbable {
    fn inspector_ui(&mut self, ui: &mut Ui, _ctx: &InspectCtx) -> bool {
        let mut c = false;
        check(ui, "Snap to hand", &mut self.snap_to_hand, &mut c);
        if self.snap_to_hand {
            vec3(ui, "Hold position", &mut self.hold_position, &mut c);
            vec3(ui, "Hold rotation", &mut self.hold_rotation, &mut c);
        }
        drag(ui, "Radius", &mut self.radius, 0.01, 0.0..=5.0, &mut c);
        check(ui, "Throwable", &mut self.throwable, &mut c);
        drag(ui, "Return after", &mut self.return_after, 0.1, 0.0..=600.0, &mut c);
        note(ui, "Must be a root object. Throwing needs a dynamic Rigid Body.");
        c
    }
}
impl Gizmo for Grabbable {
    fn gizmos(&self) -> Vec<GizmoSpec> {
        vec![GizmoSpec::Range { center: Vec3::ZERO, radius: self.radius, color: GIZMO_GREEN }]
    }
    fn apply_gizmo_edit(&mut self, edit: GizmoEdit) {
        if let GizmoEdit::Range { radius, .. } = edit {
            self.radius = radius.max(0.0);
        }
    }
}

impl Inspect for VrAvatar {
    fn inspector_ui(&mut self, ui: &mut Ui, ctx: &InspectCtx) -> bool {
        let mut c = false;
        c |= ctx.object_ref(ui, "Head", &mut self.head);
        drag(ui, "Neck drop", &mut self.neck_drop, 0.01, 0.0..=1.0, &mut c);
        drag(ui, "Turn rate", &mut self.turn_rate, 0.1, 0.0..=50.0, &mut c);
        c
    }
}
impl Gizmo for VrAvatar {}
