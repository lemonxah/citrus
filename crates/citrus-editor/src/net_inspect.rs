//! Inspectors and gizmos for the networking components (Network Object,
//! Network Variables, Network Animator, Network Avatar, Network Manager).
//! A child module of `components` so it shares the themed row helpers.

use citrus_core::{
    InterpolationMode, NetAutoStart, NetValue, NetVar, NetworkAnimator, NetworkAvatar, NetworkManager, NetworkObject,
    NetworkVariables, ObjectRef, OwnershipMode,
};
use egui::{DragValue, RichText, Ui};

use super::{Gizmo, GizmoEdit, GizmoSpec, Inspect, InspectCtx, enum_row, property_row};
use crate::{icons::Icon, theme, widgets};

impl Inspect for NetworkObject {
    fn inspector_ui(&mut self, ui: &mut Ui, _ctx: &InspectCtx) -> bool {
        let mut changed = false;
        if self.state.connected {
            ui.horizontal(|ui| {
                let owner = match self.state.owner {
                    0 => "Unowned (host)".to_string(),
                    p => format!("Owner: peer {p}"),
                };
                ui.label(RichText::new(owner).font(theme::mono(11.0)));
                if self.state.authority {
                    widgets::chip(ui, "You simulate this");
                } else {
                    widgets::badge(ui, "remote");
                }
            });
            ui.add_space(4.0);
        }
        enum_row(ui, "Ownership", "citrus-net-own", &mut self.ownership, &OwnershipMode::ALL, OwnershipMode::label, &mut changed);
        widgets::eyebrow(ui, "Replicate");
        property_row(ui, "Transform", &mut changed, |ui| ui.checkbox(&mut self.sync_transform, ""));
        property_row(ui, "Scale", &mut changed, |ui| ui.checkbox(&mut self.sync_scale, ""));
        property_row(ui, "Rigid Body", &mut changed, |ui| {
            ui.checkbox(&mut self.sync_rigidbody, "")
                .on_hover_text("The owner simulates; other peers make the body kinematic and follow")
        });
        property_row(ui, "Parent", &mut changed, |ui| ui.checkbox(&mut self.sync_parent, ""));
        property_row(ui, "Active", &mut changed, |ui| ui.checkbox(&mut self.sync_active, ""));
        property_row(ui, "Predicted", &mut changed, |ui| {
            ui.checkbox(&mut self.predicted, "").on_hover_text(
                "For a Pawn, First / Third Person Controller or VR Player: the host simulates it from its owner's input (server authority); the owner moves at once and reconciles with the host's state",
            )
        });
        widgets::eyebrow(ui, "Smoothing");
        enum_row(
            ui,
            "Mode",
            "citrus-net-interp",
            &mut self.interpolation,
            &InterpolationMode::ALL,
            InterpolationMode::label,
            &mut changed,
        );
        property_row(ui, "Delay", &mut changed, |ui| {
            ui.add(DragValue::new(&mut self.interp_delay).speed(0.005).range(0.0..=1.0).suffix(" s"))
                .on_hover_text("How far behind the newest snapshot remote peers render (absorbs jitter); the minimum when Adaptive")
        });
        property_row(ui, "Adaptive", &mut changed, |ui| {
            ui.checkbox(&mut self.adaptive_delay, "")
                .on_hover_text("Raise the delay when the measured jitter and packet loss need it, and lower it back when the link calms down")
        });
        widgets::eyebrow(ui, "Bandwidth");
        property_row(ui, "Send Rate", &mut changed, |ui| {
            ui.add(DragValue::new(&mut self.send_rate).speed(0.5).range(1.0..=120.0).suffix(" Hz"))
        });
        property_row(ui, "Move Threshold", &mut changed, |ui| {
            ui.add(DragValue::new(&mut self.position_threshold).speed(0.0005).range(0.0..=1.0).suffix(" m"))
        });
        property_row(ui, "Turn Threshold", &mut changed, |ui| {
            ui.add(DragValue::new(&mut self.rotation_threshold).speed(0.05).range(0.0..=45.0).suffix(" deg"))
        });
        property_row(ui, "Quantize", &mut changed, |ui| {
            ui.checkbox(&mut self.quantize, "").on_hover_text("1 mm positions, 32-bit rotations")
        });
        property_row(ui, "Relevancy", &mut changed, |ui| {
            ui.add(DragValue::new(&mut self.relevancy).speed(0.5).range(0.0..=100_000.0).suffix(" m"))
                .on_hover_text("Only send to players within this distance (0 = everyone)")
        });
        property_row(ui, "Despawn On Leave", &mut changed, |ui| {
            ui.checkbox(&mut self.despawn_on_owner_leave, "")
                .on_hover_text("Remove the object when its owner disconnects (otherwise it becomes unowned)")
        });
        changed
    }
}

