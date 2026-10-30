//! Networking hooks of the play session (a child module of `play` so it can
//! reach the session's physics and prefab cache): applies the world changes
//! the network asks for, hands physics bodies between local simulation and
//! remote control, and applies networked spawn / despawn / variable commands
//! locally before they are replicated.

use citrus_core::{ComponentCommand, NetSpawnSource, NetWorldOp, NetworkVariables, ObjectId};
use glam::{Quat, Vec3};

use super::{PlaySession, World};
use crate::prefab::Prefab;

impl PlaySession {
    /// Before the frame runs: remote spawns / despawns / activation, scene
    /// switches, pawn possession and physics hand-over.
    pub(super) fn net_pre_tick(&mut self, w: &mut World, net: &citrus_core::NetView, host: &mut Vec<ComponentCommand>) {
        for op in &net.world_ops {
            match op {
                NetWorldOp::Spawn { source, root_id, translation, rotation, .. } => {
                    self.net_spawn(w, source, *root_id, *translation, *rotation)
                }
                NetWorldOp::Despawn(id) => self.net_remove(w, *id),
                NetWorldOp::SetActive(id, on) => {
                    if let Some(i) = w.scene.index_of(*id)
                        && w.scene.objects[i].enabled != *on
                    {
                        w.scene.objects[i].enabled = *on;
                        self.physics.mark_dirty();
                    }
                }
                NetWorldOp::LoadScene(path) => host.push(ComponentCommand::LoadScene(path.clone())),
            }
        }
        crate::net_sync::unpossess_remote_pawns(w.scene, net);
        if !net.remote_bodies.is_empty() || !net.reclaimed_bodies.is_empty() {
            self.physics.sync_structure(w.scene);
            for id in net.remote_bodies.keys() {
                self.physics.set_net_driven(w.scene, *id, true, Vec3::ZERO);
            }
            for (id, vel) in &net.reclaimed_bodies {
                self.physics.set_net_driven(w.scene, *id, false, *vel);
            }
        }
    }

    /// Networked commands from gameplay: apply locally, then hand to the host
    /// loop for replication.
    pub(super) fn apply_net_command(&mut self, w: &mut World, cmd: ComponentCommand, host: &mut Vec<ComponentCommand>) {
        match &cmd {
            ComponentCommand::NetSpawn { source, root_id, translation, rotation, .. } => {
                self.net_spawn(w, source, *root_id, *translation, *rotation)
            }
            ComponentCommand::NetDespawn(id) => self.net_remove(w, *id),
            ComponentCommand::NetSetVar { id, key, value } => {
                if let Some(i) = w.scene.index_of(*id)
                    && let Some(v) = w.scene.objects[i]
                        .components
                        .iter_mut()
                        .find_map(|c| c.as_any_mut().downcast_mut::<NetworkVariables>())
                {
                    v.set(key, value.clone());
                }
            }
            _ => {}
        }
        host.push(cmd);
    }

    fn net_remove(&mut self, w: &mut World, id: ObjectId) {
        if let Some(i) = w.scene.index_of(id) {
            w.scene.remove_object(i);
            self.physics.mark_dirty();
        }
    }

    /// Instantiate a networked spawn with deterministic ids (same on every peer).
    pub(super) fn net_spawn(&mut self, w: &mut World, source: &NetSpawnSource, root_id: ObjectId, translation: Vec3, rotation: Quat) {
        if w.scene.index_of(root_id).is_some() {
            return;
        }
        match source {
            NetSpawnSource::Prefab(path) => {
                if !self.prefabs.contains_key(path) {
                    match Prefab::load(citrus_core::asset_ref::resolve_path(&self.project_root, path)) {
                        Ok(p) => {
                            self.prefabs.insert(path.to_owned(), p);
                        }
                        Err(e) => {
                            tracing::error!("net spawn {path:?}: {e:#}");
                            return;
                        }
                    }
                }
                let entries = crate::net_sync::spawn_entries(&self.prefabs[path], root_id, translation, rotation);
                let Some(renderer) = w.renderer.as_deref_mut() else {
                    tracing::error!("net spawn {path:?}: no renderer");
                    return;
                };
                match w.scene.append_entries(renderer, w.shaders, &self.project_root, w.registry, &entries) {
                    Ok(_) => self.physics.mark_dirty(),
                    Err(e) => tracing::error!("net spawn {path:?}: {e:#}"),
                }
            }
            NetSpawnSource::Clone(original) => {
                if let Some(i) = w.scene.index_of(*original)
                    && let Some(n) = w.scene.duplicate_object(i, w.registry)
                {
                    crate::net_sync::reid_subtree(w.scene, n, root_id);
                    let o = &mut w.scene.objects[n];
                    o.parent = None;
                    o.enabled = true;
                    o.translation = translation;
                    o.rotation = rotation;
                    self.physics.mark_dirty();
                }
            }
        }
    }
}
