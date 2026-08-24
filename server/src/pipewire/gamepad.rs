use std::collections::HashMap;

use inputtino::{
    DeviceDefinition, Joypad, JoypadMotionType, JoypadStickPosition, PS5Joypad,
};

use shared::client_datagram::{gamepad_client_buttons_to_inputtino, GamepadMotion};

use crate::logging::log;

/// Manages the set of virtual gamepads we expose to the host.
///
/// A virtual joypad is only created when the first `Gamepad` snapshot for a
/// given id arrives, so a capture that never receives gamepad input never
/// creates a device. Use [`GamepadManager::remove`] to tear one down when the
/// client reports the controller disconnecting.
pub struct GamepadManager {
    gamepads: HashMap<u8, Joypad>,
}

impl GamepadManager {
    pub fn new() -> Self {
        Self {
            gamepads: HashMap::new(),
        }
    }

    /// Apply a full gamepad state snapshot. Creates the virtual device lazily
    /// on first use for this `id`.
    pub fn update(
        &mut self,
        id: u8,
        buttons: u32,
        lx: i16,
        ly: i16,
        rx: i16,
        ry: i16,
        lt: i16,
        rt: i16,
        motion: Option<GamepadMotion>,
    ) {
        if !self.gamepads.contains_key(&id) {
            let joypad = Self::create(id);
            self.gamepads.insert(id, joypad);
        }

        let joypad = self.gamepads.get(&id).expect("gamepad just inserted");
        // Buttons are delivered as a full snapshot; inputtino diffs internally.
        joypad.set_pressed(gamepad_client_buttons_to_inputtino(buttons) as i32);
        joypad.set_stick(JoypadStickPosition::LS, lx, ly);
        joypad.set_stick(JoypadStickPosition::RS, rx, ry);
        joypad.set_triggers(lt, rt);
        if let Some(m) = motion {
            // Only the PS5 joypad exposes motion sensors in inputtino. We store
            // the device as a PS5 joypad (see `create`), so reach into it here.
            if let Joypad::PS5(inner) = joypad {
                // inputtino takes separate ACCELERATION and GYROSCOPE updates.
                inner.set_motion(
                    JoypadMotionType::ACCELERATION,
                    m.accel_x,
                    m.accel_y,
                    m.accel_z,
                );
                inner.set_motion(
                    JoypadMotionType::GYROSCOPE,
                    m.gyro_x,
                    m.gyro_y,
                    m.gyro_z,
                );
            }
        }
    }

    /// Remove and drop the virtual device for `id`, if present.
    pub fn remove(&mut self, id: u8) {
        if self.gamepads.remove(&id).is_some() {
            log(format!("Gamepad: removed virtual device {id}"));
        }
    }

    fn create(id: u8) -> Joypad {
        let name = format!("Webshooter Gamepad {id}");
        let def = DeviceDefinition::new(&name, 0x054C, 0x0CE6, 0x8111, "", "");
        let joypad =
            Joypad::PS5(PS5Joypad::new(&def).expect("failed to create virtual gamepad"));
        if let Joypad::PS5(inner) = &joypad {
            match inner.get_nodes() {
                Ok(nodes) => log(format!(
                    "Gamepad: created virtual device {id} at {nodes:?}"
                )),
                Err(e) => log(format!("Gamepad: created device {id} (nodes unknown: {e})")),
            }
        }
        joypad
    }
}

impl Default for GamepadManager {
    fn default() -> Self {
        Self::new()
    }
}
