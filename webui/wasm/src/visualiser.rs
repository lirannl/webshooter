//! A full-window rendering of the session's audio: the [`AnalyserNode`] the
//! audio player already routes every decoded buffer through on its way to the
//! speakers is read out on a fixed cadence and drawn as a row of frequency
//! bars.
//!
//! Purely a client-side decoration. It changes nothing the audio player does
//! and nothing the server sees: the same Opus frames are decoded and played
//! whether or not anything here reads the analyser.

use std::cell::Cell;
use std::rc::Rc;

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

use web_sys::{AnalyserNode, CanvasRenderingContext2d, HtmlCanvasElement};

/// Draw cadence (about 30 fps). A visualiser does not need the full display
/// refresh rate, and a fixed interval avoids the requestAnimationFrame
/// re-registration dance the closure would otherwise need to survive in.
const DRAW_INTERVAL_MS: u32 = 33;

/// How many bars to draw across the window.
const BARS: usize = 64;

/// The full-window audio visualiser for the audio-only page.
///
/// The canvas and the draw timer are owned by the timer (via a forgotten
/// closure), so the handle itself only carries the flag that stops the drawing
/// when it is dropped at session end.
pub struct Visualiser {
    stopped: Rc<Cell<bool>>,
}

impl Visualiser {
    pub fn new(analyser: &AnalyserNode) -> Visualiser {
        let window = web_sys::window().unwrap();
        let document = window.document().unwrap();
        let canvas = document
            .create_element("canvas")
            .unwrap()
            .dyn_into::<HtmlCanvasElement>()
            .unwrap();
        canvas.style().set_css_text(
            "position:fixed;inset:0;width:100%;height:100%;background:transparent;\
             pointer-events:none;z-index:1;",
        );
        document.body().unwrap().append_child(&canvas).unwrap();
        let ctx = canvas
            .get_context("2d")
            .ok()
            .flatten()
            .and_then(|v| v.dyn_into::<CanvasRenderingContext2d>().ok())
            .expect("no 2d context for audio visualiser");

        let stopped = Rc::new(Cell::new(false));

        let tick_canvas = canvas.clone();
        let tick_ctx = ctx.clone();
        let tick_analyser = analyser.clone();
        let tick_stopped = stopped.clone();
        let tick = Closure::wrap(Box::new(move || {
            if tick_stopped.get() {
                return;
            }
            draw(&tick_canvas, &tick_ctx, &tick_analyser);
        }) as Box<dyn FnMut()>);
        if let Some(win) = web_sys::window() {
            let _ = win.set_interval_with_callback_and_timeout_and_arguments_0(
                tick.as_ref().unchecked_ref(),
                DRAW_INTERVAL_MS as i32,
            );
        }
        tick.forget();

        Visualiser { stopped }
    }
}

impl Drop for Visualiser {
    fn drop(&mut self) {
        // Stop drawing. The transparent canvas is left in place; the timer
        // keeps firing but its closure bails on the flag.
        self.stopped.set(true);
    }
}

fn draw(canvas: &HtmlCanvasElement, ctx: &CanvasRenderingContext2d, analyser: &AnalyserNode) {
    let width_css = canvas.client_width() as f64;
    let height_css = canvas.client_height() as f64;
    if width_css <= 0.0 || height_css <= 0.0 {
        return;
    }

    // Backing store sized to the display density; CSS keeps the element at
    // full window. Set a dpr-mapped transform once per frame so both the
    // resize and the bar coordinates below work in CSS pixels.
    let dpr = web_sys::window()
        .map(|w| w.device_pixel_ratio())
        .unwrap_or(1.0)
        .max(1.0);
    let want_w = (width_css * dpr).round() as u32;
    let want_h = (height_css * dpr).round() as u32;
    if canvas.width() != want_w {
        canvas.set_width(want_w);
    }
    if canvas.height() != want_h {
        canvas.set_height(want_h);
    }
    // Set a dpr-scale transform. Resets what a previous frame may have left in
    // the context's transform matrix (clear_rect and the bars below both run
    // in CSS-pixel space, so the display density is the only scaling).
    let _ = ctx.set_transform(dpr, 0.0, 0.0, dpr, 0.0, 0.0);
    ctx.clear_rect(0.0, 0.0, width_css, height_css);

    // A silent analyser still answers with a flat spectrum; draw the bars it
    // reports. Frequency bins are averaged into the fixed bar count below.
    let bins = analyser.frequency_bin_count() as usize;
    if bins == 0 {
        return;
    }
    let mut data = vec![0u8; bins];
    analyser.get_byte_frequency_data(&mut data);

    let bar_w = width_css / BARS as f64;
    for i in 0..BARS {
        let lo = i * bins / BARS;
        let hi = ((i + 1) * bins / BARS).max(lo + 1);
        let avg = data[lo..hi].iter().map(|&b| b as f32).sum::<f32>() / (hi - lo) as f32;
        // Square the loudness so quiet signals get visible bars too.
        let v = (avg / 255.0) as f64;
        let bh = (v * v * height_css * 0.92).max(1.5);
        let x = i as f64 * bar_w + 0.5;
        let w = (bar_w - 1.0).max(0.5);
        ctx.set_fill_style(&JsValue::from_str(&bar_color(v)));
        ctx.fill_rect(x, height_css - bh, w, bh);
        if v < 0.02 {
            // Idle baseline so a silent stream still reads as "listening".
            ctx.set_fill_style(&JsValue::from_str("rgba(255,255,255,0.10)"));
            ctx.fill_rect(x, height_css - 1.5, w, 1.5);
        }
    }
}

/// Deep indigo at rest, hot near-white when loud — echoes the page's accent.
fn bar_color(v: f64) -> String {
    let r = (40.0 + v * 215.0) as u32;
    let g = (60.0 + v * 185.0) as u32;
    format!("rgb({r},{g},255)")
}