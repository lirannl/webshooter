use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use js_sys::Uint8Array;
use shared::server_datagram::ServerDatagram;
use wasm_bindgen_futures::JsFuture;

use crate::log::log;
use crate::with_wt;

type SubId = usize;

struct Inner {
    next_id: SubId,
    queues: HashMap<SubId, VecDeque<ServerDatagram>>,
}

#[derive(Clone)]
pub struct Receiver {
    inner: Rc<RefCell<Inner>>,
}

pub struct Subscriber {
    inner: Rc<RefCell<Inner>>,
    id: SubId,
}

impl Receiver {
    pub fn new() -> (Receiver, Subscriber) {
        let inner = Rc::new(RefCell::new(Inner {
            next_id: 0,
            queues: HashMap::new(),
        }));
        let sub_id = {
            let mut borrow = inner.borrow_mut();
            let id = borrow.next_id;
            borrow.next_id += 1;
            borrow.queues.insert(id, VecDeque::new());
            id
        };
        (
            Receiver {
                inner: inner.clone(),
            },
            Subscriber {
                inner,
                id: sub_id,
            },
        )
    }

    pub fn push(&self, datagram: ServerDatagram) {
        let mut borrow = self.inner.borrow_mut();
        for queue in borrow.queues.values_mut() {
            queue.push_back(datagram.clone());
        }
    }
}

impl Subscriber {
    pub fn recv(&self) -> Option<ServerDatagram> {
        self.inner.borrow_mut().queues.get_mut(&self.id)?.pop_front()
    }

    pub fn subscribe(&self) -> Subscriber {
        let mut borrow = self.inner.borrow_mut();
        let id = borrow.next_id;
        borrow.next_id += 1;
        borrow.queues.insert(id, VecDeque::new());
        Subscriber {
            inner: self.inner.clone(),
            id,
        }
    }
}

pub fn spawn_reader(receiver: Receiver, closed: Rc<std::cell::Cell<bool>>) {
    wasm_bindgen_futures::spawn_local(async move {
        loop {
            let promise = with_wt(|gwt| gwt.reader.read());
            let result = JsFuture::from(promise).await;

            let data: Vec<u8> = match result {
                Ok(val) => {
                    if val.is_undefined() || val.is_null() {
                        break;
                    }
                    let done = js_sys::Reflect::get(&val, &"done".into())
                        .ok()
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    if done {
                        break;
                    }
                    let value = match js_sys::Reflect::get(&val, &"value".into()) {
                        Ok(v) if !v.is_undefined() && !v.is_null() => v,
                        _ => break,
                    };
                    let arr = Uint8Array::new(&value);
                    let mut buf = vec![0u8; arr.length() as usize];
                    arr.copy_to(&mut buf);
                    buf
                }
                Err(_) => break,
            };

            match ServerDatagram::from_bytes(&data) {
                Ok(datagram) => receiver.push(datagram),
                Err(_) => continue,
            }
        }
        closed.set(true);
        log("reader: stream ended");
    });
}
