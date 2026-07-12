use std::time::Duration;

use reis::ei;
use reis::event as reis_event;

use shared::client_datagram::Modifiers;

use crate::logging::log;

use super::eis::timestamp_us;

// ---------------------------------------------------------------------------
// Linux input event key codes (linux/input-event-codes.h)
// ---------------------------------------------------------------------------

pub const KEY_ESC: u32 = 1;
pub const KEY_1: u32 = 2;
pub const KEY_2: u32 = 3;
pub const KEY_3: u32 = 4;
pub const KEY_4: u32 = 5;
pub const KEY_5: u32 = 6;
pub const KEY_6: u32 = 7;
pub const KEY_7: u32 = 8;
pub const KEY_8: u32 = 9;
pub const KEY_9: u32 = 10;
pub const KEY_0: u32 = 11;
pub const KEY_MINUS: u32 = 12;
pub const KEY_EQUAL: u32 = 13;
pub const KEY_BACKSPACE: u32 = 14;
pub const KEY_TAB: u32 = 15;
pub const KEY_Q: u32 = 16;
pub const KEY_W: u32 = 17;
pub const KEY_E: u32 = 18;
pub const KEY_R: u32 = 19;
pub const KEY_T: u32 = 20;
pub const KEY_Y: u32 = 21;
pub const KEY_U: u32 = 22;
pub const KEY_I: u32 = 23;
pub const KEY_O: u32 = 24;
pub const KEY_P: u32 = 25;
pub const KEY_LEFTBRACE: u32 = 26;
pub const KEY_BACKSLASH: u32 = 27;
pub const KEY_ENTER: u32 = 28;
pub const KEY_LEFTCTRL: u32 = 29;
pub const KEY_A: u32 = 30;
pub const KEY_S: u32 = 31;
pub const KEY_D: u32 = 32;
pub const KEY_F: u32 = 33;
pub const KEY_G: u32 = 34;
pub const KEY_H: u32 = 35;
pub const KEY_J: u32 = 36;
pub const KEY_K: u32 = 37;
pub const KEY_L: u32 = 38;
pub const KEY_SEMICOLON: u32 = 39;
pub const KEY_APOSTROPHE: u32 = 40;
pub const KEY_GRAVE: u32 = 41;
pub const KEY_LEFTSHIFT: u32 = 42;
pub const KEY_BACKSLASH2: u32 = 43;
pub const KEY_Z: u32 = 44;
pub const KEY_X: u32 = 45;
pub const KEY_C: u32 = 46;
pub const KEY_V: u32 = 47;
pub const KEY_B: u32 = 48;
pub const KEY_N: u32 = 49;
pub const KEY_M: u32 = 50;
pub const KEY_COMMA: u32 = 51;
pub const KEY_DOT: u32 = 52;
pub const KEY_SLASH: u32 = 53;
#[allow(dead_code)]
pub const KEY_RIGHTSHIFT: u32 = 54;
pub const KEY_LEFTALT: u32 = 56;
pub const KEY_SPACE: u32 = 57;
pub const KEY_CAPSLOCK: u32 = 58;
pub const KEY_F1: u32 = 59;
pub const KEY_F2: u32 = 60;
pub const KEY_F3: u32 = 61;
pub const KEY_F4: u32 = 62;
pub const KEY_F5: u32 = 63;
pub const KEY_F6: u32 = 64;
pub const KEY_F7: u32 = 65;
pub const KEY_F8: u32 = 66;
pub const KEY_F9: u32 = 67;
pub const KEY_F10: u32 = 68;
pub const KEY_NUMLOCK: u32 = 69;
pub const KEY_SCROLLLOCK: u32 = 70;
pub const KEY_F11: u32 = 87;
pub const KEY_F12: u32 = 88;
#[allow(dead_code)]
pub const KEY_RIGHTCTRL: u32 = 97;
pub const KEY_SYSRQ: u32 = 99;
#[allow(dead_code)]
pub const KEY_RIGHTALT: u32 = 100;
pub const KEY_HOME: u32 = 102;
pub const KEY_UP: u32 = 103;
pub const KEY_PAGEUP: u32 = 104;
pub const KEY_LEFT: u32 = 105;
pub const KEY_RIGHT: u32 = 106;
pub const KEY_END: u32 = 107;
pub const KEY_DOWN: u32 = 108;
pub const KEY_PAGEDOWN: u32 = 109;
pub const KEY_INSERT: u32 = 110;
pub const KEY_DELETE: u32 = 111;
pub const KEY_PAUSE: u32 = 119;
pub const KEY_LEFTMETA: u32 = 125;
#[allow(dead_code)]
pub const KEY_RIGHTMETA: u32 = 126;

