use shared::client_datagram::ClientDatagram;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    ReadableStreamDefaultReader, WritableStreamDefaultWriter,
    WebTransport, WebTransportDatagramDuplexStream,
};

pub struct WebTransportWrapper {
    pub wt: WebTransport,
    pub writer: WritableStreamDefaultWriter,
    pub reader: ReadableStreamDefaultReader,
    max_dgram_size: f64,
}

impl WebTransportWrapper {
    pub async fn connect(url: &str) -> Result<Self, JsValue> {
        let wt = WebTransport::new(url)?;
        let ready = JsFuture::from(wt.ready());
        ready.await?;

        let datagrams: WebTransportDatagramDuplexStream = wt.datagrams();
        let readable = datagrams.readable();
        let writable = datagrams.writable();
        let writer = writable.get_writer();
        let reader = readable.get_reader();
        let max_dgram_size = datagrams.max_datagram_size();

        Ok(Self {
            wt,
            writer,
            reader,
            max_dgram_size,
        })
    }

    pub async fn send(&mut self, msg: &ClientDatagram) -> Result<(), JsValue> {
        let bytes = msg.to_bytes();
        let len = bytes.len() as f64;

        if len >= self.max_dgram_size {
            let stream_promise = self.wt.create_unidirectional_stream();
            let stream_val = JsFuture::from(stream_promise).await?;
            let stream: web_sys::WritableStream = stream_val.dyn_into()?;
            let uni_writer = stream.get_writer();
            let buf = js_sys::Uint8Array::from(&bytes[..]);
            JsFuture::from(uni_writer.write(&JsValue::from(buf))).await?;
            JsFuture::from(uni_writer.close()).await?;
        } else {
            let buf = js_sys::Uint8Array::from(&bytes[..]);
            JsFuture::from(self.writer.write(&JsValue::from(buf))).await?;
        }

        Ok(())
    }

    pub fn closed(&self) -> js_sys::Promise {
        self.wt.closed()
    }

    pub async fn read_datagram(&mut self) -> Result<Option<Vec<u8>>, JsValue> {
        let promise = self.reader.read();
        let result = JsFuture::from(promise).await?;

        if result.is_undefined() || result.is_null() {
            return Ok(None);
        }

        let done = js_sys::Reflect::get(&result, &"done".into())
            .ok()
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if done {
            return Ok(None);
        }

        let value = js_sys::Reflect::get(&result, &"value".into())?;
        if value.is_undefined() || value.is_null() {
            return Ok(None);
        }

        let arr = js_sys::Uint8Array::new(&value);
        let mut buf = vec![0u8; arr.length() as usize];
        arr.copy_to(&mut buf);
        Ok(Some(buf))
    }
}