impl Gizmo for NetworkObject {
    fn gizmos(&self) -> Vec<GizmoSpec> {
        if self.relevancy <= 0.0 {
            return Vec::new();
        }
        vec![GizmoSpec::Range { center: glam::Vec3::ZERO, radius: self.relevancy, color: egui::Color32::from_rgb(90, 200, 250) }]
    }
    fn apply_gizmo_edit(&mut self, edit: GizmoEdit) {
        if let GizmoEdit::Range { radius, .. } = edit {
            self.relevancy = radius.max(0.0);
        }
    }
}

const VALUE_KINDS: [&str; 5] = ["Bool", "Int", "Float", "Vec3", "String"];

fn convert(v: &NetValue, kind: &str) -> NetValue {
    match kind {
        "Bool" => NetValue::Bool(v.as_bool().unwrap_or(false)),
        "Int" => NetValue::Int(v.as_int().unwrap_or(0)),
        "Float" => NetValue::Float(v.as_float().unwrap_or(0.0)),
        "Vec3" => NetValue::Vec3(v.as_vec3().unwrap_or_default().to_array()),
        _ => NetValue::Str(v.as_str().unwrap_or_default().to_owned()),
    }
}

fn value_editor(ui: &mut Ui, v: &mut NetValue) -> bool {
    match v {
        NetValue::Bool(b) => ui.checkbox(b, "").changed(),
        NetValue::Int(i) => ui.add(DragValue::new(i)).changed(),
        NetValue::Float(f) => ui.add(DragValue::new(f).speed(0.01)).changed(),
        NetValue::Vec3(a) => widgets::vec3_field(ui, a, 2, 0.02),
        NetValue::Str(s) => ui.text_edit_singleline(s).changed(),
        NetValue::Quat(_) | NetValue::Bytes(_) => {
            ui.label(RichText::new("(set at runtime)").weak());
            false
        }
    }
}

impl Inspect for NetworkVariables {
    fn inspector_ui(&mut self, ui: &mut Ui, _ctx: &InspectCtx) -> bool {
        let mut changed = false;
        let mut remove = None;
        for (k, var) in self.vars.iter_mut().enumerate() {
            ui.push_id(("citrus-netvar", k), |ui| {
                ui.horizontal(|ui| {
                    changed |= ui
                        .add(egui::TextEdit::singleline(&mut var.name).hint_text("name").desired_width(96.0))
                        .changed();
                    let mut kind = var.value.kind_label();
                    egui::ComboBox::from_id_salt("kind").width(64.0).selected_text(kind).show_ui(ui, |ui| {
                        for k in VALUE_KINDS {
                            ui.selectable_value(&mut kind, k, k);
                        }
                    });
                    if kind != var.value.kind_label() {
                        var.value = convert(&var.value, kind);
                        changed = true;
                    }
                    if widgets::icon_button(ui, Icon::Trash, "Remove variable", false).clicked() {
                        remove = Some(k);
                    }
                });
                widgets::property_row(ui, "Default", |ui| changed |= value_editor(ui, &mut var.value));
            });
        }
        if let Some(k) = remove {
            self.vars.remove(k);
            changed = true;
        }
        if widgets::dashed_button(ui, Some(Icon::Plus), "Add variable").clicked() {
            let name = format!("var{}", self.vars.len());
            self.vars.push(NetVar { name, value: NetValue::Int(0) });
            changed = true;
        }
        property_row(ui, "Anyone Can Write", &mut changed, |ui| {
            ui.checkbox(&mut self.anyone_can_write, "")
                .on_hover_text("Let any peer write these, not only the object's authority")
        });
        widgets::info_note(ui, "Read with ctx.net_var, write with ctx.set_net_var. Late joiners get current values.");
        changed
    }
}
impl Gizmo for NetworkVariables {}