// ---------------------------------------------------------------------------
// Numpad
// ---------------------------------------------------------------------------

pub const KEY_KP0: u32 = 82;
pub const KEY_KP1: u32 = 79;
pub const KEY_KP2: u32 = 80;
pub const KEY_KP3: u32 = 81;
pub const KEY_KP4: u32 = 75;
pub const KEY_KP5: u32 = 76;
pub const KEY_KP6: u32 = 77;
pub const KEY_KP7: u32 = 71;
pub const KEY_KP8: u32 = 72;
pub const KEY_KP9: u32 = 73;
pub const KEY_KPPLUS: u32 = 78;
pub const KEY_KPMINUS: u32 = 74;
pub const KEY_KPSLASH: u32 = 98;
pub const KEY_KPASTERISK: u32 = 55;
pub const KEY_KPENTER: u32 = 96;
pub const KEY_KPDOT: u32 = 83;

// ---------------------------------------------------------------------------
// Modifier helpers
// ---------------------------------------------------------------------------

/// Return the (left) modifier keycodes for the given modifier flags,
/// in a stable order: Ctrl, Shift, Alt, Meta.
fn modifier_keycodes(mods: Modifiers) -> Vec<u32> {
    let mut v = Vec::new();
    if mods.contains(Modifiers::CTRL) {
        v.push(KEY_LEFTCTRL);
    }
    if mods.contains(Modifiers::SHIFT) {
        v.push(KEY_LEFTSHIFT);
    }
    if mods.contains(Modifiers::ALT) {
        v.push(KEY_LEFTALT);
    }
    if mods.contains(Modifiers::META) {
        v.push(KEY_LEFTMETA);
    }
    v
}

// ---------------------------------------------------------------------------
// Modifier-state tracker
// ---------------------------------------------------------------------------

/// Tracks which modifier keys are logically held and only emits press/release
/// events when the state actually changes.  This avoids the problem where
/// pressing Alt+Shift immediately releases Alt before Shift arrives.
pub struct ModifierState {
    current: Modifiers,
}

impl ModifierState {
    pub fn new() -> Self {
        Self {
            current: Modifiers::empty(),
        }
    }

    /// Synchronise modifier state to `new_mods` and return the press/release
    /// events that need to be sent **before** the actual key event.
    pub fn sync(&mut self, new_mods: Modifiers) -> Vec<EisKeyboardEvent> {
        let added = new_mods & !self.current;
        let removed = self.current & !new_mods;
        self.current = new_mods;

        let mut events = Vec::with_capacity(8);

        // Release removed modifiers (reverse order so that e.g. Ctrl is
        // released before Shift if both were held).
        for &m in modifier_keycodes(removed).iter().rev() {
            events.push(EisKeyboardEvent {
                key: m,
                press: false,
            });
        }

        // Press newly-added modifiers.
        for &m in &modifier_keycodes(added) {
            events.push(EisKeyboardEvent {
                key: m,
                press: true,
            });
        }

        events
    }

