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

enum EisTouchEvent {
    Down { x: u16, y: u16, index: u8 },
    Motion { x: u16, y: u16, index: u8 },
    Up { index: u8 },
}

pub fn touch_task(
    eis_fd: OwnedFd,
    stream_pos: (i32, i32),
    client_rx: &mut Receiver<ClientDatagram>,
    cancel: &CancellationToken,
) -> JoinHandle<()> {
    let (touch_tx, touch_rx) = mpsc::channel::<EisTouchEvent>(64);
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
            rt.block_on(eis_main(eis_fd, stream_pos, touch_rx, cancel_eis));
        })
        .expect("eis thread");

    // Async forwarding task on the main tokio runtime. Reads touch events
    // from the broadcast channel and sends them to the EIS thread.
    let mut touch_rx = client_rx.resubscribe();
    tokio::spawn({
        let cancel = cancel_task;
        async move {
            let mut active_slots: HashSet<u8> = Default::default();
            loop {
                let msg = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    msg = touch_rx.recv() => msg,
                    // Release all touch points if no event arrives within 1 s,
                    // so stuck touches don't persist after disconnect.
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {
                        let indices: Vec<u8> = active_slots.drain().collect();
                        for idx in indices {
                            let _ = touch_tx.send(EisTouchEvent::Up { index: idx }).await;
                        }
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
                        let _ = touch_tx.send(ev).await;
                    }
                    Ok(ClientDatagram::TouchscreenRelease { index }) => {
                        if active_slots.remove(&index) {
                            let _ = touch_tx.send(EisTouchEvent::Up { index }).await;
                        }
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
    mut touch_rx: mpsc::Receiver<EisTouchEvent>,
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

    // Wait for a touch device to be added and resumed.
    let device = match wait_for_touch_device(&mut eis_stream, &connection, &cancel).await {
        Some(d) => d,
        None => return,
    };

    let touchscreen = match device.interface::<ei::Touchscreen>() {
        Some(ts) => ts,
        None => {
            log("EIS: no touchscreen interface on device");
            return;
        }
    };

    let mut sequence = 0u32;
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
            cmd = touch_rx.recv() => {
                match cmd {
                    Some(event) => {
                        let event = offset_touch_event(event, stream_pos);
                        send_touch_event(&connection, &device, &touchscreen, &mut sequence, event);
                    }
                    None => break,
                }
            }
        }
    }
}

async fn wait_for_touch_device(
    eis_stream: &mut EiConvertEventStream,
    connection: &reis::event::Connection,
    cancel: &CancellationToken,
) -> Option<reis::event::Device> {
    let mut touch_device = None;
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return None,
            event = eis_stream.next() => {
                match event {
                    Some(Ok(EiEvent::SeatAdded(ev))) => {
                        ev.seat.bind_capabilities((DeviceCapability::Touch).into());
                        if let Err(e) = connection.flush() {
                            log(format!("EIS: seat bind flush: {e}"));
                        }
                    }
                    Some(Ok(EiEvent::DeviceAdded(ev))) => {
                        if ev.device.has_capability(DeviceCapability::Touch) {
                            touch_device = Some(ev.device.clone());
                        }
                    }
                    Some(Ok(EiEvent::DeviceResumed(_))) => {
                        if touch_device.is_some() {
                            return touch_device;
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

fn timestamp_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as u64
}
