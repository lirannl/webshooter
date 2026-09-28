use std::collections::VecDeque;
use std::os::unix::io::OwnedFd;
use std::os::unix::net::UnixStream;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;

use futures_util::future::Either;
use futures_util::StreamExt;
use reis::ei;
use reis::event::{DeviceCapability, EiEvent};
use reis::tokio::EiConvertEventStream;
use tokio::sync::{broadcast, mpsc};
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use shared::client_datagram::{ClientDatagram, coalesce_input};
use shared::server_datagram::ServerDatagram;

use super::eis_keyboard::{EisKeyboardEvent, KeyboardState, send_keyboard_key};
use super::gamepad::GamepadManager;
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

/// Run one input event inside an EIS emulation frame: bump the sequence,
/// start emulating, emit the event(s), then frame + stop emulating + flush as
/// one unit. Every EIS sender (keyboard, pointer, touch) uses this skeleton.
pub(crate) fn with_emulation(
    connection: &reis::event::Connection,
    device: &reis::event::Device,
    sequence: &mut u32,
    event_name: &str,
    emit: impl FnOnce(),
) {
    let serial = connection.serial();
    *sequence = sequence.wrapping_add(1);
    device.device().start_emulating(serial, *sequence);
    emit();
    device.device().frame(serial, timestamp_us());
    device.device().stop_emulating(serial);
    if let Err(e) = connection.flush() {
        log::error!("EIS: {event_name} flush error: {e}");
    }
}

/// How often the server samples its input-pipeline load to decide whether to
/// ask the client to throttle. Fast enough to react to a sustained burst
/// within ~100 ms, slow enough not to spam the client with control datagrams.
const THROTTLE_SAMPLE_INTERVAL: Duration = Duration::from_millis(100);

/// Floor on the spacing the input gate enforces *independently* of the
/// fill-driven throttle. Without it, the moment the pipeline has headroom
/// (`interval_ms == 0`) a flood is forwarded straight to the EIS thread and
/// compositor at full network rate, which can stall rendering and visibly
/// freeze the session for as long as the flood lasts — the monitor only reacts
/// after the pipeline *fills*, which never happens when the consumer keeps up
/// at a rate that still overwhelms the compositor. Human input never arrives
/// more often than a few hundred events per second, so a 1 ms floor (~1000
/// events/s cap, further coalesced by the batch path) is invisible in practice
/// while bounding what a non-cooperative client can inject.
const INPUT_GATE_MIN_INTERVAL: Duration = Duration::from_millis(1);

/// Map the fraction of the input pipeline that is in use (0..=1) onto the
/// minimum spacing (in milliseconds) the client must keep between consecutive
/// input datagrams. 0 means "no throttling". The fill ratio is derived from
/// the channel that queues input events for the EIS thread: as it climbs
/// toward the point where a burst would block or overflow, the server is
/// nearly overloaded, so it asks the client to slow down.
fn throttle_interval_from_fill(fill: f64) -> u16 {
    let pct = (fill.clamp(0.0, 1.0) * 100.0) as u8;
    match pct {
        0..=49 => 0,    // plenty of headroom: no throttling
        50..=64 => 8,   // could get busy: cap ~125 events/s
        65..=79 => 16,  // getting heavy: cap ~62 events/s
        80..=89 => 32,  // heavy: cap ~31 events/s
        90..=94 => 64,  // severe: cap ~15 events/s
        _ => 128,       // nearly full: cap ~8 events/s
    }
}

