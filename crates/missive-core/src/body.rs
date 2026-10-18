//! What `missive show` reads from a raw message: its text, its HTML and its attachments.
//!
//! Nothing here interprets the HTML: it is handed back as the sender wrote it, and reduced to
//! text only for people, by dropping its markup.

use mail_parser::decoders::html::html_to_text;
use mail_parser::{Message, MessageParser, MimeHeaders, PartType};
use serde::Serialize;

use crate::copy::Person;

/// An attachment of a message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Attachment {
    /// The file name the sender gave, if any.
    pub name: Option<String>,
    /// `application/pdf`, `image/png`… `application/octet-stream` when the sender said none.
    pub media_type: String,
    /// The size in bytes, once decoded.
    pub size: usize,
    /// The part's number in the message, in the order the parser meets them (0 is the message
    /// itself): the name a later command gives to take the attachment out.
    pub part: usize,
}

/// A message's content, as far as it can be read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Body {
    pub reply_to: Vec<Person>,
    /// The text parts, one after the other; `None` when there is none.
    pub text: Option<String>,
    /// The HTML parts as written; `None` when there is none.
    pub html: Option<String>,
    /// `text`, else the HTML reduced to text: what people read. Empty when there is neither.
    pub readable: String,
    pub attachments: Vec<Attachment>,
}

