//! Reading a `mailto:` link (RFC 6068) into the fields of a message to compose.
//!
//! A link comes from anywhere (a web page, a chat, a document), so nothing in it is trusted:
//! only `to`, `cc`, `bcc`, `subject`, `body` and `in-reply-to` are read, and every other header
//! is ignored, `from`, `attach` and `attachment` included: a link never chooses the sender and
//! never attaches a file. A header field holding a line break once decoded (`%0D%0ABcc:…`) is
//! refused, never repaired.

use crate::compose::Mailbox;

/// The longest link read, in bytes. A link longer than this is refused before it is looked at.
pub const MAX_LEN: usize = 32 * 1024;

/// What a link asks to compose. Addresses are checked as `missive compose --to` checks them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Mailto {
    pub to: Vec<Mailbox>,
    pub cc: Vec<Mailbox>,
    pub bcc: Vec<Mailbox>,
    pub subject: Option<String>,
    pub body: Option<String>,
    /// The Message-ID of the message answered, without angle brackets.
    pub in_reply_to: Option<String>,
}

/// Reads a `mailto:` link. The first of a field that may be given once (`subject`, `body`,
/// `in-reply-to`) is the one kept: a link that repeats one is not trusted to choose.
pub fn parse(uri: &str) -> Result<Mailto, String> {
    if uri.len() > MAX_LEN {
        return Err(format!(
            "the mailto link is too long ({} bytes, {MAX_LEN} at most)",
            uri.len()
        ));
    }
    let rest = uri
        .get(..SCHEME.len())
        .filter(|scheme| scheme.eq_ignore_ascii_case(SCHEME))
        .map(|_| &uri[SCHEME.len()..])
        .ok_or("this is not a mailto link (mailto:name@example.org)")?;
    let rest = rest.split_once('#').map_or(rest, |(before, _)| before);
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));

    let mut mailto = Mailto::default();
    if !path.is_empty() {
        for address in path.split(',') {
            mailto.to.push(mailbox(&decode(address)?)?);
        }
    }
    for field in query.split('&').filter(|field| !field.is_empty()) {
        let (name, value) = field.split_once('=').unwrap_or((field, ""));
        let name = decode(name)?.to_ascii_lowercase();
        match name.as_str() {
            "to" | "cc" | "bcc" => {
                let value = header(&name, decode(value)?)?;
                let list = match name.as_str() {
                    "to" => &mut mailto.to,
                    "cc" => &mut mailto.cc,
                    _ => &mut mailto.bcc,
                };
                for address in value.split(',') {
                    list.push(mailbox(address)?);
                }
            }
            "subject" => {
                let value = header(&name, decode(value)?)?;
                mailto.subject.get_or_insert(value);
            }
            "in-reply-to" => {
                let value = header(&name, decode(value)?)?;
                if mailto.in_reply_to.is_none() {
                    mailto.in_reply_to = Some(message_id(&value)?);
                }
            }
            "body" => {
                let value = decode(value)?;
                if value
                    .chars()
                    .any(|c| c.is_control() && !matches!(c, '\t' | '\r' | '\n'))
                {
                    return Err("the mailto link's body holds a control character".to_owned());
                }
                mailto.body.get_or_insert(value);
            }
            // `from`, `attach`, `attachment` and any other header: a link never sets them.
            _ => {}
        }
    }
    Ok(mailto)
}

const SCHEME: &str = "mailto:";

/// Percent-decodes `text` as UTF-8. A plus sign stays a plus sign: RFC 6068 does not make it a
/// space.
fn decode(text: &str) -> Result<String, String> {
    let invalid = || "the mailto link has an invalid percent-encoding".to_owned();
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' {
            let hex = bytes.get(at + 1..at + 3).ok_or_else(invalid)?;
            let hex = std::str::from_utf8(hex).map_err(|_| invalid())?;
            decoded.push(u8::from_str_radix(hex, 16).map_err(|_| invalid())?);
            at += 3;
        } else {
            decoded.push(bytes[at]);
            at += 1;
        }
    }
    String::from_utf8(decoded).map_err(|_| invalid())
}

/// A decoded header field value, refused when it holds a line break (or another control
/// character): written into the message it would start a header the link was not allowed to set.
fn header(name: &str, value: String) -> Result<String, String> {
    let breaks = value
        .chars()
        .any(|c| (c.is_control() && c != '\t') || matches!(c, '\u{2028}' | '\u{2029}'));
    if breaks {
        return Err(format!(
            "the mailto link's {name} holds a line break or a control character"
        ));
    }
    Ok(value)
}

fn mailbox(text: &str) -> Result<Mailbox, String> {
    let text = header("address", text.to_owned())?;
    Mailbox::parse(&text).map_err(|why| format!("the mailto link names a bad address: {why}"))
}