    /// Release every modifier that is currently held and reset state.
    pub fn release_all(&mut self) -> Vec<EisKeyboardEvent> {
        let held = self.current;
        self.current = Modifiers::empty();
        modifier_keycodes(held)
            .into_iter()
            .rev()
            .map(|key| EisKeyboardEvent { key, press: false })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Key event type (used by the EIS thread)
// ---------------------------------------------------------------------------

pub struct EisKeyboardEvent {
    pub key: u32,
    pub press: bool,
}

/// If the keycode is not a modifier and is recognised, return the Linux
/// keycode.  Modifier key state is managed by [`ModifierState`] and should
/// not be sent as standalone key events.
pub fn js_keycode_to_eis_key(code: &str) -> Option<u32> {
    Some(match code {
        "KeyA" => KEY_A,
        "KeyB" => KEY_B,
        "KeyC" => KEY_C,
        "KeyD" => KEY_D,
        "KeyE" => KEY_E,
        "KeyF" => KEY_F,
        "KeyG" => KEY_G,
        "KeyH" => KEY_H,
        "KeyI" => KEY_I,
        "KeyJ" => KEY_J,
        "KeyK" => KEY_K,
        "KeyL" => KEY_L,
        "KeyM" => KEY_M,
        "KeyN" => KEY_N,
        "KeyO" => KEY_O,
        "KeyP" => KEY_P,
        "KeyQ" => KEY_Q,
        "KeyR" => KEY_R,
        "KeyS" => KEY_S,
        "KeyT" => KEY_T,
        "KeyU" => KEY_U,
        "KeyV" => KEY_V,
        "KeyW" => KEY_W,
        "KeyX" => KEY_X,
        "KeyY" => KEY_Y,
        "KeyZ" => KEY_Z,
        "Digit0" => KEY_0,
        "Digit1" => KEY_1,
        "Digit2" => KEY_2,
        "Digit3" => KEY_3,
        "Digit4" => KEY_4,
        "Digit5" => KEY_5,
        "Digit6" => KEY_6,
        "Digit7" => KEY_7,
        "Digit8" => KEY_8,
        "Digit9" => KEY_9,
        "Enter" => KEY_ENTER,
        "Escape" => KEY_ESC,
        "Backspace" => KEY_BACKSPACE,
        "Tab" => KEY_TAB,
        "Space" => KEY_SPACE,
        "Delete" => KEY_DELETE,
        "Insert" => KEY_INSERT,
        "Home" => KEY_HOME,
        "End" => KEY_END,
        "PageUp" => KEY_PAGEUP,
        "PageDown" => KEY_PAGEDOWN,
        "ArrowLeft" => KEY_LEFT,
        "ArrowUp" => KEY_UP,
        "ArrowRight" => KEY_RIGHT,
        "ArrowDown" => KEY_DOWN,
        "F1" => KEY_F1,
        "F2" => KEY_F2,
        "F3" => KEY_F3,
        "F4" => KEY_F4,
        "F5" => KEY_F5,
        "F6" => KEY_F6,
        "F7" => KEY_F7,
        "F8" => KEY_F8,
        "F9" => KEY_F9,
        "F10" => KEY_F10,
        "F11" => KEY_F11,
        "F12" => KEY_F12,
        "Semicolon" => KEY_SEMICOLON,
        "Equal" => KEY_EQUAL,
        "Comma" => KEY_COMMA,
        "Minus" => KEY_MINUS,
        "Period" => KEY_DOT,
        "Slash" => KEY_SLASH,
        "Backquote" => KEY_GRAVE,
        "BracketLeft" => KEY_LEFTBRACE,
        "Backslash" => KEY_BACKSLASH,
        "BracketRight" => KEY_BACKSLASH2,
        "Quote" => KEY_APOSTROPHE,
        "CapsLock" => KEY_CAPSLOCK,
        "NumLock" => KEY_NUMLOCK,
        "ScrollLock" => KEY_SCROLLLOCK,
        "PrintScreen" => KEY_SYSRQ,
        "Pause" => KEY_PAUSE,
        // Numpad
        "Numpad0" => KEY_KP0,
        "Numpad1" => KEY_KP1,
        "Numpad2" => KEY_KP2,
        "Numpad3" => KEY_KP3,
        "Numpad4" => KEY_KP4,
        "Numpad5" => KEY_KP5,
        "Numpad6" => KEY_KP6,
        "Numpad7" => KEY_KP7,
        "Numpad8" => KEY_KP8,
        "Numpad9" => KEY_KP9,
        "NumpadAdd" => KEY_KPPLUS,
        "NumpadSubtract" => KEY_KPMINUS,
        "NumpadDivide" => KEY_KPSLASH,
        "NumpadMultiply" => KEY_KPASTERISK,
        "NumpadEnter" => KEY_KPENTER,
        "NumpadDecimal" => KEY_KPDOT,
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// EIS keyboard sender
// ---------------------------------------------------------------------------

pub fn send_keyboard_key(
    connection: &reis_event::Connection,
    device: &reis_event::Device,
    keyboard: &ei::Keyboard,
    sequence: &mut u32,
    key: u32,
    press: bool,
) {
    let serial = connection.serial();
    *sequence = sequence.wrapping_add(1);

    device.device().start_emulating(serial, *sequence);
    let state = if press {
        ei::keyboard::KeyState::Press
    } else {
        ei::keyboard::KeyState::Released
    };
    keyboard.key(key, state);
    device.device().frame(serial, timestamp_us());
    device.device().stop_emulating(serial);

    if let Err(e) = connection.flush() {
        crate::logging::log(format!("EIS: keyboard flush error: {e}"));
    }
}

// ---------------------------------------------------------------------------
// Keyboard state — owns modifier tracking and timeout
// ---------------------------------------------------------------------------

/// Modifier keycodes that the browser sends but should not be forwarded as
/// standalone key events (modifier state is managed by [`ModifierState`]).
fn is_modifier_code(code: &str) -> bool {
    matches!(
        code,
        "ShiftLeft" | "ShiftRight"
            | "ControlLeft" | "ControlRight"
            | "AltLeft" | "AltRight"
            | "MetaLeft" | "MetaRight"
    )
}

pub struct KeyboardState {
    mod_state: ModifierState,
    mod_timeout: std::pin::Pin<Box<tokio::time::Sleep>>,
}

impl KeyboardState {
    pub fn new() -> Self {
        let mut mod_timeout = Box::pin(tokio::time::sleep(Duration::from_secs(1)));
        mod_timeout.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(1));
        Self {
            mod_state: ModifierState::new(),
            mod_timeout,
        }
    }

    /// Process a keyboard event from the browser. Returns the EIS events
    /// that should be sent (modifier sync + key press/release).
    pub fn handle_event(&mut self, keycode: &str, modifiers: Modifiers) -> Vec<EisKeyboardEvent> {
        let mut events = self.mod_state.sync(modifiers);

        if is_modifier_code(keycode) {
            // Modifier keys are handled by ModifierState; ignore them here.
        } else if let Some(key) = js_keycode_to_eis_key(keycode) {
            events.push(EisKeyboardEvent { key, press: true });
            events.push(EisKeyboardEvent { key, press: false });
        } else {
            log(format!("EIS keyboard: unmapped keycode: {keycode}"));
        }

        events
    }

    /// Returns a future that resolves when the modifier timeout fires
    /// (no keyboard event received for 1 s).
    pub async fn timeout_fired(&mut self) {
        (&mut self.mod_timeout).await;
    }

    /// Reset the modifier timeout after a keyboard event arrives.
    pub fn reset_timeout(&mut self) {
        self.mod_timeout
            .as_mut()
            .reset(tokio::time::Instant::now() + Duration::from_secs(1));
    }

    /// Release all currently held modifiers.
    pub fn release_all_modifiers(&mut self) -> Vec<EisKeyboardEvent> {
        self.mod_state.release_all()
    }
}
