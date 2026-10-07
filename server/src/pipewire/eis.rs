use std::collections::VecDeque;
use std::os::unix::io::OwnedFd;
use std::os::unix::net::UnixStream;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;

use futures_util::StreamExt;
use futures_util::future::Either;
use reis::ei;
use reis::event::{DeviceCapability, EiEvent};
use reis::tokio::EiConvertEventStream;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use shared::client_datagram::{ClientDatagram, coalesce_input};
use shared::server_datagram::ServerDatagram;
use shared::throttle::spacing_ms_after_events;

use super::eis_keyboard::{EisKeyboardEvent, KeyboardState, send_keyboard_keys};
use super::gamepad::GamepadManager;
use super::pointer::{
    EisButtonEvent, EisPointerEvent, EisScrollEvent, MouseState, send_button_event,
    send_pointer_motion, send_scroll_event, web_button_to_linux,
};
use super::touch::{
    CoordMap, EisTouchEvent, InputRegion, SLOTS_PER_SESSION, TouchState, input_space,
    map_touch_event, offset_touch_event, send_touch_events, shift_slots,
};

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

// The floor on input spacing lives in `shared::throttle`, because the client
// applies the same one and the two must not disagree about the rate they are
// each enforcing on the same stream. `input_throttle_monitor` can still ask for
// something coarser; nothing can ask for something finer.

/// How many queued input events one emulated frame may carry.
///
/// Bounded rather than unbounded because the batch is also the delay: the
/// compositor is usually the slow part, and an unbounded batch would hold back
/// its own first event until the queue drained. Well above the number of
/// simultaneous fingers, so a real gesture never hits it.
const MAX_INPUT_BATCH: usize = 64;

