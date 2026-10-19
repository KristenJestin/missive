//! Answering a message: who a reply goes to, its subject, the thread it joins and the quote of
//! the original, or the subject and attachment of a forward.
//!
//! Everything here is a pure function of the original message (its raw form) and of the
//! sending account's address, so that every case is a unit test. The result is a [`Draft`]'s
//! fields; writing and sending it is the business of [`crate::compose`].
//!
//! - **Reply** goes to `Reply-To`, else `From`. **Reply all** adds the original's `To` and `Cc`,
//!   but not the sending account's own address, and no address twice (case does not matter).
//! - The subject gets one `Re: `: a subject that already starts with `Re:` (in any case) is kept.
//! - The thread: `In-Reply-To` is the original's `Message-ID`, `References` is the original's
//!   own followed by that `Message-ID`. A thread longer than [`MAX_REFERENCES`] keeps its first
//!   identifier (the root) and its latest ones: a header that grows with every reply would end
//!   by being refused by servers.
//! - The quote is the original's text (its HTML reduced to text when it has none), every line
//!   prefixed by `> `, below an attribution line. The date in it is written in the original's
//!   own offset, in a form that does not depend on any language.
//! - An odd original is answered all the same: line breaks of its subject become spaces, and the
//!   identifiers that cannot be written back (whitespace in them, or longer than a line holds)
//!   are left out. [`Cleaned`] counts what was changed, so that it can be said.
//! - **Forward** has a `Fwd: ` subject, no threading header, and the original goes along as a
//!   `message/rfc822` attachment, byte for byte.

use mail_parser::{DateTime, HeaderValue, MessageParser};

use crate::body;
use crate::compose::{Attachment, Mailbox, is_identifier};

/// How many identifiers a `References` header keeps: the first, and the latest ones.
pub const MAX_REFERENCES: usize = 20;

/// Whether to answer the sender alone, or everyone the message went to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Reply,
    ReplyAll,
}

/// What an answer reads of the original message.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Original {
    pub from: Vec<Mailbox>,
    pub reply_to: Vec<Mailbox>,
    pub to: Vec<Mailbox>,
    pub cc: Vec<Mailbox>,
    pub subject: Option<String>,
    /// Without angle brackets.
    pub message_id: Option<String>,
    pub in_reply_to: Vec<String>,
    pub references: Vec<String>,
    pub date: Option<DateTime>,
    /// The text people read: its text, else its HTML reduced to text.
    pub text: String,
    /// What reading it changed.
    pub cleaned: Cleaned,
}

/// What was changed in an original to answer it, counts and kinds only: its content is private.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cleaned {
    /// Line breaks in the subject, replaced by spaces.
    pub subject: bool,
    /// Identifiers (`Message-ID`, `In-Reply-To`, `References`) left out.
    pub identifiers: usize,
}

impl Cleaned {
    /// One line telling what was changed; `None` when nothing was.
    pub fn notice(&self) -> Option<String> {
        let mut parts = Vec::new();
        if self.subject {
            parts.push("line breaks in its subject were replaced by spaces".to_owned());
        }
        match self.identifiers {
            0 => {}
            1 => parts.push("1 message identifier was left out".to_owned()),
            n => parts.push(format!("{n} message identifiers were left out")),
        }
        (!parts.is_empty()).then(|| format!("The original message is odd: {}.", parts.join(", ")))
    }
}

/// The fields of a reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reply {
    pub to: Vec<Mailbox>,
    pub cc: Vec<Mailbox>,
    pub subject: String,
    pub in_reply_to: Vec<String>,
    pub references: Vec<String>,
    /// The attribution line and the quoted text.
    pub quote: String,
}

