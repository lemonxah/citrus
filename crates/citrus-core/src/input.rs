//! Input binding system (engine-agnostic). Defines abstract actions
//! (`Move`, `Jump`, `Look`, ...) and control schemes that map physical
//! inputs (keys, mouse, gamepad) to them, plus the per-frame [`InputState`]
//! snapshot a component reads through `ComponentCtx`.
//!
//! This crate is winit/gilrs-free, so physical inputs are mirrored as
//! plain enums ([`Key`], [`MouseButton`], [`PadButton`], [`PadAxis`]). The engine
//! translates real device events into [`RawInput`] and [`resolve`]s it against the
//! active [`ControlScheme`] into an [`InputState`].

use std::collections::HashMap;

use glam::Vec2;
use serde::{Deserialize, Serialize};

/// A physical keyboard key (layout-independent, mirrors winit `KeyCode`). Only
/// the commonly-bound keys are enumerated; anything else maps to [`Key::Unknown`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum Key {
    A, B, C, D, E, F, G, H, I, J, K, L, M,
    N, O, P, Q, R, S, T, U, V, W, X, Y, Z,
    Num0, Num1, Num2, Num3, Num4, Num5, Num6, Num7, Num8, Num9,
    F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12,
    Up, Down, Left, Right,
    Space, Enter, Escape, Tab, Backspace, Delete,
    LShift, RShift, LCtrl, RCtrl, LAlt, RAlt,
    Minus, Equals, LBracket, RBracket, Semicolon, Comma, Period, Slash, Backslash, Grave,
    Unknown,
}

/// A mouse button.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

/// A mouse analog axis (relative motion / wheel).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum MouseAxis {
    X,
    Y,
    Wheel,
}

/// A gamepad button (Xbox naming; mirrors `gilrs::Button`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum PadButton {
    South, East, West, North,
    LeftBumper, RightBumper,
    Select, Start,
    LeftThumb, RightThumb,
    DPadUp, DPadDown, DPadLeft, DPadRight,
}

/// A gamepad analog axis (sticks + triggers; mirrors `gilrs::Axis`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum PadAxis {
    LeftStickX,
    LeftStickY,
    RightStickX,
    RightStickY,
    LeftTrigger,
    RightTrigger,
}

/// One physical input that can drive an action.
#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
pub enum InputSource {
    Key(Key),
    Mouse(MouseButton),
    MouseAxis(MouseAxis),
    PadButton(PadButton),
    PadAxis(PadAxis),
}

impl InputSource {
    /// Short human label for the editor binding UI.
    pub fn label(&self) -> String {
        match self {
            InputSource::Key(k) => format!("{k:?}"),
            InputSource::Mouse(b) => format!("Mouse{b:?}"),
            InputSource::MouseAxis(a) => format!("Mouse{a:?}"),
            InputSource::PadButton(b) => format!("Pad{b:?}"),
            InputSource::PadAxis(a) => format!("Pad{a:?}"),
        }
    }
}

/// The shape of an action's value.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum ActionKind {
    /// On/off (Jump, Fire, Interact).
    Button,
    /// One-dimensional axis in [-1, 1] (Throttle).
    Axis1,
    /// Two-dimensional axis, each component in [-1, 1] (Move, Look).
    Axis2,
}

/// Binding of one named action to physical inputs. Composite fields let a 2D axis
/// be built from four buttons (WASD) and/or an analog stick.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActionBinding {
    pub name: String,
    pub kind: ActionKind,
    /// Button action: pressed if ANY of these are down.
    #[serde(default)]
    pub buttons: Vec<InputSource>,
    /// Axis1/Axis2 X: positive / negative digital contributions.
    #[serde(default)]
    pub pos_x: Vec<InputSource>,
    #[serde(default)]
    pub neg_x: Vec<InputSource>,
    /// Axis2 Y: positive / negative digital contributions.
    #[serde(default)]
    pub pos_y: Vec<InputSource>,
    #[serde(default)]
    pub neg_y: Vec<InputSource>,
    /// Analog axis for Axis1, or X for Axis2 (e.g. a stick / mouse axis).
    #[serde(default)]
    pub analog_x: Option<InputSource>,
    /// Analog Y for Axis2.
    #[serde(default)]
    pub analog_y: Option<InputSource>,
    /// Values below this magnitude (analog) read as zero.
    #[serde(default)]
    pub deadzone: f32,
    /// Output multiplier (also use negative to invert).
    #[serde(default = "one")]
    pub scale: f32,
}