/// Reads a raw message. A message that cannot be read at all gives an empty [`Body`]; one that is
/// only partly readable gives the parts that are.
pub fn read(raw: &[u8]) -> Body {
    let Some(message) = MessageParser::default().parse(raw) else {
        return Body::default();
    };
    let join = |parts: Vec<&str>| (!parts.is_empty()).then(|| parts.join("\n"));
    let text = join(
        message
            .text_body
            .iter()
            .filter_map(|id| match &message.parts.get(*id as usize)?.body {
                PartType::Text(text) => Some(text.as_ref()),
                _ => None,
            })
            .collect(),
    );
    let html = join(
        message
            .html_body
            .iter()
            .filter_map(|id| match &message.parts.get(*id as usize)?.body {
                PartType::Html(html) => Some(html.as_ref()),
                _ => None,
            })
            .collect(),
    );
    let readable = match &text {
        Some(text) => text.clone(),
        None => message
            .text_body
            .iter()
            .filter_map(|id| match &message.parts.get(*id as usize)?.body {
                PartType::Html(html) => Some(html_to_text(&with_link_targets(html))),
                PartType::Text(text) => Some(text.to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    };
    let attachments = message
        .attachments
        .iter()
        .filter_map(|id| {
            let part = message.parts.get(*id as usize)?;
            Some(Attachment {
                name: part.attachment_name().map(str::to_owned),
                media_type: part
                    .content_type()
                    .map(|t| match t.subtype() {
                        Some(subtype) => format!("{}/{subtype}", t.ctype()),
                        None => t.ctype().to_owned(),
                    })
                    .unwrap_or_else(|| "application/octet-stream".to_owned()),
                size: part.len(),
                part: *id as usize,
            })
        })
        .collect();
    Body {
        reply_to: message
            .reply_to()
            .into_iter()
            .flat_map(|address| address.iter())
            .map(|mailbox| Person {
                name: mailbox.name().map(str::to_owned),
                address: mailbox.address().map(str::to_owned),
            })
            .collect(),
        text,
        html,
        readable,
        attachments,
    }
}

/// Whether a message has something a person would call an attachment: a file, or a message
/// attached as a file. An image the HTML shows inside the text is not one: a part with a
/// `Content-ID` that the HTML names as `cid:`, or an `inline` image inside `multipart/related`.
/// A message nested without being attached (a bounce's copy of the original, say) is not one
/// either.
pub fn has_attachments(message: &Message) -> bool {
    let html = message
        .html_body
        .iter()
        .filter_map(|id| match &message.parts.get(*id as usize)?.body {
            PartType::Html(html) => Some(html.to_ascii_lowercase()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let related: Vec<u32> = message
        .parts
        .iter()
        .filter(|part| {
            part.content_type()
                .is_some_and(|t| t.ctype() == "multipart" && t.subtype() == Some("related"))
        })
        .filter_map(|part| match &part.body {
            PartType::Multipart(children) => Some(children),
            _ => None,
        })
        .flatten()
        .copied()
        .collect();
    message.attachments.iter().any(|id| {
        let Some(part) = message.parts.get(*id as usize) else {
            return false;
        };
        if let PartType::Message(_) = part.body {
            return part.attachment_name().is_some()
                || part
                    .content_disposition()
                    .is_some_and(|d| d.is_attachment());
        }
        let image = part.content_type().is_some_and(|t| t.ctype() == "image");
        let shown = part.content_id().is_some_and(|cid| {
            references(&html, &cid.trim_matches(['<', '>']).to_ascii_lowercase())
        }) || related.contains(id)
            && part
                .content_disposition()
                .is_some_and(|d| d.ctype() == "inline");
        !(image && shown)
    })
}

/// Whether lowercased `html` holds `cid:<cid>` as a whole reference, not as the start of a
/// longer one.
fn references(html: &str, cid: &str) -> bool {
    let needle = format!("cid:{cid}");
    html.match_indices(&needle).any(|(at, _)| {
        html[at + needle.len()..]
            .chars()
            .next()
            .is_none_or(|c| matches!(c, '"' | '\'' | ')' | '>' | ' ' | '\t' | '\r' | '\n'))
    })
}

/// `html` with each link followed by its target, `text [url]`, so that the text keeps what the
/// markup said. Only `http`, `https` and `mailto` targets are kept: nothing else is something a
/// person can follow, and nothing is fetched.
fn with_link_targets(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut copied = 0;
    // The target of the link being read, and where its text starts.
    let mut open: Option<(String, usize)> = None;
    let mut at = 0;
    while let Some(found) = html[at..].find('<') {
        let start = at + found;
        let Some(length) = html[start..].find('>') else {
            break;
        };
        let end = start + length + 1;
        let tag = &html[start + 1..end - 1];
        let name = tag.split(|c: char| c.is_ascii_whitespace()).next();
        if name.is_some_and(|name| name.eq_ignore_ascii_case("a")) {
            open = href(tag).map(|url| (url, end));
        } else if tag.eq_ignore_ascii_case("/a")
            && let Some((url, text_start)) = open.take()
        {
            // A link that is its own text says it once.
            if html_to_text(&html[text_start..start]).trim() != html_to_text(&url).trim() {
                out.push_str(&html[copied..start]);
                out.push_str(" [");
                // The target goes back through the text reader: it must hold no markup.
                out.push_str(&url.replace('<', "&lt;").replace('>', "&gt;"));
                out.push(']');
                copied = start;
            }
        }
        at = end;
    }
    out.push_str(&html[copied..]);
    out
}

/// The `href` of an opening `<a …>` tag (its text between the brackets), when it is a target
/// worth showing.
fn href(tag: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    let value = loop {
        let at = from + lower[from..].find("href")?;
        from = at + 4;
        // The attribute name stands alone: `data-href=` is another one.
        let before = lower[..at].chars().next_back();
        if before.is_some_and(|c| !c.is_ascii_whitespace() && c != '"' && c != '\'') {
            continue;
        }
        let after = tag[from..].trim_start();
        let Some(after) = after.strip_prefix('=') else {
            continue;
        };
        let after = after.trim_start();
        break match after.chars().next()? {
            quote @ ('"' | '\'') => after[1..].split(quote).next()?,
            _ => after.split(|c: char| c.is_ascii_whitespace()).next()?,
        };
    };
    let value = value.trim();
    let scheme = value.split(':').next()?.to_ascii_lowercase();
    (value.contains(':') && matches!(scheme.as_str(), "http" | "https" | "mailto"))
        .then(|| value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_that_is_none_gives_an_empty_body() {
        assert_eq!(read(b""), Body::default());
        // Whatever the parser makes of bytes that are no message, nothing panics and no part
        // is made up.
        let body = read(b"\x00\x01\xff\xfe not a mail\r\n\x80");
        assert!(body.html.is_none() && body.attachments.is_empty());
    }

    #[test]
    fn an_attachment_has_a_name_a_type_a_size_and_a_part() {
        let body = read(
            b"Subject: A\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=b\r\n\r\n\
              --b\r\nContent-Type: text/plain\r\n\r\nHi\r\n\
              --b\r\nContent-Type: image/png\r\nContent-Disposition: attachment; filename=logo.png\r\n\
              Content-Transfer-Encoding: base64\r\n\r\nAAEC\r\n\
              --b\r\nContent-Disposition: attachment\r\n\r\nraw\r\n--b--\r\n",
        );

        assert_eq!(body.text.as_deref(), Some("Hi"));
        assert_eq!(
            body.attachments,
            [
                Attachment {
                    name: Some("logo.png".to_owned()),
                    media_type: "image/png".to_owned(),
                    size: 3,
                    part: 2
                },
                Attachment {
                    name: None,
                    // The sender said none.
                    media_type: "application/octet-stream".to_owned(),
                    size: 3,
                    part: 3
                }
            ]
        );
    }

    #[test]
    fn html_is_handed_back_as_written_and_read_as_text_only_without_a_text_part() {
        let html = "<p>One</p><script>no()</script>";
        let only = read(format!("Content-Type: text/html\r\n\r\n{html}").as_bytes());
        assert_eq!(only.html.as_deref(), Some(html));
        assert_eq!(only.text, None);
        assert_eq!(only.readable.trim(), "One");

        let both = read(
            format!(
                "Content-Type: multipart/alternative; boundary=b\r\n\r\n\
                 --b\r\nContent-Type: text/plain\r\n\r\nPlain\r\n\
                 --b\r\nContent-Type: text/html\r\n\r\n{html}\r\n--b--\r\n"
            )
            .as_bytes(),
        );
        assert_eq!(both.readable, "Plain");
    }

    fn attached(raw: &str) -> bool {
        has_attachments(&MessageParser::default().parse(raw.as_bytes()).unwrap())
    }

    const PDF: &str = "--b\r\nContent-Type: application/pdf\r\n\
        Content-Disposition: attachment; filename=a.pdf\r\n\r\nPDF\r\n";

    #[test]
    fn an_image_the_html_shows_inside_the_text_is_no_attachment() {
        let newsletter = "Content-Type: multipart/related; boundary=b\r\n\r\n\
            --b\r\nContent-Type: text/html\r\n\r\n<p>Hi</p><img src=\"cid:logo@example.org\">\r\n\
            --b\r\nContent-Type: image/png\r\nContent-ID: <logo@example.org>\r\n\
            Content-Transfer-Encoding: base64\r\n\r\nAAEC\r\n--b--\r\n";
        assert!(!attached(newsletter));
        // Without a Content-Disposition at all, the reference is what tells.
        assert!(!attached(&newsletter.replace("related", "mixed")));
    }

    #[test]
    fn an_inline_image_inside_multipart_related_is_no_attachment_even_unreferenced() {
        assert!(!attached(
            "Content-Type: multipart/related; boundary=b\r\n\r\n\
             --b\r\nContent-Type: text/html\r\n\r\n<p>Hi</p>\r\n\
             --b\r\nContent-Type: image/gif; name=x.gif\r\nContent-Disposition: inline\r\n\
             \r\nGIF\r\n--b--\r\n"
        ));
    }

    #[test]
    fn a_file_next_to_an_inline_image_still_makes_an_attachment() {
        let raw = format!(
            "Content-Type: multipart/mixed; boundary=b\r\n\r\n\
             --b\r\nContent-Type: multipart/related; boundary=r\r\n\r\n\
             --r\r\nContent-Type: text/html\r\n\r\n<img src=\"cid:logo\">\r\n\
             --r\r\nContent-Type: image/png\r\nContent-ID: <logo>\r\n\r\nPNG\r\n--r--\r\n\
             {PDF}--b--\r\n"
        );
        assert!(attached(&raw));
    }

    #[test]
    fn an_image_the_html_does_not_show_is_an_attachment() {
        let raw = |disposition: &str| {
            format!(
                "Content-Type: multipart/mixed; boundary=b\r\n\r\n\
                 --b\r\nContent-Type: text/html\r\n\r\n<img src=\"cid:other\">\r\n\
                 --b\r\nContent-Type: image/png\r\nContent-ID: <logo>\r\n{disposition}\r\n\
                 PNG\r\n--b--\r\n"
            )
        };
        assert!(attached(&raw("")));
        assert!(attached(&raw(
            "Content-Disposition: attachment; filename=p.png\r\n"
        )));
        // `cid:logo2` is not `cid:logo`.
        assert!(attached(&raw("").replace("cid:other", "cid:logo2")));
    }

    #[test]
    fn a_nested_message_is_an_attachment_only_when_it_is_attached() {
        let raw = |disposition: &str| {
            format!(
                "Content-Type: multipart/mixed; boundary=b\r\n\r\n\
                 --b\r\nContent-Type: text/plain\r\n\r\nSee below\r\n\
                 --b\r\nContent-Type: message/rfc822\r\n{disposition}\r\n\
                 Subject: Inner\r\n\r\nInner body\r\n--b--\r\n"
            )
        };
        assert!(!attached(&raw("")));
        assert!(!attached(&raw("Content-Disposition: inline\r\n")));
        assert!(attached(&raw("Content-Disposition: attachment\r\n")));
        assert!(attached(&raw(
            "Content-Disposition: attachment; filename=fwd.eml\r\n"
        )));
    }

    #[test]
    fn a_message_in_text_and_html_versions_has_no_attachment() {
        assert!(!attached(
            "Content-Type: multipart/alternative; boundary=b\r\n\r\n\
             --b\r\nContent-Type: text/plain\r\n\r\nPlain\r\n\
             --b\r\nContent-Type: text/html\r\n\r\n<p>Plain</p>\r\n--b--\r\n"
        ));
    }

    #[test]
    fn the_links_of_an_html_message_are_kept_in_the_text() {
        let read = |html: &str| {
            read(format!("Content-Type: text/html\r\n\r\n{html}").as_bytes())
                .readable
                .trim()
                .to_owned()
        };
        assert_eq!(
            read("<p>Go to <a href=\"https://example.org/a?x=1&amp;y=2\">the site</a> now</p>"),
            "Go to the site [https://example.org/a?x=1&y=2] now"
        );
        assert_eq!(
            read("<A class=x HREF='mailto:bob@example.org'>Write</A>"),
            "Write [mailto:bob@example.org]"
        );
        // A link that is its own text says it once.
        assert_eq!(
            read("<a href=\"https://example.org/\">https://example.org/</a>"),
            "https://example.org/"
        );
        // Only what a person can follow with nothing run or fetched: no script, no data, no
        // relative link, and an anchor with no target stays its text.
        for href in [
            "javascript:alert(1)",
            "data:text/html,x",
            "/relative",
            "ftp://example.org/f",
            "",
        ] {
            assert_eq!(
                read(&format!("<a href=\"{href}\">Click</a>")),
                "Click",
                "{href:?}"
            );
        }
        assert_eq!(read("<a name=top>Top</a>"), "Top");
        // A target holding markup cannot make some.
        assert_eq!(
            read("<a href=\"https://example.org/<b\">T</a>"),
            "T [https://example.org/<b]"
        );
    }

    #[test]
    fn the_links_are_left_out_when_the_message_has_a_text_part() {
        let both = read(
            b"Content-Type: multipart/alternative; boundary=b\r\n\r\n\
              --b\r\nContent-Type: text/plain\r\n\r\nPlain\r\n\
              --b\r\nContent-Type: text/html\r\n\r\n<a href=\"https://example.org/\">Link</a>\r\n--b--\r\n",
        );
        assert_eq!(both.readable, "Plain");
        // The HTML itself is still handed back as written.
        assert!(both.html.unwrap().contains("href"));
    }
}
