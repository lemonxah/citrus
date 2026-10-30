//! Animator Controller asset (`.animctrl`): typed parameters plus layers, each
//! a state machine of motion states joined by conditional transitions. Modelled
//! on Unity's Animator Controller (states, Any State, exit time, triggers,
//! layers with masks and additive blending), with Unreal-style state enter/exit
//! notifies so other systems can react. Evaluated by the engine's anim graph.

use std::path::Path;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use citrus_core::BlendTree;

pub const ANIM_CONTROLLER_EXTENSION: &str = "animctrl";

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum AnimParamKind {
    #[default]
    Float,
    Int,
    Bool,
    /// A bool that resets itself when a transition consumes it.
    Trigger,
}

impl AnimParamKind {
    pub const ALL: [AnimParamKind; 4] = [Self::Float, Self::Int, Self::Bool, Self::Trigger];
    pub fn label(self) -> &'static str {
        match self {
            Self::Float => "Float",
            Self::Int => "Int",
            Self::Bool => "Bool",
            Self::Trigger => "Trigger",
        }
    }
}

/// A controller parameter. Every kind is stored as an f32 at runtime (ints
/// rounded, bools and triggers 0 or 1), so `default` covers all of them.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct AnimParameter {
    pub name: String,
    pub kind: AnimParamKind,
    pub default: f32,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum LayerBlend {
    #[default]
    Override,
    /// Adds the layer pose's offset from the rest pose on top of the layers below.
    Additive,
}

/// What a state plays.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Motion {
    /// An imported clip name, or a project-relative `.anim` path.
    Clip(String),
    BlendTree(BlendTree),
}