/// Map the fraction of the input pipeline that is in use (0..=1) onto the
/// minimum spacing (in milliseconds) the client must keep between consecutive
/// input datagrams. 0 means "no throttling". The fill ratio is derived from
/// the channel that queues input events for the EIS thread: as it climbs
/// toward the point where a burst would block or overflow, the server is
/// nearly overloaded, so it asks the client to slow down.
fn throttle_interval_from_fill(fill: f64) -> u16 {
    let pct = (fill.clamp(0.0, 1.0) * 100.0) as u8;
    match pct {
        0..=49 => 0,   // plenty of headroom: no throttling
        50..=64 => 8,  // could get busy: cap ~125 events/s
        65..=79 => 16, // getting heavy: cap ~62 events/s
        80..=89 => 32, // heavy: cap ~31 events/s
        90..=94 => 64, // severe: cap ~15 events/s
        _ => 128,      // nearly full: cap ~8 events/s
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
/// never shorter than [`shared::throttle::INPUT_MIN_INTERVAL_MS`], so even when the fill
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
        let requested_ms = interval_ms.load(Ordering::Relaxed);
        // Wake once the cooling window elapses if throttled input is waiting;
        // otherwise park this select arm so only client events run.
        let flush_fut: Either<Pin<&mut tokio::time::Sleep>, futures_util::future::Pending<()>> =
            match flush_at.as_mut() {
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
                    let forwarded = forward_batch(&gated_tx, &mut pending).await;
                    next_allowed = tokio::time::Instant::now()
                        + Duration::from_millis(spacing_ms_after_events(forwarded, requested_ms));
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
                            next_allowed = now
                                + Duration::from_millis(spacing_ms_after_events(1, requested_ms));
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

/// Send a coalesced batch of pending input datagrams, and report how many went.
///
/// `broadcast::Sender` is best-effort: it errors only when every receiver has
/// hung up, which ends the session.
async fn forward_batch(
    gated_tx: &broadcast::Sender<ClientDatagram>,
    pending: &mut VecDeque<ClientDatagram>,
) -> usize {
    let msgs = std::mem::take(pending);
    let count = msgs.len();
    for msg in msgs {
        if gated_tx.send(msg).is_err() {
            return count;
        }
    }
    count
}

// The per-event spacing arithmetic is `shared::throttle::spacing_ms_after_events`,
// for the same reason as the floor above.
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
    frame: (u16, u16),
    client_id: u64,
    pipeline_restart: Arc<watch::Sender<Option<&'static str>>>,
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
            rt.block_on(eis_main(
                eis_fd,
                stream_pos,
                frame,
                client_id,
                pipeline_restart,
                input_rx,
                cancel_eis,
            ));
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
                    _ = cancel.cancelled() => {
                        // A client that goes away mid-gesture leaves fingers
                        // down, and nothing else will lift them: the compositor
                        // keeps them until an explicit release or the emulating
                        // connection drops. `try_send` rather than `send`,
                        // because this is teardown and must not block on a
                        // channel whose reader may already be gone.
                        for ev in touch_state.release_all() {
                            let _ = input_tx.try_send(EisInputEvent::Touch(ev));
                        }
                        break;
                    }
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
                    // Release all held modifiers if no keyboard event arrives
                    // within 1 s, so stuck modifiers don't persist. Unlike
                    // touches, this one earns its timeout: a modifier stuck down
                    // corrupts every later keystroke, and each key event
                    // re-asserts the ones that are genuinely held.
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
                        let _ = input_tx
                            .send(EisInputEvent::Button(EisButtonEvent::Button {
                                button: linux_btn,
                                pressed,
                            }))
                            .await;
                    }
                    Ok(ClientDatagram::Scroll { dx, dy }) => {
                        let _ = input_tx
                            .send(EisInputEvent::Scroll(EisScrollEvent::Scroll { dx, dy }))
                            .await;
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
    frame: (u16, u16),
    client_id: u64,
    pipeline_restart: Arc<watch::Sender<Option<&'static str>>>,
    mut input_rx: mpsc::Receiver<EisInputEvent>,
    cancel: CancellationToken,
) {
    // Keyed by client name and session id, which is what `client_id` already is:
    // the lowest id not in use, so no two live sessions share one. Saturating
    // because a base past `u32` would wrap onto another session's slots, and no
    // real session count gets near it.
    let slot_base = (client_id.saturating_mul(u64::from(SLOTS_PER_SESSION))) as u32;
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

    let (
        touch_device,
        keyboard_device,
        pointer_device,
        touchscreen,
        keyboard,
        pointer,
        button,
        scroll,
        regions,
    ) = match wait_for_devices(&mut eis_stream, &connection, &cancel).await {
        Some(result) => result,
        None => return,
    };

    // The client reports coordinates in frame pixels; libei accepts them within
    // the device's region. The region is the only authority on that space, so
    // the two are bridged here rather than guessed at from the portal's
    // reported stream size, which describes the captured region and not where
    // input lands.
    //
    // Which region is ours is decided per session, because the device is not:
    // it lists one region per monitor, so a second concurrent session's
    // virtual monitor is in that list too.
    let input_space = input_space(&regions, frame, stream_pos);
    let coord_map = match &input_space {
        Some(region) => CoordMap::new(frame, (region.w, region.h)),
        // No region could be attributed to this stream, so coordinates go
        // through untouched. Unscaled is a visible fault — touches land off by
        // the desktop's scale factor — whereas scaling by another monitor's
        // region is invisible and lands them nowhere near where they were aimed.
        None => {
            log::warn!(
                "no touch region matches this session's stream at {stream_pos:?} \
                 (frames {frame:?}, {} region(s) advertised); coordinates sent \
                 unscaled",
                regions.len()
            );
            CoordMap::new(frame, frame)
        }
    };
    log::info!(
        "touch space: frames {frame:?}, stream at {stream_pos:?}, -> {:?}",
        input_space.as_ref().map(|r| (r.w, r.h, r.x, r.y, &r.id))
    );

    let mut touch_sequence = 0u32;
    let mut touches_seen = 0u64;
    let mut last_touch = None;
    // The rate over the last window, which is the number that matters when a
    // session freezes: a cumulative count says how much arrived in total, not
    // how fast it was arriving while it was unresponsive.
    let mut window_events = 0u64;
    // A running count per session, because "is this client's touch still being
    // received" and "is it being received and then ignored" look identical from
    // the outside: both produce no complaints. An interval rather than a sleep,
    // so it keeps ticking while input flows instead of only while it does not.
    let mut liveness = tokio::time::interval(Duration::from_secs(5));
    liveness.tick().await;
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
                    // The compositor replaces its `eis_device` objects whenever
                    // the set of outputs changes — `EisContext::updateScreens`
                    // calls `EisDevice::changeDevice` on *every* connected client,
                    // which removes the old `eis_device`. libei promises a
                    // `DeviceRemoved` first, so this is where a session learns
                    // that the handle it has been sending to all along is dead,
                    // and from then on every touch is dropped in silence.
                    Some(Ok(EiEvent::DeviceRemoved(ev))) => {
                        // The handle is gone and cannot be revived: the
                        // compositor removed the object, so every event sent to
                        // it from here on is discarded without complaint. The
                        // only way back is a new device, which means a new EIS
                        // connection — so ask for the same full rebuild that a
                        // lost GPU context asks for.
                        log::warn!(
                            "EIS: session {client_id} lost device {:?}; input on it is \
                             discarded until the pipeline is rebuilt",
                            ev.device.name()
                        );
                        let _ = pipeline_restart.send(Some("input device removed by the compositor"));
                        break;
                    }
                    Some(Ok(EiEvent::DeviceResumed(_))) => {}
                    Some(Ok(EiEvent::Frame(_))) => {}
                    Some(Ok(other)) => {
                        log::debug!("EIS: {other:?}");
                    }
                    Some(Err(e)) => {
                        log::warn!("EIS: event error: {e}");
                        break;
                    }
                    None => break,
                }
            }
            _ = liveness.tick() => {
                log::info!(
                    "session {client_id}: {window_events} input events in 5s \
                     ({touches_seen} total), last touch {last_touch:?} \
                     (frames {frame:?}, slots from {slot_base})"
                );
                window_events = 0;
                continue;
            }
            cmd = input_rx.recv() => {
                let Some(first) = cmd else { break };
                // Drain everything already queued, up to a bound.
                //
                // This is the other half of the input gate's coalescing: the gate
                // hands over a *batch*, and spending four protocol messages and a
                // flush on each event inside it is what lets a multitouch flood
                // saturate the compositor — which is why the display stalls, not
                // just the input. The bound keeps latency bounded when the
                // compositor is the slow part: a huge batch would delay its own
                // first event.
                let mut batch = Vec::with_capacity(MAX_INPUT_BATCH);
                batch.push(first);
                while batch.len() < MAX_INPUT_BATCH {
                    match input_rx.try_recv() {
                        Ok(event) => batch.push(event),
                        Err(_) => break,
                    }
                }
                let batched = batch.len();

                let mut touches: Vec<EisTouchEvent> = Vec::new();
                let mut keys: Vec<EisKeyboardEvent> = Vec::new();
                let mut pointers: Vec<EisPointerEvent> = Vec::new();
                let mut buttons: Vec<EisButtonEvent> = Vec::new();
                let mut scrolls: Vec<EisScrollEvent> = Vec::new();
                for event in batch {
                    match event {
                        EisInputEvent::Touch(event) => {
                            // Frame pixels into the portal's own units first, then
                            // into screen coordinates for a multi-monitor layout.
                            let from = touch_point(&event);
                            let event =
                                offset_touch_event(map_touch_event(event, coord_map), stream_pos);
                            let Some(event) = shift_slots(event, slot_base) else {
                                log::warn!(
                                    "touch slot outside this session's range of \
                                     {SLOTS_PER_SESSION}, dropped"
                                );
                                continue;
                            };
                            // The first few touches of a session, so a session whose
                            // input goes nowhere can be told apart from one that never
                            // sent any. A count alone cannot: it is zero in both the
                            // "client is not sending" and "we are dropping them" cases,
                            // and those need opposite fixes.
                            touches_seen += 1;
                            last_touch = touch_point(&event);
                            if touches_seen <= 3 {
                                log::info!(
                                    "touch #{touches_seen} {from:?} -> {:?} slot {:?} \
                                     (frames {frame:?} at {stream_pos:?}, space {:?})",
                                    touch_point(&event),
                                    touch_slot(&event),
                                    input_space.as_ref().map(|r| (r.w, r.h))
                                );
                            }
                            touches.push(event);
                        }
                        EisInputEvent::Keyboard(ev) => {
                            window_events += 1;
                            keys.push(ev);
                        }
                        EisInputEvent::Pointer(ev) => {
                            window_events += 1;
                            pointers.push(ev);
                        }
                        EisInputEvent::Button(ev) => {
                            window_events += 1;
                            buttons.push(ev);
                        }
                        EisInputEvent::Scroll(ev) => {
                            window_events += 1;
                            scrolls.push(ev);
                        }
                    }
                }
                if batched > 1 {
                    log::debug!(
                        "session {client_id}: {} events in one frame \
                         ({} touch, {} key, {} pointer, {} button, {} scroll)",
                        batched,
                        touches.len(),
                        keys.len(),
                        pointers.len(),
                        buttons.len(),
                        scrolls.len()
                    );
                }
                send_touch_events(
                    &connection,
                    &touch_device,
                    &touchscreen,
                    &mut touch_sequence,
                    &touches,
                );
                send_keyboard_keys(
                    &connection,
                    &keyboard_device,
                    &keyboard,
                    &mut keyboard_sequence,
                    &keys,
                );
                for event in pointers {
                    send_pointer_motion(&connection, &pointer_device, &pointer, &mut pointer_sequence, event);
                }
                for event in buttons {
                    send_button_event(&connection, &pointer_device, &button, &mut button_sequence, event);
                }
                for event in scrolls {
                    send_scroll_event(&connection, &pointer_device, &scroll, &mut scroll_sequence, event);
                }
            }
        }
    }
}