fn one() -> f32 {
    1.0
}

impl ActionBinding {
    pub fn button(name: &str, inputs: impl IntoIterator<Item = InputSource>) -> Self {
        Self {
            name: name.to_string(),
            kind: ActionKind::Button,
            buttons: inputs.into_iter().collect(),
            ..Self::empty(name, ActionKind::Button)
        }
    }

    fn empty(name: &str, kind: ActionKind) -> Self {
        Self {
            name: name.to_string(),
            kind,
            buttons: Vec::new(),
            pos_x: Vec::new(),
            neg_x: Vec::new(),
            pos_y: Vec::new(),
            neg_y: Vec::new(),
            analog_x: None,
            analog_y: None,
            deadzone: 0.15,
            scale: 1.0,
        }
    }
}

/// A named set of bindings (e.g. "KB+Mouse", "Gamepad").
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ControlScheme {
    pub name: String,
    pub actions: Vec<ActionBinding>,
}

impl ControlScheme {
    pub fn action_mut(&mut self, name: &str) -> Option<&mut ActionBinding> {
        self.actions.iter_mut().find(|a| a.name == name)
    }
}

/// All control schemes for a project + the active one. Serialized to
/// `project.citrus` (so it survives reloads) and editable at runtime.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Bindings {
    pub schemes: Vec<ControlScheme>,
    #[serde(default)]
    pub active: usize,
}

impl Default for Bindings {
    fn default() -> Self {
        Self {
            schemes: vec![ControlScheme::default_kb_mouse(), ControlScheme::default_gamepad()],
            active: 0,
        }
    }
}

impl Bindings {
    pub fn active_scheme(&self) -> Option<&ControlScheme> {
        self.schemes.get(self.active)
    }
    pub fn active_scheme_mut(&mut self) -> Option<&mut ControlScheme> {
        self.schemes.get_mut(self.active)
    }
}

impl ControlScheme {
    /// The default keyboard + mouse scheme: WASD move, mouse look, common verbs.
    pub fn default_kb_mouse() -> Self {
        use InputSource::*;
        let mut move_act = ActionBinding::empty("Move", ActionKind::Axis2);
        move_act.pos_x = vec![Key(self::Key::D)];
        move_act.neg_x = vec![Key(self::Key::A)];
        move_act.pos_y = vec![Key(self::Key::W)];
        move_act.neg_y = vec![Key(self::Key::S)];
        move_act.deadzone = 0.0;

        let mut look = ActionBinding::empty("Look", ActionKind::Axis2);
        look.analog_x = Some(MouseAxis(self::MouseAxis::X));
        look.analog_y = Some(MouseAxis(self::MouseAxis::Y));
        look.deadzone = 0.0;

        Self {
            name: "KB+Mouse".to_string(),
            actions: vec![
                move_act,
                look,
                ActionBinding::button("Jump", [Key(self::Key::Space)]),
                ActionBinding::button("Sprint", [Key(self::Key::LShift)]),
                ActionBinding::button("Crouch", [Key(self::Key::LCtrl)]),
                ActionBinding::button("Fire", [Mouse(self::MouseButton::Left)]),
                ActionBinding::button("Interact", [Key(self::Key::E)]),
                ActionBinding::button("Grab", [Key(self::Key::F), Mouse(self::MouseButton::Right)]),
                ActionBinding::button("Voice", [Key(self::Key::V)]),
            ],
        }
    }

