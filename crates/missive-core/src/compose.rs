//! Writing a message to send: its addresses read and checked, then the message itself, built with
//! `mail-builder` (UTF-8 headers encoded as RFC 2047 asks, attachments with their media type
//! guessed from their name and their file name encoded as RFC 2231 asks).
//!
//! What is built is what is stored in the outbox and sent, byte for byte. Bcc recipients are in
//! the envelope only: they are never written in the message sent. The copy Missive appends to the
//! Sent folder keeps them, in a `Bcc:` header put before the message ([`Built::bcc_header`]): the
//! Sent folder is the user's own record, and it is how every mail client remembers who was sent
//! a blind copy. A server that saves sent mail itself keeps what it was sent, without them.
//!
//! A draft Missive writes carries [`OWN_DRAFT`] (`X-Missive-Draft: 1`), and only such a draft is
//! ever replaced or removed by Missive; the message sent from it does not carry it. The same
//! goes for the link to the message a draft answers or forwards ([`Followup`]): a header of the
//! draft alone, so that a draft picked up again later still marks the original once sent.

use mail_builder::MessageBuilder;
use mail_builder::headers::Header;
use mail_builder::headers::address::Address;
use mail_builder::headers::content_type::ContentType;
use mail_builder::mime::MimePart;
use mail_parser::{HeaderValue, MessageParser, MimeHeaders, PartType};

use crate::account::Account;

/// A recipient: an address, and the name it is shown with, if any.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mailbox {
    pub name: Option<String>,
    pub address: String,
}

impl Mailbox {
    /// Reads `alice@example.org` or `Alice Example <alice@example.org>`. The address must be
    /// plain ASCII, with one `@`, and nothing that would end or change a command to the SMTP
    /// server (spaces, angle brackets, control characters): Missive does not speak SMTPUTF8.
    pub fn parse(text: &str) -> Result<Mailbox, String> {
        let text = text.trim();
        let (name, address) = match text.strip_suffix('>').and_then(|t| t.rsplit_once('<')) {
            Some((name, address)) => {
                let name = name.trim().trim_matches('"').trim();
                ((!name.is_empty()).then(|| name.to_owned()), address.trim())
            }
            None => (None, text),
        };
        if name
            .as_deref()
            .is_some_and(|n| n.chars().any(char::is_control))
        {
            return Err(format!("{text:?}: the name holds a control character"));
        }
        let valid = address.is_ascii()
            && address.matches('@').count() == 1
            && !address.starts_with('@')
            && !address.ends_with('@')
            && !address.chars().any(|c| {
                c.is_ascii_control() || c.is_ascii_whitespace() || "<>()[],;:\\\"".contains(c)
            });
        if !valid {
            return Err(format!(
                "{text:?} is not an address Missive can send to (name@example.org, ASCII only)"
            ));
        }
        Ok(Mailbox {
            name,
            address: address.to_owned(),
        })
    }

    /// `alice@example.org` or `"Alice Example" <alice@example.org>`: what [`Mailbox::parse`]
    /// reads back.
    pub fn to_text(&self) -> String {
        match &self.name {
            Some(name) => format!("\"{}\" <{}>", name.replace('"', ""), self.address),
            None => self.address.clone(),
        }
    }

    fn header(&self) -> Address<'_> {
        Address::new_address(self.name.as_deref(), self.address.as_str())
    }
}

/// The longest message identifier written. A line holds 998 bytes at most (RFC 5322, 2.1.1) and
/// an identifier cannot be folded, so one that fills the line would be refused by servers; this
/// leaves room for the field name.
pub const MAX_IDENTIFIER: usize = 900;

/// Whether `id` (without angle brackets) can be written in `In-Reply-To` or `References`.
pub fn is_identifier(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_IDENTIFIER
        && !id.contains(|c: char| c.is_control() || c.is_whitespace() || "<>".contains(c))
}

/// A file to attach: its name (without any folder) and its content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attachment {
    pub name: String,
    pub data: Vec<u8>,
}

/// A part of the message that its HTML draws from (`cid:`), not a file the user attached: a
/// logo, a picture of a signature. It stays with the HTML it belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inline {
    /// The part's `Content-ID`, without angle brackets.
    pub content_id: String,
    pub media_type: String,
    /// The file name the part came with, as a client shows it under the image.
    pub name: Option<String>,
    /// The charset of a text part, which [`Inline::data`] is written in (not always UTF-8).
    pub charset: Option<String>,
    /// The part's content, as it was before the message's transfer encoding.
    pub data: Vec<u8>,
}

/// What the user wrote.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Draft {
    pub to: Vec<Mailbox>,
    pub cc: Vec<Mailbox>,
    pub bcc: Vec<Mailbox>,
    pub subject: Option<String>,
    pub body: String,
    /// The HTML alternative of the text, kept as it was written by the client that made it: a
    /// draft read back and written again carries it unchanged. Only [`Draft::set_body`] drops it.
    pub html: Option<String>,
    /// The parts [`Draft::html`] draws from: kept as long as it is, and left out of
    /// [`Draft::attachments`], so that taking an attachment off never breaks an image.
    pub inline: Vec<Inline>,
    pub attachments: Vec<Attachment>,
    /// The message it answers, and its thread: Message-IDs without angle brackets.
    pub in_reply_to: Vec<String>,
    pub references: Vec<String>,
}

