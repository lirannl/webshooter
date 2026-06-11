use tokio::sync::broadcast::error::RecvError;
use tokio::time::sleep;

use crate::extensions::AsyncRecvExt;
use crate::logging::log;

use std::collections::HashSet;
use std::time::Duration;

use tokio::spawn;

use tokio::task::JoinHandle;

use tokio_util::sync::CancellationToken;

use crate::client_datagram::ClientDatagram;

use tokio::sync::broadcast::Receiver;

use ashpd::desktop::remote_desktop::RemoteDesktop;

use std::sync::Arc;

pub fn touch_task(
    remote_desktop: Arc<RemoteDesktop>,
    session: &Arc<ashpd::desktop::Session<RemoteDesktop>>,
    node_id: u32,
    client_rx: &mut Receiver<ClientDatagram>,
    cancel: &CancellationToken,
) -> JoinHandle<()> {
    spawn({
        let remote_desktop = remote_desktop.clone();
        let session = session.clone();
        let cancel = cancel.clone();
        let mut touch_rx = client_rx.resubscribe();
        async move {
            // Slots that are currently pressed. The first event for a slot
            // is a touch-down; subsequent events are motion, as the portal
            // expects.
            let mut active_slots: HashSet<u8> = Default::default();
            loop {
                let msg = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    msg = touch_rx.recv_matching(touch_event_filter) => msg,
                    _ = sleep(Duration::from_secs(1)) => Ok(ClientDatagram::TouchscreenRelease { index: None })
                };
                match msg {
                    Ok(ClientDatagram::Touchscreen { x, y, index }) => {
                        let (x, y) = (x as f64, y as f64);
                        let slot = index as u32;
                        let result = if active_slots.insert(index) {
                            remote_desktop
                                .notify_touch_down(
                                    &session,
                                    node_id,
                                    slot,
                                    x,
                                    y,
                                    Default::default(),
                                )
                                .await
                        } else {
                            remote_desktop
                                .notify_touch_motion(
                                    &session,
                                    node_id,
                                    slot,
                                    x,
                                    y,
                                    Default::default(),
                                )
                                .await
                        };
                        if let Err(e) = result {
                            log(format!("touch down/motion failed: {e}"));
                        }
                    }
                    Ok(ClientDatagram::TouchscreenRelease { index }) => {
                        let indices: Vec<u8> = if let Some(index) = index {
                            if active_slots.remove(&index) {
                                vec![index]
                            } else {
                                vec![]
                            }
                        } else {
                            active_slots.drain().collect()
                        };
                        for index in indices {
                            if let Err(e) = remote_desktop
                                .notify_touch_up(&session, index as u32, Default::default())
                                .await
                            {
                                log(format!("touch up failed: {e}"));
                            }
                        }
                    }
                    // Unrelated datagrams (resize, keyboard, keepalive).
                    Ok(_) => continue,
                    Err(e) => {
                        match e.downcast_ref::<RecvError>() {
                            // Dropped messages under load — keep going.
                            Some(RecvError::Lagged(_)) => continue,
                            _ => break,
                        }
                    }
                }
            }
        }
    })
}

fn touch_event_filter(event: ClientDatagram) -> Option<ClientDatagram> {
    match event {
        ClientDatagram::Touchscreen { .. } => Some(event),
        ClientDatagram::TouchscreenRelease { .. } => Some(event),
        _ => None,
    }
}
