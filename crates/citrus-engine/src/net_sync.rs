//! Scene-level replication: the bridge between a [`NetSession`] and the
//! scene. Runs once per frame after the gameplay tick (`LoadedScene::network_sync`):
//!
//! - objects with authority (owner, or the host when unowned) send delta /
//!   quantized transforms at their send rate, parent / active changes,
//!   Animator parameters and avatar poses;
//! - everything else is written from the snapshot buffers (interpolated
//!   `interp_delay` behind, optionally extrapolated);
//! - received synced variables and parent changes land on their objects;
//! - networked dynamic bodies another peer simulates are reported back so the
//!   play session makes them kinematic;
//! - the host spawns the Network Manager's player prefab for every peer.
//!
//! Host-loop helpers (`route_command`, `config_for`, `auto_start`) and the
//! deterministic spawn helpers used by the play session live here too.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};

use citrus_core::{
    Animator, BodyKind, ComponentCommand, InterpolationMode, NetAutoStart, NetSpawnSource, NetworkAnimator,
    NetworkAvatar, NetworkManager, NetworkObject, NetworkVariables, ObjectId, Pawn, RigidBody, SpawnPoint,
    Sync as SyncComp,
};
use glam::{Mat4, Quat, Vec3};

use crate::net::{NetConfig, NetSession, SnapEntry, XformState, sample_avatar, sample_snapshots};
use crate::prefab::Prefab;
use crate::scene::LoadedScene;

/// Seconds between full (non-delta) transform keyframes.
const KEYFRAME: f64 = 1.0;

fn component<T: 'static>(o: &crate::scene::SceneObject) -> Option<&T> {
    o.components.iter().find_map(|c| c.as_any().downcast_ref::<T>())
}

fn component_mut<T: 'static>(o: &mut crate::scene::SceneObject) -> Option<&mut T> {
    o.components.iter_mut().find_map(|c| c.as_any_mut().downcast_mut::<T>())
}

/// Replication settings for an object, or None if it isn't networked. A
/// legacy `Sync` maps onto default `NetworkObject` settings; the other
/// network components give an object identity without transform sync.
fn settings(o: &crate::scene::SceneObject) -> Option<NetworkObject> {
    if let Some(n) = component::<NetworkObject>(o) {
        return Some(n.clone());
    }
    if let Some(s) = component::<SyncComp>(o) {
        return Some(NetworkObject::from_sync(s.smoothing));
    }
    let other = component::<NetworkVariables>(o).is_some()
        || component::<NetworkAnimator>(o).is_some()
        || component::<NetworkAvatar>(o).is_some();
    other.then(|| NetworkObject { sync_transform: false, sync_parent: false, sync_active: false, ..Default::default() })
}

fn is_dynamic_body(o: &crate::scene::SceneObject) -> bool {
    component::<RigidBody>(o).is_some_and(|r| r.kind == BodyKind::Dynamic)
}

