mod audio;
mod gamepad;
mod input;
mod log;
mod moq;
mod throttle;
mod video;
mod visualiser;

use js_sys::Uint8Array;
use shared::client_datagram::ClientDatagram;
use shared::server_datagram::ServerDatagram;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    HtmlDivElement, WebTransport, WebTransportDatagramDuplexStream, WebTransportOptions,
    WritableStream, WritableStreamDefaultWriter,
};

use crate::log::init as init_log;

// ---------------------------------------------------------------------------
// Global WebTransport handle
// ---------------------------------------------------------------------------

#[allow(dead_code)]
pub(crate) struct GlobalWt {
    pub writer: WritableStreamDefaultWriter,
    pub wt: WebTransport,
}

thread_local! {
    static GLOBAL_WT: RefCell<Option<GlobalWt>> = const { RefCell::new(None) };
}

// ---------------------------------------------------------------------------
// MoQ client connection
// ---------------------------------------------------------------------------

/// One [`moq_net::Driver`] ran by [`moq_net::time::run`]: the in-session
/// driver *and* the origin driver have to keep moving for subscriptions to
/// land on the model. `time::run` contents one driver at a time, so we fuse.
struct Both {
    session: moq_net::Driver<moq::BrowserSession>,
    origin: moq_net::origin::Driver,
}

impl moq_net::time::Driver for Both {
    fn poll(
        &mut self,
        now: moq_net::time::Instant,
        waiter: &moq_net::kio::Waiter,
    ) -> Result<Option<moq_net::time::Instant>, moq_net::Error> {
        let session_deadline = self.session.poll(now, waiter)?;
        let origin_deadline = self.origin.poll(now, waiter)?;
        Ok(match (session_deadline, origin_deadline) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(at), None) | (None, Some(at)) => Some(at),
            (None, None) => None,
        })
    }
}

