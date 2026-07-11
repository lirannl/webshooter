use crate::{log::log, with_wt};
use shared::client_datagram::ClientDatagram;
use shared::codec::Codec;
use shared::server_datagram::ServerDatagram;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{CanvasRenderingContext2d, EncodedVideoChunk, HtmlCanvasElement, VideoFrame};

// ---------------------------------------------------------------------------
// Canvas
// ---------------------------------------------------------------------------

pub fn setup_canvas() -> HtmlCanvasElement {
    let document = web_sys::window().unwrap().document().unwrap();
    let canvas = document
        .create_element("canvas")
        .unwrap()
        .dyn_into::<HtmlCanvasElement>()
        .unwrap();
    canvas.style().set_css_text(
        "position:fixed;inset:0;width:100%;height:100%;background:#000;cursor:pointer;",
    );
    document.body().unwrap().append_child(&canvas).unwrap();
    canvas
}

pub fn send_initial_resize(canvas: &HtmlCanvasElement) -> Result<(), JsError> {
    let window = web_sys::window().ok_or(JsError::new("Window not found"))?;
    let w = canvas.offset_width() as f64;
    let h = canvas.offset_height() as f64;
    let msg = ClientDatagram::ResizeDisplay {
        index: 0,
        width: (w * window.device_pixel_ratio()) as u16,
        height: (h * window.device_pixel_ratio()) as u16,
    };
    let bytes = msg.to_bytes();
    let buf = js_sys::Uint8Array::from(&bytes[..]);
    with_wt(|gwt| {
        let _ = gwt.writer.write_with_chunk(buf.as_ref());
    });
    Ok(())
}

// ---------------------------------------------------------------------------
// Resize prompt
// ---------------------------------------------------------------------------

pub fn setup_resize_prompt(canvas: &HtmlCanvasElement) {
    let window = web_sys::window().unwrap();
    let performance = window.performance().unwrap();

    // Debounced resize sender: 2s cooldown after initial send.
    // Not started as None because send_initial_resize already fired before
    // the ResizeObserver was attached — its first callback would duplicate.
    let last_sent = RefCell::new(Some(performance.now()));

    let send_resize = move || -> Result<(), JsError> {
        let now = performance.now();
        let should_send = {
            let mut last = last_sent.borrow_mut();
            if let Some(last_time) = *last {
                if now - last_time >= 2000.0 {
                    *last = Some(now);
                    true
                } else {
                    false
                }
            } else {
                *last = Some(now);
                true
            }
        };
        if should_send {
            let window = web_sys::window().ok_or(JsError::new("Window not found"))?;
            let w = canvas.offset_width() as f64;
            let h = canvas.offset_height() as f64;
            let msg = ClientDatagram::ResizeDisplay {
                index: 0,
                width: (w * window.device_pixel_ratio()) as u16,
                height: (h * window.device_pixel_ratio()) as u16,
            };
            let bytes = msg.to_bytes();
            let buf = js_sys::Uint8Array::from(&bytes[..]);
            with_wt(|gwt| {
                let _ = gwt.writer.write_with_chunk(buf.as_ref());
            });
        }
        Ok(())
    };

    let resize_cb = Closure::wrap(Box::new(move || {
        send_resize().unwrap_or_else(|err| log(err));
    }) as Box<dyn FnMut()>);

    let ro = web_sys::ResizeObserver::new(resize_cb.as_ref().unchecked_ref::<js_sys::Function>())
        .unwrap();
    ro.observe(canvas);
    resize_cb.forget();

    let fullscreen_cb = Closure::wrap(Box::new(move || {
        let window = web_sys::window().unwrap();
        let document = window.document().unwrap();
        let w = std::cmp::max(
            document
                .document_element()
                .map(|e| e.client_width())
                .unwrap_or(0) as u16,
            window
                .inner_width()
                .ok()
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0) as u16,
        );
        let h = std::cmp::max(
            document
                .document_element()
                .map(|e| e.client_height())
                .unwrap_or(0) as u16,
            window
                .inner_height()
                .ok()
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0) as u16,
        );
        let msg = ClientDatagram::ResizeDisplay {
            index: 0,
            width: w * window.device_pixel_ratio() as u16,
            height: h * window.device_pixel_ratio() as u16,
        };
        let bytes = msg.to_bytes();
        let buf = js_sys::Uint8Array::from(&bytes[..]);
        with_wt(|gwt| {
            let _ = gwt.writer.write_with_chunk(buf.as_ref());
        });
    }) as Box<dyn FnMut()>);
    let _ = canvas.add_event_listener_with_callback(
        "fullscreenchange",
        fullscreen_cb.as_ref().unchecked_ref::<js_sys::Function>(),
    );
    fullscreen_cb.forget();
}

