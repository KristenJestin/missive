//! The only way Blitz gets a resource: from the message itself, never from the network.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use base64::Engine as _;
use blitz_traits::net::{Bytes, NetHandler, NetProvider, Request};
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

/// `data:[<type>][;base64],<data>`
pub fn decode_data_url(url: &str) -> Option<Vec<u8>> {
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    let data = percent_decode_str(data).collect::<Vec<u8>>();
    if meta.ends_with(";base64") {
        let compact: Vec<u8> = data
            .into_iter()
            .filter(|byte| !byte.is_ascii_whitespace())
            .collect();
        base64::engine::general_purpose::STANDARD
            .decode(compact)
            .ok()
    } else {
        Some(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_base64_data_url_is_decoded() {
        assert_eq!(
            decode_data_url("data:image/png;base64,aGVs bG8=").unwrap(),
            b"hello"
        );
    }

    #[test]
    fn a_percent_encoded_data_url_is_decoded() {
        assert_eq!(
            decode_data_url("data:text/css,a%7Bcolor:red%7D").unwrap(),
            b"a{color:red}"
        );
    }

    #[test]
    fn a_data_url_without_a_comma_is_refused() {
        assert!(decode_data_url("data:image/png;base64").is_none());
    }
}
