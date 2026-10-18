//! Reads a raw message into what is drawn: the HTML body and the parts it may embed.

use std::collections::HashMap;

use base64::Engine as _;
use mail_parser::{MessageParser, MimeHeaders};
use percent_encoding::percent_decode_str;

pub struct Message {
    /// The HTML body, or the text body turned into HTML by mail-parser when there is no HTML.
    pub html: String,
    /// Parts named by a Content-ID, without its angle brackets: what `cid:` URLs point at.
    pub parts: HashMap<String, Vec<u8>>,
}

pub fn read(raw: &[u8]) -> Result<Message, String> {
    let message = MessageParser::default().parse(raw).ok_or("not a message")?;
    let html = message
        .body_html(0)
        .map(|html| html.into_owned())
        .ok_or("no readable body")?;
    let parts = message
        .parts
        .iter()
        .filter_map(|part| {
            let id = part.content_id()?;
            let id = id.trim().trim_start_matches('<').trim_end_matches('>');
            Some((id.to_string(), part.contents().to_vec()))
        })
        .collect();
    Ok(Message { html, parts })
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
    fn a_plain_text_message_becomes_html() {
        let raw = b"From: a@example.org\r\nTo: b@example.org\r\nSubject: s\r\n\r\nHello there\r\n";
        let message = read(raw).unwrap();
        assert!(message.html.contains("Hello there"));
    }

    #[test]
    fn a_part_is_found_by_its_content_id() {
        let raw = concat!(
            "From: a@example.org\r\n",
            "Content-Type: multipart/related; boundary=b\r\n\r\n",
            "--b\r\nContent-Type: text/html\r\n\r\n<img src=\"cid:logo@example.org\">\r\n",
            "--b\r\nContent-Type: image/png\r\nContent-ID: <logo@example.org>\r\n",
            "Content-Transfer-Encoding: base64\r\n\r\naGVsbG8=\r\n--b--\r\n"
        );
        let message = read(raw.as_bytes()).unwrap();
        assert_eq!(message.parts["logo@example.org"], b"hello");
    }

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
