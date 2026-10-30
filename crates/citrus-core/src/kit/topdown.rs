//! Top-down click-to-move and interactables.

use glam::Vec3;
use serde::{Deserialize, Serialize};

use crate::{ComponentCtx, NavAgent, ObjectId, ObjectRef, TypedComponent};

/// An [`Interactable`] was used.
#[derive(Clone, Copy, Debug)]
pub struct InteractEvent {
    pub object: ObjectId,
    pub by: ObjectId,
    pub uses: u32,
}

/// Something the player can use: a switch, a door, a chest. Used by
/// [`ClickToMove`] (walk there and use it) or from code with
/// `ctx.modify::<Interactable>(id, |i| i.request(user))`.
#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct Interactable {
    pub prompt: String,
    /// How close the user must be.
    pub range: f32,
    /// Toggle this object's active state on use (open a gate, switch a lamp).
    pub toggle: ObjectRef,
    /// Re-bake navmeshes after toggling (gates that open new paths).
    pub rebake_navmesh: bool,
    pub sound: String,
    /// Seconds before it can be used again.
    pub cooldown: f32,
    pub single_use: bool,
    #[serde(skip)]
    pub uses: u32,
    #[serde(skip)]
    pub requested: Option<ObjectId>,
    #[serde(skip)]
    pub cooldown_left: f32,
}

impl Default for Interactable {
    fn default() -> Self {
        Self {
            prompt: "Use".into(),
            range: 1.8,
            toggle: ObjectRef::NONE,
            rebake_navmesh: false,
            sound: String::new(),
            cooldown: 0.5,
            single_use: false,
            uses: 0,
            requested: None,
            cooldown_left: 0.0,
        }
    }
}

impl Interactable {
    /// Ask to be used by `by` (applied on this component's next update).
    pub fn request(&mut self, by: ObjectId) {
        self.requested = Some(by);
    }

    pub fn available(&self) -> bool {
        self.cooldown_left <= 0.0 && !(self.single_use && self.uses > 0)
    }
}

impl TypedComponent for Interactable {
    const NAME: &'static str = "Interactable";

    fn update(&mut self, ctx: &mut ComponentCtx) {
        self.cooldown_left = (self.cooldown_left - ctx.dt).max(0.0);
        let Some(by) = self.requested.take() else {
            return;
        };
        if !self.available() {
            return;
        }
        self.uses += 1;
        self.cooldown_left = self.cooldown;
        if self.toggle.is_set() {
            let on = ctx.is_active(self.toggle);
            ctx.set_active(self.toggle, !on);
            if self.rebake_navmesh {
                ctx.rebake_navmesh();
            }
        }
        if !self.sound.is_empty() {
            let at = ctx.self_transform().translation;
            ctx.play_sound_at(self.sound.clone(), at, 1.0);
        }
        ctx.emit(InteractEvent { object: ctx.self_id(), by, uses: self.uses });
    }
}

/// Click the ground to walk there over the navmesh (needs a [`NavAgent`] on
/// the same object and a baked `NavMesh Surface` in the scene); click an
/// [`Interactable`] to walk up to it and use it. Shows `marker` at the target.
#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct ClickToMove {
    pub click_action: String,
    /// Keep re-targeting while the button is held.
    pub hold_to_move: bool,
    /// Click-target marker (a root object; shown at the destination).
    pub marker: ObjectRef,
    pub max_ray_distance: f32,
    #[serde(skip)]
    pub destination: Option<Vec3>,
    #[serde(skip)]
    pub pending_interact: Option<ObjectId>,
    #[serde(skip)]
    pub retarget: f32,
}

impl Default for ClickToMove {
    fn default() -> Self {
        Self {
            click_action: "Click".into(),
            hold_to_move: true,
            marker: ObjectRef::NONE,
            max_ray_distance: 500.0,
            destination: None,
            pending_interact: None,
            retarget: 0.0,
        }
    }
}

impl ClickToMove {
    fn click(&mut self, ctx: &mut ComponentCtx) {
        let Some((origin, dir)) = ctx.pointer_ray() else {
            return;
        };
        let Some(hit) = ctx.raycast(origin, dir, self.max_ray_distance) else {
            return;
        };
        let interactable = ctx.get::<Interactable>(hit.object).map(|i| ctx.position_of(hit.object.into()).map(|p| (p, i.range)));
        let (dest, marker_at) = match interactable {
            Some(Some((pos, range))) => {
                self.pending_interact = Some(hit.object);
                let me = ctx.self_transform().translation;
                let back = (me - pos).with_y(0.0).normalize_or(Vec3::Z) * (range * 0.6);
                let stand = ctx.nearest_walkable(pos + back).unwrap_or(pos + back);
                (stand, pos)
            }
            _ => {
                self.pending_interact = None;
                let p = ctx.nearest_walkable(hit.point).unwrap_or(hit.point);
                (p, hit.point)
            }
        };
        self.destination = Some(dest);
        ctx.set_destination(ctx.self_id(), dest);
        if self.marker.is_set() {
            ctx.set_local_transform(self.marker, Some(marker_at + Vec3::Y * 0.03), None, None);
            ctx.set_active(self.marker, true);
        }
    }
}

impl TypedComponent for ClickToMove {
    const NAME: &'static str = "Click To Move";

    fn start(&mut self, ctx: &mut ComponentCtx) {
        if self.marker.is_set() {
            ctx.set_active(self.marker, false);
        }
    }

    fn update(&mut self, ctx: &mut ComponentCtx) {
        if ctx.dt <= 0.0 {
            return;
        }
        self.retarget -= ctx.dt;
        let clicked = !ctx.pointer_over_ui()
            && (ctx.input.pressed(&self.click_action)
                || (self.hold_to_move && ctx.input.down(&self.click_action) && self.retarget <= 0.0));
        if clicked {
            self.retarget = 0.15;
            self.click(ctx);
            // The agent sees its new destination after this pass.
            return;
        }
        let me = ctx.self_transform().translation;
        if let Some(target) = self.pending_interact {
            let range = ctx.get::<Interactable>(target).map(|i| i.range);
            match (range, ctx.position_of(target.into())) {
                (Some(range), Some(pos)) if me.with_y(0.0).distance(pos.with_y(0.0)) <= range => {
                    let by = ctx.self_id();
                    ctx.modify::<Interactable>(target, move |i| i.request(by));
                    ctx.stop_agent(ctx.self_id());
                    self.pending_interact = None;
                    self.destination = None;
                }
                (None, _) | (_, None) => self.pending_interact = None,
                _ => {}
            }
        }
        let arrived = ctx.get_self::<NavAgent>().is_none_or(|a| a.arrived());
        if let Some(d) = self.destination
            && (arrived || me.with_y(0.0).distance(d.with_y(0.0)) < 0.3)
        {
            self.destination = None;
        }
        if self.destination.is_none() && self.pending_interact.is_none() && self.marker.is_set() && ctx.is_active(self.marker) {
            ctx.set_active(self.marker, false);
        }
    }
}
