//! Animation state machine + 1D blend trees (ENGINE_FEATURE_CHECKLIST T1 #21).
//!
//! A runtime AnimGraph over the existing rig/clip import: named states (each a clip
//! or a 1D blend tree), parameter-driven transitions with a blend duration, and a
//! tick that advances time + cross-fades. This is the controller logic (fully
//! testable); the skinning system samples `current_pose()` weights.

use std::collections::HashMap;

/// A transition condition on a float parameter.
#[derive(Clone, Debug)]
pub enum Condition {
    Greater(String, f32),
    Less(String, f32),
    /// A trigger parameter (consumed when the transition fires).
    Trigger(String),
}

#[derive(Clone, Debug)]
pub struct Transition {
    pub to: String,
    pub conditions: Vec<Condition>,
    /// Cross-fade duration in seconds.
    pub blend: f32,
}

/// A state is either a single clip or a 1D blend between clips driven by a param.
#[derive(Clone, Debug)]
pub enum Motion {
    Clip(String),
    /// (param, [(threshold, clip)]) sorted by threshold; sampled by the param value.
    Blend1D(String, Vec<(f32, String)>),
}

#[derive(Clone, Debug)]
pub struct State {
    pub name: String,
    pub motion: Motion,
    pub transitions: Vec<Transition>,
}

/// A sampled clip contribution: which clip + its blend weight (weights sum to 1).
#[derive(Clone, Debug, PartialEq)]
pub struct ClipWeight {
    pub clip: String,
    pub weight: f32,
}

#[derive(Default)]
pub struct AnimGraph {
    states: HashMap<String, State>,
    params: HashMap<String, f32>,
    triggers: std::collections::HashSet<String>,
    current: String,
    /// In-progress cross-fade: (previous state, remaining seconds, total).
    blending: Option<(String, f32, f32)>,
    time: f32,
}

impl AnimGraph {
    pub fn new(entry: &str) -> Self {
        Self { current: entry.to_string(), ..Default::default() }
    }

    pub fn add_state(&mut self, state: State) {
        self.states.insert(state.name.clone(), state);
    }

    pub fn set_param(&mut self, name: &str, value: f32) {
        self.params.insert(name.to_string(), value);
    }
    pub fn set_trigger(&mut self, name: &str) {
        self.triggers.insert(name.to_string());
    }
    pub fn current_state(&self) -> &str {
        &self.current
    }
    pub fn is_blending(&self) -> bool {
        self.blending.is_some()
    }

    fn cond_met(&self, c: &Condition) -> bool {
        match c {
            Condition::Greater(p, v) => self.params.get(p).copied().unwrap_or(0.0) > *v,
            Condition::Less(p, v) => self.params.get(p).copied().unwrap_or(0.0) < *v,
            Condition::Trigger(t) => self.triggers.contains(t),
        }
    }

    /// Advance the graph by `dt`. Evaluates transitions from the current state, then
    /// advances any active cross-fade.
    pub fn tick(&mut self, dt: f32) {
        self.time += dt;
        // Only evaluate new transitions when not already blending (Unity-style).
        if self.blending.is_none() {
            if let Some(state) = self.states.get(&self.current).cloned() {
                for t in &state.transitions {
                    if t.conditions.iter().all(|c| self.cond_met(c)) {
                        // Consume triggers used by this transition.
                        for c in &t.conditions {
                            if let Condition::Trigger(name) = c {
                                self.triggers.remove(name);
                            }
                        }
                        if t.to != self.current && self.states.contains_key(&t.to) {
                            let prev = std::mem::replace(&mut self.current, t.to.clone());
                            if t.blend > 1e-4 {
                                self.blending = Some((prev, t.blend, t.blend));
                            }
                            self.time = 0.0;
                            break;
                        }
                    }
                }
            }
        }
        // Advance the cross-fade.
        if let Some((_, remaining, _)) = &mut self.blending {
            *remaining -= dt;
            if *remaining <= 0.0 {
                self.blending = None;
            }
        }
    }

    /// Sample a state's motion into clip weights (handles Blend1D).
    fn sample_state(&self, name: &str) -> Vec<ClipWeight> {
        match self.states.get(name).map(|s| &s.motion) {
            Some(Motion::Clip(c)) => vec![ClipWeight { clip: c.clone(), weight: 1.0 }],
            Some(Motion::Blend1D(param, points)) if !points.is_empty() => {
                let v = self.params.get(param).copied().unwrap_or(0.0);
                blend_1d(v, points)
            }
            _ => Vec::new(),
        }
    }

