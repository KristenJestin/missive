//! Writing a message to send: its addresses read and checked, then the message itself, built with
//! `mail-builder` (UTF-8 headers encoded as RFC 2047 asks, attachments with their media type
//! guessed from their name and their file name encoded as RFC 2231 asks).
//!
//! What is built is what is stored in the outbox and sent, byte for byte. Bcc recipients are in
//! the envelope only: they are never written in the message sent. The copy Missive appends to the
//! Sent folder keeps them, in a `Bcc:` header put before the message ([`Built::bcc_header`]): the
//! Sent folder is the user's own record, and it is how every mail client remembers who was sent
//! a blind copy. A server that saves sent mail itself keeps what it was sent, without them.

use mail_builder::MessageBuilder;
use mail_builder::headers::Header;
use mail_builder::headers::address::Address;

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

    fn header(&self) -> Address<'_> {
        Address::new_address(self.name.as_deref(), self.address.as_str())
    }
}

/// A file to attach: its name (without any folder) and its content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attachment {
    pub name: String,
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
    pub attachments: Vec<Attachment>,
}

impl Draft {
    /// Whether it can be sent: it has a recipient.
    pub fn check(&self) -> Result<(), String> {
        if self.to.is_empty() && self.cc.is_empty() && self.bcc.is_empty() {
            return Err("the message has no recipient (--to, --cc or --bcc)".to_owned());
        }
        if self
            .subject
            .as_deref()
            .is_some_and(|s| s.contains(['\r', '\n']))
        {
            return Err("the subject holds a line break".to_owned());
        }
        for attachment in &self.attachments {
            if attachment.name.is_empty() || attachment.name.contains(['/', '\r', '\n']) {
                return Err(format!("{:?} is not a file name", attachment.name));
            }
        }
        Ok(())
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

/// Builds the message `account` sends for `draft`, dated `now` (seconds since the epoch). The
/// Message-ID is random, at the domain of the account's address.
pub fn build(account: &Account, draft: &Draft, now: i64) -> Result<Built, String> {
    draft.check()?;
    let domain = account
        .address
        .rsplit_once('@')
        .map(|(_, domain)| domain)
        .filter(|domain| !domain.is_empty())
        .ok_or_else(|| "the account's address has no domain".to_owned())?;
    let message_id = format!("{}@{domain}", random_hex());

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
    message = message.text_body(crlf(&draft.body));
    for attachment in &draft.attachments {
        let media_type = mime_guess::from_path(&attachment.name)
            .first_or_octet_stream()
            .essence_str()
            .to_owned();
        message = message.attachment(
            media_type,
            attachment.name.as_str(),
            attachment.data.as_slice(),
        );
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
    use mail_parser::{MessageParser, MimeHeaders};

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
}
