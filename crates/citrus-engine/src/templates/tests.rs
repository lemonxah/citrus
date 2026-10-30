use std::collections::HashSet;
use std::path::{Path, PathBuf};

use citrus_assets::{MaterialRef, ObjectSource};
use citrus_core::{
    ActionBinding, Component, ComponentRegistry, ControlScheme, FirstPersonController, Health, HitscanWeapon, HudBinding,
    HudValue, InputSource, InputState, Key, MouseButton, NetView, ObjectId, ObjectRef, RawInput, UiText,
};
use glam::{Mat4, Quat, Vec3};

use super::TEMPLATES;
use crate::play::{PlaySession, TickInput, World};
use crate::scene::{LoadedScene, SceneObject};
use crate::shaders::ShaderLibrary;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("citrus-template-test-{tag}-{}", ObjectId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Every object id referenced by a component's RON (ObjectRefs serialize as
/// quoted UUID strings).
fn referenced_ids(data: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = data;
    while let Some(start) = rest.find('"') {
        let tail = &rest[start + 1..];
        let Some(end) = tail.find('"') else { break };
        let s = &tail[..end];
        if s.len() == 36 && s.matches('-').count() == 4 && s.parse::<ObjectId>().is_ok() {
            out.push(s.to_string());
        }
        rest = &tail[end + 1..];
    }
    out
}

#[test]
fn every_template_scaffolds_and_loads() {
    let registry = ComponentRegistry::with_builtins();
    let parent = temp_dir("scaffold");
    for template in TEMPLATES {
        let root = crate::bundle::scaffold_project(&parent, template.id, template.id)
            .unwrap_or_else(|e| panic!("{}: scaffold failed: {e:#}", template.id));
        let project = citrus_assets::load_project_file(&root).unwrap_or_else(|e| panic!("{}: {e:#}", template.id));
        let boot = project.boot_scene.clone().expect("boot scene set");
        assert_eq!(project.last_scene.as_deref(), Some(boot.as_str()));
        assert!(!project.bindings.schemes.is_empty(), "{}: bindings", template.id);
        assert!(root.join("Cargo.toml").is_file() && root.join("src/main.rs").is_file());

        let scene_path = root.join(&boot);
        let scene = citrus_assets::load_scene_file(&scene_path).unwrap_or_else(|e| panic!("{}: {e:#}", template.id));
        assert!(!scene.entries.is_empty());
        let ids: HashSet<&str> = scene.entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids.len(), scene.entries.len(), "{}: object ids are unique", template.id);
        for (i, e) in scene.entries.iter().enumerate() {
            if let Some(p) = e.parent {
                assert!(p < i, "{}: {} parent comes first", template.id, e.name);
            }
            if let MaterialRef::File(m) = &e.material {
                assert!(root.join(m).is_file(), "{}: material {m} written", template.id);
            }
            if let ObjectSource::Model { path, .. } = &e.source {
                assert!(root.join(path).is_file(), "{}: model {path} copied", template.id);
            }
            for c in &e.components {
                assert!(
                    registry.load(&c.kind, &c.data).is_some(),
                    "{}: {} component {:?} must load: {}",
                    template.id,
                    e.name,
                    c.kind,
                    c.data
                );
                for r in referenced_ids(&c.data) {
                    assert!(ids.contains(r.as_str()), "{}: {} / {} references a missing object", template.id, e.name, c.kind);
                }
            }
        }
        if let Some(sky) = &scene.skybox {
            assert!(root.join(sky).is_file(), "{}: skybox copied", template.id);
        }
        let cameras = scene.entries.iter().filter(|e| matches!(e.source, ObjectSource::Camera)).count();
        assert!(cameras >= 1, "{}: has a camera", template.id);

        for entry in std::fs::read_dir(root.join("prefabs")).unwrap().flatten() {
            let prefab = crate::prefab::Prefab::load(entry.path()).expect("prefab loads");
            assert_eq!(prefab.roots().len(), 1);
            for e in &prefab.entries {
                for c in &e.components {
                    assert!(registry.load(&c.kind, &c.data).is_some(), "prefab component {}", c.kind);
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(&parent);
}

#[test]
fn unknown_template_is_rejected() {
    let parent = temp_dir("unknown");
    assert!(crate::bundle::scaffold_project(&parent, "x", "no-such-template").is_err());
    assert!(!parent.join("x").exists());
    let _ = std::fs::remove_dir_all(&parent);
}

#[test]
fn template_ids_are_unique_and_default_exists() {
    let ids: HashSet<&str> = TEMPLATES.iter().map(|t| t.id).collect();
    assert_eq!(ids.len(), TEMPLATES.len());
    assert!(super::find(super::DEFAULT_TEMPLATE).is_some());
    assert!(TEMPLATES.iter().any(|t| t.category == super::TemplateCategory::Vr));
}

// ------------------------------------------------------- headless gameplay

fn obj(scene: &mut LoadedScene, pos: Vec3, components: Vec<Box<dyn Component>>) -> ObjectId {
    let id = ObjectId::new();
    scene.objects.push(SceneObject {
        id,
        name: String::new(),
        render: None,
        extra_render: Vec::new(),
        source: ObjectSource::Empty,
        enabled: true,
        static_geometry: false,
        lightmap_scale: 1.0,
        layer: 0,
        parent: None,
        translation: pos,
        rotation: Quat::IDENTITY,
        scale: Vec3::ONE,
        components,
    });
    id
}

fn get<T: Component>(scene: &LoadedScene, id: ObjectId) -> &T {
    let i = scene.index_of(id).expect("object");
    scene.objects[i].components.iter().find_map(|c| c.as_any().downcast_ref::<T>()).expect("component")
}

fn input_with(keys_down: &[Key], mouse_down: &[MouseButton], prev: &InputState) -> InputState {
    let scheme = ControlScheme {
        name: "test".into(),
        actions: vec![
            super::builder::wasd("Move"),
            ActionBinding::button("Fire", [InputSource::Mouse(MouseButton::Left)]),
            ActionBinding::button("Crouch", [InputSource::Key(Key::C)]),
        ],
    };
    let mut raw = RawInput::default();
    raw.keys_down.extend(keys_down.iter().copied());
    raw.mouse_down.extend(mouse_down.iter().copied());
    citrus_core::resolve(&scheme, &raw, prev)
}

fn run(session: &mut PlaySession, scene: &mut LoadedScene, frames: usize, input: impl Fn(usize) -> InputState) {
    let registry = ComponentRegistry::with_builtins();
    let mut shaders = ShaderLibrary::default();
    let net = NetView::default();
    let screen = citrus_core::ScreenInfo::default();
    for f in 0..frames {
        let input = input(f);
        session.tick(
            &mut World { scene, renderer: None, registry: &registry, shaders: &mut shaders },
            &TickInput { dt: 1.0 / 60.0, input: &input, net: &net, listener_fallback: Mat4::IDENTITY, screen: &screen },
        );
    }
}

#[test]
fn hitscan_weapon_damages_health_through_physics() {
    use citrus_core::BoxCollider;
    let mut scene = LoadedScene::empty();
    let shooter = obj(
        &mut scene,
        Vec3::new(0.0, 1.0, 0.0),
        vec![Box::new(HitscanWeapon { spread_deg: 0.0, automatic: false, damage: 30.0, ..Default::default() })],
    );
    let target = obj(
        &mut scene,
        Vec3::new(0.0, 1.0, -6.0),
        vec![Box::new(BoxCollider { size: [2.0, 2.0, 0.5], ..Default::default() }), Box::new(Health::default())],
    );
    let mut session = PlaySession::new(&scene, Path::new("."), None);
    // Frame 2 presses Fire (frames 0/1 let the physics query structure build).
    let held = input_with(&[], &[MouseButton::Left], &InputState::default());
    run(&mut session, &mut scene, 6, |f| if f == 2 { held.clone() } else { InputState::default() });
    assert_eq!(get::<Health>(&scene, target).current, 70.0, "one shot landed");
    let w = get::<HitscanWeapon>(&scene, shooter);
    assert_eq!((w.shots_fired, w.ammo), (1, 29));
    assert!(w.last_hit.is_some_and(|p| (p.z + 5.75).abs() < 0.05), "hit the near face: {:?}", w.last_hit);
}

#[test]
fn hud_binding_writes_health_text() {
    let mut scene = LoadedScene::empty();
    let player = obj(&mut scene, Vec3::ZERO, vec![Box::new(Health { max: 100.0, current: 40.0, ..Default::default() })]);
    let label = obj(
        &mut scene,
        Vec3::ZERO,
        vec![
            Box::new(UiText::default()),
            Box::new(HudBinding { source: ObjectRef::new(player), value: HudValue::Health, format: "HP {value}/{max}".into(), ..Default::default() }),
        ],
    );
    let mut session = PlaySession::new(&scene, Path::new("."), None);
    run(&mut session, &mut scene, 3, |_| InputState::default());
    assert_eq!(get::<UiText>(&scene, label).text, "HP 40/100");
}

#[test]
fn first_person_controller_walks_forward_on_w() {
    let mut scene = LoadedScene::empty();
    let player = obj(&mut scene, Vec3::ZERO, vec![Box::new(FirstPersonController { lock_cursor: false, ..Default::default() })]);
    let mut session = PlaySession::new(&scene, Path::new("."), None);
    let w = input_with(&[Key::W], &[], &InputState::default());
    run(&mut session, &mut scene, 60, |_| w.clone());
    let i = scene.index_of(player).unwrap();
    let p = scene.objects[i].translation;
    assert!(p.z < -2.0, "moved forward (-Z): {p:?}");
    assert!(p.x.abs() < 1e-3 && p.y.abs() < 1e-3, "stayed on the floor line: {p:?}");
}

#[test]
fn crouch_shrinks_the_capsule_and_waits_for_headroom() {
    use citrus_core::{BoxCollider, CharacterController};
    let build = |ceiling: bool| {
        let mut scene = LoadedScene::empty();
        obj(&mut scene, Vec3::new(0.0, -0.5, 0.0), vec![Box::new(BoxCollider { size: [20.0, 1.0, 20.0], ..Default::default() })]);
        if ceiling {
            // Underside at 1.45 m: a crouched (1.2 m) capsule fits, a standing one doesn't.
            obj(&mut scene, Vec3::new(0.0, 1.7, 0.0), vec![Box::new(BoxCollider { size: [6.0, 0.5, 6.0], ..Default::default() })]);
        }
        let player = obj(
            &mut scene,
            Vec3::ZERO,
            vec![Box::new(FirstPersonController { lock_cursor: false, ..Default::default() }), Box::new(CharacterController::default())],
        );
        (scene, player)
    };
    let crouch = input_with(&[Key::C], &[], &InputState::default());
    for ceiling in [false, true] {
        let (mut scene, player) = build(ceiling);
        let mut session = PlaySession::new(&scene, Path::new("."), None);
        run(&mut session, &mut scene, 10, |_| crouch.clone());
        let cc = get::<CharacterController>(&scene, player);
        assert!((cc.height - 1.2).abs() < 1e-4 && (cc.center[1] - 0.6).abs() < 1e-4, "crouched capsule, feet kept: {} {:?}", cc.height, cc.center);
        run(&mut session, &mut scene, 10, |_| InputState::default());
        let (cc, fpc) = (get::<CharacterController>(&scene, player), get::<FirstPersonController>(&scene, player));
        if ceiling {
            assert!(fpc.crouched && (cc.height - 1.2).abs() < 1e-4, "stays crouched under the ceiling");
        } else {
            assert!(!fpc.crouched && (cc.height - 1.8).abs() < 1e-4, "stands back up in the open");
        }
        let p = scene.objects[scene.index_of(player).unwrap()].translation;
        assert!(p.y.abs() < 0.05, "feet stay on the floor: {p:?}");
    }
}

// ------------------------------------------------------- template playthroughs

/// A template's boot scene as a headless `LoadedScene` (no meshes), plus its bindings.
fn load_template(id: &str) -> (LoadedScene, citrus_core::ControlScheme) {
    let registry = ComponentRegistry::with_builtins();
    let content = super::build(id).unwrap();
    let file = &content.scenes[0].1;
    let mut scene = LoadedScene::empty();
    scene.layers = file.layers.clone();
    push_entries(&mut scene, &registry, &file.entries);
    (scene, content.bindings.schemes[0].clone())
}

/// Append scene entries (parents local to `entries`) as headless objects.
fn push_entries(scene: &mut LoadedScene, registry: &ComponentRegistry, entries: &[citrus_assets::SceneEntry]) {
    let base = scene.objects.len();
    for e in entries {
        scene.objects.push(SceneObject {
            id: e.id.parse().unwrap(),
            name: e.name.clone(),
            render: None,
            extra_render: Vec::new(),
            source: e.source.clone(),
            enabled: e.enabled,
            static_geometry: e.static_geometry,
            lightmap_scale: e.lightmap_scale,
            layer: e.layer,
            parent: e.parent.map(|p| p + base),
            translation: Vec3::from(e.translation),
            rotation: Quat::from_array(e.rotation),
            scale: Vec3::from(e.scale),
            components: e.components.iter().filter_map(|c| registry.load(&c.kind, &c.data)).collect(),
        });
    }
}

/// Straight-down camera over `center` (screen top = -Z), 800 x 600.
fn top_down_screen(center: Vec3, height: f32) -> citrus_core::ScreenInfo {
    citrus_core::ScreenInfo {
        size: glam::Vec2::new(800.0, 600.0),
        view: Mat4::look_at_rh(center + Vec3::Y * height, center, Vec3::NEG_Z),
        proj: Mat4::perspective_rh(60f32.to_radians(), 800.0 / 600.0, 0.1, 500.0),
        ..Default::default()
    }
}

struct Frame {
    pointer: Option<glam::Vec2>,
    mouse: Vec<MouseButton>,
}

fn play(scene: &mut LoadedScene, scheme: &ControlScheme, base: &citrus_core::ScreenInfo, frames: usize, f: impl Fn(usize) -> Frame) {
    let registry = ComponentRegistry::with_builtins();
    let mut shaders = ShaderLibrary::default();
    let net = NetView::default();
    let mut session = PlaySession::new(scene, Path::new("."), None);
    let mut state = InputState::default();
    for i in 0..frames {
        let frame = f(i);
        let mut raw = RawInput::default();
        raw.mouse_down.extend(frame.mouse.iter().copied());
        state = citrus_core::resolve(scheme, &raw, &state);
        let mut screen = base.clone();
        screen.pointer = frame.pointer;
        session.tick(
            &mut World { scene, renderer: None, registry: &registry, shaders: &mut shaders },
            &TickInput { dt: 1.0 / 60.0, input: &state, net: &net, listener_fallback: Mat4::IDENTITY, screen: &screen },
        );
    }
}

fn idle(_: usize) -> Frame {
    Frame { pointer: None, mouse: Vec::new() }
}

fn position(scene: &LoadedScene, name: &str) -> Vec3 {
    let i = scene.objects.iter().position(|o| o.name == name).unwrap_or_else(|| panic!("no {name}"));
    scene.world_transform(i).w_axis.truncate()
}

#[test]
fn every_template_runs_headless() {
    for t in TEMPLATES {
        let (mut scene, scheme) = load_template(t.id);
        play(&mut scene, &scheme, &citrus_core::ScreenInfo::default(), 90, idle);
    }
}

#[test]
fn shooter_players_stand_on_the_ground() {
    for id in ["fps", "tps"] {
        let (mut scene, scheme) = load_template(id);
        play(&mut scene, &scheme, &citrus_core::ScreenInfo::default(), 120, idle);
        let p = position(&scene, "Player");
        assert!(p.y.abs() < 0.15, "{id}: player rests on the ground: {p:?}");
    }
}

#[test]
fn topdown_click_walks_the_player_over_the_navmesh() {
    let (mut scene, scheme) = load_template("topdown");
    let screen = top_down_screen(Vec3::ZERO, 30.0);
    let start = position(&scene, "Player");
    play(&mut scene, &scheme, &screen, 300, |i| Frame {
        pointer: Some(glam::Vec2::new(600.0, 300.0)),
        mouse: if i == 30 { vec![MouseButton::Left] } else { Vec::new() },
    });
    let end = position(&scene, "Player");
    assert!(end.x > start.x + 7.0, "player walked toward the click: {start:?} -> {end:?}");
}

#[test]
fn strategy_box_select_and_formation_move() {
    let (mut scene, scheme) = load_template("strategy");
    let screen = top_down_screen(Vec3::new(0.0, 0.0, 7.0), 30.0);
    play(&mut scene, &scheme, &screen, 330, |i| match i {
        30 => Frame { pointer: Some(glam::Vec2::new(1.0, 1.0)), mouse: vec![MouseButton::Left] },
        31..=33 => Frame { pointer: Some(glam::Vec2::new(799.0, 599.0)), mouse: vec![MouseButton::Left] },
        34 => Frame { pointer: Some(glam::Vec2::new(799.0, 599.0)), mouse: Vec::new() },
        36 => Frame { pointer: Some(glam::Vec2::new(600.0, 300.0)), mouse: vec![MouseButton::Right] },
        _ => Frame { pointer: Some(glam::Vec2::new(400.0, 300.0)), mouse: Vec::new() },
    });
    let cam = scene.objects.iter().position(|o| o.name == "Camera").unwrap();
    let selection = scene.objects[cam].components.iter().find_map(|c| c.as_any().downcast_ref::<citrus_core::RtsSelection>()).unwrap();
    assert_eq!(selection.selected.len(), 8, "box selected every unit");
    let xs: Vec<f32> = (1..=8).map(|k| position(&scene, &format!("Unit {k}")).x).collect();
    assert!(xs.iter().all(|x| *x > 6.0), "units moved to the order point: {xs:?}");
    let spread = xs.iter().cloned().fold(f32::MIN, f32::max) - xs.iter().cloned().fold(f32::MAX, f32::min);
    assert!(spread > 1.0, "units hold a formation instead of stacking: {xs:?}");
}

#[test]
fn vr_player_desktop_grabs_what_it_looks_at_and_fires_it() {
    use citrus_core::{Grabbable, VrPlayer, WeaponTrigger};
    let mut scene = LoadedScene::empty();
    let rig = obj(&mut scene, Vec3::ZERO, vec![]);
    let head = obj(&mut scene, Vec3::new(0.0, 1.65, 0.0), vec![]);
    let left = obj(&mut scene, Vec3::ZERO, vec![]);
    let right = obj(&mut scene, Vec3::ZERO, vec![]);
    for child in [1, 2, 3] {
        scene.objects[child].parent = Some(0);
    }
    let i = scene.index_of(rig).unwrap();
    scene.objects[i].components.push(Box::new(VrPlayer {
        head: ObjectRef::new(head),
        left_hand: ObjectRef::new(left),
        right_hand: ObjectRef::new(right),
        ..Default::default()
    }));
    let gun = obj(
        &mut scene,
        Vec3::new(0.0, 1.6, -1.5),
        vec![
            Box::new(Grabbable { snap_to_hand: true, ..Default::default() }),
            Box::new(HitscanWeapon { trigger: WeaponTrigger::Grabbed, automatic: false, spread_deg: 0.0, ..Default::default() }),
        ],
    );
    let target = obj(
        &mut scene,
        Vec3::new(0.2, 1.4, -8.0),
        vec![Box::new(citrus_core::BoxCollider { size: [3.0, 3.0, 0.5], ..Default::default() }), Box::new(Health::default())],
    );
    let scheme = super::vr_shooter::vr_bindings(Vec::new()).schemes[0].clone();
    play(&mut scene, &scheme, &citrus_core::ScreenInfo::default(), 40, |i| Frame {
        pointer: None,
        mouse: match i {
            10..=39 => {
                let mut m = vec![MouseButton::Right];
                if i == 30 {
                    m.push(MouseButton::Left);
                }
                m
            }
            _ => Vec::new(),
        },
    });
    assert!(get::<Grabbable>(&scene, gun).held, "grabbed by looking at it");
    let hand = scene.world_transform(scene.index_of(right).unwrap()).w_axis.truncate();
    let gun_pos = scene.world_transform(scene.index_of(gun).unwrap()).w_axis.truncate();
    assert!(hand.distance(gun_pos) < 0.05, "snapped into the right hand: {hand:?} vs {gun_pos:?}");
    assert_eq!(get::<HitscanWeapon>(&scene, gun).shots_fired, 1, "the trigger fired it once");
    assert!(get::<Health>(&scene, target).current < 100.0, "the shot landed");
}

#[test]
fn vr_social_avatar_prefab_mirrors_the_local_player_for_others() {
    let content = super::build("vr_social").unwrap();
    let (_, prefab) = content
        .prefabs
        .iter()
        .find(|(path, _)| path.as_str() == "prefabs/net_avatar.prefab")
        .expect("avatar prefab");
    let (mut scene, scheme) = load_template("vr_social");
    let mut net_manager = scene.objects.iter().filter_map(|o| o.components.iter().find_map(|c| c.as_any().downcast_ref::<citrus_core::NetworkManager>()));
    assert_eq!(net_manager.next().map(|m| m.player_prefab.as_str()), Some("prefabs/net_avatar.prefab"));
    let base = scene.objects.len();
    push_entries(&mut scene, &ComponentRegistry::with_builtins(), &prefab.instantiate([0.0, 0.0, 8.0]));
    play(&mut scene, &scheme, &citrus_core::ScreenInfo::default(), 30, idle);

    let find = |name: &str, from: usize| (from..scene.objects.len()).find(|&i| scene.objects[i].name == name).unwrap();
    let player = find("VR Player", 0);
    let local_head = find("Head", 0);
    assert!(local_head < base);
    let avatar = find("Net Avatar", base);
    let avatar_head = find("Head", base);
    let avatar_hand = find("Right Hand", base);
    let local_hand = find("Right Hand", 0);
    let at = |i: usize| scene.world_transform(i).w_axis.truncate();
    assert!(at(avatar).distance(at(player)) < 1e-3, "avatar root sits on the rig: {:?} vs {:?}", at(avatar), at(player));
    assert!(at(avatar_head).distance(at(local_head)) < 1e-3, "head follows: {:?} vs {:?}", at(avatar_head), at(local_head));
    assert!(at(avatar_hand).distance(at(local_hand)) < 1e-3, "hand follows");
    for name in ["Head Model", "Avatar Body"] {
        assert!(!scene.objects[find(name, base)].enabled, "{name} hidden for the owner");
    }
}
