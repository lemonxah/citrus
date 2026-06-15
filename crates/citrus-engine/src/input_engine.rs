//! Engine-side input capture for the binding system (2C). Translates winit
//! keyboard/mouse events and `gilrs` gamepad state into a [`RawInput`], resolves
//! it against the active control scheme, and exposes the per-frame
//! [`InputState`] that components read through `ComponentCtx`.

use citrus_core::{
    Bindings, ControlScheme, InputState, Key, MouseButton, PadAxis, PadButton, RawInput, resolve,
};
use winit::event::MouseButton as WMouse;
use winit::keyboard::KeyCode;

/// Owns gamepad polling + accumulated raw input + the resolved snapshot.
pub struct InputManager {
    gilrs: Option<gilrs::Gilrs>,
    pub bindings: Bindings,
    raw: RawInput,
    state: InputState,
    /// Empty scheme used when bindings have none (keeps resolve total).
    fallback: ControlScheme,
}

impl Default for InputManager {
    fn default() -> Self {
        Self::new(Bindings::default())
    }
}

impl InputManager {
    pub fn new(bindings: Bindings) -> Self {
        let gilrs = match gilrs::Gilrs::new() {
            Ok(g) => Some(g),
            Err(e) => {
                tracing::warn!("gamepad init failed (gamepads disabled): {e}");
                None
            }
        };
        Self {
            gilrs,
            bindings,
            raw: RawInput::default(),
            state: InputState::default(),
            fallback: ControlScheme {
                name: "none".into(),
                actions: Vec::new(),
            },
        }
    }

    /// The resolved action snapshot for the current frame.
    pub fn state(&self) -> &InputState {
        &self.state
    }

    // --- winit event feed (accumulated through the frame) ---

    pub fn key(&mut self, code: KeyCode, pressed: bool) {
        let k = map_key(code);
        if pressed {
            self.raw.keys_down.insert(k);
        } else {
            self.raw.keys_down.remove(&k);
        }
    }

    pub fn mouse_button(&mut self, button: WMouse, pressed: bool) {
        let Some(b) = map_mouse(button) else { return };
        if pressed {
            self.raw.mouse_down.insert(b);
        } else {
            self.raw.mouse_down.remove(&b);
        }
    }

    pub fn mouse_motion(&mut self, dx: f64, dy: f64) {
        self.raw.mouse_delta.x += dx as f32;
        self.raw.mouse_delta.y += dy as f32;
    }

    pub fn wheel(&mut self, dy: f32) {
        self.raw.wheel += dy;
    }

    /// Clear keys/buttons (e.g. on focus loss) so nothing sticks "down".
    pub fn clear_held(&mut self) {
        self.raw.keys_down.clear();
        self.raw.mouse_down.clear();
    }

    /// Poll gamepads and resolve the accumulated raw input into the snapshot.
    /// Call once per frame before driving components. Returns the new snapshot.
    pub fn resolve_frame(&mut self) -> &InputState {
        self.poll_gamepads();
        let scheme = self.bindings.active_scheme().unwrap_or(&self.fallback);
        self.state = resolve(scheme, &self.raw, &self.state);
        // Per-frame deltas reset after resolving; held sets persist.
        self.raw.mouse_delta = glam::Vec2::ZERO;
        self.raw.wheel = 0.0;
        &self.state
    }

    fn poll_gamepads(&mut self) {
        let Some(gilrs) = self.gilrs.as_mut() else {
            return;
        };
        // Pump the event queue so cached gamepad state is current.
        while gilrs.next_event().is_some() {}
        self.raw.pad_down.clear();
        self.raw.pad_axes.clear();
        // Merge all connected pads (local co-op shares one snapshot for now).
        for (_id, pad) in gilrs.gamepads() {
            for (gb, pb) in PAD_BUTTONS {
                if pad.is_pressed(gb) {
                    self.raw.pad_down.insert(pb);
                }
            }
            for (ga, pa) in PAD_AXES {
                let v = pad.value(ga);
                if v != 0.0 {
                    *self.raw.pad_axes.entry(pa).or_insert(0.0) += v;
                }
            }
        }
    }
}