/// Run the MoQ client over the session the control path already owns.
///
/// Three loops, in the order they become possible:
///
/// * the two `moq-net` drivers, which is the only thing that moves bytes or
///   subscriptions once [`moq_net::Client::connect_lite`] returns — it hands
///   back a driver rather than a completed handshake;
/// * one task per media track, each blocking on its own group reads;
/// * the control-message dispatch below, awaited *by this function* rather than
///   spawned, so that this function — and so `start` — lives exactly as long as
///   the session does. Every control message is one the session must act on
///   before it can end, so parking here is what parks the whole page; the track
///   tasks are spawned because a track ending is not the session ending.
async fn run_moq(
    session: moq::BrowserSession,
    app: moq::AppSide,
    display: Option<video::Display>,
    audio: Option<Rc<crate::audio::AudioPlayer>>,
    release_flag: Rc<Cell<bool>>,
) {
    // The origin the session inserts the server's broadcasts into, and its own
    // driver: the session driver moves the protocol and the origin driver moves
    // the content, so a subscription on the model below only lands when both
    // are stepped.
    let (sub_origin, origin_driver) =
        moq_net::origin::Producer::new(moq_net::origin::Config::default());
    let client = moq_net::Client::new().with_subscriber(sub_origin.clone());
    let consumer = sub_origin.consume();

    // `connect_lite` does not handshake: it reports which ALPN the transport
    // claims, builds the machine, and returns the driver to run. `_sess` is kept
    // alive for the rest of this function on purpose — it is one of the handles
    // that keeps the session's content alive.
    let (_sess, driver) = match client
        .connect_lite(moq_net::time::Instant::now(), session)
        .await
    {
        Ok(pair) => pair,
        Err(err) => {
            ::log::error!("MoQ handshake refused: {err:#?}");
            return;
        }
    };
    let both = Both {
        session: driver,
        origin: origin_driver,
    };
    wasm_bindgen_futures::spawn_local(async move {
        let outcome = moq_net::time::run(both).await;
        // A client session only "finishes" because it died, so the reason it
        // died is an always-want-to-know. Logged at error level because the
        // client's own log floor is `info`.
        ::log::error!("MoQ client session finished: {outcome}");
    });

    // Wait for the announcement, *then* ask for the broadcast. `request_broadcast`
    // is a question with an immediate answer: a path nothing has announced comes
    // back `Unroutable` rather than "not yet" — and the server cannot have
    // announced anything before it has read this client's SETUP, which is still in
    // flight. Asking first is how this page used to tear itself down on arrival.
    let mut announced = consumer.announced();
    loop {
        let Some(update) = announced.next().await else {
            // The origin is finished: the session is over, not merely quiet. A
            // server that never announces ends the page here rather than parking
            // on a subscription that can never resolve.
            ::log::error!("MoQ session ended before the server announced its media");
            return;
        };
        // An announcement covers the broadcast when it *is* the broadcast or a
        // shorter prefix of it — the root included, which is how an announcement
        // of everything is spelled.
        let covers = update
            .prefix
            .strip_prefix(shared::track_names::BROADCAST)
            .is_some();
        if update.kind.is_active() && covers {
            break;
        }
    }
    let broadcast = match consumer
        .request_broadcast(shared::track_names::BROADCAST)
        .await
    {
        Ok(broadcast) => broadcast,
        Err(err) => {
            ::log::error!("MoQ broadcast unavailable: {err:#?}");
            return;
        }
    };

    // The audio track exists before the announcement, so this subscription is
    // established immediately; it then waits for groups, and each group waits
    // for the sink the server only creates once AudioReady arrives.
    if let Some(audio) = audio.clone() {
        let broadcast = broadcast.clone();
        wasm_bindgen_futures::spawn_local(async move {
            audio_track_loop(broadcast, audio).await;
        });
    }

    // Control messages. The video track's name depends on a codec the server
    // only picks once it has an encoder, so it arrives here rather than being
    // knowable at setup; each one starts that codec's track task.
    use crate::video::is_installed_pwa;
    while let Some(bytes) = app.next_control().await {
        let Ok(msg) = ServerDatagram::from_bytes(&bytes) else {
            ::log::warn!("unparsable server message, ignored");
            continue;
        };
        match msg {
            ServerDatagram::AudioLevel { level } => {
                if let Some(audio) = &audio {
                    audio.set_volume(level);
                }
            }
            ServerDatagram::LogLevel { level } => crate::log::apply_server_level(level),
            ServerDatagram::ReleaseMouse => release_flag.set(true),
            ServerDatagram::Throttle { interval_ms } => crate::throttle::set_throttle(interval_ms),
            ServerDatagram::ToggleFullscreen => {
                let Some(display) = display.as_ref() else {
                    continue;
                };
                let document = web_sys::window().unwrap().document().unwrap();
                if document.fullscreen_element().is_some() {
                    document.exit_fullscreen();
                } else if is_installed_pwa(&web_sys::window().unwrap()) {
                    let _entered_fullscreen = display.canvas.request_fullscreen();
                } else {
                    // Not a gesture yet: the resize prompt's pointerdown applies it.
                    display.pending_fullscreen.set(true);
                }
            }
            ServerDatagram::VideoTrack { codec } => {
                let Some(display) = display.as_ref() else {
                    continue;
                };
                let Ok(path) = video::frame_path(&display.canvas) else {
                    continue;
                };
                let broadcast = broadcast.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    video::run_video_track(&broadcast, codec, path).await;
                });
            }
        }
    }
}

/// Drain the audio track: each group is a run of Opus packets, and each packet
/// is a whole frame of audio, so a group ends at the publisher's boundary
/// rather than at anything the decoder cares about.
async fn audio_track_loop(
    broadcast: moq_net::broadcast::Consumer,
    audio: Rc<crate::audio::AudioPlayer>,
) {
    let track = match broadcast.track(shared::track_names::AUDIO_TRACK) {
        Ok(track) => track,
        Err(err) => {
            ::log::error!("audio track not in the broadcast: {err:#?}");
            return;
        }
    };
    let mut subscribed = match track
        .subscribe(moq_net::track::Subscription::default())
        .await
    {
        Ok(subscribed) => subscribed,
        Err(err) => {
            ::log::error!("audio subscribe failed: {err:#?}");
            return;
        }
    };
    loop {
        let mut group = match subscribed.recv_group().await {
            Ok(Some(group)) => group,
            Ok(None) => {
                ::log::debug!("audio track finished");
                return;
            }
            Err(err) => {
                ::log::warn!("audio track ended: {err:#?}");
                return;
            }
        };
        loop {
            match group.read_frame().await {
                Ok(Some(frame)) => audio.push(frame.payload.to_vec()),
                // The group is done: its next sibling is a different group.
                Ok(None) => break,
                // One group's stream failed. The next group is a new stream, so
                // carrying on loses at most the rest of this group.
                Err(err) => {
                    ::log::warn!("audio group stream error: {err:#?}");
                    break;
                }
            }
        }
    }
}

