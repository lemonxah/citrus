//! Prefab / reusable-object template (ENGINE_FEATURE_CHECKLIST T0 #7).
//!
//! A `.prefab` (RON) is a self-contained object subtree (`SceneEntry` list with
//! parent indices local to the prefab; roots have `parent = None`). The editor
//! makes one from a selection; `instantiate` clones it into a scene at a given
//! transform with FRESH ids (so instances don't collide), applying per-instance
//! root transform overrides — Unity/Godot's core "designer-scale content" workflow,
//! replacing the previous "duplicate only".

use citrus_assets::SceneEntry;
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
    /// - **ids cleared** so the engine assigns fresh unique ids on insert (instances
    ///   never share an id with the prefab or each other),
    /// - root entries moved to `translation` (the per-instance override). Parent
    ///   indices stay local; the caller offsets them by the scene's current length.
    ///
    /// Returns the new entries (root-relative parent indices preserved).
    pub fn instantiate(&self, translation: [f32; 3]) -> Vec<SceneEntry> {
        let mut out = self.entries.clone();
        for (i, e) in out.iter_mut().enumerate() {
            e.id = String::new(); // engine assigns a fresh id on load
            if self.entries[i].parent.is_none() {
                // Per-instance override: place the root(s) at `translation`.
                e.translation = translation;
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
    use citrus_assets::{MaterialRef, ObjectSource};

    fn entry(name: &str, parent: Option<usize>) -> SceneEntry {
        SceneEntry {
            id: format!("orig-{name}"),
            name: name.into(),
            source: ObjectSource::Empty,
            enabled: true,
            static_geometry: false,
            lightmap_scale: 1.0,
            layer: 0,
            material: MaterialRef::File(String::new()),
            extra_materials: vec![],
            parent,
            components: vec![],
            translation: [0.0, 0.0, 0.0],
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: [1.0, 1.0, 1.0],
        }
    }

    fn turret() -> Prefab {
        // root "Base" + child "Gun".
        Prefab::new(vec![entry("Base", None), entry("Gun", Some(0))])
    }

    #[test]
    fn instantiate_clears_ids_and_moves_root() {
        let p = turret();
        let inst = p.instantiate([5.0, 0.0, 2.0]);
        assert_eq!(inst.len(), 2);
        assert!(inst.iter().all(|e| e.id.is_empty()), "ids cleared for fresh assignment");
        assert_eq!(inst[0].translation, [5.0, 0.0, 2.0]); // root moved
        assert_eq!(inst[1].parent, Some(0)); // child parent preserved (local)
        assert_eq!(inst[1].translation, [0.0, 0.0, 0.0]); // child unchanged
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