impl Default for Motion {
    fn default() -> Self {
        Motion::Clip(String::new())
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct AnimState {
    pub name: String,
    pub motion: Motion,
    pub speed: f32,
    /// Float parameter multiplying `speed` (empty = none).
    pub speed_parameter: String,
    pub looping: bool,
    /// Graph-editor position (canvas units).
    pub position: [f32; 2],
    /// Event names fired when the state is entered / left (delivered like
    /// clip events: `on_animation_event` + `AnimationEvent`). Empty = none.
    pub on_enter: String,
    pub on_exit: String,
}

impl Default for AnimState {
    fn default() -> Self {
        Self {
            name: "State".into(),
            motion: Motion::default(),
            speed: 1.0,
            speed_parameter: String::new(),
            looping: true,
            position: [0.0, 0.0],
            on_enter: String::new(),
            on_exit: String::new(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum CondOp {
    /// Bool / trigger is set.
    #[default]
    If,
    IfNot,
    Greater,
    Less,
    Equals,
    NotEquals,
}

impl CondOp {
    pub const ALL: [CondOp; 6] = [Self::If, Self::IfNot, Self::Greater, Self::Less, Self::Equals, Self::NotEquals];
    pub fn label(self) -> &'static str {
        match self {
            Self::If => "true",
            Self::IfNot => "false",
            Self::Greater => "Greater",
            Self::Less => "Less",
            Self::Equals => "Equals",
            Self::NotEquals => "Not Equal",
        }
    }
    pub fn uses_threshold(self) -> bool {
        !matches!(self, Self::If | Self::IfNot)
    }
    pub fn test(self, value: f32, threshold: f32) -> bool {
        match self {
            Self::If => value > 0.5,
            Self::IfNot => value <= 0.5,
            Self::Greater => value > threshold,
            Self::Less => value < threshold,
            Self::Equals => (value - threshold).abs() < 1e-4,
            Self::NotEquals => (value - threshold).abs() >= 1e-4,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct AnimCondition {
    pub parameter: String,
    pub op: CondOp,
    pub threshold: f32,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct AnimTransition {
    /// Source state name; None = Any State.
    pub from: Option<String>,
    pub to: String,
    /// All must hold.
    pub conditions: Vec<AnimCondition>,
    pub has_exit_time: bool,
    /// Normalized time of the source state at which the transition may fire.
    pub exit_time: f32,
    /// Cross-fade length in seconds.
    pub duration: f32,
    /// Normalized start time in the target state.
    pub offset: f32,
    /// Other transitions may cut in while this one is blending.
    pub interruptible: bool,
    /// Any State only: may re-enter the state already playing.
    pub can_transition_to_self: bool,
}

impl Default for AnimTransition {
    fn default() -> Self {
        Self {
            from: None,
            to: String::new(),
            conditions: Vec::new(),
            has_exit_time: false,
            exit_time: 1.0,
            duration: 0.25,
            offset: 0.0,
            interruptible: false,
            can_transition_to_self: false,
        }
    }
}

/// One layer's state machine.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct StateMachine {
    pub states: Vec<AnimState>,
    /// Entered on start (empty = the first state).
    pub default_state: String,
    pub transitions: Vec<AnimTransition>,
    pub entry_position: [f32; 2],
    pub any_state_position: [f32; 2],
}

impl Default for StateMachine {
    fn default() -> Self {
        Self {
            states: Vec::new(),
            default_state: String::new(),
            transitions: Vec::new(),
            entry_position: [-260.0, -60.0],
            any_state_position: [-260.0, 60.0],
        }
    }
}

impl StateMachine {
    pub fn state_index(&self, name: &str) -> Option<usize> {
        self.states.iter().position(|s| s.name == name)
    }

    pub fn default_index(&self) -> Option<usize> {
        self.state_index(&self.default_state).or(if self.states.is_empty() { None } else { Some(0) })
    }

    /// Rename a state and every reference to it.
    pub fn rename_state(&mut self, old: &str, new: &str) {
        for s in &mut self.states {
            if s.name == old {
                s.name = new.to_owned();
            }
        }
        if self.default_state == old {
            self.default_state = new.to_owned();
        }
        for t in &mut self.transitions {
            if t.from.as_deref() == Some(old) {
                t.from = Some(new.to_owned());
            }
            if t.to == old {
                t.to = new.to_owned();
            }
        }
    }

    /// Remove a state and the transitions touching it.
    pub fn remove_state(&mut self, index: usize) {
        if index >= self.states.len() {
            return;
        }
        let name = self.states.remove(index).name;
        self.transitions.retain(|t| t.to != name && t.from.as_deref() != Some(name.as_str()));
        if self.default_state == name {
            self.default_state = self.states.first().map(|s| s.name.clone()).unwrap_or_default();
        }
    }

    /// A state name not used yet, based on `base`.
    pub fn unique_name(&self, base: &str) -> String {
        if self.state_index(base).is_none() {
            return base.to_owned();
        }
        (1..).map(|n| format!("{base} {n}")).find(|n| self.state_index(n).is_none()).unwrap()
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct AnimLayer {
    pub name: String,
    /// 0..1; the base layer is always applied at full weight.
    pub weight: f32,
    pub blend: LayerBlend,
    /// Bone names (or object paths) this layer affects, each with its
    /// descendants. Empty = everything.
    pub mask: Vec<String>,
    pub machine: StateMachine,
}

impl Default for AnimLayer {
    fn default() -> Self {
        Self {
            name: "Base Layer".into(),
            weight: 1.0,
            blend: LayerBlend::Override,
            mask: Vec::new(),
            machine: StateMachine::default(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct AnimatorController {
    pub parameters: Vec<AnimParameter>,
    pub layers: Vec<AnimLayer>,
}

impl Default for AnimatorController {
    fn default() -> Self {
        Self { parameters: Vec::new(), layers: vec![AnimLayer::default()] }
    }
}

impl AnimatorController {
    pub fn parameter(&self, name: &str) -> Option<&AnimParameter> {
        self.parameters.iter().find(|p| p.name == name)
    }

    /// Rename a parameter and every condition / tree / speed reference to it.
    pub fn rename_parameter(&mut self, old: &str, new: &str) {
        for p in &mut self.parameters {
            if p.name == old {
                p.name = new.to_owned();
            }
        }
        let fix = |s: &mut String| {
            if s == old {
                *s = new.to_owned();
            }
        };
        for layer in &mut self.layers {
            for t in &mut layer.machine.transitions {
                for c in &mut t.conditions {
                    fix(&mut c.parameter);
                }
            }
            for s in &mut layer.machine.states {
                fix(&mut s.speed_parameter);
                if let Motion::BlendTree(tree) = &mut s.motion {
                    fix(&mut tree.parameter);
                    fix(&mut tree.parameter_y);
                }
            }
        }
    }
}

pub fn load_anim_controller(path: impl AsRef<Path>) -> Result<AnimatorController> {
    let path = path.as_ref();
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    ron::from_str(&text).with_context(|| format!("parsing animator controller {}", path.display()))
}

pub fn save_anim_controller(path: impl AsRef<Path>, ctrl: &AnimatorController) -> Result<()> {
    let path = path.as_ref();
    let text = ron::ser::to_string_pretty(ctrl, ron::ser::PrettyConfig::default())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use citrus_core::BlendMotion;

    #[test]
    fn round_trips_through_ron() {
        let mut c = AnimatorController::default();
        c.parameters.push(AnimParameter { name: "Speed".into(), kind: AnimParamKind::Float, default: 0.0 });
        c.parameters.push(AnimParameter { name: "Jump".into(), kind: AnimParamKind::Trigger, default: 0.0 });
        let m = &mut c.layers[0].machine;
        m.states.push(AnimState { name: "Idle".into(), motion: Motion::Clip("Idle".into()), ..Default::default() });
        m.states.push(AnimState {
            name: "Move".into(),
            motion: Motion::BlendTree(BlendTree {
                name: "Move".into(),
                parameter: "Speed".into(),
                parameter_y: String::new(),
                motions: vec![BlendMotion { clip: "Walk".into(), position: [1.0, 0.0] }],
            }),
            ..Default::default()
        });
        m.transitions.push(AnimTransition {
            from: Some("Idle".into()),
            to: "Move".into(),
            conditions: vec![AnimCondition { parameter: "Speed".into(), op: CondOp::Greater, threshold: 0.1 }],
            ..Default::default()
        });
        let text = ron::ser::to_string_pretty(&c, ron::ser::PrettyConfig::default()).unwrap();
        let back: AnimatorController = ron::from_str(&text).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn rename_and_remove_patch_references() {
        let mut m = StateMachine::default();
        m.states.push(AnimState { name: "A".into(), ..Default::default() });
        m.states.push(AnimState { name: "B".into(), ..Default::default() });
        m.default_state = "A".into();
        m.transitions.push(AnimTransition { from: Some("A".into()), to: "B".into(), ..Default::default() });
        m.rename_state("A", "Idle");
        assert_eq!(m.default_state, "Idle");
        assert_eq!(m.transitions[0].from.as_deref(), Some("Idle"));
        m.remove_state(1);
        assert!(m.transitions.is_empty());
    }
}