pub(crate) fn with_wt<F, R>(f: F) -> R
where
    F: FnOnce(&mut GlobalWt) -> R,
{
    GLOBAL_WT.with(|cell| {
        let mut opt = cell.borrow_mut();
        f(opt.as_mut().expect("WebTransport not initialised"))
    })
}

/// Send an arbitrary [`ClientDatagram`] to the server over the WebTransport
/// datagram stream.
pub(crate) fn send_datagram(d: shared::client_datagram::ClientDatagram) {
    let bytes = d.to_bytes();
    let buf = Uint8Array::from(&bytes[..]);
    with_wt(|gwt| {
        let _ = gwt.writer.write_with_chunk(buf.as_ref());
    });
}

/// Send a [`ClientDatagram`] *reliably*, over a freshly opened unidirectional
/// stream rather than a datagram. Datagrams are best-effort and can be silently
/// dropped in transit; streams are ordered and delivered. Use this for state
/// transitions whose loss would leave the server with a stale virtual device —
/// most importantly `GamepadDisconnect`, which must always tear down the host's
/// virtual controller. Returns `false` when the transport is gone or the send
/// failed (the datagram path is unaffected by a failure here).
pub(crate) async fn send_reliable(d: shared::client_datagram::ClientDatagram) -> bool {
    let bytes = d.to_bytes();
    let buf = Uint8Array::from(&bytes[..]);
    let wt = match GLOBAL_WT.with(|cell| cell.borrow().as_ref().map(|g| g.wt.clone())) {
        Some(wt) => wt,
        None => return false,
    };
    let stream_promise = wt.create_unidirectional_stream();
    let stream: WritableStream = match JsFuture::from(stream_promise).await {
        Ok(v) => match v.dyn_into() {
            Ok(s) => s,
            Err(_) => return false,
        },
        Err(_) => return false,
    };
    let writer = match stream.get_writer() {
        Ok(w) => w,
        Err(_) => return false,
    };
    if JsFuture::from(writer.write_with_chunk(&JsValue::from(buf)))
        .await
        .is_err()
    {
        return false;
    }
    JsFuture::from(writer.close()).await.is_ok()
}

// ---------------------------------------------------------------------------
// Server-initiated streams
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Deferred reporting
// ---------------------------------------------------------------------------

/// Where a message that could not be delivered over a dying transport is
/// parked for the next session.
const DEFERRED_REPORT_KEY: &str = "webshooter.deferred-report";

/// Stash a message for the *next* session's server log.
///
/// The WebTransport session is the only live channel to the server, so anything
/// worth reporting at the moment it dies has nowhere to go: the record is
/// written to a writer that is already closing. `sessionStorage` is same-origin
/// and survives the reload, so the next connect can carry it instead of the
/// detail being lost. Best-effort throughout — a browser with storage disabled
/// simply drops it, and the console mirror is all that is left.
fn defer_report(msg: &str) {
    let stored = web_sys::window()
        .and_then(|window| window.session_storage().ok().flatten())
        .is_some_and(|storage| storage.set_item(DEFERRED_REPORT_KEY, msg).is_ok());
    if !stored {
        web_sys::console::warn_1(&"(previous session's close reason could not be deferred)".into());
    }
}

/// Report whatever [`defer_report`] parked, now that the transport can carry it.
fn flush_deferred_report() {
    let parked = web_sys::window()
        .and_then(|window| window.session_storage().ok().flatten())
        .and_then(|storage| {
            let previous = storage.get_item(DEFERRED_REPORT_KEY).ok().flatten()?;
            storage.remove_item(DEFERRED_REPORT_KEY).ok()?;
            Some(previous)
        });
    if let Some(parked) = parked {
        ::log::error!("from the previous session: {parked}");
    }
}

