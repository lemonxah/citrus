//! Third Person Shooter template.

use citrus_core::{CharacterController, Health, ThirdPersonController};

use super::TemplateContent;
use super::builder::{self, Scene};

/// A capsule character body (feet at the parent origin) with a visor so
/// facing reads at a glance.
pub(super) fn character_body(s: &mut Scene, parent: usize, material: &str) -> usize {
    let _ = material;
    super::avatar::visuals(s, parent, parent, false)
}

pub fn build() -> TemplateContent {
    let mut s = Scene::new();
    super::fps::arena(&mut s);
    let hit = builder::impact_effect(&mut s);

    let player = s.empty("Player", None, [0.0, 0.0, 12.0]);
    s.character_layer(player);
    character_body(&mut s, player, "player");
    let gun = super::weapons::model(&mut s, "Rifle", player, [0.32, 1.2, -0.4], super::weapons::RIFLE);
    let muzzle = s.empty("Muzzle", Some(gun), [0.0, 0.09, -0.316]);
    s.with(muzzle, builder::burst_emitter([1.0, 0.8, 0.35, 1.0], [0.5, 2.0], [0.14, 0.02], [0.04, 0.09], 25.0));

    let cam = s.camera("Camera", None, [0.55, 1.55, 15.6], 65.0);
    let mut weapon = super::fps::rifle(s.r(cam), s.r(muzzle), s.r(hit));
    weapon.spread_deg = 0.8;
    s.with(player, ThirdPersonController { camera: s.r(cam), ..Default::default() })
        .with(player, CharacterController::default())
        .with(player, Health { max: 100.0, current: 100.0, regen_per_sec: 5.0, ..Default::default() })
        .with(player, weapon);

    let player_ref = s.r(player);
    builder::shooter_hud(&mut s, player_ref, player_ref, "Hold right mouse to aim. Shoot the targets");
    builder::pause_menu(&mut s, builder::MAIN_SCENE, true);
    builder::content(s.finish(), Vec::new(), builder::shooter_bindings(), Vec::new())
}