    /// The default gamepad scheme: left stick move, right stick look, A jump, etc.
    pub fn default_gamepad() -> Self {
        use InputSource::*;
        let mut move_act = ActionBinding::empty("Move", ActionKind::Axis2);
        move_act.analog_x = Some(PadAxis(self::PadAxis::LeftStickX));
        move_act.analog_y = Some(PadAxis(self::PadAxis::LeftStickY));

        let mut look = ActionBinding::empty("Look", ActionKind::Axis2);
        look.analog_x = Some(PadAxis(self::PadAxis::RightStickX));
        look.analog_y = Some(PadAxis(self::PadAxis::RightStickY));
        look.scale = 8.0;

        Self {
            name: "Gamepad".to_string(),
            actions: vec![
                move_act,
                look,
                ActionBinding::button("Jump", [PadButton(self::PadButton::South)]),
                ActionBinding::button("Sprint", [PadButton(self::PadButton::LeftThumb)]),
                ActionBinding::button("Crouch", [PadButton(self::PadButton::East)]),
                ActionBinding::button("Fire", [PadAxis(self::PadAxis::RightTrigger)]),
                ActionBinding::button("Interact", [PadButton(self::PadButton::North)]),
                ActionBinding::button("Grab", [PadButton(self::PadButton::West)]),
                ActionBinding::button("Voice", [PadButton(self::PadButton::LeftBumper)]),
            ],
        }
    }
}

impl PadAxis {
}

/// Raw per-frame device state the engine fills before resolving. Digital sets are
/// "currently down"; the engine also tracks edges for just-pressed/released.
#[derive(Default, Clone)]
pub struct RawInput {
    pub keys_down: std::collections::HashSet<Key>,
    pub mouse_down: std::collections::HashSet<MouseButton>,
    pub pad_down: std::collections::HashSet<PadButton>,
    pub pad_axes: HashMap<PadAxis, f32>,
    /// Mouse motion this frame (pixels).
    pub mouse_delta: Vec2,
    /// Mouse wheel this frame (lines/notches).
    pub wheel: f32,
}

impl RawInput {
    fn source_value(&self, src: &InputSource) -> f32 {
        match src {
            InputSource::Key(k) => self.keys_down.contains(k) as i32 as f32,
            InputSource::Mouse(b) => self.mouse_down.contains(b) as i32 as f32,
            InputSource::PadButton(b) => self.pad_down.contains(b) as i32 as f32,
            InputSource::MouseAxis(a) => match a {
                MouseAxis::X => self.mouse_delta.x,
                MouseAxis::Y => self.mouse_delta.y,
                MouseAxis::Wheel => self.wheel,
            },
            InputSource::PadAxis(a) => self.pad_axes.get(a).copied().unwrap_or(0.0),
        }
    }

    fn any_down(&self, srcs: &[InputSource]) -> bool {
        srcs.iter().any(|s| self.source_value(s).abs() > 0.5)
    }
}

/// The resolved value of one action this frame.
#[derive(Clone, Copy, Default)]
pub struct ActionValue {
    pub down: bool,
    pub just_pressed: bool,
    pub just_released: bool,
    pub value: f32,
    pub axis2: Vec2,
}

/// Per-frame snapshot of every action, read by components via `ComponentCtx`.
#[derive(Default, Clone)]
pub struct InputState {
    actions: HashMap<String, ActionValue>,
    /// Raw mouse delta this frame (for free look that bypasses a bound action).
    pub mouse_delta: Vec2,
}

impl InputState {
    fn get(&self, name: &str) -> ActionValue {
        self.actions.get(name).copied().unwrap_or_default()
    }
    /// True while a button action is held.
    pub fn down(&self, name: &str) -> bool {
        self.get(name).down
    }
    /// True the frame a button action was pressed.
    pub fn pressed(&self, name: &str) -> bool {
        self.get(name).just_pressed
    }
    /// True the frame a button action was released.
    pub fn released(&self, name: &str) -> bool {
        self.get(name).just_released
    }
    /// Scalar value of a 1D-axis action (or 0/1 for a button).
    pub fn axis(&self, name: &str) -> f32 {
        self.get(name).value
    }
    /// 2D value of an axis2 action.
    pub fn axis2(&self, name: &str) -> Vec2 {
        self.get(name).axis2
    }
}