fn show_connection_lost() -> Option<()> {
    let document = web_sys::window()?.document()?;
    let body = document.body()?;

    // Remove existing canvas
    if let Some(old_canvas) = document.query_selector("canvas").ok().flatten() {
        let _ = old_canvas.remove();
    }
    // Create connection lost overlay
    let div = document
        .create_element("div")
        .ok()
        .and_then(|d| d.dyn_into::<HtmlDivElement>().ok())?;
    let _ = div.set_attribute("style", "position:fixed;inset:0;background:rgba(0,0,0,0.9);color:white;display:flex;flex-direction:column;align-items:center;justify-content:center;font-family:sans-serif;z-index:9999;");
    div.set_inner_html("<h2 style='margin:0 0 1rem;'>Connection lost</h2><p style='margin:0;color:#aaa;'>The server disconnected. Please refresh the page to reconnect.</p>");
    let _ = body.append_child(&div);

    Some(())
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Open a session and run it until the connection ends.
///
/// `audio_only` is how the page asks for a session with no display, and it is
/// deliberately a *client* decision the server has no way to be told about.
/// The server's capture is gated on the first
/// [`ClientDatagram::ResizeDisplay`] and parks until one arrives, so a page
/// that never announces a size never brings a virtual monitor, a portal
/// dialog, an encoder or an input path into existence — while the transport,
/// the keepalive, the audio sink and the control channel are all built exactly
/// as they are for a video session. Getting this wrong in either direction
/// costs a quiet failure rather than a loud one: an audio-only session that
/// sent a size would capture a display nobody watches, and a video session
/// that withheld one would wait forever.
#[wasm_bindgen]
pub async fn start(audio_only: bool) -> Result<(), JsValue> {
    init_log();
    let window = web_sys::window().ok_or("no window")?;
    let location = window.location();
    let href = location.href()?;

    // 1. Negotiate — fetch token + server certificate hash.
    let negotiate_resp: web_sys::Response = JsFuture::from(window.fetch_with_str("negotiate_wt"))
        .await?
        .dyn_into()?;
    let token = negotiate_resp
        .headers()
        .get("token")?
        .ok_or_else(|| JsValue::from("no token header"))?;
    let cert_hash_buf = JsFuture::from(negotiate_resp.array_buffer()?).await?;
    let cert_hash_arr = Uint8Array::new(&cert_hash_buf);
    let cert_hash_js: JsValue = cert_hash_arr.buffer().into();

    // 2. Build WebTransportOptions with serverCertificateHashes.
    let opts = WebTransportOptions::new();
    js_sys::Reflect::set(&opts, &"requireUnreliable".into(), &JsValue::TRUE)?;
    let hash_entry = js_sys::Object::new();
    js_sys::Reflect::set(&hash_entry, &"algorithm".into(), &"sha-256".into())?;
    js_sys::Reflect::set(&hash_entry, &"value".into(), &cert_hash_js)?;
    let hashes = js_sys::Array::new();
    hashes.push(&hash_entry);
    js_sys::Reflect::set(&opts, &"serverCertificateHashes".into(), &hashes)?;

    let url = format!("{}?token={}", href, token);
    let wt = WebTransport::new_with_options(&url, &opts)?;

    // Report *why* the transport died. The browser is the only party that knows:
    // it can tell a network change, a failed handshake and an aborted session
    // apart, and it puts the reason in the close event. Nothing else in the
    // pipeline can — the server sees an already-dead QUIC connection with no
    // close reason recorded, and reports that as its own ambiguous fallback. With
    // no listener here the teardown is silent from the client's side, and a
    // session lost on a marginal link is indistinguishable from one the user
    // simply closed.
    let closed = Closure::wrap(Box::new(move |event: web_sys::CloseEvent| {
        let reason = event.reason();
        let report = format!(
            "WebTransport closed: code={} clean={} reason={reason:?}",
            event.code(),
            event.was_clean(),
        );
        ::log::error!("{report}");
        // This runs *because* the session ended, so the record above is written
        // to a writer that is already closing and will most likely never reach
        // the server. Park it so the next connect delivers it instead.
        defer_report(&report);
    }) as Box<dyn FnMut(web_sys::CloseEvent)>);
    // Set reflectively: web-sys models `WebTransport` as extending `Object`
    // rather than `EventTarget`, so there is no typed `add_event_listener` for
    // it, but `onclose` is a real IDL attribute and assigning it is equivalent.
    js_sys::Reflect::set(&wt, &JsValue::from_str("onclose"), closed.as_ref())
        .map_err(|err| ::log::warn!("could not observe WebTransport close: {err:?}"))
        .ok();
    closed.forget();

    // 3. Wait for ready.
    JsFuture::from(wt.ready()).await?;

    // 4. Open datagram streams.
    let datagrams: WebTransportDatagramDuplexStream = wt.datagrams();
    let writer = datagrams.writable().get_writer().unwrap();

    // 5. Store in global handle.
    GLOBAL_WT.with(|cell| {
        *cell.borrow_mut() = Some(GlobalWt {
            writer,
            wt: wt.clone(),
        });
    });

    // Anything the last session could not report over its own dying transport,
    // now that this one can carry it.
    flush_deferred_report();

    // Transport ready — from here we can send errors via WT
    let result = async {
        // 7. Keepalive every 50 ms.
        let keepalive = Closure::wrap(Box::new(|| {
            send_datagram(ClientDatagram::KeepAlive);
        }) as Box<dyn FnMut()>);
        let keepalive_id = window.set_interval_with_callback_and_timeout_and_arguments_0(
            keepalive.as_ref().unchecked_ref::<js_sys::Function>(),
            50,
        )?;
        keepalive.forget();

        // 8. Advertise decoder capabilities. Sent by both kinds of session: it
        // costs one datagram, needs no display, and the server only reads it
        // when a capture actually starts.
        video::send_decoder_capabilities()
            .unwrap_or_else(|err| ::log::warn!("decoder capabilities not sent: {err:#?}"));

        // 9. The display, which is the whole of the difference between the two
        // kinds of session. Building it is what sends the first
        // `ResizeDisplay`, which is the server's signal to capture -- so
        // leaving it out is what leaves a session with no virtual monitor and
        // no input path.
        let display = if audio_only {
            ::log::info!("audio-only session: no display, no input");
            None
        } else {
            let canvas = video::setup_canvas();
            video::send_initial_resize(&canvas)
                .unwrap_or_else(|err| ::log::warn!("initial resize not sent: {err:#?}"));
            let pending_fullscreen = video::setup_resize_prompt(&canvas);
            Some(video::Display {
                canvas,
                pending_fullscreen,
            })
        };

        // 10. MoQ rides the session the control path already holds. From here
        // the pumps own both of its reads and route each message by its first
        // byte, so nothing below may read the WebTransport itself: a second
        // reader would take messages from whichever half polled first.
        let release_flag = Rc::new(Cell::new(false));
        // The datagram writer is handed to MoQ rather than kept private: both
        // protocols' datagrams go out through the one writer the stream admits.
        let writer = with_wt(|wt| wt.writer.clone());
        let (session, app_side) = moq::attach(wt.clone(), writer);
        // One player, shared: the track task decodes into it and the control
        // loop changes its volume, and a cloned player would not be the same
        // object for both.
        let audio = crate::audio::AudioPlayer::new().map(Rc::new);
        if audio.is_none() {
            ::log::error!(
                "audio: AudioPlayer::new returned None — no Opus AudioDecoder / AudioContext"
            );
        }
        // The status element, start button and visualiser are /audio-page
        // decorations: a video session keeps the whole screen for the remote
        // desktop.
        let _audio_page_ui = match (&audio, &display) {
            (Some(player), None) => {
                let container = video::create_audio_container();
                let status =
                    crate::audio::status_element_with_activity(&container, player.activity());
                crate::audio::audio_start_button(&container, &player.audio_context(), status);
                Some(crate::visualiser::Visualiser::new(
                    &player.analyser(),
                    &container,
                ))
            }
            _ => None,
        };
        if audio.is_none() && display.is_none() {
            let container = video::create_audio_container();
            crate::audio::status_element_unavailable(&container);
        }
        let mq_client = run_moq(
            session,
            app_side,
            display.clone(),
            audio,
            release_flag.clone(),
        );

        // 11. Input handlers, bound to the display they are injected into, and
        // skipped with it rather than on their own account: there is no virtual
        // monitor on the host for the server to inject into, so an input event
        // here would have nowhere to go but the real devices.
        if let Some(display) = &display {
            input::setup_keyboard(&display.canvas);
            input::setup_touch(&display.canvas);
            gamepad::setup_gamepad();
            input::setup_mouse(&display.canvas, release_flag);
        }

        // 12. `mq_client` parks on the control path, which only ends when the
        // session does — the same terminal signal the render loop used to give,
        // and the one the input handlers above stay live for.
        mq_client.await;
        show_connection_lost();

        window.clear_interval_with_handle(keepalive_id);
        Ok::<(), JsValue>(())
    }
    .await;

    if let Err(e) = result {
        ::log::error!("start error after transport ready: {e:?}");
        show_connection_lost();
    }
    Ok(())
}
