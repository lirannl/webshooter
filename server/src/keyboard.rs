use std::ops::Deref;

use inputtino::{DeviceDefinition, Keyboard as DefinedKeyboard};

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
}