/// Replicate the scene for one frame.
pub fn sync(scene: &mut LoadedScene, s: &mut NetSession, _dt: f32) {
    if !s.is_connected() {
        return;
    }
    let now = s.now();
    let is_host = s.is_host();
    let ids = scene.id_index();
    apply_pending(scene, s, &ids);
    let world = scene.world_transforms();
    if is_host {
        s.world.modes.clear();
        s.world.var_anyone.clear();
        s.world.relevancy.clear();
    }
    let mut entries: Vec<SnapEntry> = Vec::new();
    let mut remote_bodies: HashMap<ObjectId, Vec3> = HashMap::new();
    let mut part_writes: Vec<(usize, Vec3, Quat)> = Vec::new();
    let mut transform_writes: Vec<(usize, Vec3, Quat, Option<Vec3>)> = Vec::new();
    let mut present: HashSet<u128> = HashSet::new();

    for i in 0..scene.objects.len() {
        let Some(cfg) = settings(&scene.objects[i]) else {
            continue;
        };
        let id = scene.objects[i].id;
        let raw = id.raw();
        present.insert(raw);
        let owner = s.owner_of_raw(raw);
        let authority = s.has_authority_raw(raw);
        if let Some(n) = component_mut::<NetworkObject>(&mut scene.objects[i]) {
            n.state = citrus_core::NetObjectState { connected: true, owner, authority };
        }
        if is_host {
            s.world.modes.insert(raw, cfg.ownership);
            if cfg.relevancy > 0.0 {
                s.world.relevancy.insert(raw, cfg.relevancy);
            }
            if cfg.despawn_on_owner_leave {
                s.world.despawn_on_leave.insert(raw);
            }
            if component::<NetworkVariables>(&scene.objects[i]).is_some_and(|v| v.anyone_can_write) {
                s.world.var_anyone.insert(raw);
            }
            if owner != 0
                && (component::<Pawn>(&scene.objects[i]).is_some()
                    || component::<NetworkAvatar>(&scene.objects[i]).is_some())
            {
                s.world.player_obj.entry(owner).or_insert(raw);
            }
        }
        let (wscale, wrot, wpos) = world[i].to_scale_rotation_translation();
        let parent_raw = scene.objects[i].parent.map(|p| scene.objects[p].id.raw()).unwrap_or(0);
        let enabled = scene.objects[i].enabled;
        let dynamic = is_dynamic_body(&scene.objects[i]);
        let mut st = s.send_state.remove(&raw).unwrap_or_default();
        st.seen = true;
        st.missing_since = None;

        if authority {
            if cfg.sync_transform && now - st.last_send >= 1.0 / cfg.send_rate.max(0.5) as f64 {
                let vel = match st.prev_pos {
                    Some((p, t)) if now > t => (wpos - p) / (now - t) as f32,
                    _ => Vec3::ZERO,
                };
                st.prev_pos = Some((wpos, now));
                let key = now - st.last_key >= KEYFRAME || st.sent.is_none();
                let sent = st.sent.unwrap_or_default();
                let pos_changed = key || sent.pos.distance(wpos) > cfg.position_threshold;
                let rot_changed = key || sent.rot.angle_between(wrot).to_degrees() > cfg.rotation_threshold;
                let scale_changed = cfg.sync_scale && (key || sent.scale.distance(wscale) > 1e-4);
                let want_vel = cfg.interpolation == InterpolationMode::Extrapolate || (cfg.sync_rigidbody && dynamic);
                if pos_changed || rot_changed || scale_changed {
                    entries.push(SnapEntry {
                        id: raw,
                        pos: pos_changed.then_some(wpos),
                        rot: rot_changed.then_some(wrot),
                        scale: scale_changed.then_some(wscale),
                        vel: (want_vel && (pos_changed || key)).then_some(vel),
                        quantized: cfg.quantize,
                    });
                    st.sent = Some(XformState { pos: wpos, rot: wrot, scale: wscale, vel });
                    st.last_send = now;
                    if key {
                        st.last_key = now;
                    }
                    s.world.xforms.insert(raw, XformState { pos: wpos, rot: wrot, scale: wscale, vel });
                }
            }
            if cfg.sync_parent {
                if st.parent.is_some_and(|p| p != parent_raw) {
                    st.parent = Some(parent_raw);
                    s.set_parent(raw, parent_raw);
                } else {
                    st.parent = Some(parent_raw);
                }
            }
            if cfg.sync_active {
                if st.active.is_some_and(|a| a != enabled) {
                    st.active = Some(enabled);
                    s.set_active(raw, enabled);
                } else {
                    st.active = Some(enabled);
                }
            }
        } else {
            st.parent = None;
            st.active = None;
            st.sent = None;
            st.prev_pos = None;
            if cfg.sync_transform
                && let Some(buf) = s.buffers.get(&raw)
            {
                let rt = s.render_time(buf.origin, cfg.interp_delay.max(0.0));
                if let Some(snap) = sample_snapshots(buf, rt, cfg.interpolation) {
                    transform_writes.push((i, snap.pos, snap.rot, cfg.sync_scale.then_some(snap.scale)));
                    if dynamic && cfg.sync_rigidbody {
                        remote_bodies.insert(id, snap.vel);
                    }
                }
            } else if dynamic && cfg.sync_rigidbody && cfg.sync_transform {
                remote_bodies.insert(id, Vec3::ZERO);
            }
        }
        s.send_state.insert(raw, st);

        sync_animator(scene, s, i, raw, authority, now);
        sync_avatar(scene, s, &ids, &world, i, raw, authority, now, cfg.interp_delay, &mut part_writes);
    }

    for (i, pos, rot, scale) in transform_writes {
        set_world_pose(scene, i, pos, rot, scale);
    }
    for (i, pos, rot) in part_writes {
        set_world_pose(scene, i, pos, rot, None);
    }
    s.send_snapshot(&entries);

    // Bodies whose simulation just came back to this peer keep their momentum.
    let reclaimed: HashMap<ObjectId, Vec3> = s
        .remote_bodies
        .iter()
        .filter(|(id, _)| !remote_bodies.contains_key(id) && present.contains(&id.raw()))
        .map(|(id, v)| (*id, *v))
        .collect();
    s.reclaimed_bodies.extend(reclaimed);
    s.remote_bodies = remote_bodies;

    // An owned runtime spawn that vanished locally (plain destroy) despawns everywhere.
    let gone: Vec<u128> = s.send_state.keys().copied().filter(|r| !present.contains(r)).collect();
    for raw in gone {
        let spawned = s.world.spawns.iter().any(|sp| sp.id == raw);
        let authority = s.has_authority_raw(raw);
        let Some(st) = s.send_state.get_mut(&raw) else {
            continue;
        };
        if !st.seen {
            continue;
        }
        if spawned && authority {
            let since = *st.missing_since.get_or_insert(now);
            if now - since > 0.25 {
                s.send_state.remove(&raw);
                s.despawn(ObjectId::from_raw(raw));
            }
        } else if !spawned {
            s.send_state.remove(&raw);
        }
    }

    if is_host {
        spawn_players(scene, s, &world);
    }
}

/// Write a world-space pose into an object's local TRS.
fn set_world_pose(scene: &mut LoadedScene, i: usize, pos: Vec3, rot: Quat, scale: Option<Vec3>) {
    let parent = scene.objects[i].parent.map(|p| scene.world_transform(p));
    let o = &mut scene.objects[i];
    match parent {
        Some(pw) => {
            let (ps, pr, _) = pw.to_scale_rotation_translation();
            o.translation = pw.inverse().transform_point3(pos);
            o.rotation = (pr.inverse() * rot).normalize();
            if let Some(s) = scale {
                o.scale = s / ps.max(Vec3::splat(1e-6));
            }
        }
        None => {
            o.translation = pos;
            o.rotation = rot;
            if let Some(s) = scale {
                o.scale = s;
            }
        }
    }
}