impl Inspect for NetworkAnimator {
    fn inspector_ui(&mut self, ui: &mut Ui, _ctx: &InspectCtx) -> bool {
        let mut changed = false;
        let mut list = self.parameters.join(", ");
        property_row(ui, "Parameters", &mut changed, |ui| {
            ui.add(egui::TextEdit::singleline(&mut list).hint_text("all").desired_width(f32::INFINITY))
                .on_hover_text("Comma-separated Animator parameters to sync; empty syncs every parameter")
        });
        if changed {
            self.parameters = list.split(',').map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect();
        }
        property_row(ui, "Send Rate", &mut changed, |ui| {
            ui.add(DragValue::new(&mut self.send_rate).speed(0.5).range(1.0..=60.0).suffix(" Hz"))
        });
        widgets::info_note(ui, "Needs an Animator on this object. Triggers sync as values; use a bool or an RPC for must-arrive one-shots.");
        changed
    }
}
impl Gizmo for NetworkAnimator {}

impl Inspect for NetworkAvatar {
    fn inspector_ui(&mut self, ui: &mut Ui, ctx: &InspectCtx) -> bool {
        let mut changed = false;
        changed |= ctx.object_ref(ui, "Head", &mut self.head);
        changed |= ctx.object_ref(ui, "Left Hand", &mut self.left_hand);
        changed |= ctx.object_ref(ui, "Right Hand", &mut self.right_hand);
        changed |= ctx.object_ref(ui, "Hips", &mut self.hips);
        changed |= ctx.object_ref(ui, "Left Foot", &mut self.left_foot);
        changed |= ctx.object_ref(ui, "Right Foot", &mut self.right_foot);
        property_row(ui, "Send Rate", &mut changed, |ui| {
            ui.add(DragValue::new(&mut self.send_rate).speed(0.5).range(1.0..=90.0).suffix(" Hz"))
        });
        property_row(ui, "Drive From VR", &mut changed, |ui| {
            ui.checkbox(&mut self.drive_from_vr, "")
                .on_hover_text("The owner's head and hands follow the local headset and controllers")
        });
        property_row(ui, "Follow Local Player", &mut changed, |ui| {
            ui.checkbox(&mut self.follow_local_player, "")
                .on_hover_text("Player prefab mode: the owner's copy mirrors the scene's VR Player rig (head and hands, VR or desktop)")
        });
        if self.follow_local_player {
            ui.label("Hidden For Owner");
            let mut remove = None;
            for (k, r) in self.owner_hidden.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    changed |= ctx.object_ref(ui, &format!("Object {}", k + 1), r);
                    if ui.small_button("x").on_hover_text("Remove").clicked() {
                        remove = Some(k);
                    }
                });
            }
            if let Some(k) = remove {
                self.owner_hidden.remove(k);
                changed = true;
            }
            if ui.small_button("+ Add").clicked() {
                self.owner_hidden.push(ObjectRef::NONE);
                changed = true;
            }
        }
        widgets::info_note(ui, "Put this on the player rig root; parts are its children. Remote peers get the poses written back and as IK targets. For a Network Manager player prefab, turn on Follow Local Player and list the avatar's own visuals under Hidden For Owner.");
        changed
    }
}
impl Gizmo for NetworkAvatar {}

