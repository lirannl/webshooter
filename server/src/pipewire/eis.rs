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
use shared::server_datagram::ServerDatagram;

use super::eis_keyboard::{EisKeyboardEvent, KeyboardState, send_keyboard_key};
use super::pointer::{
    EisButtonEvent, EisPointerEvent, EisScrollEvent, MouseState, send_button_event,
    send_pointer_motion, send_scroll_event, web_button_to_linux,
};
use super::touch::{EisTouchEvent, TouchState, offset_touch_event, send_touch_event};

pub(crate) fn timestamp_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}

enum EisInputEvent {
    Touch(EisTouchEvent),
    Keyboard(EisKeyboardEvent),
    Pointer(EisPointerEvent),
    Button(EisButtonEvent),
    Scroll(EisScrollEvent),
}

pub fn eis_task(
    eis_fd: OwnedFd,
    stream_pos: (i32, i32),
    client_rx: &mut Receiver<ClientDatagram>,
    server_tx: &mpsc::Sender<ServerDatagram>,
    mut cursor_rx: mpsc::Receiver<(i32, i32)>,
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
    let server_tx = server_tx.clone();
    tokio::spawn({
        let cancel = cancel_task;
        async move {
            let mut touch_state = TouchState::new();
            let mut keyboard_state = KeyboardState::new();
            let mut mouse_state = MouseState::new();
            loop {
                let msg = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    msg = event_rx.recv() => msg,
                    cursor = cursor_rx.recv() => {
                        match cursor {
                            Some((x, y)) => {
                                if mouse_state.update_compositor_pos(x, y) {
                                    let _ = server_tx.send(ServerDatagram::ReleaseMouse).await;
                                }
                            }
                            None => {}
                        }
                        continue;
                    }
                    // Release all touch points if no event arrives within 1 s,
                    // so stuck touches don't persist after disconnect.
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {
                        for ev in touch_state.release_all() {
                            let _ = input_tx.send(EisInputEvent::Touch(ev)).await;
                        }
                        continue;
                    }
                    // Release all held modifiers if no keyboard event arrives
                    // within 1 s, so stuck modifiers don't persist.
                    _ = keyboard_state.timeout_fired() => {
                        for ev in keyboard_state.release_all_modifiers() {
                            let _ = input_tx.send(EisInputEvent::Keyboard(ev)).await;
                        }
                        continue;
                    }
                };
                match msg {
                    Ok(ClientDatagram::Touchscreen { index, x, y }) => {
                        for ev in touch_state.handle_touch(index, x, y) {
                            let _ = input_tx.send(EisInputEvent::Touch(ev)).await;
                        }
                    }
                    Ok(ClientDatagram::TouchscreenRelease { index }) => {
                        if let Some(ev) = touch_state.handle_release(index) {
                            let _ = input_tx.send(EisInputEvent::Touch(ev)).await;
                        }
                    }
                    Ok(ClientDatagram::Keyboard { keycode, modifiers }) => {
                        for ev in keyboard_state.handle_event(&keycode, modifiers) {
                            let _ = input_tx.send(EisInputEvent::Keyboard(ev)).await;
                        }
                        keyboard_state.reset_timeout();
                    }
                    Ok(ClientDatagram::MouseMove { dx, dy }) => {
                        let event = mouse_state.handle_move(dx, dy);
                        let _ = input_tx.send(EisInputEvent::Pointer(event)).await;
                    }
                    Ok(ClientDatagram::MouseButton { button, pressed }) => {
                        let linux_btn = web_button_to_linux(button);
                        let _ = input_tx.send(EisInputEvent::Button(
                            EisButtonEvent::Button { button: linux_btn, pressed },
                        )).await;
                    }
                    Ok(ClientDatagram::Scroll { dx, dy }) => {
                        let _ = input_tx.send(EisInputEvent::Scroll(
                            EisScrollEvent::Scroll { dx, dy },
                        )).await;
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

    let (touch_device, keyboard_device, pointer_device, touchscreen, keyboard, pointer, button, scroll) =
        match wait_for_devices(&mut eis_stream, &connection, &cancel).await {
            Some(result) => result,
            None => return,
        };

    let mut touch_sequence = 0u32;
    let mut keyboard_sequence = 0u32;
    let mut pointer_sequence = 0u32;
    let mut button_sequence = 0u32;
    let mut scroll_sequence = 0u32;
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
                    Some(EisInputEvent::Pointer(event)) => {
                        send_pointer_motion(&connection, &pointer_device, &pointer, &mut pointer_sequence, event);
                    }
                    Some(EisInputEvent::Button(event)) => {
                        send_button_event(&connection, &pointer_device, &button, &mut button_sequence, event);
                    }
                    Some(EisInputEvent::Scroll(event)) => {
                        send_scroll_event(&connection, &pointer_device, &scroll, &mut scroll_sequence, event);
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
    reis::event::Device,
    ei::Touchscreen,
    ei::Keyboard,
    ei::Pointer,
    ei::Button,
    ei::Scroll,
)> {
    let mut touch_device = None;
    let mut keyboard_device = None;
    let mut pointer_device = None;
    let mut touch_resumed = false;
    let mut keyboard_resumed = false;
    let mut pointer_resumed = false;
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return None,
            event = eis_stream.next() => {
                match event {
                    Some(Ok(EiEvent::SeatAdded(ev))) => {
                        ev.seat.bind_capabilities((DeviceCapability::Touch | DeviceCapability::Keyboard | DeviceCapability::Pointer | DeviceCapability::Button | DeviceCapability::Scroll).into());
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
                        if ev.device.has_capability(DeviceCapability::Pointer) && pointer_device.is_none() {
                            pointer_device = Some(ev.device.clone());
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
                        if let Some(ref pd) = pointer_device {
                            if ev.device == *pd {
                                pointer_resumed = true;
                            }
                        }
                        if let (Some(td), Some(kd), Some(pd)) = (&touch_device, &keyboard_device, &pointer_device) {
                            if touch_resumed && keyboard_resumed && pointer_resumed {
                                let touchscreen = td.interface::<ei::Touchscreen>()?;
                                let keyboard = kd.interface::<ei::Keyboard>()?;
                                let pointer = pd.interface::<ei::Pointer>()?;
                                let button = pd.interface::<ei::Button>()?;
                                let scroll = pd.interface::<ei::Scroll>()?;
                                let same_device = td == kd && kd == pd;
                                log(format!("EIS: devices ready (all same device: {same_device})"));
                                return Some((td.clone(), kd.clone(), pd.clone(), touchscreen, keyboard, pointer, button, scroll));
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
