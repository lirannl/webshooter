use std::collections::HashSet;
use std::os::unix::io::OwnedFd;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use futures_util::StreamExt;
use reis::ei;
use reis::event::{DeviceCapability, EiEvent};
use reis::tokio::EiConvertEventStream;
use tokio::sync::{broadcast::Receiver, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::logging::log;
use shared::client_datagram::ClientDatagram;

use super::eis_keyboard::{
    EisKeyboardEvent, ModifierState, js_keycode_to_eis_key, send_keyboard_key,
};

enum EisTouchEvent {
    Down { x: u16, y: u16, index: u8 },
    Motion { x: u16, y: u16, index: u8 },
    Up { index: u8 },
}

enum EisInputEvent {
    Touch(EisTouchEvent),
    Keyboard(EisKeyboardEvent),
}

pub fn touch_task(
    eis_fd: OwnedFd,
    stream_pos: (i32, i32),
    client_rx: &mut Receiver<ClientDatagram>,
    cancel: &CancellationToken,
) -> JoinHandle<()> {
    let (input_tx, input_rx) = mpsc::channel::<EisInputEvent>(64);
    let cancel_task = cancel.clone();
    let cancel_eis = cancel.clone();

    // Dedicated thread for the EIS event loop. The reis event stream is !Send
    // because EiEventConverter stores dyn FnOnce callbacks internally, so it
    // must live on a single thread with a current-thread tokio runtime.
    std::thread::Builder::new()
        .name("webshooter-eis".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("eis tokio runtime");
            rt.block_on(eis_main(eis_fd, stream_pos, input_rx, cancel_eis));
        })
        .expect("eis thread");

    // Async forwarding task on the main tokio runtime. Reads events
    // from the broadcast channel and sends them to the EIS thread.
    let mut event_rx = client_rx.resubscribe();
    tokio::spawn({
        let cancel = cancel_task;
        async move {
            let mut active_slots: HashSet<u8> = Default::default();
            let mut mod_state = ModifierState::new();
            let mut mod_timeout = Box::pin(tokio::time::sleep(Duration::from_secs(1)));
            mod_timeout.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(1));
            loop {
                let msg = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    msg = event_rx.recv() => msg,
                    // Release all touch points if no event arrives within 1 s,
                    // so stuck touches don't persist after disconnect.
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {
                        let indices: Vec<u8> = active_slots.drain().collect();
                        for idx in indices {
                            let _ = input_tx.send(EisInputEvent::Touch(EisTouchEvent::Up { index: idx })).await;
                        }
                        continue;
                    }
                    // Release all held modifiers if no keyboard event arrives
                    // within 1 s, so stuck modifiers don't persist.
                    _ = &mut mod_timeout => {
                        for ev in mod_state.release_all() {
                            let _ = input_tx.send(EisInputEvent::Keyboard(ev)).await;
                        }
                        mod_timeout.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(1));
                        continue;
                    }
                };
                match msg {
                    Ok(ClientDatagram::Touchscreen { index, x, y }) => {
                        let is_new = active_slots.insert(index);
                        let ev = if is_new {
                            EisTouchEvent::Down { x, y, index }
                        } else {
                            EisTouchEvent::Motion { x, y, index }
                        };
                        let _ = input_tx.send(EisInputEvent::Touch(ev)).await;
                    }
                    Ok(ClientDatagram::TouchscreenRelease { index }) => {
                        if active_slots.remove(&index) {
                            let _ = input_tx.send(EisInputEvent::Touch(EisTouchEvent::Up { index })).await;
                        }
                    }
                    Ok(ClientDatagram::Keyboard { keycode, modifiers }) => {
                        // Synchronise modifier state — only emits press/release
                        // when the modifier flags actually change between events.
                        for ev in mod_state.sync(modifiers) {
                            let _ = input_tx.send(EisInputEvent::Keyboard(ev)).await;
                        }
                        // Modifier keys are handled by ModifierState; ignore them here.
                        if matches!(
                            keycode.as_str(),
                            "ShiftLeft" | "ShiftRight"
                                | "ControlLeft" | "ControlRight"
                                | "AltLeft" | "AltRight"
                                | "MetaLeft" | "MetaRight"
                        ) {
                            // nothing to do
                        } else if let Some(key) = js_keycode_to_eis_key(&keycode) {
                            let _ = input_tx.send(EisInputEvent::Keyboard(EisKeyboardEvent { key, press: true })).await;
                            let _ = input_tx.send(EisInputEvent::Keyboard(EisKeyboardEvent { key, press: false })).await;
                        } else {
                            log(format!("EIS keyboard: unmapped keycode: {keycode}"));
                        }
                        mod_timeout.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(1));
                    }
                    Ok(_) => continue,
                    Err(_) => break,
                }
            }
        }
    })
}

