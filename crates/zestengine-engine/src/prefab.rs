//! Prefab / reusable-object template (ENGINE_FEATURE_CHECKLIST T0 #7).
//!
//! A `.prefab` (RON) is a self-contained object subtree (`SceneEntry` list with
//! parent indices local to the prefab; roots have `parent = None`). The editor
//! makes one from a selection; `instantiate` clones it into a scene at a given
//! transform with FRESH ids (so instances don't collide), applying per-instance
//! root transform overrides — Unity/Godot's core "designer-scale content" workflow,
//! replacing the previous "duplicate only".

use zestengine_assets::SceneEntry;
use zestengine_core::ObjectId;
use serde::{Deserialize, Serialize};

/// A reusable object template.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Prefab {
    /// Subtree entries. `parent` indices are local to this list; root entries (the
    /// instantiation anchors) have `parent: None`.
    pub entries: Vec<SceneEntry>,
}

impl Prefab {
    /// Build a prefab from a set of scene entries (already re-indexed so parent
    /// references are local to `entries`).
    pub fn new(entries: Vec<SceneEntry>) -> Self {
        Self { entries }
    }

    pub fn from_ron(s: &str) -> Result<Self, ron::error::SpannedError> {
        ron::from_str(s)
    }

    pub fn to_ron(&self) -> String {
        ron::ser::to_string_pretty(self, ron::ser::PrettyConfig::default()).unwrap_or_default()
    }

    pub fn save(&self, path: impl AsRef<std::path::Path>) -> anyhow::Result<()> {
        std::fs::write(path, self.to_ron())?;
        Ok(())
    }

    pub fn load(path: impl AsRef<std::path::Path>) -> anyhow::Result<Self> {
        Ok(Self::from_ron(&std::fs::read_to_string(path)?)?)
    }

    /// Root entry indices (those with no parent within the prefab).
    pub fn roots(&self) -> Vec<usize> {
        (0..self.entries.len())
            .filter(|&i| self.entries[i].parent.is_none())
            .collect()
    }

    /// Clone the prefab into a fresh set of entries ready to add to a scene:
    /// - **fresh ids** (instances never share an id with the prefab or each
    ///   other); object references inside component data that point at other
    ///   prefab objects are rewritten to the new ids,
    /// - root entries moved to `translation` (the per-instance override). Parent
    ///   indices stay local; the caller offsets them by the scene's current length.
    ///
    /// Returns the new entries (root-relative parent indices preserved).
    pub fn instantiate(&self, translation: [f32; 3]) -> Vec<SceneEntry> {
        self.instantiate_rooted(translation, None)
    }

    /// [`instantiate`](Self::instantiate) with a caller-chosen id for the first
    /// root (e.g. the id `ctx.spawn_prefab` already handed to gameplay code).
    pub fn instantiate_rooted(&self, translation: [f32; 3], root_id: Option<ObjectId>) -> Vec<SceneEntry> {
        let first_root = self.entries.iter().position(|e| e.parent.is_none());
        let new_ids: Vec<String> = (0..self.entries.len())
            .map(|i| match root_id {
                Some(id) if Some(i) == first_root => id.to_string(),
                _ => ObjectId::new().to_string(),
            })
            .collect();
        let mut out = self.entries.clone();
        for (i, e) in out.iter_mut().enumerate() {
            e.id = new_ids[i].clone();
            if self.entries[i].parent.is_none() {
                // Per-instance override: place the root(s) at `translation`.
                e.translation = translation;
            }
            for c in &mut e.components {
                for (old, new) in self.entries.iter().map(|e| e.id.as_str()).zip(&new_ids) {
                    if !old.is_empty() && c.data.contains(old) {
                        c.data = c.data.replace(old, new);
                    }
                }
            }
        }
        out
    }

    /// Like [`instantiate`] but also overrides each root's rotation + scale.
    pub fn instantiate_with(
        &self,
        translation: [f32; 3],
        rotation: [f32; 4],
        scale: [f32; 3],
    ) -> Vec<SceneEntry> {
        let mut out = self.instantiate(translation);
        for (i, e) in out.iter_mut().enumerate() {
            if self.entries[i].parent.is_none() {
                e.rotation = rotation;
                e.scale = scale;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zestengine_assets::{MaterialRef, ObjectSource};

    fn entry(name: &str, parent: Option<usize>) -> SceneEntry {
        SceneEntry {
            id: format!("orig-{name}"),
            name: name.into(),
            source: ObjectSource::Empty,
            enabled: true,
            static_geometry: false,
            lightmap_scale: 1.0,
            cast_shadows: zestengine_core::ShadowCasting::On,
            layer: 0,
            material: MaterialRef::File(String::new()),
            extra_materials: vec![],
            parent,
            components: vec![],
            translation: [0.0, 0.0, 0.0],
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: [1.0, 1.0, 1.0],
            model: None,
            model_part: None,
        }
    }

    fn turret() -> Prefab {
        // root "Base" + child "Gun".
        Prefab::new(vec![entry("Base", None), entry("Gun", Some(0))])
    }

    #[test]
    fn instantiate_gives_fresh_ids_and_moves_root() {
        let p = turret();
        let inst = p.instantiate([5.0, 0.0, 2.0]);
        let again = p.instantiate([5.0, 0.0, 2.0]);
        assert_eq!(inst.len(), 2);
        assert!(inst.iter().all(|e| !e.id.is_empty() && !e.id.starts_with("orig-")), "fresh ids");
        assert_ne!(inst[0].id, again[0].id, "instances never share ids");
        assert_eq!(inst[0].translation, [5.0, 0.0, 2.0]); // root moved
        assert_eq!(inst[1].parent, Some(0)); // child parent preserved (local)
        assert_eq!(inst[1].translation, [0.0, 0.0, 0.0]); // child unchanged
    }

    #[test]
    fn instantiate_remaps_internal_references() {
        let gun_id = ObjectId::new().to_string();
        let mut base = entry("Base", None);
        base.components.push(zestengine_assets::ComponentData::new("Turret", format!("(barrel: Some(\"{gun_id}\"))")));
        let mut gun = entry("Gun", Some(0));
        gun.id = gun_id.clone();
        let p = Prefab::new(vec![base, gun]);
        let root = ObjectId::new();
        let inst = p.instantiate_rooted([0.0; 3], Some(root));
        assert_eq!(inst[0].id, root.to_string());
        assert!(inst[0].components[0].data.contains(&inst[1].id), "reference follows the new id");
        assert!(!inst[0].components[0].data.contains(&gun_id));
    }

    #[test]
    fn roots_are_parentless_entries() {
        assert_eq!(turret().roots(), vec![0]);
    }

    #[test]
    fn ron_round_trips_and_overrides_apply() {
        let p = turret();
        let s = p.to_ron();
        let back = Prefab::from_ron(&s).unwrap();
        let inst = back.instantiate_with([1.0, 2.0, 3.0], [0.0, 0.0, 0.0, 1.0], [2.0, 2.0, 2.0]);
        assert_eq!(inst[0].translation, [1.0, 2.0, 3.0]);
        assert_eq!(inst[0].scale, [2.0, 2.0, 2.0]);
    }
}
