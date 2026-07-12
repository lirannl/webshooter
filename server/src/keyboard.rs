use std::ops::Deref;

use inputtino::{DeviceDefinition, Keyboard as DefinedKeyboard};
use shared::client_datagram::Modifiers;

pub struct Keyboard(DefinedKeyboard);

pub const ENTER: i16 = 0x0D;

impl Deref for Keyboard {
    type Target = DefinedKeyboard;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Keyboard {
    pub fn new(name: &str) -> Option<Self> {
        println!("[keyboard] creating keyboard");
        let def = DeviceDefinition::new(name, 0xAB, 0xCD, 0xEF, "", "");
        let kb = match DefinedKeyboard::new(&def) {
            Ok(kb) => kb,
            Err(e) => {
                println!("[keyboard] failed to create keyboard — {e:?}");
                return None;
            }
        };
        match kb.get_nodes() {
            Ok(nodes) => println!("[keyboard] keyboard created at nodes: {:?}", nodes),
            Err(e) => println!("[keyboard] keyboard created but get_nodes failed: {e:?}"),
        }
        Some(Self(kb))
    }


    pub fn press_enter(&mut self) {
        self.press_key(ENTER);
    }

    pub fn release_enter(&mut self) {
        self.release_key(ENTER);
    }

    pub fn handle_key_event(&mut self, keycode: &str, modifiers: Modifiers) {
        let Some(vk) = js_keycode_to_vk(keycode) else {
            println!("[keyboard] unmapped keycode: {keycode}");
            return;
        };

        let modifier_keys = modifier_vks(modifiers);
        for &m in &modifier_keys {
            self.press_key(m);
        }

        self.press_key(vk);
        self.release_key(vk);

        for &m in modifier_keys.iter().rev() {
            self.release_key(m);
        }
    }
}

fn modifier_vks(modifiers: Modifiers) -> Vec<i16> {
    let mut v = Vec::new();
    if modifiers.contains(Modifiers::CTRL) {
        v.push(0xA2); // VK_LCONTROL
    }
    if modifiers.contains(Modifiers::SHIFT) {
        v.push(0xA0); // VK_LSHIFT
    }
    if modifiers.contains(Modifiers::ALT) {
        v.push(0xA4); // VK_LMENU
    }
    if modifiers.contains(Modifiers::META) {
        v.push(0x5B); // VK_LWIN
    }
    v
}

fn js_keycode_to_vk(code: &str) -> Option<i16> {
    Some(match code {
        "KeyA" => 0x41,
        "KeyB" => 0x42,
        "KeyC" => 0x43,
        "KeyD" => 0x44,
        "KeyE" => 0x45,
        "KeyF" => 0x46,
        "KeyG" => 0x47,
        "KeyH" => 0x48,
        "KeyI" => 0x49,
        "KeyJ" => 0x4A,
        "KeyK" => 0x4B,
        "KeyL" => 0x4C,
        "KeyM" => 0x4D,
        "KeyN" => 0x4E,
        "KeyO" => 0x4F,
        "KeyP" => 0x50,
        "KeyQ" => 0x51,
        "KeyR" => 0x52,
        "KeyS" => 0x53,
        "KeyT" => 0x54,
        "KeyU" => 0x55,
        "KeyV" => 0x56,
        "KeyW" => 0x57,
        "KeyX" => 0x58,
        "KeyY" => 0x59,
        "KeyZ" => 0x5A,
        "Digit0" => 0x30,
        "Digit1" => 0x31,
        "Digit2" => 0x32,
        "Digit3" => 0x33,
        "Digit4" => 0x34,
        "Digit5" => 0x35,
        "Digit6" => 0x36,
        "Digit7" => 0x37,
        "Digit8" => 0x38,
        "Digit9" => 0x39,
        "Enter" => ENTER,
        "Escape" => 0x1B,
        "Backspace" => 0x08,
        "Tab" => 0x09,
        "Space" => 0x20,
        "Delete" => 0x2E,
        "Insert" => 0x2D,
        "Home" => 0x24,
        "End" => 0x23,
        "PageUp" => 0x21,
        "PageDown" => 0x22,
        "ArrowLeft" => 0x25,
        "ArrowUp" => 0x26,
        "ArrowRight" => 0x27,
        "ArrowDown" => 0x28,
        "F1" => 0x70,
        "F2" => 0x71,
        "F3" => 0x72,
        "F4" => 0x73,
        "F5" => 0x74,
        "F6" => 0x75,
        "F7" => 0x76,
        "F8" => 0x77,
        "F9" => 0x78,
        "F10" => 0x79,
        "F11" => 0x7A,
        "F12" => 0x7B,
        "ShiftLeft" => 0xA0,
        "ShiftRight" => 0xA1,
        "ControlLeft" => 0xA2,
        "ControlRight" => 0xA3,
        "AltLeft" => 0xA4,
        "AltRight" => 0xA5,
        "MetaLeft" => 0x5B,
        "MetaRight" => 0x5C,
        "Semicolon" => 0xBA,
        "Equal" => 0xBB,
        "Comma" => 0xBC,
        "Minus" => 0xBD,
        "Period" => 0xBE,
        "Slash" => 0xBF,
        "Backquote" => 0xC0,
        "BracketLeft" => 0xDB,
        "Backslash" => 0xDC,
        "BracketRight" => 0xDD,
        "Quote" => 0xDE,
        "CapsLock" => 0x14,
        "NumLock" => 0x90,
        "ScrollLock" => 0x91,
        "PrintScreen" => 0x2C,
        "Pause" => 0x13,
        _ => return None,
    })
}