// ---------------------------------------------------------------------------
// Codec capability probing
// ---------------------------------------------------------------------------

pub fn send_decoder_capabilities() -> Result<(), JsError> {
    let decoders = probe_codecs();
    let msg = ClientDatagram::DecoderCapabilities { decoders };
    let bytes = msg.to_bytes();
    let buf = js_sys::Uint8Array::from(&bytes[..]);
    with_wt(|gwt| {
        let _ = gwt.writer.write_with_chunk(buf.as_ref());
    });
    log(&format!("sent decoder capabilities"));
    Ok(())
}

fn probe_codecs() -> Vec<Codec> {
    // VideoDecoder.isConfigSupported is async and may not be available in
    // all contexts.  Use MediaSource.isTypeSupported as a sync fallback.
    let mut supported = Vec::new();
    for codec in &Codec::ALL {
        let codec_str = codec.web_codec_string();
        let mime = format!("video/mp4; codecs=\"{codec_str}\"");
        if web_sys::MediaSource::is_type_supported(&mime) {
            supported.push(*codec);
        }
    }
    // Fallback: if MediaSource is not available or returned nothing, try
    // VideoDecoder.isConfigSupported (async) — but for simplicity, just
    // always include AV1 and H.264 as a safe fallback.
    if supported.is_empty() {
        supported.push(Codec::Av1);
        supported.push(Codec::H264);
    }
    log(&format!("probed codecs: {supported:?}"));
    supported
}

// ---------------------------------------------------------------------------
// Frame reassembly
// ---------------------------------------------------------------------------

struct PendingFrame {
    fragments: Vec<Option<Vec<u8>>>,
    is_keyframe: bool,
    received: usize,
}

fn u16_leq(a: u16, b: u16) -> bool {
    ((b.wrapping_sub(a)) & 0xffff) < 0x8000
}

// ---------------------------------------------------------------------------
// Render loop
// ---------------------------------------------------------------------------