fn apply_pending(scene: &mut LoadedScene, s: &mut NetSession, ids: &HashMap<ObjectId, usize>) {
    s.pending_vars.retain(|p| {
        let Some(&i) = ids.get(&ObjectId::from_raw(p.id)) else {
            return true;
        };
        if let Some(v) = component_mut::<NetworkVariables>(&mut scene.objects[i]) {
            v.set(&p.key, p.value.clone());
        }
        false
    });
    let mut parents = std::mem::take(&mut s.pending_parents);
    parents.retain(|(id, parent, _)| {
        let Some(&i) = ids.get(&ObjectId::from_raw(*id)) else {
            return true;
        };
        let new_parent = if *parent == 0 {
            None
        } else {
            match ids.get(&ObjectId::from_raw(*parent)) {
                Some(&p) => Some(p),
                None => return true,
            }
        };
        // Refuse cycles.
        let mut k = new_parent;
        let mut guard = 0;
        while let Some(p) = k {
            if p == i || guard > 64 {
                return false;
            }
            k = scene.objects[p].parent;
            guard += 1;
        }
        let world = scene.world_transform(i);
        scene.objects[i].parent = new_parent;
        let (sc, r, t) = world.to_scale_rotation_translation();
        set_world_pose(scene, i, t, r, Some(sc));
        false
    });
    s.pending_parents.extend(parents);
}

