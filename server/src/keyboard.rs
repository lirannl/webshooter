use inputtino::{DeviceDefinition, Keyboard as DefinedKeyboard};

pub struct Keyboard {
    _definition: DeviceDefinition,
    kb: DefinedKeyboard,
}

impl Keyboard {
    pub fn new(name: &str) -> Option<Self> {
        println!("[portal_auth] creating keyboard");
        let def = DeviceDefinition::new(name, 0xAB, 0xCD, 0xEF, "", "");
        let kb = match DefinedKeyboard::new(&def) {
            Ok(kb) => kb,
            Err(e) => {
                println!("[portal_auth] failed to create keyboard — {e:?}");
                return None;
            }
        };
        match kb.get_nodes() {
            Ok(nodes) => println!("[portal_auth] keyboard created at nodes: {:?}", nodes),
            Err(e) => println!("[portal_auth] keyboard created but get_nodes failed: {e:?}"),
        }
        Some(Self {
            _definition: def,
            kb,
        })
    }

    pub fn press_key(&mut self, hid_keycode: i16) {
        println!("[portal_auth] press key 0x{hid_keycode:02X}");
        self.kb.press_key(hid_keycode);
    }

    pub fn release_key(&mut self, hid_keycode: i16) {
        println!("[portal_auth] release key 0x{hid_keycode:02X}");
        self.kb.release_key(hid_keycode);
    }

    pub fn press_enter(&mut self) {
        self.press_key(0x0D_i16);
    }

    pub fn release_enter(&mut self) {
        self.release_key(0x0D_i16);
    }
}