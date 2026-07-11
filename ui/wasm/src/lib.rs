mod input;
mod log;
mod video;

use js_sys::Uint8Array;
use shared::client_datagram::ClientDatagram;
use shared::codec::Codec;
use std::cell::RefCell;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    HtmlDivElement, ReadableStreamDefaultReader, WebTransport, WebTransportDatagramDuplexStream,
    WebTransportOptions, WritableStreamDefaultWriter,
};

use crate::log::log;

// ---------------------------------------------------------------------------
// Global WebTransport handle
// ---------------------------------------------------------------------------

#[allow(dead_code)]
pub(crate) struct GlobalWt {
    pub writer: WritableStreamDefaultWriter,
    pub reader: ReadableStreamDefaultReader,
    pub wt: WebTransport,
    pub max_dgram_size: f64,
}

thread_local! {
    static GLOBAL_WT: RefCell<Option<GlobalWt>> = const { RefCell::new(None) };
}

pub(crate) fn send_error(msg: &str) {
    let bytes = ClientDatagram::Error {
        message: msg.to_string(),
    }
    .to_bytes();
    let buf = Uint8Array::from(&bytes[..]);
    with_wt(|gwt| {
        let _ = gwt.writer.write_with_chunk(buf.as_ref());
    });
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

#[wasm_bindgen]
pub async fn start() -> Result<(), JsValue> {
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

    // 3. Wait for ready.
    JsFuture::from(wt.ready()).await?;

    // 4. Open datagram streams.
    let datagrams: WebTransportDatagramDuplexStream = wt.datagrams();
    let writer = datagrams.writable().get_writer().unwrap();
    let reader: ReadableStreamDefaultReader = datagrams
        .readable()
        .get_reader()
        .dyn_into()
        .expect("get_reader() did not return a ReadableStreamDefaultReader");
    let max_dgram_size: f64 = datagrams.max_datagram_size().into();

    // 5. Store in global handle.
    GLOBAL_WT.with(|cell| {
        *cell.borrow_mut() = Some(GlobalWt {
            writer,
            reader,
            wt: wt.clone(),
            max_dgram_size,
        });
    });

    // Transport ready — from here we can send errors via WT
    let result = async {
        // 6. Keepalive every 50 ms.
        let keepalive_bytes = ClientDatagram::KeepAlive.to_bytes();
        let buf = Uint8Array::from(&keepalive_bytes[..]);
        let keepalive = Closure::wrap(Box::new(move || {
            with_wt(|gwt| {
                let _ = gwt.writer.write_with_chunk(buf.as_ref());
            });
        }) as Box<dyn FnMut()>);
        let keepalive_id = window.set_interval_with_callback_and_timeout_and_arguments_0(
            keepalive.as_ref().unchecked_ref::<js_sys::Function>(),
            50,
        )?;
        keepalive.forget();

        // 7. Advertise decoder capabilities.
        video::send_decoder_capabilities().unwrap_or_else(|err| log(err));

        // 8. Canvas + video
        let canvas = video::setup_canvas();
        video::send_initial_resize(&canvas).unwrap_or_else(|err| log(err));
        video::setup_resize_prompt(&canvas);

        // 9. Render loop
        let render_loop = video::render_loop(&canvas);

        // 10. Input handlers
        input::setup_keyboard(&canvas);
        input::setup_touch(&canvas);

        // 10. Wait for render loop to finish (signals connection closed).
        if let Err(e) = render_loop.await {
            send_error(&format!("render_loop error: {e:?}"));
            show_connection_lost();
        }

        window.clear_interval_with_handle(keepalive_id);
        Ok::<(), JsValue>(())
    }
    .await;

    if let Err(e) = result {
        send_error(&format!("start error after transport ready: {e:?}"));
        show_connection_lost();
    }
    Ok(())
}