pub async fn render_loop(canvas: &HtmlCanvasElement) -> Result<(), JsValue> {
    let canvas = canvas.clone();
    let ctx = canvas
        .get_context("2d")
        .ok()
        .flatten()
        .and_then(|v| v.dyn_into::<CanvasRenderingContext2d>().ok())
        .expect("no 2d context");

    // VideoDecoder callbacks
    let output_cb = {
        let ctx = ctx.clone();
        let canvas = canvas.clone();
        Closure::wrap(Box::new(move |frame: VideoFrame| {
            if canvas.width() != frame.display_width() || canvas.height() != frame.display_height()
            {
                canvas.set_width(frame.display_width());
                canvas.set_height(frame.display_height());
            }
            let _ = ctx.draw_image_with_video_frame(&frame, 0.0, 0.0);
            frame.close();
        }) as Box<dyn FnMut(VideoFrame)>)
    };

    let error_cb = Closure::wrap(Box::new(move |err: JsValue| {
        web_sys::console::error_1(&format!("VideoDecoder error: {:?}", err).into());
    }) as Box<dyn FnMut(JsValue)>);

    // Build init dict via Reflect.set for compatibility.
    let init = js_sys::Object::new();
    js_sys::Reflect::set(&init, &"output".into(), output_cb.as_ref().unchecked_ref()).ok();
    js_sys::Reflect::set(&init, &"error".into(), error_cb.as_ref().unchecked_ref()).ok();

    let decoder = web_sys::VideoDecoder::new(init.unchecked_ref::<web_sys::VideoDecoderInit>())
        .map_err(|_| web_sys::console::error_1(&"Failed to create VideoDecoder".into()))
        .ok();

    let decoder = match decoder {
        Some(d) => d,
        None => {
            log("VideoDecoder creation failed");
            return Err(JsValue::from_str("VideoDecoder creation failed"));
        }
    };

    // Decoder is configured on first frame (or when codec changes).
    let current_codec: Rc<RefCell<Option<Codec>>> = Rc::new(RefCell::new(None));

    let pending: Rc<RefCell<HashMap<u16, PendingFrame>>> = Rc::new(RefCell::new(HashMap::new()));

    loop {
        // Read one datagram from the reader.
        let promise = with_wt(|gwt| gwt.reader.read());
        let result = JsFuture::from(promise).await;

        let data: Vec<u8> = match result {
            Ok(val) => {
                if val.is_undefined() || val.is_null() {
                    log("render_loop: stream ended (null/undefined)");
                    return Err(JsValue::from_str("stream ended"));
                }
                let done = js_sys::Reflect::get(&val, &"done".into())
                    .ok()
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if done {
                    log("render_loop: stream done");
                    return Err(JsValue::from_str("stream done"));
                }
                let value = match js_sys::Reflect::get(&val, &"value".into()) {
                    Ok(v) if !v.is_undefined() && !v.is_null() => v,
                    _ => {
                        log("render_loop: missing value");
                        return Err(JsValue::from_str("missing value"));
                    }
                };
                let arr = js_sys::Uint8Array::new(&value);
                let mut buf = vec![0u8; arr.length() as usize];
                arr.copy_to(&mut buf);
                buf
            }
            Err(e) => {
                log(&format!("render_loop: read error: {e:?}"));
                return Err(e);
            }
        };

        let msg = match ServerDatagram::from_bytes(&data) {
            Ok(m) => m,
            Err(_) => continue,
        };

        let ServerDatagram::VideoFrame {
            frame_id,
            frag_idx,
            num_frags,
            is_keyframe,
            codec,
            payload,
        } = msg;

        let assembled = {
            let mut map = pending.borrow_mut();
            let entry = map.entry(frame_id).or_insert_with(|| PendingFrame {
                fragments: vec![None; num_frags as usize],
                is_keyframe: false,
                received: 0,
            });
            if is_keyframe {
                entry.is_keyframe = true;
            }
            if entry.fragments[frag_idx as usize].is_some() {
                continue;
            }
            entry.fragments[frag_idx as usize] = Some(payload);
            entry.received += 1;

            if entry.received < entry.fragments.len() {
                continue;
            }

            let total: usize = entry
                .fragments
                .iter()
                .map(|f| f.as_ref().map_or(0, |v| v.len()))
                .sum();
            let mut assembled = Vec::with_capacity(total);
            for frag in entry.fragments.iter() {
                if let Some(d) = frag {
                    assembled.extend_from_slice(d);
                }
            }
            let is_keyframe = entry.is_keyframe;

            let keys: Vec<u16> = map.keys().copied().collect();
            for id in keys {
                if u16_leq(id, frame_id) {
                    map.remove(&id);
                }
            }

            (is_keyframe, assembled)
        };

        let (entry_is_keyframe, assembled) = assembled;

        // Reconfigure the decoder if the codec changed.
        {
            let mut cur = current_codec.borrow_mut();
            if *cur != Some(codec) {
                log(&format!("codec changed: {cur:?} -> {codec:?}"));
                let config = js_sys::Object::new();
                js_sys::Reflect::set(
                    &config,
                    &"codec".into(),
                    &codec.web_codec_string().into(),
                )
                .ok();
                js_sys::Reflect::set(
                    &config,
                    &"optimizeForLatency".into(),
                    &JsValue::TRUE,
                )
                .ok();
                decoder.configure(config.unchecked_ref::<web_sys::VideoDecoderConfig>());
                *cur = Some(codec);
            }
        }

        let chunk_init = js_sys::Object::new();
        let chunk_type = if entry_is_keyframe {
            JsValue::from_str("key")
        } else {
            JsValue::from_str("delta")
        };
        js_sys::Reflect::set(&chunk_init, &"type".into(), &chunk_type).ok();
        js_sys::Reflect::set(
            &chunk_init,
            &"timestamp".into(),
            &JsValue::from_f64((frame_id as u64 * 1000) as f64),
        )
        .ok();
        let data_arr = js_sys::Uint8Array::from(&assembled[..]);
        js_sys::Reflect::set(&chunk_init, &"data".into(), &data_arr).ok();

        let chunk =
            EncodedVideoChunk::new(chunk_init.unchecked_ref::<web_sys::EncodedVideoChunkInit>());
        match chunk {
            Ok(chunk) => decoder.decode(&chunk),
            Err(e) => {
                log(&format!("EncodedVideoChunk creation failed: {e:?}"));
            }
        }
    }
}