/// The header that marks a draft as Missive's own. It is in the message itself, not in the
/// local copy: it survives a copy rebuilt, and another client that writes its own version of
/// the draft does not carry it over.
pub const OWN_DRAFT: &str = "X-Missive-Draft";

/// Whether this draft is one Missive wrote, and so may replace or remove.
pub fn is_own_draft(raw: &[u8]) -> bool {
    MessageParser::default()
        .parse_headers(raw)
        .is_some_and(|parsed| parsed.header(OWN_DRAFT).is_some())
}

/// The header of a draft that names the message it answers (its identifier, as `list` prints it).
pub const ANSWERS: &str = "X-Missive-Answers";

/// The same for the message it forwards.
pub const FORWARDS: &str = "X-Missive-Forwards";

/// The message a draft is a reply to or a forward of, to mark answered or forwarded once it is
/// sent. Kept in the draft, never in the message sent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Followup {
    pub answers: Option<String>,
    pub forwards: Option<String>,
}

impl Followup {
    /// What a draft that carries [`OWN_DRAFT`] says it follows up. Whether to believe it is for
    /// [`Followup::trusted`]: these headers can be written by anyone.
    pub fn read(raw: &[u8]) -> Followup {
        let Some(parsed) = MessageParser::default().parse_headers(raw) else {
            return Followup::default();
        };
        if parsed.header(OWN_DRAFT).is_none() {
            return Followup::default();
        }
        let text = |name: &str| {
            parsed
                .header(name)
                .and_then(HeaderValue::as_text)
                .map(|text| text.trim().to_owned())
                .filter(|text| !text.is_empty())
        };
        Followup {
            answers: text(ANSWERS),
            forwards: text(FORWARDS),
        }
    }

    /// What `raw` says it follows up, believed only when `recorded` (what Missive recorded when
    /// it saved the draft, `None` for a draft it did not write) says the same: anyone who can
    /// write to the Drafts folder can add these headers to a draft, so a draft Missive did not
    /// save names nothing, and neither does a version of one whose headers were changed.
    pub fn trusted(raw: &[u8], recorded: Option<&Followup>) -> Followup {
        let read = Followup::read(raw);
        if recorded == Some(&read) {
            read
        } else {
            Followup::default()
        }
    }

    /// Whether it can be written as a header: one message, named by something that is no more
    /// than an identifier (no line break, no space).
    pub fn check(&self) -> Result<(), String> {
        if self.answers.is_some() && self.forwards.is_some() {
            return Err("a message answers or forwards another, not both".to_owned());
        }
        for id in self.answers.iter().chain(&self.forwards) {
            if !is_identifier(id) {
                return Err(format!("{id:?} is not a message identifier"));
            }
        }
        Ok(())
    }
}

/// The `Message-ID` of a draft, without its angle brackets: the same for every version of it.
pub fn draft_message_id(raw: &[u8]) -> Option<String> {
    MessageParser::default()
        .parse_headers(raw)
        .and_then(|parsed| parsed.message_id().map(str::to_owned))
}

impl Draft {
    /// Whether it can be sent: it has a recipient, and can be written.
    pub fn check(&self) -> Result<(), String> {
        if self.to.is_empty() && self.cc.is_empty() && self.bcc.is_empty() {
            return Err("the message has no recipient (--to, --cc or --bcc)".to_owned());
        }
        self.check_fields()
    }

    /// Whether it can be written as a message, recipient or not: what a draft needs.
    pub fn check_fields(&self) -> Result<(), String> {
        if self
            .subject
            .as_deref()
            .is_some_and(|s| s.contains(['\r', '\n']))
        {
            return Err("the subject holds a line break".to_owned());
        }
        for id in self.in_reply_to.iter().chain(&self.references) {
            if !is_identifier(id) {
                return Err(format!("{id:?} is not a message identifier"));
            }
        }
        for attachment in &self.attachments {
            if attachment.name.is_empty() || attachment.name.contains(['/', '\r', '\n']) {
                return Err(format!("{:?} is not a file name", attachment.name));
            }
        }
        Ok(())
    }
}

impl Draft {
    /// Gives the draft these attachments. The parts its HTML draws from are not among them
    /// ([`Draft::inline`]): taking an attachment off leaves the HTML whole.
    pub fn set_attachments(&mut self, attachments: Vec<Attachment>) {
        self.attachments = attachments;
    }

    /// Gives the draft this text. Its HTML alternative stays when the text is the one it has
    /// (line ends aside: a window or a pipe does not write them as the message holds them), and
    /// is dropped when the text differs, since it would say something else than the text. Whether
    /// an HTML part was dropped.
    pub fn set_body(&mut self, body: String) -> bool {
        let same = crlf(&body) == crlf(&self.body);
        self.body = body;
        if same {
            return false;
        }
        // Nothing draws the images of an HTML part that is gone.
        self.inline.clear();
        self.html.take().is_some()
    }