async fn eis_main(
    eis_fd: OwnedFd,
    stream_pos: (i32, i32),
    mut input_rx: mpsc::Receiver<EisInputEvent>,
    cancel: CancellationToken,
) {
    let stream = UnixStream::from(eis_fd);
    let context = match ei::Context::new(stream) {
        Ok(c) => c,
        Err(e) => {
            log(format!("EIS: failed to create context: {e}"));
            return;
        }
    };

    let (connection, mut eis_stream) = match context
        .handshake_tokio("webshooter", ei::handshake::ContextType::Sender)
        .await
    {
        Ok(pair) => pair,
        Err(e) => {
            log(format!("EIS: handshake failed: {e}"));
            return;
        }
    };
    if let Err(e) = connection.flush() {
        log(format!("EIS: initial flush failed: {e}"));
        return;
    }

    let (touch_device, keyboard_device, touchscreen, keyboard) =
        match wait_for_devices(&mut eis_stream, &connection, &cancel).await {
            Some(result) => result,
            None => return,
        };

    let mut touch_sequence = 0u32;
    let mut keyboard_sequence = 0u32;
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            event = eis_stream.next() => {
                match event {
                    Some(Ok(EiEvent::DevicePaused(_))) => {
                        wait_for_resume(&mut eis_stream, &cancel).await;
                    }
                    Some(Ok(EiEvent::Disconnected(d))) => {
                        log(format!("EIS: disconnected: {:?}", d.reason));
                        break;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        log(format!("EIS: event error: {e}"));
                        break;
                    }
                    None => break,
                }
            }
            cmd = input_rx.recv() => {
                match cmd {
                    Some(EisInputEvent::Touch(event)) => {
                        let event = offset_touch_event(event, stream_pos);
                        send_touch_event(&connection, &touch_device, &touchscreen, &mut touch_sequence, event);
                    }
                    Some(EisInputEvent::Keyboard(ev)) => {
                        send_keyboard_key(&connection, &keyboard_device, &keyboard, &mut keyboard_sequence, ev.key, ev.press);
                    }
                    None => break,
                }
            }
        }
    }
}

async fn wait_for_devices(
    eis_stream: &mut EiConvertEventStream,
    connection: &reis::event::Connection,
    cancel: &CancellationToken,
) -> Option<(
    reis::event::Device,
    reis::event::Device,
    ei::Touchscreen,
    ei::Keyboard,
)> {
    let mut touch_device = None;
    let mut keyboard_device = None;
    let mut touch_resumed = false;
    let mut keyboard_resumed = false;
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return None,
            event = eis_stream.next() => {
                match event {
                    Some(Ok(EiEvent::SeatAdded(ev))) => {
                        ev.seat.bind_capabilities((DeviceCapability::Touch | DeviceCapability::Keyboard).into());
                        if let Err(e) = connection.flush() {
                            log(format!("EIS: seat bind flush: {e}"));
                        }
                    }
                    Some(Ok(EiEvent::DeviceAdded(ev))) => {
                        if ev.device.has_capability(DeviceCapability::Touch) && touch_device.is_none() {
                            touch_device = Some(ev.device.clone());
                        }
                        if ev.device.has_capability(DeviceCapability::Keyboard) && keyboard_device.is_none() {
                            keyboard_device = Some(ev.device.clone());
                        }
                    }
                    Some(Ok(EiEvent::DeviceResumed(ev))) => {
                        if let Some(ref td) = touch_device {
                            if ev.device == *td {
                                touch_resumed = true;
                            }
                        }
                        if let Some(ref kd) = keyboard_device {
                            if ev.device == *kd {
                                keyboard_resumed = true;
                            }
                        }
                        if let (Some(td), Some(kd)) = (&touch_device, &keyboard_device) {
                            if touch_resumed && keyboard_resumed {
                                let touchscreen = td.interface::<ei::Touchscreen>()?;
                                let keyboard = kd.interface::<ei::Keyboard>()?;
                                let same_device = td == kd;
                                log(format!("EIS: devices ready (touch+keyboard same device: {same_device})"));
                                return Some((td.clone(), kd.clone(), touchscreen, keyboard));
                            }
                        }
                    }
                    Some(Ok(EiEvent::Disconnected(d))) => {
                        log(format!("EIS: disconnected: {:?}", d.reason));
                        return None;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        log(format!("EIS: event error during setup: {e}"));
                        return None;
                    }
                    None => return None,
                }
            }
        }
    }
}

async fn wait_for_resume(eis_stream: &mut EiConvertEventStream, cancel: &CancellationToken) {
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return,
            event = eis_stream.next() => {
                match event {
                    Some(Ok(EiEvent::DeviceResumed(_))) | Some(Ok(EiEvent::Disconnected(_))) => return,
                    Some(Ok(_)) => {}
                    Some(Err(_)) => return,
                    None => return,
                }
            }
        }
    }
}

fn offset_touch_event(event: EisTouchEvent, (ox, oy): (i32, i32)) -> EisTouchEvent {
    match event {
        EisTouchEvent::Down { x, y, index } => EisTouchEvent::Down {
            x: (x as i32 + ox).max(0).min(u16::MAX as i32) as u16,
            y: (y as i32 + oy).max(0).min(u16::MAX as i32) as u16,
            index,
        },
        EisTouchEvent::Motion { x, y, index } => EisTouchEvent::Motion {
            x: (x as i32 + ox).max(0).min(u16::MAX as i32) as u16,
            y: (y as i32 + oy).max(0).min(u16::MAX as i32) as u16,
            index,
        },
        EisTouchEvent::Up { .. } => event,
    }
}

fn send_touch_event(
    connection: &reis::event::Connection,
    device: &reis::event::Device,
    touchscreen: &ei::Touchscreen,
    sequence: &mut u32,
    event: EisTouchEvent,
) {
    let serial = connection.serial();
    *sequence = sequence.wrapping_add(1);

    device.device().start_emulating(serial, *sequence);
    match event {
        EisTouchEvent::Down { x, y, index } => {
            touchscreen.down(index as u32, x as f32, y as f32);
        }
        EisTouchEvent::Motion { x, y, index } => {
            touchscreen.motion(index as u32, x as f32, y as f32);
        }
        EisTouchEvent::Up { index } => {
            touchscreen.up(index as u32);
        }
    }
    device.device().frame(serial, timestamp_us());
    device.device().stop_emulating(serial);

    if let Err(e) = connection.flush() {
        log(format!("EIS: flush error: {e}"));
    }
}

pub(crate) fn timestamp_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}
