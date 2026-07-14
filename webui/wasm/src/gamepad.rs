use crate::with_wt;
use shared::client_datagram::{ClientDatagram, GamepadMotion};
use std::cell::RefCell;
use std::rc::Rc;
use wasm_bindgen::prelude::*;
use web_sys::{DeviceMotionEvent, Gamepad as WebGamepad, GamepadButton};

/// Maximum number of gamepads we forward. The server keys devices by a `u8`
/// id, and the standard Gamepad API exposes at most a handful.
const MAX_GAMEPADS: usize = 4;

/// State we remember per slot so we can detect connect/disconnect and only
/// send snapshots when something actually changed.
struct PadState {
    connected: bool,
    /// Last sent client-side button bitmask.
    buttons: u32,
    lx: i16,
    ly: i16,
    rx: i16,
    ry: i16,
    lt: i16,
    rt: i16,
}

impl PadState {
    fn new() -> Self {
        Self {
            connected: false,
            buttons: 0,
            lx: 0,
            ly: 0,
            rx: 0,
            ry: 0,
            lt: 0,
            rt: 0,
        }
    }
}

fn clamp_i16(v: f64) -> i16 {
    v.clamp(i16::MIN as f64, i16::MAX as f64) as i16
}

/// Build the client-side button bitmask from the Web Gamepad `buttons` array.
/// Bit `i` is set when standard button `i` is pressed. We forward at most
/// [`shared::client_datagram::GAMEPAD_NUM_BUTTONS`] buttons.
fn buttons_mask(gp: &WebGamepad) -> u32 {
    let mut mask = 0u32;
    let buttons = gp.buttons();
    let count = buttons.length().min(shared::client_datagram::GAMEPAD_NUM_BUTTONS as u32);
    for i in 0..count {
        let btn: GamepadButton = buttons.get(i).unchecked_into();
        if btn.pressed() {
            mask |= 1 << i;
        }
    }
    mask
}

/// Read analogue stick values (range -32768..=32767). The standard mapping
/// puts left stick in axes[0],[1] and right stick in axes[2],[3].
fn stick_value(axes: &js_sys::Array, idx: usize) -> i16 {
    let v = axes.get(idx as u32).as_f64().unwrap_or(0.0);
    // Gamepad axes are -1..1; map to i16.
    clamp_i16(v * i16::MAX as f64)
}

/// Read trigger value (range 0..=32767). Some browsers expose triggers as
/// buttons[6]/[7] with analogue `value`, others as axes[2]/[3] after the
/// sticks. We prefer the button `value` when present, else fall back to the
/// axis at the standard trigger position.
fn trigger_value(gp: &WebGamepad, button_idx: usize, axis_idx: Option<usize>) -> i16 {
    let buttons = gp.buttons();
    let btn: GamepadButton = buttons.get(button_idx as u32).unchecked_into();
    let v = btn.value();
    if v > 0.0 {
        return clamp_i16(v * i16::MAX as f64);
    }
    if let Some(axis_idx) = axis_idx {
        let axes = gp.axes();
        if (axis_idx as u32) < axes.length() {
            // Axis is -1..1; map 0..1 -> 0..32767.
            let v = axes.get(axis_idx as u32).as_f64().unwrap_or(0.0);
            return clamp_i16(((v + 1.0) * 0.5) * i16::MAX as f64);
        }
    }
    0
}

fn send(msg: ClientDatagram) {
    let bytes = msg.to_bytes();
    let buf = js_sys::Uint8Array::from(&bytes[..]);
    with_wt(|gwt| {
        let _ = gwt.writer.write_with_chunk(buf.as_ref());
    });
}