fn sync_animator(scene: &mut LoadedScene, s: &mut NetSession, i: usize, raw: u128, authority: bool, now: f64) {
    let Some(na) = component::<NetworkAnimator>(&scene.objects[i]).cloned() else {
        return;
    };
    let Some(anim) = component_mut::<Animator>(&mut scene.objects[i]) else {
        return;
    };
    if authority {
        let st = s.send_state.entry(raw).or_default();
        if now - st.last_anim < 1.0 / na.send_rate.max(0.5) as f64 {
            return;
        }
        st.last_anim = now;
        let full = now - st.last_anim_full >= KEYFRAME;
        if full {
            st.last_anim_full = now;
        }
        let mut changed = Vec::new();
        for (k, v) in &anim.state.params {
            if !na.parameters.is_empty() && !na.parameters.iter().any(|p| p == k) {
                continue;
            }
            let prev = st.anim.get(k).copied();
            if full || prev.is_none_or(|p| (p - v).abs() > 1e-4) {
                changed.push((k.clone(), *v));
                st.anim.insert(k.clone(), *v);
            }
        }
        s.send_anim(raw, &changed);
    } else if let Some(params) = s.anim_in.get(&raw) {
        for (k, v) in params {
            anim.set_float(k, *v);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn sync_avatar(
    scene: &mut LoadedScene,
    s: &mut NetSession,
    ids: &HashMap<ObjectId, usize>,
    world: &[Mat4],
    i: usize,
    raw: u128,
    authority: bool,
    now: f64,
    delay: f32,
    part_writes: &mut Vec<(usize, Vec3, Quat)>,
) {
    let Some(av) = component::<NetworkAvatar>(&scene.objects[i]) else {
        return;
    };
    let parts: Vec<Option<usize>> = av.parts().iter().map(|r| r.id().and_then(|id| ids.get(&id).copied())).collect();
    let rate = av.send_rate;
    let tracked = av.drive_from_vr || av.follow_local_player;
    let scene_face = scene.local_face().cloned();
    let poses: [Option<(Vec3, Quat)>; citrus_core::TRACKER_SLOTS] = if authority {
        // Every tracker the local player has (calibrated bone targets), else
        // the part objects' poses.
        let mut poses = if tracked { scene.local_tracking_world().map(|t| t.slots()).unwrap_or_default() } else { Default::default() };
        for (k, p) in parts.iter().enumerate() {
            if let (None, Some(pi)) = (poses[k], p) {
                let (_, r, t) = world[*pi].to_scale_rotation_translation();
                poses[k] = Some((t, r));
            }
        }
        let st = s.send_state.entry(raw).or_default();
        let due = now - st.last_avatar >= 1.0 / rate.max(0.5) as f64;
        if due {
            st.last_avatar = now;
            s.send_avatar(raw, &poses);
            if tracked && let Some(face) = scene.local_face() {
                s.send_face(raw, face);
            }
        }
        poses
    } else {
        let Some(buf) = s.avatar_in.get(&raw) else {
            return;
        };
        let rt = s.render_time(buf.origin, delay.max(0.0));
        let Some(poses) = sample_avatar(buf, rt) else {
            return;
        };
        for (k, p) in parts.iter().enumerate() {
            if let (Some(pi), Some((t, r))) = (p, poses[k]) {
                part_writes.push((*pi, t, r));
            }
        }
        poses
    };
    if let Some(av) = component_mut::<NetworkAvatar>(&mut scene.objects[i]) {
        av.targets = citrus_core::TrackerTargets::from_slots(poses);
        av.remote = !authority;
        av.face = if authority {
            if tracked { scene_face.clone() } else { None }
        } else {
            s.face_in.get(&raw).map(|(_, f)| f.clone())
        };
        // The worn avatar package rides on a synced variable (reliable, with
        // late-join catch-up).
        if authority {
            if av.wearing != av.wearing_sent {
                let value = av.wearing.as_ref().map(|w| w.encode()).unwrap_or_default();
                s.set_var(ObjectId::from_raw(raw), citrus_core::WORN_AVATAR_VAR, citrus_core::NetValue::Str(value));
                av.wearing_sent = av.wearing.clone();
            }
        } else if let Some(vars) = s.world.vars.get(&raw) {
            let worn = vars.iter().find(|(k, _)| k == citrus_core::WORN_AVATAR_VAR).and_then(|(_, v)| match v {
                citrus_core::NetValue::Str(s) => citrus_core::WornAvatar::decode(s),
                _ => None,
            });
            if worn != av.wearing {
                av.wearing = worn;
            }
        }
    }
}

/// Host: spawn the Network Manager's player prefab for the host and each joiner.
fn spawn_players(scene: &LoadedScene, s: &mut NetSession, world: &[Mat4]) {
    let Some((mi, m)) = scene
        .objects
        .iter()
        .enumerate()
        .find_map(|(i, o)| component::<NetworkManager>(o).map(|m| (i, m.clone())))
    else {
        s.players_to_spawn.clear();
        return;
    };
    if m.player_prefab.is_empty() {
        s.players_to_spawn.clear();
        s.host_player_done = true;
        return;
    }
    let mut todo = std::mem::take(&mut s.players_to_spawn);
    if !s.host_player_done {
        s.host_player_done = true;
        if !m.dedicated {
            todo.insert(0, citrus_core::HOST_PEER);
        }
    }
    let mut points: Vec<(u32, usize)> = scene
        .objects
        .iter()
        .enumerate()
        .filter_map(|(i, o)| component::<SpawnPoint>(o).filter(|sp| sp.tag == m.spawn_tag).map(|sp| (sp.index, i)))
        .collect();
    points.sort();
    for peer in todo {
        if s.world.player_obj.get(&peer).is_some_and(|id| s.world.spawns.iter().any(|sp| sp.id == *id)) {
            continue;
        }
        let at = if points.is_empty() {
            mi
        } else {
            points[s.spawn_counter as usize % points.len()].1
        };
        s.spawn_counter += 1;
        let (_, r, t) = world[at].to_scale_rotation_translation();
        let id = s.spawn_everywhere(NetSpawnSource::Prefab(m.player_prefab.clone()), t, r, peer);
        s.world.player_obj.insert(peer, id.raw());
        s.world.despawn_on_leave.insert(id.raw());
    }
}

// ------------------------------------------------------------------ host loop helpers

/// The scene's Network Manager settings as a session config + port.
pub fn config_for(scene: &LoadedScene) -> (NetConfig, u16) {
    match scene.objects.iter().find_map(component::<NetworkManager>) {
        Some(m) => (NetConfig::from_manager(m), m.port),
        None => (NetConfig::default(), 9000),
    }
}

/// Built games: start hosting / joining once if the scene's Network Manager asks for it.
pub fn auto_start(net: &mut Option<NetSession>, scene: &LoadedScene) {
    static TRIED: AtomicBool = AtomicBool::new(false);
    if net.is_some() || TRIED.load(Ordering::Relaxed) {
        return;
    }
    let Some(m) = scene.objects.iter().find_map(component::<NetworkManager>) else {
        return;
    };
    TRIED.store(true, Ordering::Relaxed);
    let cfg = NetConfig::from_manager(m);
    let result = match m.auto_start {
        NetAutoStart::Off => return,
        NetAutoStart::Host => NetSession::host_with(m.port, cfg),
        NetAutoStart::Join => NetSession::join_with(&m.address, cfg),
    };
    match result {
        Ok(s) => *net = Some(s),
        Err(e) => tracing::error!("network auto-start failed: {e}"),
    }
}

/// Apply a networking command from gameplay. Returns the command back when it
/// isn't a networking one (or, offline, the local fallback it maps to).
pub fn route_command(net: &mut Option<NetSession>, scene: &LoadedScene, cmd: ComponentCommand) -> Option<ComponentCommand> {
    use ComponentCommand as C;
    match cmd {
        C::RequestOwnership(id) => {
            if let Some(n) = net {
                n.request_ownership(id);
            }
        }
        C::ReleaseOwnership(id) => {
            if let Some(n) = net {
                n.release_ownership(id);
            }
        }
        C::TransferOwnership(id, peer) => {
            if let Some(n) = net {
                n.transfer_ownership(id, peer);
            }
        }
        C::NetMessage { to, text } => {
            if let Some(n) = net {
                n.send_message(to, &text);
            }
        }
        C::NetSpawn { source, root_id, translation, rotation, owner } => {
            if let Some(n) = net {
                n.spawn(source, root_id, translation, rotation, owner);
            }
        }
        C::NetDespawn(id) => {
            if let Some(n) = net {
                n.despawn(id);
            }
        }
        C::NetSetVar { id, key, value } => {
            if let Some(n) = net {
                n.set_var(id, &key, value);
            }
        }
        C::NetRpc { object, name, args, target, reliable } => {
            if let Some(n) = net {
                n.send_rpc(object, &name, &args, target, reliable);
            }
        }
        C::NetHost { port } => {
            let (cfg, default_port) = config_for(scene);
            let port = if port == 0 { default_port } else { port };
            match NetSession::host_with(port, cfg) {
                Ok(s) => *net = Some(s),
                Err(e) => tracing::error!("net_host({port}) failed: {e}"),
            }
        }
        C::NetJoin { address } => {
            let (cfg, _) = config_for(scene);
            match NetSession::join_with(&address, cfg) {
                Ok(s) => *net = Some(s),
                Err(e) => tracing::error!("net_join({address}) failed: {e}"),
            }
        }
        C::NetDisconnect => {
            if let Some(mut n) = net.take() {
                n.disconnect();
            }
        }
        C::NetLoadScene(path) => match net {
            Some(n) if n.is_connected() => {
                if n.is_host() {
                    n.load_scene(&path);
                } else {
                    tracing::warn!("net_load_scene({path}) ignored: only the host switches scenes");
                }
            }
            _ => return Some(C::LoadScene(path)),
        },
        other => return Some(other),
    }
    None
}

// ------------------------------------------------------------------ spawn helpers

/// Deterministic id of the `k`-th object of a networked spawn rooted at
/// `root`, so every peer gives the same children the same ids.
pub fn child_id(root: ObjectId, k: usize) -> ObjectId {
    if k == 0 {
        return root;
    }
    let mut x = root.raw() ^ (k as u128).wrapping_mul(0x9E37_79B9_7F4A_7C15_F39C_C060_5CED_C835);
    x ^= x >> 67;
    x = x.wrapping_mul(0x2545_F491_4F6C_DD1D_9E37_79B9_7F4A_7C15);
    x ^= x >> 59;
    ObjectId::from_raw(x | 1)
}

/// Prefab entries for a networked spawn: deterministic ids (root first) and
/// object references inside the prefab remapped to the new ids.
pub fn spawn_entries(prefab: &Prefab, root_id: ObjectId, translation: Vec3, rotation: Quat) -> Vec<citrus_assets::SceneEntry> {
    let mut entries = prefab.instantiate(translation.to_array());
    let first_root = prefab.entries.iter().position(|e| e.parent.is_none()).unwrap_or(0);
    let mut order: Vec<usize> = (0..entries.len()).collect();
    order.retain(|&k| k != first_root);
    order.insert(0, first_root);
    let mut new_ids = vec![ObjectId::NIL; entries.len()];
    for (slot, &k) in order.iter().enumerate() {
        new_ids[k] = child_id(root_id, slot);
    }
    // `instantiate` already remapped internal references to its own fresh ids.
    let originals: Vec<String> = entries.iter().map(|e| e.id.clone()).collect();
    for (k, e) in entries.iter_mut().enumerate() {
        e.id = new_ids[k].to_string();
        if e.parent.is_none() {
            e.rotation = rotation.to_array();
        }
        for c in &mut e.components {
            for (old, new) in originals.iter().zip(&new_ids) {
                if !old.is_empty() && c.data.contains(old) {
                    c.data = c.data.replace(old, &new.to_string());
                }
            }
        }
    }
    entries
}

/// Give a freshly cloned subtree (root at `root`) deterministic ids.
pub fn reid_subtree(scene: &mut LoadedScene, root: usize, root_id: ObjectId) {
    let mut subtree = vec![root];
    let mut k = 0;
    while k < subtree.len() {
        let p = subtree[k];
        for c in 0..scene.objects.len() {
            if scene.objects[c].parent == Some(p) && !subtree.contains(&c) {
                subtree.push(c);
            }
        }
        k += 1;
    }
    for (slot, &i) in subtree.iter().enumerate() {
        scene.objects[i].id = child_id(root_id, slot);
    }
}

/// Networked pawns follow ownership: a remote player's pawn never reads local input.
pub fn unpossess_remote_pawns(scene: &mut LoadedScene, net: &citrus_core::NetView) {
    if !net.connected {
        return;
    }
    for o in &mut scene.objects {
        if settings(o).is_none() || net.has_authority(o.id) {
            continue;
        }
        if let Some(p) = component_mut::<Pawn>(o) {
            p.possessed = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::MemHub;
    use crate::play::{PlaySession, TickInput, World};
    use crate::scene::SceneObject;
    use crate::shaders::ShaderLibrary;
    use citrus_assets::ObjectSource;
    use citrus_core::{
        Component, ComponentRegistry, InputState, NetValue, NetView, ObjectRef, ScreenInfo, TypedComponent,
    };
    use serde::{Deserialize, Serialize};
    use std::path::Path;

    const DT: f32 = 1.0 / 60.0;

    /// Host-side gameplay: clones a template for everyone, then sets a synced variable on it.
    #[derive(Serialize, Deserialize, Default)]
    struct TestSpawner {
        template: ObjectRef,
        #[serde(skip)]
        spawned: Option<ObjectId>,
        #[serde(skip)]
        wrote: bool,
    }
    impl TypedComponent for TestSpawner {
        const NAME: &'static str = "Test Net Spawner";
        fn update(&mut self, ctx: &mut citrus_core::ComponentCtx) {
            if !ctx.is_server() {
                return;
            }
            match self.spawned {
                None => self.spawned = ctx.net_clone(self.template, Vec3::new(3.0, 0.0, 0.0), Quat::IDENTITY),
                Some(id) if !self.wrote => self.wrote = ctx.set_net_var(id, "hp", 42),
                _ => {}
            }
        }
    }

    /// Collects RPCs addressed to its object and answers pings.
    #[derive(Serialize, Deserialize, Default)]
    struct RpcProbe {
        #[serde(skip)]
        got: Vec<String>,
        #[serde(skip)]
        sent: bool,
    }
    impl TypedComponent for RpcProbe {
        const NAME: &'static str = "Test Rpc Probe";
        fn update(&mut self, ctx: &mut citrus_core::ComponentCtx) {
            let names: Vec<String> = ctx.rpcs_for_self().map(|r| r.name.clone()).collect();
            self.got.extend(names);
            if ctx.is_client() && !self.sent && ctx.peers().len() > 1 {
                self.sent = true;
                ctx.rpc_self("Ping", vec![NetValue::from(1)], citrus_core::RpcTarget::All);
            }
        }
    }

    fn registry() -> ComponentRegistry {
        let mut r = ComponentRegistry::with_builtins();
        r.register::<TestSpawner>();
        r.register::<RpcProbe>();
        r
    }

    fn obj(scene: &mut LoadedScene, id: ObjectId, parent: Option<usize>, enabled: bool, components: Vec<Box<dyn Component>>) -> usize {
        scene.objects.push(SceneObject {
            id,
            name: String::new(),
            render: None,
            extra_render: Vec::new(),
            source: ObjectSource::Empty,
            enabled,
            static_geometry: false,
            lightmap_scale: 1.0,
            layer: 0,
            parent,
            translation: Vec3::ZERO,
            rotation: Quat::IDENTITY,
            scale: Vec3::ONE,
            components,
        });
        scene.objects.len() - 1
    }

    #[test]
    fn humanoid_ik_picks_network_targets_for_remote_avatars_and_local_ones_otherwise() {
        use citrus_core::{HumanoidIk, TrackerTargets, VrPlayer};
        let mut s = LoadedScene::empty();
        let rig = obj(&mut s, ObjectId::new(), None, true, vec![Box::new(VrPlayer::default())]);
        s.objects[rig].translation = Vec3::new(5.0, 0.0, 0.0);
        let remote_targets = TrackerTargets { head: Some((Vec3::new(1.0, 1.6, 0.0), Quat::IDENTITY)), ..Default::default() };
        let remote = obj(
            &mut s,
            ObjectId::new(),
            None,
            true,
            vec![
                Box::new(HumanoidIk::default()),
                Box::new(NetworkAvatar { remote: true, targets: remote_targets, ..Default::default() }),
            ],
        );
        s.objects[remote].translation = Vec3::new(1.0, 0.0, 0.0);
        let local = obj(&mut s, ObjectId::new(), None, true, vec![Box::new(HumanoidIk::default())]);
        s.objects[local].translation = Vec3::new(5.0, 0.0, 0.0);
        s.set_ik_targets(Some(TrackerTargets { head: Some((Vec3::new(0.0, 1.5, 0.0), Quat::IDENTITY)), ..Default::default() }));
        let world = s.world_transforms();
        let active = s.active_flags();
        let inputs = s.humanoid_ik_inputs(&active, &world);
        let head = |i: usize| inputs[&i].0.head.unwrap().0;
        assert!(head(remote).distance(Vec3::new(0.0, 1.6, 0.0)) < 1e-4, "remote: replicated, in the avatar's space");
        assert!(head(local).distance(Vec3::new(0.0, 1.5, 0.0)) < 1e-4, "local: stage space on the VR Player rig");
    }

    struct Ids {
        cube: ObjectId,
        head: ObjectId,
        template: ObjectId,
        pawn: ObjectId,
        probe: ObjectId,
    }

    /// The same scene on every peer (same object ids, like a loaded .scene).
    fn build_scene(ids: &Ids) -> LoadedScene {
        let mut s = LoadedScene::empty();
        let anim = Animator { play_on_start: false, ..Default::default() };
        obj(
            &mut s,
            ids.cube,
            None,
            true,
            vec![Box::new(NetworkObject::default()), Box::new(anim), Box::new(NetworkAnimator::default())],
        );
        let rig = obj(
            &mut s,
            ObjectId::from_raw(ids.head.raw() ^ 1),
            None,
            true,
            vec![Box::new(NetworkAvatar { head: ObjectRef::new(ids.head), drive_from_vr: false, ..Default::default() })],
        );
        obj(&mut s, ids.head, Some(rig), true, vec![]);
        obj(
            &mut s,
            ids.template,
            None,
            false,
            vec![Box::new(NetworkObject::default()), Box::new(NetworkVariables::default())],
        );
        obj(&mut s, ObjectId::new(), None, true, vec![Box::new(TestSpawner { template: ObjectRef::new(ids.template), ..Default::default() })]);
        obj(&mut s, ids.pawn, None, true, vec![Box::new(NetworkObject::default()), Box::new(Pawn::default())]);
        obj(&mut s, ids.probe, None, true, vec![Box::new(NetworkObject::default()), Box::new(RpcProbe::default())]);
        s
    }

    struct Peer {
        net: Option<NetSession>,
        scene: LoadedScene,
        play: PlaySession,
    }

    impl Peer {
        fn new(net: NetSession, ids: &Ids) -> Self {
            let scene = build_scene(ids);
            let play = PlaySession::new(&scene, Path::new("."), None);
            Self { net: Some(net), scene, play }
        }

        /// One frame of the host loop: pump, tick, route commands, replicate.
        fn frame(&mut self, registry: &ComponentRegistry) {
            let net = self.net.as_mut().unwrap();
            net.advance(DT as f64);
            net.pump();
            let view: NetView = net.view();
            let mut shaders = ShaderLibrary::default();
            let input = InputState::default();
            let screen = ScreenInfo::default();
            let host = self.play.tick(
                &mut World { scene: &mut self.scene, renderer: None, registry, shaders: &mut shaders },
                &TickInput { dt: DT, input: &input, net: &view, listener_fallback: Mat4::IDENTITY, screen: &screen },
            );
            for c in host {
                route_command(&mut self.net, &self.scene, c);
            }
            self.scene.network_sync(self.net.as_mut().unwrap(), DT);
        }

        fn net(&mut self) -> &mut NetSession {
            self.net.as_mut().unwrap()
        }

        fn index(&self, id: ObjectId) -> usize {
            self.scene.index_of(id).expect("object exists")
        }

        fn pos(&self, id: ObjectId) -> Vec3 {
            self.scene.world_transform(self.index(id)).w_axis.truncate()
        }
    }

    fn run(peers: &mut [&mut Peer], registry: &ComponentRegistry, frames: usize) {
        for _ in 0..frames {
            for p in peers.iter_mut() {
                p.frame(registry);
            }
        }
    }

    fn session(hub: &MemHub, host: Option<&NetSession>, name: &str) -> NetSession {
        let cfg = NetConfig { player_name: name.into(), discovery: false, ..Default::default() };
        let mut s = match host {
            None => NetSession::host_on(Box::new(hub.endpoint()), cfg),
            Some(h) => NetSession::join_on(Box::new(hub.endpoint()), h.local_addr().unwrap(), cfg),
        };
        s.use_manual_clock();
        s
    }

    fn ids() -> Ids {
        Ids { cube: ObjectId::new(), head: ObjectId::new(), template: ObjectId::new(), pawn: ObjectId::new(), probe: ObjectId::new() }
    }

    #[test]
    fn transform_animator_avatar_converge_and_pawns_unpossess() {
        let registry = registry();
        let ids = ids();
        let hub = MemHub::new();
        let hs = session(&hub, None, "Host");
        let cs = session(&hub, Some(&hs), "Client");
        let mut host = Peer::new(hs, &ids);
        let mut client = Peer::new(cs, &ids);
        run(&mut [&mut host, &mut client], &registry, 10);
        assert!(client.net().is_connected());

        // The host (authority over unowned objects) moves the cube along a path.
        for k in 0..90 {
            let i = host.index(ids.cube);
            host.scene.objects[i].translation = Vec3::new(k as f32 * 0.05, 1.0, 0.0);
            host.scene.objects[i].rotation = Quat::from_rotation_y(k as f32 * 0.02);
            let h = host.index(ids.head);
            host.scene.objects[h].translation = Vec3::new(0.0, 1.7, k as f32 * 0.01);
            run(&mut [&mut host, &mut client], &registry, 1);
            if k == 60 {
                // Mid-motion the client renders ~interp_delay behind, between samples.
                let (hp, cp) = (host.pos(ids.cube), client.pos(ids.cube));
                assert!(cp.x < hp.x && cp.x > hp.x - 0.6, "interpolating behind: host {hp} client {cp}");
            }
        }
        let i = host.index(ids.cube);
        if let Some(a) = component_mut::<Animator>(&mut host.scene.objects[i]) {
            a.set_float("Speed", 3.5);
        }
        run(&mut [&mut host, &mut client], &registry, 40);
        let (hp, cp) = (host.pos(ids.cube), client.pos(ids.cube));
        assert!(hp.distance(cp) < 0.002, "converged: host {hp} client {cp}");
        let hr = host.scene.objects[host.index(ids.cube)].rotation;
        let cr = client.scene.objects[client.index(ids.cube)].rotation;
        assert!(hr.angle_between(cr).to_degrees() < 0.5);
        let speed = component::<Animator>(&client.scene.objects[client.index(ids.cube)]).unwrap().get_float("Speed");
        assert_eq!(speed, 3.5, "animator parameter replicated");
        assert!(host.pos(ids.head).distance(client.pos(ids.head)) < 0.002, "avatar part replicated");
        let rig = client.index(ObjectId::from_raw(ids.head.raw() ^ 1));
        assert!(component::<NetworkAvatar>(&client.scene.objects[rig]).unwrap().targets.head.is_some());
        // Full-body trackers without part objects (chest, knees) replicate too.
        let host_rig = host.index(ObjectId::from_raw(ids.head.raw() ^ 1));
        component_mut::<NetworkAvatar>(&mut host.scene.objects[host_rig]).unwrap().drive_from_vr = true;
        let chest = Vec3::new(0.0, 1.3, 0.1);
        host.scene.set_ik_targets(Some(citrus_core::TrackerTargets {
            chest: Some((chest, Quat::IDENTITY)),
            left_knee: Some((Vec3::new(0.1, 0.5, 0.1), Quat::IDENTITY)),
            ..Default::default()
        }));
        let mut face = citrus_core::FaceFrame::default();
        face.set_param("JawOpen", 0.5);
        host.scene.set_local_face(Some(face));
        run(&mut [&mut host, &mut client], &registry, 20);
        let av = component::<NetworkAvatar>(&client.scene.objects[rig]).unwrap();
        assert!(av.remote, "the client sees a remote avatar");
        let jaw = citrus_core::expression_index("JawOpen").unwrap();
        assert!(av.face.as_ref().is_some_and(|f| (f.weights[jaw] - 0.5).abs() < 0.01), "face replicated");
        assert!(av.targets.chest.is_some_and(|(p, _)| p.distance(chest) < 0.01), "chest tracker replicated");
        assert!(av.targets.left_knee.is_some());
        assert!(!component::<NetworkAvatar>(&host.scene.objects[host_rig]).unwrap().remote);
        let no = component::<NetworkObject>(&client.scene.objects[client.index(ids.cube)]).unwrap();
        assert!(no.state.connected && !no.state.authority);

        // The host-authority pawn must not read the client's input.
        assert!(!component::<Pawn>(&client.scene.objects[client.index(ids.pawn)]).unwrap().possessed);
        assert!(component::<Pawn>(&host.scene.objects[host.index(ids.pawn)]).unwrap().possessed);

        // Ownership moves authority: now the client drives and the host follows.
        client.net().request_ownership(ids.cube);
        run(&mut [&mut host, &mut client], &registry, 10);
        let ci = client.index(ids.cube);
        client.scene.objects[ci].translation = Vec3::new(-4.0, 2.0, 1.0);
        run(&mut [&mut host, &mut client], &registry, 40);
        assert!(host.pos(ids.cube).distance(Vec3::new(-4.0, 2.0, 1.0)) < 0.002, "host follows the new owner");
    }

    #[test]
    fn worn_avatar_package_reaches_peers_and_late_joiners() {
        let registry = registry();
        let ids = ids();
        let hub = MemHub::new();
        let hs = session(&hub, None, "Host");
        let cs = session(&hub, Some(&hs), "Client");
        let mut host = Peer::new(hs, &ids);
        let mut client = Peer::new(cs, &ids);
        run(&mut [&mut host, &mut client], &registry, 10);
        let rig_id = ObjectId::from_raw(ids.head.raw() ^ 1);
        let worn = citrus_core::WornAvatar { id: "abc123".into(), url: "http://example.com/fox.citruspak".into() };
        let hr = host.index(rig_id);
        component_mut::<NetworkAvatar>(&mut host.scene.objects[hr]).unwrap().wearing = Some(worn.clone());
        run(&mut [&mut host, &mut client], &registry, 10);
        let cr = client.index(rig_id);
        assert_eq!(component::<NetworkAvatar>(&client.scene.objects[cr]).unwrap().wearing, Some(worn.clone()));

        let ls = session(&hub, Some(host.net.as_ref().unwrap()), "Late");
        let mut late = Peer::new(ls, &ids);
        run(&mut [&mut host, &mut client, &mut late], &registry, 20);
        let li = late.index(rig_id);
        assert_eq!(component::<NetworkAvatar>(&late.scene.objects[li]).unwrap().wearing, Some(worn), "late joiner catches up");
    }

    #[test]
    fn spawn_vars_rpc_and_late_join() {
        let registry = registry();
        let ids = ids();
        let hub = MemHub::new();
        let hs = session(&hub, None, "Host");
        let cs = session(&hub, Some(&hs), "A");
        let mut host = Peer::new(hs, &ids);
        let mut a = Peer::new(cs, &ids);
        run(&mut [&mut host, &mut a], &registry, 30);

        // The host's TestSpawner cloned the template for everyone and set hp.
        let spawned = host
            .scene
            .objects
            .iter()
            .find_map(|o| component::<TestSpawner>(o).and_then(|s| s.spawned))
            .expect("host spawned");
        let ai = a.index(spawned);
        assert!(a.scene.objects[ai].enabled);
        assert_eq!(component::<NetworkVariables>(&a.scene.objects[ai]).unwrap().get("hp"), Some(&NetValue::Int(42)));
        assert!(a.pos(spawned).distance(Vec3::new(3.0, 0.0, 0.0)) < 0.01);

        // The client's RPC on the probe reached the host and (All) itself.
        let got = |p: &Peer| component::<RpcProbe>(&p.scene.objects[p.index(ids.probe)]).unwrap().got.clone();
        assert_eq!(got(&host), vec!["Ping"]);
        assert_eq!(got(&a), vec!["Ping"]);

        // Move the spawned object on the host, then a late joiner catches up.
        let hi = host.index(spawned);
        host.scene.objects[hi].translation = Vec3::new(5.0, 1.0, -2.0);
        run(&mut [&mut host, &mut a], &registry, 30);
        let ls = session(&hub, host.net.as_ref(), "Late");
        let mut late = Peer::new(ls, &ids);
        run(&mut [&mut host, &mut a, &mut late], &registry, 40);
        let li = late.index(spawned);
        assert_eq!(component::<NetworkVariables>(&late.scene.objects[li]).unwrap().get("hp"), Some(&NetValue::Int(42)));
        assert!(late.pos(spawned).distance(Vec3::new(5.0, 1.0, -2.0)) < 0.01, "late joiner has the current transform");
        assert_eq!(late.net().peer_count(), 3);

        // Destroying the owned spawn on the host despawns it everywhere.
        let hi = host.index(spawned);
        host.scene.remove_object(hi);
        run(&mut [&mut host, &mut a, &mut late], &registry, 30);
        assert!(a.scene.index_of(spawned).is_none() && late.scene.index_of(spawned).is_none());
    }

    #[test]
    fn deterministic_spawn_ids_and_ref_remap() {
        use citrus_assets::{ComponentData, MaterialRef, SceneEntry};
        let entry = |id: &str, parent: Option<usize>, data: &str| SceneEntry {
            id: id.into(),
            name: id.into(),
            source: ObjectSource::Empty,
            enabled: true,
            static_geometry: false,
            lightmap_scale: 1.0,
            layer: 0,
            material: MaterialRef::File(String::new()),
            extra_materials: vec![],
            parent,
            components: if data.is_empty() { vec![] } else { vec![ComponentData { kind: "Network Avatar".into(), data: data.into() }] },
            translation: [0.0; 3],
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: [1.0; 3],
            model: None,
            model_part: None,
        };
        let head = "11111111-2222-4333-8444-555555555555";
        let prefab = Prefab::new(vec![entry("aaaaaaaa-2222-4333-8444-555555555555", None, &format!("(head: Some(\"{head}\"))")), entry(head, Some(0), "")]);
        let root = ObjectId::new();
        let a = spawn_entries(&prefab, root, Vec3::X, Quat::IDENTITY);
        let b = spawn_entries(&prefab, root, Vec3::X, Quat::IDENTITY);
        assert_eq!(a[0].id, root.to_string());
        assert_eq!(a[1].id, b[1].id, "children get the same ids on every peer");
        assert_ne!(a[1].id, head);
        assert!(a[0].components[0].data.contains(&a[1].id), "intra-prefab reference remapped");
    }
}
