//! Animator Controller asset (`.animctrl`): typed parameters plus layers, each
//! a state machine of motion states joined by conditional transitions. Modelled
//! on Unity's Animator Controller (states, Any State, exit time, triggers,
//! layers with masks and additive blending), with Unreal-style state enter/exit
//! notifies so other systems can react. Evaluated by the engine's anim graph.

use std::path::Path;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use zestengine_core::BlendTree;

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
    /// Set = this is a sub-state machine node: it plays nothing itself, and
    /// `motion`, `speed`, `looping` and the events are ignored. See
    /// [`StateMachine::flatten`] for how it runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sub_machine: Option<Box<StateMachine>>,
}

impl AnimState {
    pub fn is_sub_machine(&self) -> bool {
        self.sub_machine.is_some()
    }
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
            sub_machine: None,
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

    /// The machine reached by following sub-machine state indices from here.
    pub fn nested(&self, path: &[usize]) -> Option<&StateMachine> {
        match path.split_first() {
            None => Some(self),
            Some((&i, rest)) => self.states.get(i)?.sub_machine.as_deref()?.nested(rest),
        }
    }

    pub fn nested_mut(&mut self, path: &[usize]) -> Option<&mut StateMachine> {
        match path.split_first() {
            None => Some(self),
            Some((&i, rest)) => self.states.get_mut(i)?.sub_machine.as_deref_mut()?.nested_mut(rest),
        }
    }

    /// This machine and every nested one, depth first.
    pub fn for_each_machine_mut(&mut self, f: &mut impl FnMut(&mut StateMachine)) {
        f(self);
        for s in &mut self.states {
            if let Some(sub) = s.sub_machine.as_deref_mut() {
                sub.for_each_machine_mut(f);
            }
        }
    }

    /// Whether any state is a sub-state machine.
    pub fn has_sub_machines(&self) -> bool {
        self.states.iter().any(|s| s.sub_machine.is_some())
    }

    /// The equivalent machine without sub-state machines, which is what the
    /// runtime plays. Every leaf state is named by its path, `Outer/Inner`
    /// (recursively, `A/B/C`); top-level states keep their names, so a
    /// machine without sub-machines flattens to itself. Within each machine
    /// references stay local (a sub-machine's transitions name its own states;
    /// the parent names the sub-machine node), so renames never cross levels.
    ///
    /// - Entering a sub-machine node (default state or transition target)
    ///   enters its default state, recursively.
    /// - A transition whose source is a sub-machine node applies from every
    ///   leaf inside it.
    /// - A sub-machine's Any State transitions apply from every leaf inside
    ///   that sub-machine (not from outside it); the top level's stay global.
    /// - Check order for a leaf: top-level Any State, then outer levels'
    ///   transitions before inner ones, and within a sub-machine its Any
    ///   State before the leaf's own transitions.
    /// - Transitions whose target cannot be resolved (missing or empty
    ///   sub-machine) are dropped.
    pub fn flatten(&self) -> StateMachine {
        if !self.has_sub_machines() {
            return self.clone();
        }
        let mut out = StateMachine {
            states: Vec::new(),
            default_state: self.entry_leaf("").unwrap_or_default(),
            transitions: Vec::new(),
            entry_position: self.entry_position,
            any_state_position: self.any_state_position,
        };
        self.flatten_into("", true, &mut out);
        out
    }

    /// Full name of the leaf entered when this machine (named by `prefix`) starts.
    fn entry_leaf(&self, prefix: &str) -> Option<String> {
        self.enter(prefix, &self.states.get(self.default_index()?)?.name)
    }

    /// Full name of the leaf entered by targeting local state `name`.
    fn enter(&self, prefix: &str, name: &str) -> Option<String> {
        let s = &self.states[self.state_index(name)?];
        let full = format!("{prefix}{}", s.name);
        match s.sub_machine.as_deref() {
            None => Some(full),
            Some(sub) => sub.entry_leaf(&format!("{full}/")),
        }
    }

    /// Full names of every leaf in this machine.
    fn leaves(&self, prefix: &str, out: &mut Vec<String>) {
        for s in &self.states {
            let full = format!("{prefix}{}", s.name);
            match s.sub_machine.as_deref() {
                None => out.push(full),
                Some(sub) => sub.leaves(&format!("{full}/"), out),
            }
        }
    }