impl Inspect for NetworkManager {
    fn inspector_ui(&mut self, ui: &mut Ui, ctx: &InspectCtx) -> bool {
        let mut changed = false;
        property_row(ui, "Session Name", &mut changed, |ui| ui.text_edit_singleline(&mut self.session_name));
        property_row(ui, "Port", &mut changed, |ui| ui.add(DragValue::new(&mut self.port).range(1..=65535)));
        property_row(ui, "Max Players", &mut changed, |ui| ui.add(DragValue::new(&mut self.max_players).range(1..=256)));
        property_row(ui, "Timeout", &mut changed, |ui| {
            ui.add(DragValue::new(&mut self.timeout).speed(0.1).range(1.0..=120.0).suffix(" s"))
        });
        property_row(ui, "LAN Discovery", &mut changed, |ui| ui.checkbox(&mut self.discovery, ""));
        widgets::eyebrow(ui, "Online");
        property_row(ui, "Relay", &mut changed, |ui| {
            ui.add(egui::TextEdit::singleline(&mut self.relay).hint_text("none (LAN / direct only)"))
                .on_hover_text("Relay server (host:port, default port 47800): hosting also opens a room there, and players join by room code without port forwarding")
        });
        property_row(ui, "Room Code", &mut changed, |ui| {
            ui.add(egui::TextEdit::singleline(&mut self.room_code).hint_text("assigned by the relay"))
                .on_hover_text("A fixed room code for hosts (empty = the relay picks one)")
        });
        property_row(ui, "Secret", &mut changed, |ui| {
            ui.add(egui::TextEdit::singleline(&mut self.secret).password(true).hint_text("none (open session)"))
                .on_hover_text("Peers must know it to join; it also keys the session encryption. Use a long random value.")
        });
        widgets::eyebrow(ui, "Security");
        property_row(ui, "Host Migration", &mut changed, |ui| {
            ui.checkbox(&mut self.host_migration, "")
                .on_hover_text("When the host leaves, the next player in line takes over and the session goes on")
        });
        property_row(ui, "Pin Host Identity", &mut changed, |ui| {
            ui.checkbox(&mut self.pin_host, "")
                .on_hover_text("Clients remember the host's player id for this address / room code and refuse a different one later")
        });
        changed |= fingerprint_list(ui, "Allow List", &mut self.allow_list, "anyone with the secret");
        changed |= fingerprint_list(ui, "Ban List", &mut self.ban_list, "nobody");
        widgets::info_note(ui, "Lists hold player ids (fingerprints like 1a2b-3c4d-...), one per line: copy them from the Network window.");
        widgets::eyebrow(ui, "Players");
        changed |= ctx.file_field(ui, "Player Prefab", &mut self.player_prefab, "prefabs/player.prefab", &["prefab"]);
        property_row(ui, "Spawn Tag", &mut changed, |ui| ui.text_edit_singleline(&mut self.spawn_tag));
        property_row(ui, "Dedicated Host", &mut changed, |ui| {
            ui.checkbox(&mut self.dedicated, "").on_hover_text("The host gets no player of its own")
        });
        property_row(ui, "Clients Can Spawn", &mut changed, |ui| ui.checkbox(&mut self.clients_can_spawn, ""));
        widgets::eyebrow(ui, "Built game");
        enum_row(ui, "Auto Start", "citrus-net-auto", &mut self.auto_start, &NetAutoStart::ALL, NetAutoStart::label, &mut changed);
        if self.auto_start == NetAutoStart::Join {
            property_row(ui, "Address", &mut changed, |ui| {
                ui.add(egui::TextEdit::singleline(&mut self.address).hint_text("host:port or room code"))
            });
        }
        widgets::info_note(ui, "The editor's Network window (Tools) hosts and joins with these settings.");
        changed
    }
}
impl Gizmo for NetworkManager {}

/// One fingerprint per line.
fn fingerprint_list(ui: &mut Ui, label: &str, list: &mut Vec<String>, empty: &str) -> bool {
    let mut changed = false;
    let mut text = list.join("\n");
    property_row(ui, label, &mut changed, |ui| {
        ui.add(egui::TextEdit::multiline(&mut text).hint_text(empty).desired_rows(2).desired_width(f32::INFINITY).font(egui::TextStyle::Monospace))
    });
    if changed {
        *list = text.lines().map(|l| l.trim().to_owned()).filter(|l| !l.is_empty()).collect();
    }
    changed
}