/// Which slot a touch event occupies, for logging.
fn touch_slot(event: &EisTouchEvent) -> Option<u32> {
    match event {
        EisTouchEvent::Down { index, .. }
        | EisTouchEvent::Motion { index, .. }
        | EisTouchEvent::Up { index } => Some(*index),
    }
}

/// Where a touch event points, for logging. `Up` carries no position: it names
/// the slot it releases and nothing else.
fn touch_point(event: &EisTouchEvent) -> Option<(u16, u16)> {
    match event {
        EisTouchEvent::Down { x, y, .. } | EisTouchEvent::Motion { x, y, .. } => Some((*x, *y)),
        EisTouchEvent::Up { .. } => None,
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
    Vec<InputRegion>,
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

                                // The touch device states its own coordinate space, and
                                // it is the only authority on it. A region is
                                // *logical* pixels plus the physical scale that
                                // produced them, so the frames — which are physical
                                // — and the coordinates libei accepts are related by
                                // exactly that scale and by nothing else.
                                //
                                // Deriving the map from the portal's reported
                                // stream size instead gets this wrong whenever the
                                // two disagree: on a 150%-scaled desktop the frames
                                // were 1080x2255 while the portal reported 720x1503,
                                // and mapping down by that factor capped every
                                // touch at y=1502 — leaving the bottom third of the
                                // screen unreachable rather than misplaced.
                                for region in td.regions() {
                                    log::info!(
                                        "touch region: {}x{} logical at ({},{}) \
                                         scale {} (physical {}x{}) id {:?}",
                                        region.width,
                                        region.height,
                                        region.x,
                                        region.y,
                                        region.scale,
                                        region.width as f32 * region.scale,
                                        region.height as f32 * region.scale,
                                        region.mapping_id,
                                    );
                                }
                                // Every region is handed back, not just one: a
                                // device advertises a region per monitor, and
                                // which of them belongs to this session is only
                                // knowable against the frame and stream
                                // position, which live in `eis_main`. Choosing
                                // here would mean guessing from order alone.
                                //
                                // A region's `scale` is deliberately not used to
                                // derive the frame side: the protocol defines it
                                // as the factor for *relative* movement between
                                // regions, and says outright that coordinates
                                // need not match the desktop's real size — the
                                // compositor maps them itself.
                                let regions = td
                                    .regions()
                                    .iter()
                                    .map(|r| InputRegion {
                                        w: r.width as u16,
                                        h: r.height as u16,
                                        x: r.x as i32,
                                        y: r.y as i32,
                                        id: r.mapping_id.clone(),
                                    })
                                    .collect();

                                return Some((td.clone(), kd.clone(), pd.clone(), touchscreen, keyboard, pointer, button, scroll, regions));
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
        assert!(!is_input_datagram(&ClientDatagram::DisplayParameters {
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
        assert!(!is_input_datagram(&ClientDatagram::DecoderCapabilities {
            decoders: Vec::new()
        }));
    }

    #[test]
    fn gate_subjects_all_input_kinds_to_the_throttle() {
        assert!(is_input_datagram(&ClientDatagram::Keyboard {
            keycode: "KeyA".into(),
            modifiers: Modifiers::empty()
        }));
        assert!(is_input_datagram(&ClientDatagram::MouseMove {
            dx: 1,
            dy: -1
        }));
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
        assert!(is_input_datagram(&ClientDatagram::TouchscreenRelease {
            index: 0
        }));
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
        assert!(is_input_datagram(&ClientDatagram::GamepadDisconnect {
            id: 0
        }));
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
        coalesce_input(
            &mut q,
            ClientDatagram::Touchscreen {
                index: 0,
                x: 10,
                y: 20,
            },
        );
        coalesce_input(
            &mut q,
            ClientDatagram::Touchscreen {
                index: 0,
                x: 30,
                y: 40,
            },
        );
        coalesce_input(
            &mut q,
            ClientDatagram::Touchscreen {
                index: 1,
                x: 1,
                y: 2,
            },
        );
        assert_eq!(q.len(), 2);
        assert_eq!(
            q[0],
            ClientDatagram::Touchscreen {
                index: 0,
                x: 30,
                y: 40
            }
        );
        assert_eq!(
            q[1],
            ClientDatagram::Touchscreen {
                index: 1,
                x: 1,
                y: 2
            }
        );

        let mut q: VecDeque<ClientDatagram> = VecDeque::new();
        coalesce_input(
            &mut q,
            ClientDatagram::Gamepad {
                id: 1,
                buttons: 0b01,
                lx: 1,
                ly: 2,
                rx: 3,
                ry: 4,
                lt: 5,
                rt: 6,
                motion: None,
            },
        );
        coalesce_input(
            &mut q,
            ClientDatagram::Gamepad {
                id: 1,
                buttons: 0b10,
                lx: 9,
                ly: 9,
                rx: 9,
                ry: 9,
                lt: 9,
                rt: 9,
                motion: None,
            },
        );
        assert_eq!(q.len(), 1);
        assert_eq!(
            q[0],
            ClientDatagram::Gamepad {
                id: 1,
                buttons: 0b10,
                lx: 9,
                ly: 9,
                rx: 9,
                ry: 9,
                lt: 9,
                rt: 9,
                motion: None,
            }
        );
    }

    /// The original rule was "only the tail is inspected", so a Keyboard event
    /// between two mouse moves split one delta into two. That is now merged, and
    /// this test used to assert the opposite — it is here because the change is
    /// deliberate: the queue is the input gate's only bound on how many events
    /// reach the compositor, so a rule that only ever looks at the tail leaves
    /// the queue growing with the event count.
    ///
    /// What is preserved is order and the total delta: the movement still lands
    /// before the key press, it just arrives as one event instead of two.
    #[test]
    fn coalesce_merges_across_an_intervening_event_without_reordering() {
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
        assert_eq!(q.len(), 3, "the two moves must collapse into one");
        assert_eq!(
            q[0],
            ClientDatagram::MouseMove { dx: 3, dy: 3 },
            "deltas must sum, and must stay ahead of the key press"
        );
        assert_eq!(
            q[1],
            ClientDatagram::Keyboard {
                keycode: "KeyA".into(),
                modifiers: Modifiers::CTRL
            }
        );
        assert_eq!(q[2], ClientDatagram::TouchscreenRelease { index: 0 });
    }
}