    /// What a message saved as a draft holds: its recipients (Bcc included, which a draft keeps),
    /// its subject, its text, its HTML alternative, its attachments and the thread it answers. An
    /// address Missive cannot send to is refused: the draft could not be written again as it is.
    /// A message with only HTML gives its text, made from it, beside the HTML itself (the first
    /// HTML part: a draft with several keeps one). Other headers (Reply-To…) are not kept.
    pub fn read(raw: &[u8]) -> Result<Draft, String> {
        let parsed = MessageParser::default()
            .parse(raw)
            .ok_or_else(|| "the draft cannot be read as a message".to_owned())?;
        let mailboxes = |address: Option<&mail_parser::Address>| -> Result<Vec<Mailbox>, String> {
            address
                .into_iter()
                .flat_map(|address| address.iter())
                .filter_map(|mailbox| {
                    let address = mailbox.address()?;
                    Some(Mailbox::parse(address).map(|parsed| Mailbox {
                        name: mailbox.name().map(str::to_owned).filter(|n| !n.is_empty()),
                        address: parsed.address,
                    }))
                })
                .collect()
        };
        let ids = |value: &HeaderValue| -> Vec<String> {
            match value {
                HeaderValue::Text(id) => vec![id.to_string()],
                HeaderValue::TextList(ids) => ids.iter().map(|id| id.to_string()).collect(),
                _ => Vec::new(),
            }
        };
        // `body_html` makes a page of a message that has only text: only a real HTML part counts.
        let html = parsed.html_part(0).and_then(|part| match &part.body {
            PartType::Html(html) => Some(html.to_string()),
            _ => None,
        });
        let lowered = html.as_deref().map(str::to_ascii_lowercase);
        let mut inline = Vec::new();
        let mut attachments = Vec::new();
        for (index, part) in parsed.attachments().enumerate() {
            // A part with a Content-ID that the HTML names is drawn by it, not attached.
            let drawn = part
                .content_id()
                .map(|id| id.trim_matches(['<', '>']))
                .filter(|id| {
                    lowered
                        .as_deref()
                        .is_some_and(|html| crate::body::references(html, &id.to_ascii_lowercase()))
                });
            if let Some(content_id) = drawn {
                // mail-parser turns a text part into UTF-8: its own charset is read back from
                // the message, or the part would be written again with a label that is not its.
                let (charset, data) = match (&part.body, part.content_type()) {
                    (PartType::Text(_), Some(t)) => t
                        .attribute("charset")
                        .zip(transfer_decoded(parsed.raw_message(), part))
                        .map_or_else(
                            || (None, part.contents().to_vec()),
                            |(charset, data)| (Some(charset.to_owned()), data),
                        ),
                    _ => (None, part.contents().to_vec()),
                };
                inline.push(Inline {
                    content_id: content_id.to_owned(),
                    media_type: part
                        .content_type()
                        .map(|t| match t.subtype() {
                            Some(subtype) => format!("{}/{subtype}", t.ctype()),
                            None => t.ctype().to_owned(),
                        })
                        .unwrap_or_else(|| "application/octet-stream".to_owned()),
                    name: part.attachment_name().map(str::to_owned),
                    charset,
                    data,
                });
                continue;
            }
            attachments.push(Attachment {
                name: part
                    .attachment_name()
                    .map_or_else(|| format!("attachment-{}", index + 1), str::to_owned),
                data: part.contents().to_vec(),
            });
        }
        Ok(Draft {
            to: mailboxes(parsed.to())?,
            cc: mailboxes(parsed.cc())?,
            bcc: mailboxes(parsed.bcc())?,
            subject: parsed.subject().map(str::to_owned),
            body: parsed
                .body_text(0)
                .map(|text| text.into_owned())
                .unwrap_or_default(),
            html,
            inline,
            attachments,
            in_reply_to: ids(parsed.in_reply_to()),
            references: ids(parsed.references()),
        })
    }
}

/// The body of `part` with its transfer encoding undone and its charset left alone.
fn transfer_decoded(raw: &[u8], part: &mail_parser::MessagePart) -> Option<Vec<u8>> {
    let body = raw.get(part.raw_body_offset() as usize..part.raw_end_offset() as usize)?;
    match part
        .content_transfer_encoding()
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("base64") => mail_parser::decoders::base64::base64_decode(body),
        Some("quoted-printable") => {
            mail_parser::decoders::quoted_printable::quoted_printable_decode(body)
        }
        _ => Some(body.to_vec()),
    }
}

/// A message ready to send.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Built {
    /// `MAIL FROM`: the account's address.
    pub sender: String,
    /// `RCPT TO`: To, Cc and Bcc, each address once.
    pub recipients: Vec<String>,
    /// Without its angle brackets.
    pub message_id: String,
    /// The message as it is sent, lines ended by CRLF: no Bcc in it.
    pub raw: Vec<u8>,
    /// `Bcc: …` and its CRLF, to put before [`Built::raw`] in the copy kept in Sent; `None`
    /// without Bcc recipients.
    pub bcc_header: Option<Vec<u8>>,
}

impl Built {
    /// The message as it is kept in the Drafts folder: [`OWN_DRAFT`], the message it follows up
    /// (`followup`, which has been checked), its Bcc header, then the message.
    pub fn as_draft(&self, followup: &Followup) -> Vec<u8> {
        let mut raw = format!("{OWN_DRAFT}: 1\r\n").into_bytes();
        for (name, id) in [(ANSWERS, &followup.answers), (FORWARDS, &followup.forwards)] {
            if let Some(id) = id {
                raw.extend(format!("{name}: {id}\r\n").into_bytes());
            }
        }
        raw.extend(self.bcc_header.iter().flatten());
        raw.extend_from_slice(&self.raw);
        raw
    }
}

