//! Built-in gameplay kit: reusable components the project templates are made
//! of (character controllers, hitscan weapons, health, HUD bindings,
//! click-to-move, RTS camera + selection, VR player rig and grabbing). Every
//! piece is an ordinary [`TypedComponent`], so games can use, tweak or replace
//! them like any other component.

mod combat;
mod controllers;
mod hud;
mod rts;
mod topdown;
mod vr_kit;

pub use combat::{
    DamageEvent, DeathAction, DeathEvent, Health, HitscanWeapon, ScoreEvent, TargetDummy, WeaponTrigger, spread_direction,
};
pub use controllers::{FirstPersonController, FollowCamera, ThirdPersonController, boom_position, look_rotation};
pub use hud::{HudBinding, HudValue, format_hud};
pub use rts::{
    ResourceAmount, ResourceStore, RtsCamera, RtsSelection, Selectable, edge_pan, formation_offsets, formation_slots,
    rect_contains, rts_camera_pose, screen_rect,
};
pub use topdown::{ClickToMove, InteractEvent, Interactable};
pub use vr_kit::{Grabbable, VrAvatar, VrHand, VrLocomotion, VrPlayer, teleport_arc};

use crate::ComponentRegistry;

pub(crate) fn register_kit(registry: &mut ComponentRegistry) {
    registry.register::<Health>();
    registry.register::<HitscanWeapon>();
    registry.register::<TargetDummy>();
    registry.register::<FirstPersonController>();
    registry.register::<ThirdPersonController>();
    registry.register::<FollowCamera>();
    registry.register::<ClickToMove>();
    registry.register::<Interactable>();
    registry.register::<RtsCamera>();
    registry.register::<RtsSelection>();
    registry.register::<Selectable>();
    registry.register::<ResourceStore>();
    registry.register::<HudBinding>();
    registry.register::<VrPlayer>();
    registry.register::<Grabbable>();
    registry.register::<VrAvatar>();
}

/// Ids of every object carrying a component of type `T`.
pub(crate) fn objects_with<T: crate::Component>(ctx: &crate::ComponentCtx) -> Vec<crate::ObjectId> {
    ctx.object_ids
        .iter()
        .copied()
        .filter(|id| ctx.get::<T>(*id).is_some())
        .collect()
}
