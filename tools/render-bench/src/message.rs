//! Reads an `.eml` file into what a way renders: the HTML body and the parts it may embed.

use std::collections::HashMap;

use mail_parser::{MessageParser, MimeHeaders};

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
}
