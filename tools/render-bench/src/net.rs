//! The only way Blitz gets a resource: from the message itself, never from the network.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use blitz_traits::net::{Bytes, NetHandler, NetProvider, Request};
use missive_render::message::decode_data_url;
use percent_encoding::percent_decode_str;

pub struct MessageOnly {
    parts: HashMap<String, Bytes>,
    /// Requests for anything but the message's own parts and `data:` URLs: never sent.
    pub blocked: AtomicUsize,
    /// Images, fonts or stylesheets served from the message.
    pub embedded: AtomicUsize,
}

impl MessageOnly {
    pub fn new(parts: HashMap<String, Vec<u8>>) -> Self {
        let parts = parts
            .into_iter()
            .map(|(id, bytes)| (id, Bytes::from(bytes)))
            .collect();
        MessageOnly {
            parts,
            blocked: AtomicUsize::new(0),
            embedded: AtomicUsize::new(0),
        }
    }
}

impl NetProvider for MessageOnly {
    fn fetch(&self, _doc_id: usize, request: Request, handler: Box<dyn NetHandler>) {
        let url = request.url;
        let body = match url.scheme() {
            "cid" => {
                let id = percent_decode_str(url.path()).decode_utf8_lossy();
                self.parts.get(id.as_ref()).cloned()
            }
            "data" => decode_data_url(url.as_str()).map(Bytes::from),
            _ => {
                self.blocked.fetch_add(1, Ordering::Relaxed);
                None
            }
        };
        if body.is_some() {
            self.embedded.fetch_add(1, Ordering::Relaxed);
        }
        // Every request is answered, an empty body when refused: a stylesheet left unanswered in
        // the head would block Blitz's layout for ever.
        handler.bytes(url.to_string(), body.unwrap_or_default());
    }
}
