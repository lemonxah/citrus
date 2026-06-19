//! Level-of-detail selection (ENGINE_FEATURE_CHECKLIST T1 #29).
//!
//! A `LodGroup` lists mesh variants by the camera distance at which each stops
//! being used, plus an optional final cull distance. `select` returns which LOD to
//! draw for a given distance (with hysteresis support to avoid LOD popping when an
//! object hovers on a boundary). The renderer swaps the chosen mesh; this module is
//! the (fully testable) selection policy.

/// One LOD: shown while `camera distance <= max_distance`. Levels are kept sorted
/// ascending by `max_distance` (LOD0 = highest detail, nearest).
#[derive(Clone, Debug, PartialEq)]
pub struct LodLevel {
    /// Upper distance (metres) this LOD is used to. The last level may use
    /// `f32::INFINITY` to never cull.
    pub max_distance: f32,
    /// Renderer mesh handle (index) for this level.
    pub mesh: usize,
}

#[derive(Clone, Debug, Default)]
pub struct LodGroup {
    levels: Vec<LodLevel>,
    /// Beyond this distance the object is culled entirely (`None` = never).
    pub cull_distance: Option<f32>,
}

impl LodGroup {
    pub fn new(mut levels: Vec<LodLevel>) -> Self {
        levels.sort_by(|a, b| a.max_distance.partial_cmp(&b.max_distance).unwrap());
        Self { levels, cull_distance: None }
    }

    pub fn with_cull(mut self, d: f32) -> Self {
        self.cull_distance = Some(d);
        self
    }

    pub fn levels(&self) -> &[LodLevel] {
        &self.levels
    }

    /// Pick the LOD index for `distance`, or `None` if culled (beyond the last
    /// level's range or past `cull_distance`).
    pub fn select(&self, distance: f32) -> Option<usize> {
        if let Some(c) = self.cull_distance {
            if distance > c {
                return None;
            }
        }
        for (i, l) in self.levels.iter().enumerate() {
            if distance <= l.max_distance {
                return Some(i);
            }
        }
        None
    }

    /// Hysteresis-aware selection: given the CURRENTLY drawn LOD, only switch when
    /// the distance crosses a boundary by more than `margin` (metres), so an object
    /// sitting exactly on a boundary doesn't flicker between two LODs frame to frame.
    pub fn select_hysteresis(&self, distance: f32, current: Option<usize>, margin: f32) -> Option<usize> {
        let target = self.select(distance);
        match (current, target) {
            (Some(cur), Some(tgt)) if cur != tgt => {
                // Bias the boundary by `margin` in the direction of the current LOD
                // so we hold the current one a little longer.
                let bound = self.levels.get(cur.min(tgt))?.max_distance;
                if (distance - bound).abs() < margin {
                    current // still within the dead-band; keep current
                } else {
                    target
                }
            }
            _ => target,
        }
    }

    /// The mesh handle for a selected index.
    pub fn mesh_for(&self, distance: f32) -> Option<usize> {
        self.select(distance).map(|i| self.levels[i].mesh)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group() -> LodGroup {
        LodGroup::new(vec![
            LodLevel { max_distance: 10.0, mesh: 0 },
            LodLevel { max_distance: 30.0, mesh: 1 },
            LodLevel { max_distance: 80.0, mesh: 2 },
        ])
    }

    #[test]
    fn selects_by_distance_band() {
        let g = group();
        assert_eq!(g.select(5.0), Some(0));
        assert_eq!(g.select(10.0), Some(0));
        assert_eq!(g.select(10.1), Some(1));
        assert_eq!(g.select(50.0), Some(2));
        assert_eq!(g.select(100.0), None); // past last band -> culled
        assert_eq!(g.mesh_for(20.0), Some(1));
    }

    #[test]
    fn cull_distance_overrides() {
        let g = group().with_cull(40.0);
        assert_eq!(g.select(35.0), Some(2));
        assert_eq!(g.select(45.0), None);
    }

    #[test]
    fn hysteresis_holds_current_in_dead_band() {
        let g = group();
        // At ~10 m (the LOD0/LOD1 boundary), staying on LOD0 should hold within margin.
        assert_eq!(g.select_hysteresis(10.3, Some(0), 0.5), Some(0));
        // Well past the boundary -> switch.
        assert_eq!(g.select_hysteresis(12.0, Some(0), 0.5), Some(1));
        // No current LOD -> just the plain selection.
        assert_eq!(g.select_hysteresis(10.3, None, 0.5), Some(1));
    }

    #[test]
    fn unsorted_input_is_normalized() {
        let g = LodGroup::new(vec![
            LodLevel { max_distance: 80.0, mesh: 2 },
            LodLevel { max_distance: 10.0, mesh: 0 },
            LodLevel { max_distance: 30.0, mesh: 1 },
        ]);
        assert_eq!(g.select(5.0), Some(0));
        assert_eq!(g.levels()[0].mesh, 0);
    }
}
