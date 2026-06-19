//! Level / world streaming (ENGINE_FEATURE_CHECKLIST T1 #33).
//!
//! Divides a world into named cells (each a sub-scene + bounds). As a focus point
//! (usually the camera) moves, [`StreamingWorld::update`] returns which cells to
//! LOAD and which to UNLOAD, with hysteresis (a larger unload radius than load) so a
//! cell hovering on the boundary doesn't thrash. The caller performs the actual
//! async scene load/free; this is the (testable) policy that decides when.

use std::collections::HashSet;

use glam::Vec3;

/// A streamable world cell: a sub-scene file + a world-space AABB.
#[derive(Clone, Debug)]
pub struct Cell {
    pub id: String,
    pub scene: String, // path to the cell's `.scene`
    pub min: Vec3,
    pub max: Vec3,
}

impl Cell {
    /// Nearest distance from `p` to the cell's AABB (0 if inside).
    pub fn distance(&self, p: Vec3) -> f32 {
        let d = (self.min - p).max(p - self.max).max(Vec3::ZERO);
        d.length()
    }
}

/// What changed this update — the caller loads `to_load`, frees `to_unload`.
#[derive(Debug, Default, PartialEq)]
pub struct StreamDelta {
    pub to_load: Vec<String>,
    pub to_unload: Vec<String>,
}

pub struct StreamingWorld {
    cells: Vec<Cell>,
    loaded: HashSet<String>,
    /// Load cells within this distance of the focus point.
    pub load_radius: f32,
    /// Unload only past this (>= load_radius) — the hysteresis band.
    pub unload_radius: f32,
}

impl StreamingWorld {
    pub fn new(cells: Vec<Cell>, load_radius: f32, unload_radius: f32) -> Self {
        Self {
            cells,
            loaded: HashSet::new(),
            load_radius,
            unload_radius: unload_radius.max(load_radius),
        }
    }

    pub fn loaded(&self) -> impl Iterator<Item = &String> {
        self.loaded.iter()
    }
    pub fn is_loaded(&self, id: &str) -> bool {
        self.loaded.contains(id)
    }

    /// Recompute desired residency for `focus`. Mutates the internal loaded set and
    /// returns the load/unload deltas to act on.
    pub fn update(&mut self, focus: Vec3) -> StreamDelta {
        let mut delta = StreamDelta::default();
        for c in &self.cells {
            let dist = c.distance(focus);
            let loaded = self.loaded.contains(&c.id);
            if !loaded && dist <= self.load_radius {
                self.loaded.insert(c.id.clone());
                delta.to_load.push(c.id.clone());
            } else if loaded && dist > self.unload_radius {
                self.loaded.remove(&c.id);
                delta.to_unload.push(c.id.clone());
            }
        }
        delta.to_load.sort();
        delta.to_unload.sort();
        delta
    }

    /// Scene path for a cell id.
    pub fn scene_of(&self, id: &str) -> Option<&str> {
        self.cells.iter().find(|c| c.id == id).map(|c| c.scene.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world() -> StreamingWorld {
        let cells = vec![
            Cell { id: "a".into(), scene: "a.scene".into(), min: Vec3::new(0.0, 0.0, 0.0), max: Vec3::new(10.0, 5.0, 10.0) },
            Cell { id: "b".into(), scene: "b.scene".into(), min: Vec3::new(50.0, 0.0, 0.0), max: Vec3::new(60.0, 5.0, 10.0) },
        ];
        StreamingWorld::new(cells, 20.0, 30.0)
    }

    #[test]
    fn loads_near_unloads_far_with_hysteresis() {
        let mut w = world();
        // Near cell a, far from b.
        let d = w.update(Vec3::new(5.0, 0.0, 5.0));
        assert_eq!(d.to_load, vec!["a".to_string()]);
        assert!(w.is_loaded("a") && !w.is_loaded("b"));

        // Move so a is 25 m away (past load 20 but inside unload 30 -> stays), and
        // b is 15 m -> loads. (a: x[0,10], focus x=35 -> 25 m; b: x[50,60] -> 15 m.)
        let d = w.update(Vec3::new(35.0, 0.0, 5.0));
        assert_eq!(d.to_load, vec!["b".to_string()]);
        assert!(w.is_loaded("a"), "a stays loaded inside the hysteresis band");

        // Move far from a (past unload 30) -> a unloads.
        let d = w.update(Vec3::new(55.0, 0.0, 5.0));
        assert_eq!(d.to_unload, vec!["a".to_string()]);
        assert!(!w.is_loaded("a"));
    }

    #[test]
    fn idempotent_when_stationary() {
        let mut w = world();
        w.update(Vec3::new(5.0, 0.0, 5.0));
        let d = w.update(Vec3::new(5.0, 0.0, 5.0));
        assert_eq!(d, StreamDelta::default()); // nothing changes
    }
}