fn apply_deadzone(v: f32, dz: f32) -> f32 {
    if v.abs() < dz {
        0.0
    } else {
        v
    }
}

/// Resolve raw device state against a control scheme into the action snapshot.
/// `prev` carries last frame's down-state so edges (just-pressed/released) work.
pub fn resolve(scheme: &ControlScheme, raw: &RawInput, prev: &InputState) -> InputState {
    let mut out = InputState {
        actions: HashMap::with_capacity(scheme.actions.len()),
        mouse_delta: raw.mouse_delta,
    };
    for a in &scheme.actions {
        let mut v = ActionValue::default();
        match a.kind {
            ActionKind::Button => {
                v.down = raw.any_down(&a.buttons);
                v.value = v.down as i32 as f32;
            }
            ActionKind::Axis1 => {
                let mut x = (raw.any_down(&a.pos_x) as i32 - raw.any_down(&a.neg_x) as i32) as f32;
                if let Some(an) = &a.analog_x {
                    let av = raw.source_value(an);
                    if av.abs() > x.abs() {
                        x = av;
                    }
                }
                v.value = apply_deadzone(x, a.deadzone) * a.scale;
                v.down = v.value.abs() > 0.5;
            }
            ActionKind::Axis2 => {
                let mut x = (raw.any_down(&a.pos_x) as i32 - raw.any_down(&a.neg_x) as i32) as f32;
                let mut y = (raw.any_down(&a.pos_y) as i32 - raw.any_down(&a.neg_y) as i32) as f32;
                if let Some(an) = &a.analog_x {
                    let av = raw.source_value(an);
                    if av.abs() > x.abs() {
                        x = av;
                    }
                }
                if let Some(an) = &a.analog_y {
                    let av = raw.source_value(an);
                    if av.abs() > y.abs() {
                        y = av;
                    }
                }
                let mut axis = Vec2::new(x, y);
                if axis.length() > 1.0 {
                    axis = axis.normalize();
                }
                if axis.length() < a.deadzone {
                    axis = Vec2::ZERO;
                }
                v.axis2 = axis * a.scale;
                v.value = v.axis2.length();
                v.down = v.value > 0.5;
            }
        }
        let was = prev.get(&a.name).down;
        v.just_pressed = v.down && !was;
        v.just_released = !v.down && was;
        out.actions.insert(a.name.clone(), v);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wasd_resolves_to_move_axis_and_jump_edges() {
        let scheme = ControlScheme::default_kb_mouse();
        let mut raw = RawInput::default();
        raw.keys_down.insert(Key::W);
        raw.keys_down.insert(Key::D);
        let s = resolve(&scheme, &raw, &InputState::default());
        let mv = s.axis2("Move");
        assert!(mv.x > 0.0 && mv.y > 0.0, "W+D should move +x/+y, got {mv:?}");
        assert!((mv.length() - 1.0).abs() < 1e-4, "diagonal move is normalized");

        // Jump edge detection across frames.
        let mut raw2 = RawInput::default();
        raw2.keys_down.insert(Key::Space);
        let s2 = resolve(&scheme, &raw2, &s);
        assert!(s2.pressed("Jump") && s2.down("Jump"), "Space → Jump just-pressed");
        let s3 = resolve(&scheme, &raw2, &s2);
        assert!(!s3.pressed("Jump") && s3.down("Jump"), "held Jump not re-pressed");
        let s4 = resolve(&scheme, &RawInput::default(), &s3);
        assert!(s4.released("Jump") && !s4.down("Jump"), "release Jump");
    }

    #[test]
    fn gamepad_stick_drives_move() {
        let scheme = ControlScheme::default_gamepad();
        let mut raw = RawInput::default();
        raw.pad_axes.insert(PadAxis::LeftStickX, 0.9);
        let s = resolve(&scheme, &raw, &InputState::default());
        assert!(s.axis2("Move").x > 0.5, "left stick X drives Move.x");
    }
}