/// One Message-ID, with or without its angle brackets.
fn message_id(value: &str) -> Result<String, String> {
    let value = value.trim();
    let id = value
        .strip_prefix('<')
        .and_then(|v| v.strip_suffix('>'))
        .unwrap_or(value);
    if id.is_empty() || id.contains(|c: char| c.is_whitespace() || matches!(c, '<' | '>')) {
        return Err("the mailto link's in-reply-to is not one message identifier".to_owned());
    }
    Ok(id.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addresses(list: &[Mailbox]) -> Vec<&str> {
        list.iter().map(|m| m.address.as_str()).collect()
    }

    // The examples of RFC 6068, section 6.

    #[test]
    fn rfc_6068_one_address() {
        let m = parse("mailto:chris@example.com").unwrap();
        assert_eq!(addresses(&m.to), ["chris@example.com"]);
        assert_eq!(m.subject, None);
        assert_eq!(m.body, None);
    }

    #[test]
    fn rfc_6068_one_address_with_a_subject_and_a_body() {
        let m = parse("mailto:infobot@example.com?subject=current-issue").unwrap();
        assert_eq!(addresses(&m.to), ["infobot@example.com"]);
        assert_eq!(m.subject.as_deref(), Some("current-issue"));
        let m = parse("mailto:infobot@example.com?body=send%20current-issue").unwrap();
        assert_eq!(m.body.as_deref(), Some("send current-issue"));
    }

    #[test]
    fn rfc_6068_a_body_with_line_breaks() {
        let m = parse(
            "mailto:infobot@example.com?body=send%20current-issue%0D%0Asend%20index%0D%0Asend%20current-issue",
        )
        .unwrap();
        assert_eq!(
            m.body.as_deref(),
            Some("send current-issue\r\nsend index\r\nsend current-issue")
        );
    }

    #[test]
    fn rfc_6068_a_reply_to_a_message() {
        let m =
            parse("mailto:list@example.org?In-Reply-To=%3C3469A91.D10AF4C@example.com%3E").unwrap();
        assert_eq!(addresses(&m.to), ["list@example.org"]);
        assert_eq!(
            m.in_reply_to.as_deref(),
            Some("3469A91.D10AF4C@example.com")
        );
    }

    #[test]
    fn rfc_6068_two_addresses_and_a_cc() {
        let m = parse("mailto:joe@example.com?cc=bob@example.com&body=hello").unwrap();
        assert_eq!(addresses(&m.to), ["joe@example.com"]);
        assert_eq!(addresses(&m.cc), ["bob@example.com"]);
        assert_eq!(m.body.as_deref(), Some("hello"));
        // `%25` is a literal percent sign.
        let m = parse("mailto:gorby%25kremvax@example.com").unwrap();
        assert_eq!(addresses(&m.to), ["gorby%kremvax@example.com"]);
    }

    #[test]
    fn several_addresses_in_the_path_and_in_the_to_field() {
        let m = parse("mailto:a@example.com,b@example.com?to=c@example.com,d@example.com").unwrap();
        assert_eq!(
            addresses(&m.to),
            [
                "a@example.com",
                "b@example.com",
                "c@example.com",
                "d@example.com"
            ]
        );
    }

    #[test]
    fn a_link_without_an_address_is_a_draft_without_recipient() {
        let m = parse("mailto:?subject=Hello&bcc=hidden@example.com").unwrap();
        assert!(m.to.is_empty());
        assert_eq!(addresses(&m.bcc), ["hidden@example.com"]);
        assert_eq!(m.subject.as_deref(), Some("Hello"));
        assert_eq!(parse("mailto:").unwrap(), Mailto::default());
    }

    #[test]
    fn the_scheme_and_the_field_names_ignore_case() {
        let m = parse("MAILTO:a@example.com?Subject=Hi&CC=b@example.com&BODY=text").unwrap();
        assert_eq!(m.subject.as_deref(), Some("Hi"));
        assert_eq!(addresses(&m.cc), ["b@example.com"]);
        assert_eq!(m.body.as_deref(), Some("text"));
    }

    #[test]
    fn percent_encoding_is_decoded_as_utf8_and_a_plus_stays_a_plus() {
        let m = parse("mailto:a@example.com?subject=Caf%C3%A9%20%E2%98%95+tea").unwrap();
        assert_eq!(m.subject.as_deref(), Some("Café ☕+tea"));
    }

    #[test]
    fn a_display_name_in_an_encoded_field_is_kept() {
        let m = parse("mailto:?to=Alice%20Example%20%3Calice@example.com%3E").unwrap();
        assert_eq!(m.to[0].name.as_deref(), Some("Alice Example"));
        assert_eq!(m.to[0].address, "alice@example.com");
    }

    #[test]
    fn a_fragment_is_not_part_of_the_message() {
        let m = parse("mailto:a@example.com?subject=Hi#top").unwrap();
        assert_eq!(m.subject.as_deref(), Some("Hi"));
    }

    #[test]
    fn the_first_of_a_repeated_single_field_wins() {
        let m = parse("mailto:a@example.com?subject=One&subject=Two&body=1&body=2").unwrap();
        assert_eq!(m.subject.as_deref(), Some("One"));
        assert_eq!(m.body.as_deref(), Some("1"));
    }

    #[test]
    fn from_attachments_and_other_headers_are_never_read() {
        let m = parse(
            "mailto:a@example.com?from=evil@example.net&attach=/etc/passwd&attachment=/etc/passwd\
             &reply-to=evil@example.net&references=%3Cx@example.net%3E&x-custom=1&subject=Hi",
        )
        .unwrap();
        assert_eq!(
            m,
            Mailto {
                to: m.to.clone(),
                subject: Some("Hi".to_owned()),
                ..Mailto::default()
            }
        );
        assert_eq!(addresses(&m.to), ["a@example.com"]);
    }

    // Hostile links.

    #[test]
    fn a_line_break_in_a_header_field_is_refused_whatever_the_encoding() {
        for uri in [
            "mailto:a@example.com?subject=Hi%0D%0ABcc:evil@example.net",
            "mailto:a@example.com?subject=Hi%0ABcc:evil@example.net",
            "mailto:a@example.com?subject=Hi%0DBcc:evil@example.net",
            "mailto:a@example.com?in-reply-to=%3Cx@example.com%3E%0D%0ABcc:evil@example.net",
            "mailto:a@example.com?cc=b@example.com%0D%0ABcc:evil@example.net",
            "mailto:a@example.com?to=b@example.com%0D%0ABcc:evil@example.net",
            "mailto:a@example.com?bcc=b@example.com%0D%0ABcc:evil@example.net",
            "mailto:a@example.com%0D%0ABcc:evil@example.net",
            "mailto:a@example.com?subject=Hi%E2%80%A8x",
            "mailto:a@example.com?subject=Hi%00x",
        ] {
            assert!(parse(uri).is_err(), "{uri} was accepted");
        }
    }

    #[test]
    fn a_line_break_in_the_body_is_text_and_cannot_start_a_header() {
        let m = parse("mailto:a@example.com?body=Hi%0D%0ABcc:%20evil@example.net").unwrap();
        assert_eq!(m.body.as_deref(), Some("Hi\r\nBcc: evil@example.net"));
        assert!(m.bcc.is_empty());
    }

    #[test]
    fn a_header_line_break_in_an_ignored_field_is_ignored_with_the_field() {
        // Not read, so not a header of the message: nothing to refuse.
        assert!(parse("mailto:a@example.com?x-custom=a%0D%0ABcc:evil@example.net").is_ok());
    }

    #[test]
    fn an_invalid_percent_encoding_is_refused() {
        for uri in [
            "mailto:a@example.com?subject=100%",
            "mailto:a@example.com?subject=%G1",
            "mailto:a@example.com?subject=%1",
            "mailto:a%2@example.com",
            "mailto:a@example.com?body=%FF%FE",
            "mailto:a@example.com?subject=%C3",
        ] {
            let why = parse(uri).unwrap_err();
            assert!(why.contains("mailto"), "{uri}: {why}");
        }
    }

    #[test]
    fn something_that_is_not_a_mailto_link_is_refused() {
        for uri in [
            "",
            "https://example.com",
            "a@example.com",
            "mailto",
            "mailto//a@example.com",
            "xmailto:a@example.com",
        ] {
            assert!(parse(uri).is_err(), "{uri:?} was accepted");
        }
    }

    #[test]
    fn an_address_missive_cannot_send_to_is_refused() {
        for uri in [
            "mailto:not-an-address",
            "mailto:a@example.com,,b@example.com",
            "mailto:a@example.com?cc=%C3%A9@example.com",
        ] {
            assert!(parse(uri).is_err(), "{uri:?} was accepted");
        }
    }

    #[test]
    fn a_huge_link_is_refused_before_it_is_read() {
        let body = "a".repeat(MAX_LEN);
        let why = parse(&format!("mailto:a@example.com?body={body}")).unwrap_err();
        assert!(why.contains("too long"), "{why}");
        // Just under the limit is fine.
        let room = MAX_LEN - "mailto:a@example.com?body=".len();
        let m = parse(&format!("mailto:a@example.com?body={}", "a".repeat(room))).unwrap();
        assert_eq!(m.body.unwrap().len(), room);
    }

    #[test]
    fn a_reply_id_is_one_message_id() {
        assert_eq!(
            parse("mailto:a@example.com?in-reply-to=x@example.com")
                .unwrap()
                .in_reply_to
                .as_deref(),
            Some("x@example.com")
        );
        for uri in [
            "mailto:a@example.com?in-reply-to=",
            "mailto:a@example.com?in-reply-to=%3C%3E",
            "mailto:a@example.com?in-reply-to=%3Cx@example.com%3E%20%3Cy@example.com%3E",
            "mailto:a@example.com?in-reply-to=x%20y",
        ] {
            assert!(parse(uri).is_err(), "{uri:?} was accepted");
        }
    }
}