/// Watches the input-event channel that feeds the EIS thread. When it starts
/// approaching capacity (the server's input pipeline is nearly saturated), it
/// sends [`ServerDatagram::Throttle`] asking the client to space its input
/// datagrams further apart. The message is only sent when the requested
/// interval changes, so an idle or comfortably fast session never sees one.
/// The current interval is also stored in `interval_ms` on every sample: the
/// server-side input gate enforces that same spacing even against a
/// non-cooperative client. Best-effort: if the control channel is gone the
/// monitor simply stops.
async fn input_throttle_monitor(
    input_tx: mpsc::Sender<EisInputEvent>,
    server_tx: mpsc::Sender<ServerDatagram>,
    interval_ms: Arc<AtomicU16>,
    cancel: CancellationToken,
) {
    let max = input_tx.max_capacity();
    let mut last_sent: Option<u16> = None;
    let mut interval = tokio::time::interval(THROTTLE_SAMPLE_INTERVAL);
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            _ = interval.tick() => {
                let fill = if max > 0 {
                    (max - input_tx.capacity()) as f64 / max as f64
                } else {
                    0.0
                };
                let desired = throttle_interval_from_fill(fill);
                // Keep the input gate on the current requested spacing, every
                // sample — even when the client message is suppressed because
                // the value did not change.
                interval_ms.store(desired, Ordering::Relaxed);
                if last_sent != Some(desired) {
                    last_sent = Some(desired);
                    if server_tx
                        .send(ServerDatagram::Throttle { interval_ms: desired })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    }
}

/// Whether a datagram is an input event (subject to the throttle) rather than
/// control traffic (keepalive, resize, logs, decoder caps, audio, …).
fn is_input_datagram(msg: &ClientDatagram) -> bool {
    matches!(
        msg,
        ClientDatagram::Keyboard { .. }
            | ClientDatagram::MouseMove { .. }
            | ClientDatagram::MouseButton { .. }
            | ClientDatagram::Scroll { .. }
            | ClientDatagram::Touchscreen { .. }
            | ClientDatagram::TouchscreenRelease { .. }
            | ClientDatagram::Gamepad { .. }
            | ClientDatagram::GamepadDisconnect { .. }
    )
}

/// Server-side enforcement of the throttle. Inputs travel as WebTransport
/// datagrams (unreliable, best-effort), so the client-side throttle is only
/// cooperative and may exceed the requested rate by a little. This gate sits
/// between the broadcast socket and the EIS forwarding task as the hard
/// backstop: an event arriving inside the current cooling window is coalesced
/// into a pending batch (mouse/scroll deltas summed, touch/gamepad reduced to
/// the newest snapshot) and discarded if a still-newer one replaces it, and
/// the batch is forwarded as soon as the rate allows. The cooling window is
/// never shorter than [`INPUT_GATE_MIN_INTERVAL`], so even when the fill
/// monitor has not (yet) asked for throttling a non-cooperative client cannot
/// push the EIS pipeline at flood rate. Control datagrams bypass the gate
/// entirely.
async fn input_gate_task(
    mut client_rx: broadcast::Receiver<ClientDatagram>,
    gated_tx: broadcast::Sender<ClientDatagram>,
    interval_ms: Arc<AtomicU16>,
    cancel: CancellationToken,
) {
    let mut pending: VecDeque<ClientDatagram> = VecDeque::new();
    let mut next_allowed = tokio::time::Instant::now();
    let mut flush_at: Option<Pin<Box<tokio::time::Sleep>>> = None;

    loop {
        let interval = Duration::from_millis(interval_ms.load(Ordering::Relaxed) as u64)
            .max(INPUT_GATE_MIN_INTERVAL);
        // Wake once the cooling window elapses if throttled input is waiting;
        // otherwise park this select arm so only client events run.
        let flush_fut: Either<
            Pin<&mut tokio::time::Sleep>,
            futures_util::future::Pending<()>,
        > = match flush_at.as_mut() {
            Some(sleep) => Either::Left(sleep.as_mut()),
            None => Either::Right(futures_util::future::pending()),
        };
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            _ = flush_fut => {
                flush_at = None;
                if pending.is_empty() {
                    continue;
                }
                if tokio::time::Instant::now() >= next_allowed {
                    forward_batch(&gated_tx, &mut pending).await;
                    next_allowed = tokio::time::Instant::now() + interval;
                }
            }
            msg = client_rx.recv() => {
                match msg {
                    // Input is coalesced, not replayed, so a lagged record is
                    // superseded by the state that follows it; only a closed
                    // bus means the client is really gone.
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => break,
                    Ok(msg) => {
                        // Control datagrams are not input: bypass the gate.
                        if !is_input_datagram(&msg) {
                            if gated_tx.send(msg).is_err() {
                                break;
                            }
                            continue;
                        }
                        let now = tokio::time::Instant::now();
                        if now >= next_allowed {
                            if gated_tx.send(msg).is_err() {
                                break;
                            }
                            next_allowed = now + interval;
                            continue;
                        }
                        // Inside the cooling window: keep the newest input and
                        // flush it once the window elapses.
                        coalesce_input(&mut pending, msg);
                        if flush_at.is_none() {
                            flush_at = Some(Box::pin(tokio::time::sleep_until(next_allowed)));
                        }
                    }
                }
            }
        }
    }
}