    /// Current clip weights, blending the previous state during a cross-fade.
    /// Weights always sum to ~1.
    pub fn current_pose(&self) -> Vec<ClipWeight> {
        let cur = self.sample_state(&self.current);
        match &self.blending {
            Some((prev, remaining, total)) => {
                let a = (1.0 - remaining / total).clamp(0.0, 1.0); // 0->1 into current
                let mut out: Vec<ClipWeight> = cur
                    .into_iter()
                    .map(|c| ClipWeight { clip: c.clip, weight: c.weight * a })
                    .collect();
                for p in self.sample_state(prev) {
                    out.push(ClipWeight { clip: p.clip, weight: p.weight * (1.0 - a) });
                }
                out
            }
            None => cur,
        }
    }
}

/// 1D blend: linearly interpolate weight between the two clips bracketing `v`.
fn blend_1d(v: f32, points: &[(f32, String)]) -> Vec<ClipWeight> {
    let mut pts = points.to_vec();
    pts.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    if v <= pts[0].0 {
        return vec![ClipWeight { clip: pts[0].1.clone(), weight: 1.0 }];
    }
    if v >= pts[pts.len() - 1].0 {
        return vec![ClipWeight { clip: pts[pts.len() - 1].1.clone(), weight: 1.0 }];
    }
    for w in pts.windows(2) {
        if v >= w[0].0 && v <= w[1].0 {
            let t = (v - w[0].0) / (w[1].0 - w[0].0).max(1e-6);
            return vec![
                ClipWeight { clip: w[0].1.clone(), weight: 1.0 - t },
                ClipWeight { clip: w[1].1.clone(), weight: t },
            ];
        }
    }
    vec![ClipWeight { clip: pts[0].1.clone(), weight: 1.0 }]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn locomotion() -> AnimGraph {
        let mut g = AnimGraph::new("idle");
        g.add_state(State {
            name: "idle".into(),
            motion: Motion::Clip("Idle".into()),
            transitions: vec![Transition {
                to: "run".into(),
                conditions: vec![Condition::Greater("speed".into(), 0.5)],
                blend: 0.2,
            }],
        });
        g.add_state(State {
            name: "run".into(),
            motion: Motion::Blend1D(
                "speed".into(),
                vec![(0.5, "Walk".into()), (3.0, "Run".into())],
            ),
            transitions: vec![Transition {
                to: "idle".into(),
                conditions: vec![Condition::Less("speed".into(), 0.5)],
                blend: 0.2,
            }],
        });
        g
    }

    #[test]
    fn param_transition_fires_and_blends() {
        let mut g = locomotion();
        assert_eq!(g.current_state(), "idle");
        g.set_param("speed", 1.0);
        g.tick(0.1);
        assert_eq!(g.current_state(), "run");
        assert!(g.is_blending());
        // During the blend both idle + run clips contribute.
        let pose = g.current_pose();
        let total: f32 = pose.iter().map(|c| c.weight).sum();
        assert!((total - 1.0).abs() < 1e-3, "weights should sum to 1: {pose:?}");
        // Finish the blend.
        g.tick(0.3);
        assert!(!g.is_blending());
    }

    #[test]
    fn blend_tree_interpolates_between_clips() {
        let mut g = locomotion();
        g.set_param("speed", 1.0);
        g.tick(0.1); // -> run
        g.tick(0.3); // finish blend
        g.set_param("speed", 1.75); // midpoint of 0.5..3.0
        let pose = g.current_pose();
        assert_eq!(pose.len(), 2);
        // ~halfway -> roughly equal Walk/Run weights.
        assert!((pose[0].weight - 0.5).abs() < 0.1, "{pose:?}");
    }

    #[test]
    fn trigger_is_consumed() {
        let mut g = AnimGraph::new("a");
        g.add_state(State {
            name: "a".into(),
            motion: Motion::Clip("A".into()),
            transitions: vec![Transition {
                to: "b".into(),
                conditions: vec![Condition::Trigger("jump".into())],
                blend: 0.0,
            }],
        });
        g.add_state(State { name: "b".into(), motion: Motion::Clip("B".into()), transitions: vec![] });
        g.set_trigger("jump");
        g.tick(0.016);
        assert_eq!(g.current_state(), "b");
    }
}