    fn flatten_into(&self, prefix: &str, top: bool, out: &mut StateMachine) {
        for s in &self.states {
            if s.sub_machine.is_none() {
                out.states.push(AnimState { name: format!("{prefix}{}", s.name), ..s.clone() });
            }
        }
        // Sub-machine Any State first, then the rest in order.
        let any_first = self.transitions.iter().filter(|t| t.from.is_none() && !top);
        let rest = self.transitions.iter().filter(|t| t.from.is_some() || top);
        for t in any_first.chain(rest) {
            let Some(to) = self.enter(prefix, &t.to) else {
                continue;
            };
            let sources: Option<Vec<String>> = match &t.from {
                None if top => None,
                None => {
                    let mut v = Vec::new();
                    self.leaves(prefix, &mut v);
                    // An Any State transition only re-enters its target when allowed.
                    v.retain(|l| t.can_transition_to_self || *l != to);
                    Some(v)
                }
                Some(f) => {
                    let Some(s) = self.state_index(f).map(|i| &self.states[i]) else {
                        continue;
                    };
                    let full = format!("{prefix}{}", s.name);
                    let mut v = Vec::new();
                    match s.sub_machine.as_deref() {
                        None => v.push(full),
                        Some(sub) => sub.leaves(&format!("{full}/"), &mut v),
                    }
                    Some(v)
                }
            };
            match sources {
                None => out.transitions.push(AnimTransition { to, ..t.clone() }),
                Some(v) => out.transitions.extend(v.into_iter().map(|from| AnimTransition {
                    from: Some(from),
                    to: to.clone(),
                    ..t.clone()
                })),
            }
        }
        for s in &self.states {
            if let Some(sub) = s.sub_machine.as_deref() {
                sub.flatten_into(&format!("{prefix}{}/", s.name), false, out);
            }
        }
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
            layer.machine.for_each_machine_mut(&mut |m| {
                for t in &mut m.transitions {
                    for c in &mut t.conditions {
                        fix(&mut c.parameter);
                    }
                }
                for s in &mut m.states {
                    fix(&mut s.speed_parameter);
                    if let Motion::BlendTree(tree) = &mut s.motion {
                        fix(&mut tree.parameter);
                        fix(&mut tree.parameter_y);
                    }
                }
            });
        }
    }

    /// Every layer's machine flattened ([`StateMachine::flatten`]): what the
    /// runtime evaluates. Parameters are shared by all levels.
    pub fn flattened(&self) -> AnimatorController {
        let mut c = self.clone();
        for l in &mut c.layers {
            l.machine = l.machine.flatten();
        }
        c
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
    use zestengine_core::BlendMotion;

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

    fn leaf(name: &str) -> AnimState {
        AnimState { name: name.into(), ..Default::default() }
    }

    fn sub(name: &str, machine: StateMachine) -> AnimState {
        AnimState { name: name.into(), sub_machine: Some(Box::new(machine)), ..Default::default() }
    }

    fn tr(from: Option<&str>, to: &str) -> AnimTransition {
        AnimTransition { from: from.map(Into::into), to: to.into(), has_exit_time: true, ..Default::default() }
    }

    fn names(m: &StateMachine) -> Vec<&str> {
        m.states.iter().map(|s| s.name.as_str()).collect()
    }

    fn pairs(m: &StateMachine) -> Vec<(Option<&str>, &str)> {
        m.transitions.iter().map(|t| (t.from.as_deref(), t.to.as_str())).collect()
    }

    /// Idle -> Locomotion{Walk (default), Run}; Locomotion -> Idle.
    fn locomotion() -> StateMachine {
        StateMachine {
            states: vec![
                leaf("Idle"),
                sub(
                    "Locomotion",
                    StateMachine {
                        states: vec![leaf("Walk"), leaf("Run")],
                        default_state: "Walk".into(),
                        transitions: vec![tr(Some("Walk"), "Run"), tr(None, "Walk")],
                        ..Default::default()
                    },
                ),
            ],
            default_state: "Idle".into(),
            transitions: vec![tr(Some("Idle"), "Locomotion"), tr(Some("Locomotion"), "Idle"), tr(None, "Idle")],
            ..Default::default()
        }
    }

    #[test]
    fn flatten_without_sub_machines_is_identity() {
        let mut m = StateMachine::default();
        m.states.push(leaf("A"));
        m.transitions.push(tr(None, "A"));
        assert_eq!(m.flatten(), m);
    }

    #[test]
    fn flatten_enters_default_and_exits_from_every_inner_state() {
        let f = locomotion().flatten();
        assert_eq!(names(&f), ["Idle", "Locomotion/Walk", "Locomotion/Run"]);
        assert_eq!(f.default_state, "Idle");
        assert_eq!(
            pairs(&f),
            [
                // Into the sub-machine = into its default state.
                (Some("Idle"), "Locomotion/Walk"),
                // Out of the sub-machine = from each inner state.
                (Some("Locomotion/Walk"), "Idle"),
                (Some("Locomotion/Run"), "Idle"),
                // Top-level Any State stays global.
                (None, "Idle"),
                // Inner Any State: from every inner state but its target, first.
                (Some("Locomotion/Run"), "Locomotion/Walk"),
                (Some("Locomotion/Walk"), "Locomotion/Run"),
            ]
        );
    }

    #[test]
    fn flatten_nested_two_levels() {
        let inner = StateMachine {
            states: vec![leaf("Light"), leaf("Heavy")],
            default_state: "Heavy".into(),
            ..Default::default()
        };
        let middle = StateMachine {
            states: vec![leaf("Draw"), sub("Swing", inner)],
            default_state: "Swing".into(),
            transitions: vec![tr(Some("Swing"), "Draw")],
            ..Default::default()
        };
        let top = StateMachine {
            states: vec![leaf("Idle"), sub("Attack", middle)],
            default_state: "Attack".into(),
            transitions: vec![tr(Some("Idle"), "Attack"), tr(Some("Attack"), "Idle")],
            ..Default::default()
        };
        let f = top.flatten();
        assert_eq!(names(&f), ["Idle", "Attack/Draw", "Attack/Swing/Light", "Attack/Swing/Heavy"]);
        // Default resolves through both levels.
        assert_eq!(f.default_state, "Attack/Swing/Heavy");
        assert_eq!(
            pairs(&f),
            [
                (Some("Idle"), "Attack/Swing/Heavy"),
                (Some("Attack/Draw"), "Idle"),
                (Some("Attack/Swing/Light"), "Idle"),
                (Some("Attack/Swing/Heavy"), "Idle"),
                (Some("Attack/Swing/Light"), "Attack/Draw"),
                (Some("Attack/Swing/Heavy"), "Attack/Draw"),
            ]
        );
    }

    #[test]
    fn flatten_follows_renames_and_drops_dangling_targets() {
        let mut m = locomotion();
        m.rename_state("Locomotion", "Move");
        m.nested_mut(&[1]).unwrap().rename_state("Walk", "Jog");
        let f = m.flatten();
        assert_eq!(names(&f), ["Idle", "Move/Jog", "Move/Run"]);
        assert!(pairs(&f).contains(&(Some("Idle"), "Move/Jog")));
        assert!(pairs(&f).contains(&(Some("Move/Run"), "Idle")));
        // Removing the inner default falls back to the first inner state.
        m.nested_mut(&[1]).unwrap().remove_state(0);
        assert!(pairs(&m.flatten()).contains(&(Some("Idle"), "Move/Run")));
        // An empty sub-machine cannot be entered: transitions into it vanish.
        m.nested_mut(&[1]).unwrap().remove_state(0);
        assert!(!pairs(&m.flatten()).iter().any(|(f, _)| *f == Some("Idle")));
    }

    #[test]
    fn sub_machines_round_trip_and_old_files_load() {
        let mut c = AnimatorController::default();
        c.layers[0].machine = locomotion();
        let text = ron::ser::to_string_pretty(&c, ron::ser::PrettyConfig::default()).unwrap();
        assert_eq!(ron::from_str::<AnimatorController>(&text).unwrap(), c);
        // A plain state writes no sub_machine field, so old files are unchanged.
        let plain = ron::to_string(&leaf("Idle")).unwrap();
        assert!(!plain.contains("sub_machine"));
        assert_eq!(ron::from_str::<AnimState>(&plain).unwrap(), leaf("Idle"));
    }

    #[test]
    fn rename_parameter_reaches_nested_machines() {
        let mut c = AnimatorController::default();
        let mut m = locomotion();
        m.nested_mut(&[1]).unwrap().transitions[0].conditions.push(AnimCondition { parameter: "Speed".into(), ..Default::default() });
        c.layers[0].machine = m;
        c.rename_parameter("Speed", "Velocity");
        assert_eq!(c.layers[0].machine.nested(&[1]).unwrap().transitions[0].conditions[0].parameter, "Velocity");
    }
}