impl Original {
    /// Reads a raw message. An address Missive cannot send to is an error: leaving it out would
    /// answer fewer people than the user meant to.
    pub fn read(raw: &[u8]) -> Result<Original, String> {
        let readable = crate::header::decoded(raw);
        let parsed = MessageParser::default()
            .parse(readable.as_ref())
            .ok_or_else(|| "the message cannot be read".to_owned())?;
        let mailboxes = |address: Option<&mail_parser::Address>| -> Result<Vec<Mailbox>, String> {
            address
                .into_iter()
                .flat_map(|address| address.iter())
                .filter_map(|mailbox| {
                    let address = mailbox.address()?;
                    Some(Mailbox::parse(address).map(|parsed| {
                        Mailbox {
                            // A name Missive would refuse to write is cleaned, not the reply lost.
                            name: mailbox
                                .name()
                                .map(|name| name.replace(char::is_control, " ").trim().to_owned())
                                .filter(|n| !n.is_empty()),
                            address: parsed.address,
                        }
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
        let mut left_out = 0;
        let mut usable = |ids: Vec<String>| -> Vec<String> {
            ids.into_iter()
                .filter(|id| {
                    let usable = is_identifier(id);
                    left_out += usize::from(!usable);
                    usable
                })
                .collect()
        };
        let in_reply_to = usable(ids(parsed.in_reply_to()));
        let references = usable(ids(parsed.references()));
        let message_id = usable(parsed.message_id().map(str::to_owned).into_iter().collect())
            .into_iter()
            .next();
        let (subject, one_line_subject) = one_line(parsed.subject());
        let cleaned = Cleaned {
            subject: one_line_subject,
            identifiers: left_out,
        };
        Ok(Original {
            from: mailboxes(parsed.from())?,
            reply_to: mailboxes(parsed.reply_to())?,
            to: mailboxes(parsed.to())?,
            cc: mailboxes(parsed.cc())?,
            subject,
            message_id,
            in_reply_to,
            references,
            date: parsed.date().cloned(),
            text: body::read(raw).readable,
            cleaned,
        })
    }
}

/// `subject` on one line, and whether it was not: its line breaks (an encoded word can decode to
/// them) become spaces, as a subject written with them would be refused.
fn one_line(subject: Option<&str>) -> (Option<String>, bool) {
    let Some(subject) = subject else {
        return (None, false);
    };
    if !subject.contains(['\r', '\n']) {
        return (Some(subject.to_owned()), false);
    }
    let lines: Vec<&str> = subject
        .split(['\r', '\n'])
        .filter(|line| !line.is_empty())
        .collect();
    (Some(lines.join(" ")), true)
}

/// The reply to `original` that the account whose address is `own` writes.
pub fn reply(original: &Original, mode: Mode, own: &str) -> Reply {
    let mut to = if original.reply_to.is_empty() {
        original.from.clone()
    } else {
        original.reply_to.clone()
    };
    let mut cc = Vec::new();
    if mode == Mode::ReplyAll {
        to.extend(original.to.iter().cloned());
        cc = original.cc.clone();
    }
    let to = distinct(to, own, &[]);
    let cc = distinct(cc, own, &to);
    let (in_reply_to, references) = thread(original);
    Reply {
        to,
        cc,
        subject: reply_subject(original.subject.as_deref()),
        in_reply_to,
        references,
        quote: quote(original),
    }
}

/// `mailboxes` without the account's own address, without those of `already`, and each address
/// once. Reply-all to a message one sent oneself must not write to oneself.
fn distinct(mailboxes: Vec<Mailbox>, own: &str, already: &[Mailbox]) -> Vec<Mailbox> {
    let mut kept: Vec<Mailbox> = Vec::new();
    for mailbox in mailboxes {
        let seen = |list: &[Mailbox]| {
            list.iter()
                .any(|m| m.address.eq_ignore_ascii_case(&mailbox.address))
        };
        if !mailbox.address.eq_ignore_ascii_case(own) && !seen(&kept) && !seen(already) {
            kept.push(mailbox);
        }
    }
    kept
}

/// `Re: ` and the subject, unless it already starts with `Re:`.
pub fn reply_subject(subject: Option<&str>) -> String {
    prefixed("Re:", &["re:"], subject)
}

/// `Fwd: ` and the subject, unless it already starts with `Fwd:` (or `Fw:`).
pub fn forward_subject(subject: Option<&str>) -> String {
    prefixed("Fwd:", &["fwd:", "fw:"], subject)
}

fn prefixed(prefix: &str, known: &[&str], subject: Option<&str>) -> String {
    let subject = subject.unwrap_or_default().trim();
    let lower = subject.to_ascii_lowercase();
    if known.iter().any(|known| lower.starts_with(known)) {
        return subject.to_owned();
    }
    format!("{prefix} {subject}").trim_end().to_owned()
}

/// `(In-Reply-To, References)` of an answer to `original`.
pub fn thread(original: &Original) -> (Vec<String>, Vec<String>) {
    // A parent without References is joined through its In-Reply-To (RFC 5322, 3.6.4).
    let mut references = if original.references.is_empty() {
        original.in_reply_to.clone()
    } else {
        original.references.clone()
    };
    references.extend(original.message_id.iter().cloned());
    let mut ids: Vec<String> = Vec::new();
    for id in references {
        if !id.is_empty() && !ids.contains(&id) {
            ids.push(id);
        }
    }
    if ids.len() > MAX_REFERENCES {
        let latest = ids.split_off(ids.len() - (MAX_REFERENCES - 1));
        ids.truncate(1);
        ids.extend(latest);
    }
    let in_reply_to = original
        .message_id
        .iter()
        .filter(|id| !id.is_empty())
        .cloned()
        .collect();
    (in_reply_to, ids)
}

/// The attribution line, then the original's text, each line prefixed by `> `.
pub fn quote(original: &Original) -> String {
    let who = original
        .from
        .first()
        .map(|from| from.name.clone().unwrap_or_else(|| from.address.clone()));
    let mut quoted = attribution(who.as_deref(), original.date.as_ref());
    quoted.push('\n');
    let text = original.text.replace("\r\n", "\n").replace('\r', "\n");
    for line in text.trim_end().split('\n') {
        quoted.push_str("> ");
        quoted.push_str(line);
        quoted.push('\n');
    }
    quoted
}

/// `On 2026-10-17 07:30 +0200, Alice Example wrote:`. The date is the one the message carries,
/// in the offset it was written in; nothing in it is translated.
pub fn attribution(who: Option<&str>, date: Option<&DateTime>) -> String {
    let who = who.unwrap_or("someone");
    match date {
        Some(date) => format!(
            "On {:04}-{:02}-{:02} {:02}:{:02} {}{:02}{:02}, {who} wrote:",
            date.year,
            date.month,
            date.day,
            date.hour,
            date.minute,
            if date.tz_before_gmt { '-' } else { '+' },
            date.tz_hour,
            date.tz_minute,
        ),
        None => format!("{who} wrote:"),
    }
}

/// The text of the message: what the user wrote, and below it the quote.
pub fn with_quote(text: &str, quote: &str) -> String {
    let text = text.trim_end();
    if text.is_empty() {
        quote.to_owned()
    } else {
        format!("{text}\n\n{quote}")
    }
}

/// The file name a forwarded message goes under: its subject, as a name a file can have.
pub fn forward_name(subject: Option<&str>) -> String {
    let name: String = subject
        .unwrap_or_default()
        .chars()
        .map(|c| if c == '/' || c.is_control() { ' ' } else { c })
        .collect();
    let name: String = name.split_whitespace().collect::<Vec<_>>().join(" ");
    let name: String = name.chars().take(100).collect();
    if name.is_empty() {
        "forwarded message.eml".to_owned()
    } else {
        format!("{name}.eml")
    }
}

/// What a message answered or forwarded gives a new message: what `missive compose --reply`
/// and the compose window both start from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Answering {
    pub to: Vec<Mailbox>,
    pub cc: Vec<Mailbox>,
    pub subject: Option<String>,
    /// The attribution and the quoted text, to put below the user's text.
    pub quote: Option<String>,
    pub in_reply_to: Vec<String>,
    pub references: Vec<String>,
    /// A forward: the message, to go along as it is.
    pub attachment: Option<Attachment>,
    /// A reply: the identifier of the message to mark answered once the reply is sent.
    pub answers: Option<String>,
    /// A forward: the identifier of the message to mark forwarded once the forward is sent.
    pub forwards: Option<String>,
    /// What had to be changed in the original to answer it, in a line to tell the user.
    pub notice: Option<String>,
}

/// A reply (`Some(mode)`) or a forward (`None`) to the message `id` of the copy, as the
/// account whose address is `own` writes it. `subject` is the one the copy shows: a forward
/// is named after it.
pub fn answering(
    id: &str,
    raw: &[u8],
    subject: Option<&str>,
    mode: Option<Mode>,
    own: &str,
) -> Result<Answering, String> {
    let Some(mode) = mode else {
        let (subject, one_line_subject) = one_line(subject);
        let cleaned = Cleaned {
            subject: one_line_subject,
            ..Cleaned::default()
        };
        return Ok(Answering {
            subject: Some(forward_subject(subject.as_deref())),
            attachment: Some(Attachment {
                name: forward_name(subject.as_deref()),
                data: raw.to_vec(),
            }),
            forwards: Some(id.to_owned()),
            notice: cleaned.notice(),
            ..Answering::default()
        });
    };
    let original = Original::read(raw).map_err(|e| format!("cannot answer {id}: {e}"))?;
    let reply = reply(&original, mode, own);
    Ok(Answering {
        to: reply.to,
        cc: reply.cc,
        subject: Some(reply.subject),
        quote: Some(reply.quote),
        in_reply_to: reply.in_reply_to,
        references: reply.references,
        answers: Some(id.to_owned()),
        notice: original.cleaned.notice(),
        ..Answering::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWN: &str = "alice@example.org";

    fn addresses(list: &[Mailbox]) -> Vec<&str> {
        list.iter().map(|m| m.address.as_str()).collect()
    }

    fn original(raw: &str) -> Original {
        Original::read(raw.as_bytes()).unwrap()
    }

    const PLAIN: &str = "From: Zoé Ça <zoe@example.com>\r\n\
        To: Alice Example <alice@example.org>, carl@example.net\r\n\
        Cc: dana@example.net\r\n\
        Subject: Lunch?\r\n\
        Message-ID: <lunch@example.com>\r\n\
        Date: Sat, 17 Oct 2026 07:30:00 +0200\r\n\
        \r\n\
        Are you free?\r\n\
        \r\n\
        Zoé\r\n";

    #[test]
    fn a_raw_latin_1_subject_and_name_are_read_as_windows_1252() {
        let raw = b"From: Zo\xe9 \xc7a <zoe@example.com>\r\nTo: alice@example.org\r\n\
            Subject: Caf\xe9?\r\nDate: Sat, 17 Oct 2026 07:30:00 +0200\r\n\r\nOui\r\n";

        let original = Original::read(raw).unwrap();

        assert_eq!(original.subject.as_deref(), Some("Café?"));
        assert_eq!(original.from[0].name.as_deref(), Some("Zoé Ça"));
    }

    #[test]
    fn a_reply_goes_to_the_sender() {
        let reply = reply(&original(PLAIN), Mode::Reply, OWN);
        assert_eq!(addresses(&reply.to), ["zoe@example.com"]);
        assert_eq!(reply.to[0].name.as_deref(), Some("Zoé Ça"));
        assert!(reply.cc.is_empty());
        assert_eq!(reply.subject, "Re: Lunch?");
    }

    #[test]
    fn a_reply_goes_to_reply_to_rather_than_from() {
        let raw = PLAIN.replace(
            "Subject:",
            "Reply-To: Lists <list@example.com>, other@example.com\r\nSubject:",
        );
        let reply = reply(&original(&raw), Mode::Reply, OWN);
        assert_eq!(
            addresses(&reply.to),
            ["list@example.com", "other@example.com"]
        );
    }

    #[test]
    fn a_reply_all_adds_to_and_cc_without_the_accounts_own_address() {
        let reply = reply(&original(PLAIN), Mode::ReplyAll, OWN);
        assert_eq!(
            addresses(&reply.to),
            ["zoe@example.com", "carl@example.net"]
        );
        assert_eq!(addresses(&reply.cc), ["dana@example.net"]);
    }

    #[test]
    fn a_reply_all_leaves_out_the_own_address_in_cc_whatever_its_case() {
        let raw = PLAIN.replace(
            "Cc: dana@example.net",
            "Cc: Alice <ALICE@Example.ORG>, dana@example.net",
        );
        let reply = reply(&original(&raw), Mode::ReplyAll, OWN);
        assert_eq!(addresses(&reply.cc), ["dana@example.net"]);
        assert!(!addresses(&reply.to).contains(&"alice@example.org"));
    }

    #[test]
    fn a_reply_all_writes_to_each_address_once() {
        let raw = PLAIN.replace(
            "Cc: dana@example.net",
            "Cc: ZOE@example.com, carl@example.net, dana@example.net, DANA@example.net",
        );
        let reply = reply(&original(&raw), Mode::ReplyAll, OWN);
        assert_eq!(
            addresses(&reply.to),
            ["zoe@example.com", "carl@example.net"]
        );
        assert_eq!(addresses(&reply.cc), ["dana@example.net"]);
    }

    #[test]
    fn a_reply_all_to_a_message_one_sent_oneself_writes_to_its_recipients() {
        let raw = PLAIN.replace("From: Zoé Ça <zoe@example.com>", "From: alice@example.org");
        let reply = reply(&original(&raw), Mode::ReplyAll, OWN);
        assert_eq!(addresses(&reply.to), ["carl@example.net"]);
    }

    #[test]
    fn the_subject_gets_one_re() {
        assert_eq!(reply_subject(Some("Lunch?")), "Re: Lunch?");
        assert_eq!(reply_subject(Some("Re: Lunch?")), "Re: Lunch?");
        assert_eq!(reply_subject(Some("RE: Lunch?")), "RE: Lunch?");
        assert_eq!(reply_subject(Some("re:Lunch?")), "re:Lunch?");
        assert_eq!(reply_subject(Some("Fwd: Lunch?")), "Re: Fwd: Lunch?");
        assert_eq!(reply_subject(Some("  Réunion ☕  ")), "Re: Réunion ☕");
        assert_eq!(reply_subject(Some("")), "Re:");
        assert_eq!(reply_subject(None), "Re:");
    }

    #[test]
    fn the_subject_of_a_forward_gets_one_fwd() {
        assert_eq!(forward_subject(Some("Lunch?")), "Fwd: Lunch?");
        assert_eq!(forward_subject(Some("Fwd: Lunch?")), "Fwd: Lunch?");
        assert_eq!(forward_subject(Some("FWD: Lunch?")), "FWD: Lunch?");
        assert_eq!(forward_subject(Some("Fw: Lunch?")), "Fw: Lunch?");
        assert_eq!(forward_subject(Some("Re: Lunch?")), "Fwd: Re: Lunch?");
        assert_eq!(forward_subject(None), "Fwd:");
    }

    #[test]
    fn a_reply_joins_the_thread_of_the_original() {
        let raw = PLAIN.replace(
            "Subject:",
            "References: <root@example.com> <middle@example.com>\r\nSubject:",
        );
        let reply = reply(&original(&raw), Mode::Reply, OWN);
        assert_eq!(reply.in_reply_to, ["lunch@example.com"]);
        assert_eq!(
            reply.references,
            [
                "root@example.com",
                "middle@example.com",
                "lunch@example.com"
            ]
        );
    }

    #[test]
    fn a_reply_to_a_message_that_starts_a_thread_references_it() {
        let reply = reply(&original(PLAIN), Mode::Reply, OWN);
        assert_eq!(reply.references, ["lunch@example.com"]);
    }

    #[test]
    fn a_very_long_references_keeps_the_root_and_the_latest_identifiers() {
        let long: Vec<String> = (1..=50).map(|n| format!("<id{n}@example.com>")).collect();
        let raw = PLAIN.replace(
            "Subject:",
            &format!("References: {}\r\nSubject:", long.join(" ")),
        );
        let reply = reply(&original(&raw), Mode::Reply, OWN);
        assert_eq!(reply.references.len(), MAX_REFERENCES);
        assert_eq!(reply.references[0], "id1@example.com");
        assert_eq!(reply.references[1], "id33@example.com");
        assert_eq!(reply.references[MAX_REFERENCES - 2], "id50@example.com");
        assert_eq!(reply.references[MAX_REFERENCES - 1], "lunch@example.com");
        assert_eq!(reply.in_reply_to, ["lunch@example.com"]);
    }

    #[test]
    fn a_reply_to_a_message_without_a_message_id_has_no_in_reply_to() {
        let raw = PLAIN.replace("Message-ID: <lunch@example.com>\r\n", "");
        assert_eq!(original(&raw).message_id, None);
        let reply = reply(&original(&raw), Mode::Reply, OWN);
        assert!(reply.in_reply_to.is_empty());
        assert!(reply.references.is_empty());

        // Its own References still go on.
        let raw = raw.replace("Subject:", "References: <root@example.com>\r\nSubject:");
        let reply = self::reply(&original(&raw), Mode::Reply, OWN);
        assert!(reply.in_reply_to.is_empty());
        assert_eq!(reply.references, ["root@example.com"]);
    }

    #[test]
    fn a_parent_without_references_is_joined_through_its_in_reply_to() {
        let raw = PLAIN.replace("Subject:", "In-Reply-To: <before@example.com>\r\nSubject:");
        let reply = reply(&original(&raw), Mode::Reply, OWN);
        assert_eq!(
            reply.references,
            ["before@example.com", "lunch@example.com"]
        );
    }

    #[test]
    fn an_identifier_is_not_referenced_twice() {
        let raw = PLAIN.replace(
            "Subject:",
            "References: <root@example.com> <lunch@example.com>\r\nSubject:",
        );
        let reply = reply(&original(&raw), Mode::Reply, OWN);
        assert_eq!(reply.references, ["root@example.com", "lunch@example.com"]);
    }

    #[test]
    fn the_text_is_quoted_below_an_attribution_in_the_originals_offset() {
        let quote = reply(&original(PLAIN), Mode::Reply, OWN).quote;
        assert_eq!(
            quote,
            "On 2026-10-17 07:30 +0200, Zoé Ça wrote:\n> Are you free?\n> \n> Zoé\n"
        );

        let raw = PLAIN.replace("+0200", "-0330");
        assert!(
            reply(&original(&raw), Mode::Reply, OWN)
                .quote
                .starts_with("On 2026-10-17 07:30 -0330, ")
        );
    }

    #[test]
    fn a_quote_without_a_name_or_a_date_says_what_it_can() {
        let raw = "From: zoe@example.com\r\nSubject: Hi\r\n\r\nHello\r\n";
        assert_eq!(quote(&original(raw)), "zoe@example.com wrote:\n> Hello\n");
        assert_eq!(attribution(None, None), "someone wrote:");
    }

    #[test]
    fn a_quote_nests_what_was_already_quoted() {
        let raw = "From: zoe@example.com\r\nSubject: Hi\r\n\r\nYes.\r\n\r\n> Are you free?\r\n";
        assert_eq!(
            quote(&original(raw)),
            "zoe@example.com wrote:\n> Yes.\n> \n> > Are you free?\n"
        );
    }

    #[test]
    fn an_html_only_original_is_quoted_from_its_text() {
        let raw = "From: zoe@example.com\r\nSubject: Hi\r\nContent-Type: text/html; charset=utf-8\r\n\r\n\
                   <html><body><p>Are you <b>free</b>?</p></body></html>\r\n";
        let quote = quote(&original(raw));
        assert!(quote.contains("> Are you free?"), "{quote}");
        assert!(!quote.contains('<'), "{quote}");
    }

    #[test]
    fn the_users_text_comes_first_and_the_quote_below() {
        assert_eq!(
            with_quote("Yes!\n", "Zoé wrote:\n> Hi\n"),
            "Yes!\n\nZoé wrote:\n> Hi\n"
        );
        assert_eq!(with_quote("", "Zoé wrote:\n> Hi\n"), "Zoé wrote:\n> Hi\n");
    }

    #[test]
    fn a_forward_is_named_after_the_subject_as_a_file_can_be() {
        assert_eq!(forward_name(Some("Lunch?")), "Lunch?.eml");
        assert_eq!(forward_name(Some("a/b\r\nc")), "a b c.eml");
        assert_eq!(forward_name(Some("   ")), "forwarded message.eml");
        assert_eq!(forward_name(None), "forwarded message.eml");
        assert_eq!(
            forward_name(Some(&"é".repeat(300))).chars().count(),
            100 + 4
        );
    }

    #[test]
    fn an_original_with_an_address_missive_cannot_send_to_is_refused() {
        let raw = "From: zoé@example.com\r\nSubject: Hi\r\n\r\nx\r\n";
        assert!(Original::read(raw.as_bytes()).is_err());
    }

    #[test]
    fn a_subject_that_decodes_to_line_breaks_is_put_on_one_line_and_said() {
        let raw = PLAIN.replace(
            "Subject: Lunch?",
            "Subject: =?utf-8?Q?Lunch=0D=0A=0D=0A?= =?utf-8?Q?now=3F?=",
        );

        let original = original(&raw);
        let reply = reply(&original, Mode::Reply, OWN);

        assert_eq!(reply.subject, "Re: Lunch now?");
        assert!(original.cleaned.subject);
        assert_eq!(original.cleaned.identifiers, 0);
        let notice = original.cleaned.notice().unwrap();
        assert!(notice.contains("subject"), "{notice}");
        assert!(!notice.contains("Lunch"), "{notice}");
    }

    #[test]
    fn an_identifier_with_whitespace_is_left_out_and_the_others_kept() {
        let raw = PLAIN.replace(
            "Subject:",
            "References: <root@example.com> <bad id@example.com> <middle@example.com>\r\n\
                 In-Reply-To: <worse\tid@example.com>\r\nSubject:",
        );

        let original = original(&raw);
        let reply = reply(&original, Mode::Reply, OWN);

        assert_eq!(
            reply.references,
            [
                "root@example.com",
                "middle@example.com",
                "lunch@example.com"
            ]
        );
        assert_eq!(reply.in_reply_to, ["lunch@example.com"]);
        assert_eq!(original.cleaned.identifiers, 2);
        assert_eq!(
            original.cleaned.notice().unwrap(),
            "The original message is odd: 2 message identifiers were left out."
        );
    }

    #[test]
    fn a_message_id_with_whitespace_leaves_the_reply_without_a_parent() {
        let raw = PLAIN
            .replace("<lunch@example.com>", "<lunch @example.com>")
            .replace("Subject:", "References: <root@example.com>\r\nSubject:");

        let original = original(&raw);
        let reply = reply(&original, Mode::Reply, OWN);

        assert!(reply.in_reply_to.is_empty());
        assert_eq!(reply.references, ["root@example.com"].map(String::from));
        assert_eq!(original.cleaned.identifiers, 1);
        assert_eq!(
            original.cleaned.notice().unwrap(),
            "The original message is odd: 1 message identifier was left out."
        );
    }

    #[test]
    fn an_identifier_longer_than_a_line_is_left_out() {
        let long = format!("<{}@example.com>", "x".repeat(1200));
        let raw = PLAIN.replace(
            "Subject:",
            &format!("References: {long} <root@example.com>\r\nSubject:"),
        );

        let original = original(&raw);
        let reply = reply(&original, Mode::Reply, OWN);

        assert_eq!(reply.references, ["root@example.com", "lunch@example.com"]);
        assert_eq!(original.cleaned.identifiers, 1);
    }

    #[test]
    fn an_ordinary_original_has_nothing_to_tell() {
        let original = original(PLAIN);
        assert_eq!(original.cleaned, Cleaned::default());
        assert_eq!(original.cleaned.notice(), None);
    }

    #[test]
    fn a_forward_of_a_subject_with_line_breaks_is_put_on_one_line_and_said() {
        let forwarded = answering(
            "alice/1/2/3",
            PLAIN.as_bytes(),
            Some("Lunch\r\n?"),
            None,
            OWN,
        )
        .unwrap();

        assert_eq!(forwarded.subject.as_deref(), Some("Fwd: Lunch ?"));
        assert_eq!(forwarded.attachment.unwrap().name, "Lunch ?.eml");
        assert!(forwarded.notice.unwrap().contains("subject"));
    }

    #[test]
    fn a_forward_names_the_message_to_mark_forwarded_and_a_reply_the_one_to_mark_answered() {
        let forwarded =
            answering("alice/1/2/3", PLAIN.as_bytes(), Some("Lunch?"), None, OWN).unwrap();
        assert_eq!(forwarded.forwards.as_deref(), Some("alice/1/2/3"));
        assert_eq!(forwarded.answers, None);

        let replied = answering(
            "alice/1/2/3",
            PLAIN.as_bytes(),
            Some("Lunch?"),
            Some(Mode::Reply),
            OWN,
        )
        .unwrap();
        assert_eq!(replied.answers.as_deref(), Some("alice/1/2/3"));
        assert_eq!(replied.forwards, None);
    }

    #[test]
    fn a_reply_all_leaves_out_the_address_of_the_account_that_sends_it() {
        // Alice's message, answered from Bob's account: Bob's address is the one to leave out,
        // Alice's stays among the recipients.
        let raw = PLAIN
            .replace("carl@example.net", "bob@example.net")
            .replace("From: Zoé Ça <zoe@example.com>", "From: zoe@example.com");

        let as_bob = reply(&original(&raw), Mode::ReplyAll, "bob@example.net");
        assert_eq!(
            addresses(&as_bob.to),
            ["zoe@example.com", "alice@example.org"]
        );

        let as_alice = reply(&original(&raw), Mode::ReplyAll, OWN);
        assert_eq!(
            addresses(&as_alice.to),
            ["zoe@example.com", "bob@example.net"]
        );
    }
}