/// Builds the message `account` sends for `draft`, dated `now` (seconds since the epoch). The
/// Message-ID is random, at the domain of the account's address.
pub fn build(account: &Account, draft: &Draft, now: i64) -> Result<Built, String> {
    draft.check()?;
    write(account, draft, now, None)
}

/// Builds `draft` as [`build`] does, but without asking for a recipient (a draft may have none
/// yet), and under `message_id` when given: every version of a draft keeps the same one.
pub fn write(
    account: &Account,
    draft: &Draft,
    now: i64,
    message_id: Option<&str>,
) -> Result<Built, String> {
    draft.check_fields()?;
    let domain = account
        .address
        .rsplit_once('@')
        .map(|(_, domain)| domain)
        .filter(|domain| !domain.is_empty())
        .ok_or_else(|| "the account's address has no domain".to_owned())?;
    let message_id = match message_id {
        Some(id) => id.to_owned(),
        None => format!("{}@{domain}", random_hex()),
    };

    let mut message = MessageBuilder::new()
        .from(Address::new_address(
            (!account.name.is_empty()).then_some(account.name.as_str()),
            account.address.as_str(),
        ))
        .message_id(message_id.as_str())
        .date(now);
    if !draft.to.is_empty() {
        message = message.to(list(&draft.to));
    }
    if !draft.cc.is_empty() {
        message = message.cc(list(&draft.cc));
    }
    if let Some(subject) = &draft.subject {
        message = message.subject(subject.as_str());
    }
    if !draft.in_reply_to.is_empty() {
        message = message.in_reply_to(draft.in_reply_to.as_slice());
    }
    if !draft.references.is_empty() {
        message = message.references(draft.references.as_slice());
    }
    match draft.html.as_deref().filter(|_| !draft.inline.is_empty()) {
        Some(html) => message = message.body(related(draft, html)),
        None => {
            message = message.text_body(crlf(&draft.body));
            if let Some(html) = &draft.html {
                message = message.html_body(html.as_str());
            }
            for attachment in &draft.attachments {
                message = message.attachment(
                    media_type(&attachment.name),
                    attachment.name.as_str(),
                    attachment.data.as_slice(),
                );
            }
        }
    }
    let mut raw = message
        .write_to_vec()
        .map_err(|e| format!("the message could not be written ({e})"))?;
    // DATA ends the message's last line: done here, what is stored is what is sent.
    if !raw.ends_with(b"\r\n") {
        raw.extend_from_slice(b"\r\n");
    }

    let bcc_header = (!draft.bcc.is_empty()).then(|| {
        let mut header = b"Bcc: ".to_vec();
        list(&draft.bcc).write_header(&mut header, "Bcc: ".len());
        header
    });
    let mut recipients: Vec<String> = Vec::new();
    for mailbox in draft.to.iter().chain(&draft.cc).chain(&draft.bcc) {
        if !recipients
            .iter()
            .any(|r| r.eq_ignore_ascii_case(&mailbox.address))
        {
            recipients.push(mailbox.address.clone());
        }
    }
    Ok(Built {
        sender: account.address.clone(),
        recipients,
        message_id,
        raw,
        bcc_header,
    })
}

/// The media type a file is attached with, guessed from its name.
fn media_type(name: &str) -> String {
    mime_guess::from_path(name)
        .first_or_octet_stream()
        .essence_str()
        .to_owned()
}

/// The body of a draft whose HTML draws from parts of the message: the text and the HTML as
/// alternatives, together with the parts they draw from (`multipart/related`), and the files
/// attached beside them.
fn related<'x>(draft: &'x Draft, html: &'x str) -> MimePart<'x> {
    let mut together = vec![MimePart::new(
        "multipart/alternative",
        vec![
            MimePart::new("text/plain", crlf(&draft.body)),
            MimePart::new("text/html", html),
        ],
    )];
    for part in &draft.inline {
        let mut content_type = ContentType::new(part.media_type.as_str());
        let mut disposition = ContentType::new("inline");
        if let Some(charset) = &part.charset {
            content_type = content_type.attribute("charset", charset.as_str());
        }
        if let Some(name) = &part.name {
            content_type = content_type.attribute("name", name.as_str());
            disposition = disposition.attribute("filename", name.as_str());
        }
        together.push(
            MimePart::new(content_type, part.data.as_slice())
                .header("Content-Disposition", disposition)
                .cid(part.content_id.as_str()),
        );
    }
    let together = MimePart::new("multipart/related", together);
    if draft.attachments.is_empty() {
        return together;
    }
    let mut parts = vec![together];
    for attachment in &draft.attachments {
        parts.push(
            MimePart::new(media_type(&attachment.name), attachment.data.as_slice())
                .attachment(attachment.name.as_str()),
        );
    }
    MimePart::new("multipart/mixed", parts)
}

fn list(mailboxes: &[Mailbox]) -> Address<'_> {
    Address::new_list(mailboxes.iter().map(Mailbox::header).collect())
}