const PAD_BUTTONS: [(gilrs::Button, PadButton); 14] = [
    (gilrs::Button::South, PadButton::South),
    (gilrs::Button::East, PadButton::East),
    (gilrs::Button::West, PadButton::West),
    (gilrs::Button::North, PadButton::North),
    (gilrs::Button::LeftTrigger, PadButton::LeftBumper),
    (gilrs::Button::RightTrigger, PadButton::RightBumper),
    (gilrs::Button::Select, PadButton::Select),
    (gilrs::Button::Start, PadButton::Start),
    (gilrs::Button::LeftThumb, PadButton::LeftThumb),
    (gilrs::Button::RightThumb, PadButton::RightThumb),
    (gilrs::Button::DPadUp, PadButton::DPadUp),
    (gilrs::Button::DPadDown, PadButton::DPadDown),
    (gilrs::Button::DPadLeft, PadButton::DPadLeft),
    (gilrs::Button::DPadRight, PadButton::DPadRight),
];

const PAD_AXES: [(gilrs::Axis, PadAxis); 6] = [
    (gilrs::Axis::LeftStickX, PadAxis::LeftStickX),
    (gilrs::Axis::LeftStickY, PadAxis::LeftStickY),
    (gilrs::Axis::RightStickX, PadAxis::RightStickX),
    (gilrs::Axis::RightStickY, PadAxis::RightStickY),
    (gilrs::Axis::LeftZ, PadAxis::LeftTrigger),
    (gilrs::Axis::RightZ, PadAxis::RightTrigger),
];

pub fn map_mouse(b: WMouse) -> Option<MouseButton> {
    match b {
        WMouse::Left => Some(MouseButton::Left),
        WMouse::Right => Some(MouseButton::Right),
        WMouse::Middle => Some(MouseButton::Middle),
        _ => None,
    }
}

pub fn map_key(code: KeyCode) -> Key {
    use KeyCode as C;
    match code {
        C::KeyA => Key::A, C::KeyB => Key::B, C::KeyC => Key::C, C::KeyD => Key::D,
        C::KeyE => Key::E, C::KeyF => Key::F, C::KeyG => Key::G, C::KeyH => Key::H,
        C::KeyI => Key::I, C::KeyJ => Key::J, C::KeyK => Key::K, C::KeyL => Key::L,
        C::KeyM => Key::M, C::KeyN => Key::N, C::KeyO => Key::O, C::KeyP => Key::P,
        C::KeyQ => Key::Q, C::KeyR => Key::R, C::KeyS => Key::S, C::KeyT => Key::T,
        C::KeyU => Key::U, C::KeyV => Key::V, C::KeyW => Key::W, C::KeyX => Key::X,
        C::KeyY => Key::Y, C::KeyZ => Key::Z,
        C::Digit0 => Key::Num0, C::Digit1 => Key::Num1, C::Digit2 => Key::Num2,
        C::Digit3 => Key::Num3, C::Digit4 => Key::Num4, C::Digit5 => Key::Num5,
        C::Digit6 => Key::Num6, C::Digit7 => Key::Num7, C::Digit8 => Key::Num8,
        C::Digit9 => Key::Num9,
        C::F1 => Key::F1, C::F2 => Key::F2, C::F3 => Key::F3, C::F4 => Key::F4,
        C::F5 => Key::F5, C::F6 => Key::F6, C::F7 => Key::F7, C::F8 => Key::F8,
        C::F9 => Key::F9, C::F10 => Key::F10, C::F11 => Key::F11, C::F12 => Key::F12,
        C::ArrowUp => Key::Up, C::ArrowDown => Key::Down,
        C::ArrowLeft => Key::Left, C::ArrowRight => Key::Right,
        C::Space => Key::Space, C::Enter => Key::Enter, C::Escape => Key::Escape,
        C::Tab => Key::Tab, C::Backspace => Key::Backspace, C::Delete => Key::Delete,
        C::ShiftLeft => Key::LShift, C::ShiftRight => Key::RShift,
        C::ControlLeft => Key::LCtrl, C::ControlRight => Key::RCtrl,
        C::AltLeft => Key::LAlt, C::AltRight => Key::RAlt,
        C::Minus => Key::Minus, C::Equal => Key::Equals,
        C::BracketLeft => Key::LBracket, C::BracketRight => Key::RBracket,
        C::Semicolon => Key::Semicolon, C::Comma => Key::Comma,
        C::Period => Key::Period, C::Slash => Key::Slash,
        C::Backslash => Key::Backslash, C::Backquote => Key::Grave,
        _ => Key::Unknown,
    }
}