pub fn setup_gamepad() {
    let states: Rc<RefCell<Vec<PadState>>> =
        Rc::new(RefCell::new((0..MAX_GAMEPADS).map(|_| PadState::new()).collect()));

    // Latest device-motion sample (accelerometer + gyroscope). `None` until the
    // first `devicemotion` event arrives. The standard Gamepad API does not
    // expose motion sensors, so we fall back to the device's motion events.
    let latest_motion: Rc<RefCell<Option<GamepadMotion>>> = Rc::new(RefCell::new(None));
    setup_device_motion(latest_motion.clone());

    let poll = {
        let states = states.clone();
        let latest_motion = latest_motion.clone();
        Closure::wrap(Box::new(move || {
            let navigator = match web_sys::window().map(|w| w.navigator()) {
                Some(n) => n,
                None => return,
            };
            let gamepads = match navigator.get_gamepads() {
                Ok(gp) => gp,
                Err(_) => return,
            };
            let mut states = states.borrow_mut();
            let motion = *latest_motion.borrow();
            for slot in 0..MAX_GAMEPADS {
                let gp_val = gamepads.get(slot as u32);
                let connected = !gp_val.is_null() && !gp_val.is_undefined();
                let prev = &mut states[slot];

                if !connected {
                    if prev.connected {
                        send(ClientDatagram::GamepadDisconnect { id: slot as u8 });
                        *prev = PadState::new();
                    }
                    continue;
                }

                let gp: WebGamepad = gp_val.unchecked_into();
                let buttons = buttons_mask(&gp);
                let axes = gp.axes();
                let lx = stick_value(&axes, 0);
                let ly = -stick_value(&axes, 1);
                let rx = stick_value(&axes, 2);
                let ry = -stick_value(&axes, 3);
                // Triggers: buttons 6/7 with analogue value, or axes 4/5.
                let lt = trigger_value(&gp, 6, Some(4));
                let rt = trigger_value(&gp, 7, Some(5));

                let changed = !prev.connected
                    || prev.buttons != buttons
                    || prev.lx != lx
                    || prev.ly != ly
                    || prev.rx != rx
                    || prev.ry != ry
                    || prev.lt != lt
                    || prev.rt != rt;

                if changed {
                    prev.connected = true;
                    prev.buttons = buttons;
                    prev.lx = lx;
                    prev.ly = ly;
                    prev.rx = rx;
                    prev.ry = ry;
                    prev.lt = lt;
                    prev.rt = rt;
                    send(ClientDatagram::Gamepad {
                        id: slot as u8,
                        buttons,
                        lx,
                        ly,
                        rx,
                        ry,
                        lt,
                        rt,
                        motion,
                    });
                }
            }
        }) as Box<dyn FnMut()>)
    };

    // Poll on a 16 ms interval (~60 Hz), enough fidelity for gamepads.
    if let Some(window) = web_sys::window() {
        let _ = window.set_interval_with_callback_and_timeout_and_arguments_0(
            poll.as_ref().unchecked_ref::<js_sys::Function>(),
            16,
        );
    }
    poll.forget();
}

/// Subscribe to `devicemotion` events and keep `latest` updated with the most
/// recent accelerometer/gyroscope reading. iOS requires an explicit permission
/// grant, which we attempt if the static method is present.
fn setup_device_motion(latest: Rc<RefCell<Option<GamepadMotion>>>) {
    let on_motion = {
        let latest = latest.clone();
        Closure::wrap(Box::new(move |event: DeviceMotionEvent| {
            // Prefer acceleration without gravity; fall back to including it.
            let accel = event.acceleration().or(event.acceleration_including_gravity());
            let (ax, ay, az) = match &accel {
                Some(a) => (
                    a.x().unwrap_or(0.0),
                    a.y().unwrap_or(0.0),
                    a.z().unwrap_or(0.0),
                ),
                None => (0.0, 0.0, 0.0),
            };
            let (gx, gy, gz) = match event.rotation_rate() {
                Some(r) => (
                    r.beta().unwrap_or(0.0),
                    r.gamma().unwrap_or(0.0),
                    r.alpha().unwrap_or(0.0),
                ),
                None => (0.0, 0.0, 0.0),
            };
            *latest.borrow_mut() = Some(GamepadMotion {
                accel_x: ax as f32,
                accel_y: ay as f32,
                accel_z: az as f32,
                gyro_x: gx as f32,
                gyro_y: gy as f32,
                gyro_z: gz as f32,
            });
        }) as Box<dyn FnMut(DeviceMotionEvent)>)
    };

    if let Some(window) = web_sys::window() {
        let _ = window
            .add_event_listener_with_callback_and_add_event_listener_options(
                "devicemotion",
                on_motion.as_ref().unchecked_ref::<js_sys::Function>(),
                web_sys::AddEventListenerOptions::new().passive(true),
            );
    }

    // iOS 13+ gates motion/orientation behind a permission prompt. Attempt it
    // opportunistically; if the API is missing this is a no-op.
    if let Some(window) = web_sys::window() {
        if let Ok(dm) = js_sys::Reflect::get(&window, &JsValue::from_str("DeviceMotionEvent")) {
            if js_sys::Reflect::has(&dm, &JsValue::from_str("requestPermission")).unwrap_or(false) {
                if let Ok(f) = js_sys::Reflect::get(&dm, &JsValue::from_str("requestPermission")) {
                    if let Ok(f) = f.dyn_into::<js_sys::Function>() {
                        let _ = f.call0(&dm);
                    }
                }
            }
        }
    }

    on_motion.forget();
}