/// Every line of `text` ended by CRLF, whatever ended it.
fn crlf(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\n', "\r\n")
}

/// 16 random bytes in hexadecimal, from the system's generator (through rustls's provider).
fn random_hex() -> String {
    let mut bytes = [0u8; 16];
    rustls::crypto::ring::default_provider()
        .secure_random
        .fill(&mut bytes)
        .expect("the system's random generator works");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{Security, Server};

    fn account() -> Account {
        let server = Server {
            host: "mail.example.org".to_owned(),
            port: 465,
            security: Security::Tls,
        };
        Account {
            name: "Élise Exemple".to_owned(),
            address: "elise@example.org".to_owned(),
            user: "elise@example.org".to_owned(),
            imap: server.clone(),
            smtp: server,
            server_saves_sent: false,
        }
    }

    fn mailbox(text: &str) -> Mailbox {
        Mailbox::parse(text).unwrap()
    }

    #[test]
    fn an_address_reads_with_or_without_a_name() {
        assert_eq!(
            mailbox("zoe@example.com"),
            Mailbox {
                name: None,
                address: "zoe@example.com".to_owned()
            }
        );
        assert_eq!(
            mailbox(" Zoé Ça <zoe@example.com> "),
            Mailbox {
                name: Some("Zoé Ça".to_owned()),
                address: "zoe@example.com".to_owned()
            }
        );
        assert_eq!(
            mailbox("\"Doe, John\" <john@example.com>").name.unwrap(),
            "Doe, John"
        );
        for bad in [
            "",
            "zoe",
            "zoe@",
            "@example.com",
            "a@b@example.com",
            "zoé@example.com",
            "zoe@example.com>\r\nRCPT TO:<x@example.com",
            "zoe @example.com",
            "Zoe <zoe@example.com",
        ] {
            assert!(Mailbox::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn an_address_written_as_text_reads_back_the_same() {
        for text in [
            "zoe@example.com",
            "Zoé Ça <zoe@example.com>",
            "\"Doe, John\" <john@example.com>",
        ] {
            let parsed = mailbox(text);
            assert_eq!(mailbox(&parsed.to_text()), parsed, "{text}");
        }
        let quoted = Mailbox {
            name: Some("Say \"hi\"".to_owned()),
            address: "a@example.com".to_owned(),
        };
        assert_eq!(mailbox(&quoted.to_text()).name.as_deref(), Some("Say hi"));
    }

    #[test]
    fn a_message_without_a_recipient_is_refused() {
        let draft = Draft {
            body: "Hello".to_owned(),
            ..Draft::default()
        };
        assert!(build(&account(), &draft, 0).is_err());
    }

    #[test]
    fn a_message_is_built_with_utf8_headers_attachments_and_no_bcc() {
        let draft = Draft {
            to: vec![mailbox("Zoé Ça <zoe@example.com>")],
            cc: vec![mailbox("carl@example.net")],
            bcc: vec![
                mailbox("Bérénice <hidden@example.net>"),
                mailbox("ZOE@example.com"),
            ],
            subject: Some("Réunion du café ☕".to_owned()),
            body: "Bonjour,\nvoici les fichiers.\n.\nÀ bientôt\n".to_owned(),
            attachments: vec![
                Attachment {
                    name: "compte rendu é.pdf".to_owned(),
                    data: b"%PDF-1.4 fake".to_vec(),
                },
                Attachment {
                    name: "photo.png".to_owned(),
                    data: vec![0x89, b'P', b'N', b'G', 0, 1, 2, 255],
                },
            ],
            ..Draft::default()
        };

        let built = build(&account(), &draft, 1_792_222_200).unwrap();

        assert_eq!(built.sender, "elise@example.org");
        assert_eq!(
            built.recipients,
            ["zoe@example.com", "carl@example.net", "hidden@example.net"]
        );
        assert!(built.message_id.ends_with("@example.org"));
        assert_eq!(built.message_id.len(), 32 + "@example.org".len());
        // Every line ends with CRLF, and the message is plain ASCII on the wire.
        assert!(built.raw.is_ascii());
        assert!(!built.raw.windows(2).any(|w| w[1] == b'\n' && w[0] != b'\r'));
        let text = String::from_utf8_lossy(&built.raw);
        assert!(!text.to_ascii_lowercase().contains("bcc"), "{text}");
        assert!(!text.contains("hidden@example.net"));

        let parsed = MessageParser::default().parse(&built.raw).unwrap();
        assert_eq!(parsed.subject(), Some("Réunion du café ☕"));
        assert_eq!(parsed.message_id(), Some(built.message_id.as_str()));
        assert_eq!(parsed.date().unwrap().to_timestamp(), 1_792_222_200);
        let from = parsed.from().unwrap().first().unwrap();
        assert_eq!(from.name(), Some("Élise Exemple"));
        let to = parsed.to().unwrap().first().unwrap();
        assert_eq!(
            (to.name(), to.address()),
            (Some("Zoé Ça"), Some("zoe@example.com"))
        );
        assert_eq!(
            parsed.body_text(0).unwrap(),
            "Bonjour,\r\nvoici les fichiers.\r\n.\r\nÀ bientôt\r\n"
        );
        let attachments: Vec<_> = parsed.attachments().collect();
        assert_eq!(attachments.len(), 2);
        assert_eq!(attachments[0].attachment_name(), Some("compte rendu é.pdf"));
        assert_eq!(
            attachments[0].content_type().unwrap().ctype(),
            "application"
        );
        assert_eq!(
            attachments[0].content_type().unwrap().subtype(),
            Some("pdf")
        );
        assert_eq!(attachments[0].contents(), b"%PDF-1.4 fake");
        assert_eq!(
            attachments[1].content_type().unwrap().subtype(),
            Some("png")
        );
        assert_eq!(
            attachments[1].contents(),
            [0x89, b'P', b'N', b'G', 0, 1, 2, 255]
        );

        let mut copy = built.bcc_header.clone().unwrap();
        copy.extend_from_slice(&built.raw);
        let copy = MessageParser::default().parse(&copy).unwrap();
        let bcc: Vec<_> = copy
            .bcc()
            .unwrap()
            .iter()
            .map(|a| a.address().unwrap())
            .collect();
        assert_eq!(bcc, ["hidden@example.net", "ZOE@example.com"]);
        assert_eq!(copy.subject(), Some("Réunion du café ☕"));
    }

    #[test]
    fn two_messages_get_two_message_ids() {
        let draft = Draft {
            to: vec![mailbox("zoe@example.com")],
            ..Draft::default()
        };
        let one = build(&account(), &draft, 0).unwrap();
        let two = build(&account(), &draft, 0).unwrap();
        assert_ne!(one.message_id, two.message_id);
        assert_eq!(one.bcc_header, None);
    }

    #[test]
    fn a_message_ends_with_crlf_as_data_sends_it() {
        for body in ["", "no final line break", "one\n"] {
            let draft = Draft {
                to: vec![mailbox("zoe@example.com")],
                body: body.to_owned(),
                ..Draft::default()
            };
            let built = build(&account(), &draft, 0).unwrap();
            assert!(built.raw.ends_with(b"\r\n"), "{body:?}");
            assert_eq!(
                crate::submit::dot_stuffed(&built.raw),
                built.raw,
                "{body:?}"
            );
        }
    }

    #[test]
    fn a_draft_reads_back_as_it_was_written_bcc_included() {
        let draft = Draft {
            to: vec![mailbox("Zoé Ça <zoe@example.com>")],
            cc: vec![mailbox("carl@example.net")],
            bcc: vec![mailbox("hidden@example.net")],
            subject: Some("Réunion ☕".to_owned()),
            body: "Bonjour,\r\nà demain.\r\n".to_owned(),
            html: Some("<p>Bonjour,<br>à <b>demain</b>.</p>".to_owned()),
            inline: Vec::new(),
            attachments: vec![Attachment {
                name: "notes é.txt".to_owned(),
                data: b"some notes".to_vec(),
            }],
            in_reply_to: vec!["asked@example.com".to_owned()],
            references: vec![
                "first@example.com".to_owned(),
                "asked@example.com".to_owned(),
            ],
        };
        let built = write(&account(), &draft, 0, Some("same@example.org")).unwrap();
        assert_eq!(built.message_id, "same@example.org");

        assert_eq!(
            Draft::read(&built.as_draft(&Followup::default())).unwrap(),
            draft
        );
        assert!(is_own_draft(&built.as_draft(&Followup::default())));
        // Without its Bcc header, the message sent reads back without them, and is no draft.
        assert!(Draft::read(&built.raw).unwrap().bcc.is_empty());
        assert!(!is_own_draft(&built.raw));
    }

    #[test]
    fn a_draft_keeps_the_message_it_answers_or_forwards_and_the_message_sent_does_not() {
        let draft = Draft {
            to: vec![mailbox("zoe@example.com")],
            ..Draft::default()
        };
        let built = write(&account(), &draft, 0, Some("same@example.org")).unwrap();
        for (followup, header) in [
            (
                Followup {
                    answers: Some("alice/1/2/3".to_owned()),
                    forwards: None,
                },
                "X-Missive-Answers: alice/1/2/3",
            ),
            (
                Followup {
                    answers: None,
                    forwards: Some("alice/1/2/4".to_owned()),
                },
                "X-Missive-Forwards: alice/1/2/4",
            ),
        ] {
            let kept = built.as_draft(&followup);
            assert!(String::from_utf8_lossy(&kept).contains(header), "{header}");
            assert_eq!(Followup::read(&kept), followup);
            assert_eq!(Draft::read(&kept).unwrap(), draft);
        }
        // Only the draft carries the link: the message that goes out never does.
        let raw = String::from_utf8_lossy(&built.raw);
        assert!(!raw.contains("X-Missive"), "{raw}");
        assert_eq!(
            Followup::read(&built.as_draft(&Followup::default())),
            Followup::default()
        );
    }

    #[test]
    fn a_draft_of_another_client_follows_nothing_even_with_the_header() {
        let raw = b"X-Missive-Answers: alice/1/2/3\r\nFrom: a@example.org\r\n\r\nHi\r\n";
        assert_eq!(Followup::read(raw), Followup::default());
    }

    #[test]
    fn a_followup_that_would_start_a_header_or_name_two_messages_is_refused() {
        for id in ["a\r\nBcc: evil@example.net", "a b", "<a>", ""] {
            let followup = Followup {
                answers: Some(id.to_owned()),
                forwards: None,
            };
            assert!(followup.check().is_err(), "{id:?}");
            let followup = Followup {
                answers: None,
                forwards: Some(id.to_owned()),
            };
            assert!(followup.check().is_err(), "{id:?}");
        }
        let both = Followup {
            answers: Some("alice/1/2/3".to_owned()),
            forwards: Some("alice/1/2/4".to_owned()),
        };
        assert!(both.check().is_err());
        assert!(Followup::default().check().is_ok());
    }

    #[test]
    fn a_draft_needs_no_recipient_but_a_message_to_send_does() {
        let draft = Draft {
            subject: Some("Later".to_owned()),
            ..Draft::default()
        };
        let built = write(&account(), &draft, 0, None).unwrap();
        assert_eq!(Draft::read(&built.raw).unwrap(), draft);
        assert!(build(&account(), &draft, 0).is_err());
        let broken = Draft {
            subject: Some("a\nb".to_owned()),
            ..Draft::default()
        };
        assert!(write(&account(), &broken, 0, None).is_err());
    }

    #[test]
    fn a_thread_identifier_that_fills_a_line_is_refused_not_written_unfolded() {
        let long = format!("{}@example.org", "x".repeat(MAX_IDENTIFIER));
        let draft = Draft {
            references: vec![long],
            ..Draft::default()
        };
        assert!(write(&account(), &draft, 0, None).is_err());

        let draft = Draft {
            references: vec![format!("{}@example.org", "x".repeat(100)); 3],
            ..Draft::default()
        };
        assert!(write(&account(), &draft, 0, None).is_ok());
    }

    #[test]
    fn a_thread_identifier_that_would_start_a_header_is_refused() {
        for id in [
            "a@example.org\r\nBcc: evil@example.net",
            "a b@example.org",
            "<a@example.org>",
            "",
        ] {
            let draft = Draft {
                in_reply_to: vec![id.to_owned()],
                ..Draft::default()
            };
            assert!(write(&account(), &draft, 0, None).is_err(), "{id:?}");
            let draft = Draft {
                references: vec![id.to_owned()],
                ..Draft::default()
            };
            assert!(write(&account(), &draft, 0, None).is_err(), "{id:?}");
        }
    }

    #[test]
    fn the_html_alternative_of_a_draft_is_kept_by_a_text_that_does_not_change() {
        let html = "<p>Hello <b>there</b>,\r\n it is é</p>";
        let mut draft = Draft {
            body: "Hello there,\r\nit is é\r\n".to_owned(),
            html: Some(html.to_owned()),
            ..Draft::default()
        };
        let built = write(&account(), &draft, 0, None).unwrap();
        assert_eq!(Draft::read(&built.raw).unwrap(), draft);

        // The same text, its line ends written otherwise: the HTML stays.
        assert!(!draft.set_body("Hello there,\nit is é\n".to_owned()));
        assert_eq!(draft.html.as_deref(), Some(html));
        assert!(!draft.set_body("Hello there,\r\nit is é\r\n".to_owned()));
        // Another text: the HTML would contradict it, and goes.
        assert!(draft.set_body("Something else".to_owned()));
        assert_eq!(draft.html, None);
        assert_eq!(draft.body, "Something else");
        // Nothing to drop: not said.
        assert!(!draft.set_body("Again something else".to_owned()));
    }

    /// A draft whose HTML draws a logo from a part of the message, beside a file attached.
    const WITH_A_LOGO: &[u8] = b"From: a@example.org\r\nSubject: Logo\r\nMIME-Version: 1.0\r\n\
        Content-Type: multipart/mixed; boundary=\"m\"\r\n\r\n\
        --m\r\nContent-Type: multipart/related; boundary=\"r\"\r\n\r\n\
        --r\r\nContent-Type: multipart/alternative; boundary=\"a\"\r\n\r\n\
        --a\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nHi\r\n\
        --a\r\nContent-Type: text/html; charset=utf-8\r\n\r\n\
        <p>Hi <img src=\"cid:logo@example.org\"></p>\r\n--a--\r\n\
        --r\r\nContent-Type: image/png; name=\"logo.png\"\r\n\
        Content-Disposition: inline; filename=\"logo.png\"\r\n\
        Content-ID: <logo@example.org>\r\nContent-Transfer-Encoding: base64\r\n\r\n\
        iVBORw0KGgo=\r\n--r--\r\n\
        --m\r\nContent-Type: text/plain; name=\"notes.txt\"\r\n\
        Content-Disposition: attachment; filename=\"notes.txt\"\r\n\r\nnotes\r\n--m--\r\n";

    #[test]
    fn taking_attachments_off_keeps_the_html_and_the_parts_it_draws_from() {
        let mut draft = Draft::read(WITH_A_LOGO).unwrap();
        // The image the HTML draws is a part of the message, not a file the user attached.
        assert_eq!(draft.attachments.len(), 1);
        assert_eq!(draft.attachments[0].name, "notes.txt");
        assert_eq!(draft.inline.len(), 1);
        assert_eq!(draft.inline[0].content_id, "logo@example.org");
        assert_eq!(draft.inline[0].media_type, "image/png");

        // With a file attached beside them, they are written and read back the same.
        let twice = Draft::read(&write(&account(), &draft, 0, None).unwrap().raw).unwrap();
        assert_eq!(
            (&twice.inline, &twice.attachments),
            (&draft.inline, &draft.attachments)
        );

        // Everything attached goes: the HTML and its image stay.
        draft.set_attachments(Vec::new());
        assert!(
            draft
                .html
                .as_deref()
                .unwrap()
                .contains("cid:logo@example.org")
        );
        assert_eq!(draft.inline.len(), 1);

        // Written and read back, the image is still the part the HTML names.
        let built = write(&account(), &draft, 0, None).unwrap();
        let again = Draft::read(&built.raw).unwrap();
        assert_eq!(again.inline, draft.inline);
        assert!(again.attachments.is_empty());
        assert_eq!(again.html, draft.html);
        let raw = String::from_utf8_lossy(&built.raw).to_lowercase();
        assert!(raw.contains("content-id: <logo@example.org>"), "{raw}");
        assert!(raw.contains("multipart/related"), "{raw}");

        // A text that replaces the HTML takes the image with it: nothing draws it any more.
        assert!(draft.set_body("Another text".to_owned()));
        assert_eq!((draft.html, draft.inline), (None, Vec::new()));
    }

    #[test]
    fn inline_parts_keep_their_file_name_and_their_charset_when_written_again() {
        // A picture with a name, and a text in Latin-1 (a caf\u{e9}), both drawn by the HTML.
        let mut raw = b"From: a@example.org\r\nSubject: Parts\r\nMIME-Version: 1.0\r\n\
            Content-Type: multipart/related; boundary=\"r\"\r\n\r\n\
            --r\r\nContent-Type: text/html; charset=utf-8\r\n\r\n\
            <img src=\"cid:logo@example.org\"><img src=\"cid:menu@example.org\">\r\n\
            --r\r\nContent-Type: image/png; name=\"logo.png\"\r\n\
            Content-Disposition: inline; filename=\"logo.png\"\r\n\
            Content-ID: <logo@example.org>\r\nContent-Transfer-Encoding: base64\r\n\r\n\
            iVBORw0KGgo=\r\n\
            --r\r\nContent-Type: text/plain; charset=iso-8859-1; name=\"menu.txt\"\r\n\
            Content-Disposition: inline; filename=\"menu.txt\"\r\n\
            Content-ID: <menu@example.org>\r\nContent-Transfer-Encoding: 8bit\r\n\r\n\
            caf"
        .to_vec();
        raw.extend_from_slice(b"\xe9\r\n--r--\r\n");
        let draft = Draft::read(&raw).unwrap();
        assert_eq!(draft.inline.len(), 2);
        assert!(draft.attachments.is_empty());

        let built = write(&account(), &draft, 0, None).unwrap();
        let parsed = MessageParser::default().parse(&built.raw).unwrap();
        let part = |id: &str| {
            parsed
                .attachments()
                .find(|part| {
                    part.content_id()
                        .is_some_and(|c| c.trim_matches(['<', '>']) == id)
                })
                .unwrap_or_else(|| {
                    panic!("no part {id} in {}", String::from_utf8_lossy(&built.raw))
                })
        };
        let logo = part("logo@example.org");
        assert_eq!(logo.attachment_name(), Some("logo.png"));
        assert_eq!(
            logo.contents(),
            [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]
        );
        let menu = part("menu@example.org");
        assert_eq!(menu.attachment_name(), Some("menu.txt"));
        assert_eq!(
            menu.content_type().and_then(|t| t.attribute("charset")),
            Some("iso-8859-1")
        );
        assert_eq!(menu.text_contents(), Some("caf\u{e9}"));

        // Written once more, nothing moves.
        let twice = Draft::read(&built.raw).unwrap();
        assert_eq!(twice.inline, draft.inline);
    }

    #[test]
    fn a_part_with_a_content_id_the_html_does_not_draw_stays_an_attachment() {
        let raw = String::from_utf8(WITH_A_LOGO.to_vec()).unwrap().replace(
            "cid:logo@example.org",
            "https://images.example.org/logo.png",
        );
        let draft = Draft::read(raw.as_bytes()).unwrap();
        assert!(draft.inline.is_empty());
        assert_eq!(
            draft
                .attachments
                .iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>(),
            ["logo.png", "notes.txt"]
        );
    }

    #[test]
    fn a_text_message_has_no_html_part_and_an_html_only_one_keeps_its_html() {
        let text = write(&account(), &Draft::default(), 0, None).unwrap();
        assert_eq!(Draft::read(&text.raw).unwrap().html, None);

        let raw = b"From: a@example.org\r\nSubject: Only html\r\nMIME-Version: 1.0\r\n\
                    Content-Type: text/html; charset=utf-8\r\n\r\n<p>Just <b>html</b></p>\r\n";
        let draft = Draft::read(raw).unwrap();
        assert_eq!(draft.html.as_deref(), Some("<p>Just <b>html</b></p>\r\n"));
        assert!(draft.body.contains("Just"), "{:?}", draft.body);
        // Written again, it is a message with both.
        let again = Draft::read(&write(&account(), &draft, 0, None).unwrap().raw).unwrap();
        assert_eq!(again.html, draft.html);
    }
}