/// Send a coalesced batch of pending input datagrams. `broadcast::Sender` is
/// best-effort: it errors only when every receiver has hung up.
async fn forward_batch(
    gated_tx: &broadcast::Sender<ClientDatagram>,
    pending: &mut VecDeque<ClientDatagram>,
) {
    let msgs = std::mem::take(pending);
    for msg in msgs {
        if gated_tx.send(msg).is_err() {
            return;
        }
    }
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
    client_rx: &mut broadcast::Receiver<ClientDatagram>,
    server_tx: &mpsc::Sender<ServerDatagram>,
    mut cursor_rx: mpsc::Receiver<(i32, i32)>,
    cancel: &CancellationToken,
) -> JoinHandle<()> {
    let (input_tx, input_rx) = mpsc::channel::<EisInputEvent>(64);
    let cancel_task = cancel.clone();
    let cancel_eis = cancel.clone();
    let cancel_gate = cancel.clone();

    // Throttle spacing, shared between the monitor (which decides it from the
    // input pipeline's fill) and the input gate (which enforces it on behalf
    // of the pipeline even against a non-cooperative client).
    let throttle_interval = Arc::new(AtomicU16::new(0));

    // Watch the input pipeline for saturation and ask the client to throttle
    // before events start blocking or overflowing (near-overload feedback).
    tokio::spawn(input_throttle_monitor(
        input_tx.clone(),
        server_tx.clone(),
        throttle_interval.clone(),
        cancel.clone(),
    ));

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

    // Server-side enforcement of the throttle. The client's own throttle is
    // cooperative and may exceed the requested rate by a little; the gate is
    // the hard backstop: it discards excess input, keeps the newest state, and
    // forwards at the permitted rate.
    let (gated_tx, mut event_rx) = broadcast::channel::<ClientDatagram>(64);
    tokio::spawn(input_gate_task(
        client_rx.resubscribe(),
        gated_tx,
        throttle_interval,
        cancel_gate,
    ));

    // Async forwarding task on the main tokio runtime. Reads events
    // from the gated channel and sends them to the EIS thread.
    let server_tx = server_tx.clone();
    tokio::spawn({
        let cancel = cancel_task;
        async move {
            let mut touch_state = TouchState::new();
            let mut keyboard_state = KeyboardState::new();
            let mut mouse_state = MouseState::new();
            let mut gamepad_state = GamepadManager::new();
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
                    Ok(ClientDatagram::Gamepad {
                        id,
                        buttons,
                        lx,
                        ly,
                        rx,
                        ry,
                        lt,
                        rt,
                        motion,
                    }) => {
                        gamepad_state.update(id, buttons, lx, ly, rx, ry, lt, rt, motion);
                    }
                    Ok(ClientDatagram::GamepadDisconnect { id }) => {
                        gamepad_state.remove(id);
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
            log::error!("EIS: failed to create context: {e}");
            return;
        }
    };

    let (connection, mut eis_stream) = match context
        .handshake_tokio("webshooter", ei::handshake::ContextType::Sender)
        .await
    {
        Ok(pair) => pair,
        Err(e) => {
            log::error!("EIS: handshake failed: {e}");
            return;
        }
    };
    if let Err(e) = connection.flush() {
        log::error!("EIS: initial flush failed: {e}");
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
                        log::warn!("EIS: disconnected: {:?}", d.reason);
                        break;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        log::warn!("EIS: event error: {e}");
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
                            log::error!("EIS: seat bind flush: {e}");
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
                                log::info!("EIS: devices ready (all same device: {same_device})");
                                return Some((td.clone(), kd.clone(), pd.clone(), touchscreen, keyboard, pointer, button, scroll));
                            }
                        }
                    }
                    Some(Ok(EiEvent::Disconnected(d))) => {
                        log::warn!("EIS: disconnected: {:?}", d.reason);
                        return None;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        log::warn!("EIS: event error during setup: {e}");
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

#[cfg(test)]
mod tests {
    use super::*;
    use shared::client_datagram::Modifiers;
    use shared::server_datagram::ServerDatagramVariants;

    #[test]
    fn throttle_interval_is_none_when_pipeline_is_idle() {
        assert_eq!(throttle_interval_from_fill(0.0), 0);
        assert_eq!(throttle_interval_from_fill(0.3), 0);
        assert_eq!(throttle_interval_from_fill(0.49), 0);
    }

    #[test]
    fn throttle_interval_ramps_up_with_pipeline_fill() {
        assert_eq!(throttle_interval_from_fill(0.5), 8);
        assert_eq!(throttle_interval_from_fill(0.7), 16);
        assert_eq!(throttle_interval_from_fill(0.85), 32);
        assert_eq!(throttle_interval_from_fill(0.92), 64);
        assert_eq!(throttle_interval_from_fill(1.0), 128);
    }

    #[test]
    fn throttle_interval_clamps_out_of_range_fill() {
        assert_eq!(throttle_interval_from_fill(-1.0), 0);
        assert_eq!(throttle_interval_from_fill(f64::NEG_INFINITY), 0);
        assert_eq!(throttle_interval_from_fill(2.0), 128);
        assert_eq!(throttle_interval_from_fill(f64::NAN), 0); // NaN clamps to 0.0
    }

    #[test]
    fn throttle_datagram_round_trips() {
        for &interval_ms in &[0u16, 1, 16, 128, u16::MAX] {
            let dgram = ServerDatagram::Throttle { interval_ms };
            let bytes = dgram.to_bytes();
            assert_eq!(bytes.len(), 3);
            assert_eq!(ServerDatagram::from_bytes(&bytes).unwrap(), dgram);
        }
    }

    #[test]
    fn throttle_datagram_rejects_truncated_payload() {
        assert!(ServerDatagram::from_bytes(&[ServerDatagramVariants::THROTTLE.0]).is_err());
        assert!(ServerDatagram::from_bytes(&[ServerDatagramVariants::THROTTLE.0, 0x00]).is_err());
    }

    #[test]
    fn gate_treats_control_datagrams_as_unthrottled() {
        assert!(!is_input_datagram(&ClientDatagram::KeepAlive));
        assert!(!is_input_datagram(&ClientDatagram::ResizeDisplay {
            index: 0,
            width: 800,
            height: 600
        }));
        assert!(!is_input_datagram(&ClientDatagram::RequestKeyframe));
        assert!(!is_input_datagram(&ClientDatagram::Error {
            level: log::Level::Info,
            message: "hi".into()
        }));
        assert!(!is_input_datagram(&ClientDatagram::AudioReady {
            channels: 2,
            rate: 48000
        }));
        assert!(!is_input_datagram(&ClientDatagram::DecoderCapabilities { decoders: Vec::new() }));
    }

    #[test]
    fn gate_subjects_all_input_kinds_to_the_throttle() {
        assert!(is_input_datagram(&ClientDatagram::Keyboard {
            keycode: "KeyA".into(),
            modifiers: Modifiers::empty()
        }));
        assert!(is_input_datagram(&ClientDatagram::MouseMove { dx: 1, dy: -1 }));
        assert!(is_input_datagram(&ClientDatagram::MouseButton {
            button: 0,
            pressed: true
        }));
        assert!(is_input_datagram(&ClientDatagram::Scroll { dx: 1, dy: 2 }));
        assert!(is_input_datagram(&ClientDatagram::Touchscreen {
            index: 0,
            x: 10,
            y: 10
        }));
        assert!(is_input_datagram(&ClientDatagram::TouchscreenRelease { index: 0 }));
        assert!(is_input_datagram(&ClientDatagram::Gamepad {
            id: 0,
            buttons: 0,
            lx: 0,
            ly: 0,
            rx: 0,
            ry: 0,
            lt: 0,
            rt: 0,
            motion: None
        }));
        assert!(is_input_datagram(&ClientDatagram::GamepadDisconnect { id: 0 }));
    }

    #[test]
    fn coalesce_sums_mouse_and_scroll_deltas() {
        let mut q: VecDeque<ClientDatagram> = VecDeque::new();
        coalesce_input(&mut q, ClientDatagram::MouseMove { dx: 3, dy: -4 });
        coalesce_input(&mut q, ClientDatagram::MouseMove { dx: 2, dy: 1 });
        assert_eq!(q.len(), 1);
        assert_eq!(q[0], ClientDatagram::MouseMove { dx: 5, dy: -3 });

        coalesce_input(&mut q, ClientDatagram::Scroll { dx: 10, dy: 0 });
        coalesce_input(&mut q, ClientDatagram::Scroll { dx: -4, dy: 7 });
        assert_eq!(q.len(), 2);
        assert_eq!(q[1], ClientDatagram::Scroll { dx: 6, dy: 7 });
    }

    #[test]
    fn coalesce_keeps_newest_touch_and_gamepad_snapshot() {
        let mut q: VecDeque<ClientDatagram> = VecDeque::new();
        coalesce_input(&mut q, ClientDatagram::Touchscreen { index: 0, x: 10, y: 20 });
        coalesce_input(&mut q, ClientDatagram::Touchscreen { index: 0, x: 30, y: 40 });
        coalesce_input(&mut q, ClientDatagram::Touchscreen { index: 1, x: 1, y: 2 });
        assert_eq!(q.len(), 2);
        assert_eq!(q[0], ClientDatagram::Touchscreen { index: 0, x: 30, y: 40 });
        assert_eq!(q[1], ClientDatagram::Touchscreen { index: 1, x: 1, y: 2 });

        let mut q: VecDeque<ClientDatagram> = VecDeque::new();
        coalesce_input(&mut q, ClientDatagram::Gamepad {
            id: 1,
            buttons: 0b01,
            lx: 1,
            ly: 2,
            rx: 3,
            ry: 4,
            lt: 5,
            rt: 6,
            motion: None,
        });
        coalesce_input(&mut q, ClientDatagram::Gamepad {
            id: 1,
            buttons: 0b10,
            lx: 9,
            ly: 9,
            rx: 9,
            ry: 9,
            lt: 9,
            rt: 9,
            motion: None,
        });
        assert_eq!(q.len(), 1);
        assert_eq!(q[0], ClientDatagram::Gamepad {
            id: 1,
            buttons: 0b10,
            lx: 9,
            ly: 9,
            rx: 9,
            ry: 9,
            lt: 9,
            rt: 9,
            motion: None,
        });
    }

    #[test]
    fn coalesce_only_merges_consecutive_same_kind_events() {
        let mut q: VecDeque<ClientDatagram> = VecDeque::new();
        coalesce_input(&mut q, ClientDatagram::MouseMove { dx: 1, dy: 1 });
        coalesce_input(
            &mut q,
            ClientDatagram::Keyboard {
                keycode: "KeyA".into(),
                modifiers: Modifiers::CTRL,
            },
        );
        coalesce_input(&mut q, ClientDatagram::MouseMove { dx: 2, dy: 2 });
        coalesce_input(&mut q, ClientDatagram::TouchscreenRelease { index: 0 });
        // Only the tail is inspected, so the second MouseMove cannot merge with
        // the first across the Keyboard event; both are kept, in order.
        assert_eq!(q.len(), 4);
        assert_eq!(q[0], ClientDatagram::MouseMove { dx: 1, dy: 1 });
        assert_eq!(
            q[1],
            ClientDatagram::Keyboard {
                keycode: "KeyA".into(),
                modifiers: Modifiers::CTRL
            }
        );
        assert_eq!(q[2], ClientDatagram::MouseMove { dx: 2, dy: 2 });
        assert_eq!(q[3], ClientDatagram::TouchscreenRelease { index: 0 });
    }
}
