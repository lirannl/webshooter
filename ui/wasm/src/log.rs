use std::fmt::Debug;

use js_sys::{Error, Uint8Array};
use shared::client_datagram::ClientDatagram;
use wasm_bindgen::{JsValue, prelude::*};

use crate::with_wt;

pub fn log(msg: impl Debug) {
    let bytes = ClientDatagram::Error {
        message: format!("{msg:#?}"),
    }
    .to_bytes();
    let buf = Uint8Array::from(&bytes[..]);
    with_wt(|gwt| {
        let _ = gwt.writer.write_with_chunk(buf.as_ref());
    });
}

#[wasm_bindgen(js_name = "log", skip_typescript)]
pub fn js_log(val: &JsValue) {
    if let Some(msg) = val.as_string() {
        log(msg);
    } else if val.is_instance_of::<Error>() {
        let err: Error = Error::unchecked_from_js(val.clone());
        log(err);
    } else {
        panic!("Couldn't log value!");
    }
}

#[wasm_bindgen(typescript_custom_section)]
const TS_LOG: &'static str = r#"
export function log(val: string | Error): void;
"#;
