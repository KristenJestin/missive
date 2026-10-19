//! What the compose window holds, without the window: the fields of a message being written,
//! its draft, and what to ask the daemon. A reply's or a forward's fields come from
//! `missive_core::reply::answering`, as `missive compose --reply` gets them.
//!
//! The text is saved as a draft through the daemon's `draft` request [`SAVE_AFTER`] once the
//! typing stops, and when the window closes; a new message left empty saves nothing. Send puts
//! the draft in the outbox through the same request, with `send`: the daemon answers `busy` to a
//! draft already on its way, so a send asked again never sends twice. Whatever the daemon
//! answers, what was typed stays in the window until it is saved or sent.

use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

use missive_core::compose::{Attachment, Draft, Mailbox};
use missive_core::protocol::{Answer, Attached, Command, SaveDraft};
use missive_core::reply::{self, Answering};

/// How long after the last change the draft is saved.
pub const SAVE_AFTER: Duration = Duration::from_secs(3);

/// A field the user types in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Field {
    To,
    Cc,
    Bcc,
    Subject,
    Body,
}

/// Where the message is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Editing,
    /// Send was asked: nothing changes until the daemon answers.
    Sending,
    /// In the outbox: the window closes.
    Sent,
    /// Saved on closing, or nothing to save: the window closes.
    Closed,
}

/// The wait after a change, to start with [`Compose::take_save_timer`] and end with
/// [`Compose::save_timer_elapsed`].
#[derive(Debug)]
pub struct SaveTimer {
    /// The change it is for: a later one makes it stale.
    edits: u64,
    pub after: Duration,
}

/// The request waiting for the daemon's answer, and the changes it carries.
#[derive(Clone, Copy, Debug)]
struct InFlight {
    send: bool,
    edits: u64,
    attachments: u64,
}

pub struct Compose {
    /// The accounts the message can be written from.
    pub accounts: Vec<String>,
    pub account: String,
    pub to: String,
    pub cc: String,
    pub bcc: String,
    /// Whether the Bcc field is shown: on asking, or when the draft has one.
    pub show_bcc: bool,
    pub subject: String,
    pub body: String,
    pub attachments: Vec<Attachment>,
    in_reply_to: Option<String>,
    references: Vec<String>,
    /// A reply: the message to mark answered once it is sent.
    answers: Option<String>,
    /// A forward: the message to mark forwarded once it is sent.
    forwards: Option<String>,
    /// The draft's identifier, once the daemon saved it.
    pub draft: Option<String>,
    pub state: State,
    /// Changes made, and those the daemon saved.
    edits: u64,
    saved: u64,
    /// The same, for the attachments alone: they are sent only when they changed.
    attachment_edits: u64,
    attachments_saved: u64,
    /// Whether a save or a send is to ask.
    save_wanted: bool,
    send_wanted: bool,
    in_flight: Option<InFlight>,
    save_timer: Option<SaveTimer>,
    /// Closing waits for the draft's save; a save that failed lets the next close discard.
    closing: bool,
    close_failed: bool,
    /// What went wrong, for people.
    pub notice: Option<String>,
}

impl Compose {
    /// A new message, from `account`.
    pub fn new(accounts: Vec<String>, account: String) -> Compose {
        Compose {
            accounts,
            account,
            to: String::new(),
            cc: String::new(),
            bcc: String::new(),
            show_bcc: false,
            subject: String::new(),
            body: String::new(),
            attachments: Vec::new(),
            in_reply_to: None,
            references: Vec::new(),
            answers: None,
            forwards: None,
            draft: None,
            state: State::Editing,
            edits: 0,
            saved: 0,
            attachment_edits: 0,
            attachments_saved: 0,
            save_wanted: false,
            send_wanted: false,
            in_flight: None,
            save_timer: None,
            closing: false,
            close_failed: false,
            notice: None,
        }
    }

    /// A reply or a forward: its recipients, subject and thread, the quote below an empty line
    /// for the user's text, the forwarded message attached.
    pub fn answering(accounts: Vec<String>, account: String, answering: Answering) -> Compose {
        let mut compose = Compose::new(accounts, account);
        compose.to = texts(&answering.to);
        compose.cc = texts(&answering.cc);
        compose.subject = answering.subject.unwrap_or_default();
        if let Some(quote) = &answering.quote {
            compose.body = format!("\n\n{}", reply::with_quote("", quote));
        }
        compose.attachments.extend(answering.attachment);
        compose.in_reply_to = answering.in_reply_to.into_iter().next();
        compose.references = answering.references;
        compose.answers = answering.answers;
        compose.forwards = answering.forwards;
        compose.notice = answering.notice;
        compose
    }

    /// A draft picked up again: `id` names it as the daemon's `draft` request takes it, `raw` is
    /// its latest version.
    pub fn draft(
        accounts: Vec<String>,
        account: String,
        id: String,
        raw: &[u8],
    ) -> Result<Compose, String> {
        let draft = Draft::read(raw)?;
        let mut compose = Compose::new(accounts, account);
        compose.to = texts(&draft.to);
        compose.cc = texts(&draft.cc);
        compose.bcc = texts(&draft.bcc);
        compose.show_bcc = !draft.bcc.is_empty();
        compose.subject = draft.subject.unwrap_or_default();
        compose.body = draft.body.replace("\r\n", "\n");
        compose.attachments = draft.attachments;
        compose.in_reply_to = draft.in_reply_to.into_iter().next();
        compose.references = draft.references;
        compose.draft = Some(id);
        Ok(compose)
    }

    /// The user changed a field.
    pub fn edit(&mut self, field: Field, value: String) {
        if self.state != State::Editing {
            return;
        }
        let text = match field {
            Field::To => &mut self.to,
            Field::Cc => &mut self.cc,
            Field::Bcc => &mut self.bcc,
            Field::Subject => &mut self.subject,
            Field::Body => &mut self.body,
        };
        if *text == value {
            return;
        }
        *text = value;
        self.changed();
    }

    /// Shows the Bcc field.
    pub fn ask_bcc(&mut self) {
        self.show_bcc = true;
    }

    /// Whether the account can still be chosen: not once the draft is saved, where it belongs
    /// to its account.
    pub fn can_pick_account(&self) -> bool {
        self.accounts.len() > 1 && self.draft.is_none() && self.in_flight.is_none()
    }

    /// Writes from another account.
    pub fn pick_account(&mut self, account: &str) {
        if self.can_pick_account()
            && self.account != account
            && self.accounts.iter().any(|a| a == account)
        {
            self.account = account.to_owned();
            self.changed();
        }
    }

    /// Attaches the files [`load`] read, and says why the others were not.
    pub fn attach(&mut self, loaded: Vec<Result<Attachment, String>>) {
        if self.state != State::Editing {
            self.notice = Some("Not attached: the message is being sent.".to_owned());
            return;
        }
        let mut problems = Vec::new();
        let mut added = false;
        for file in loaded {
            match file {
                Ok(attachment) => {
                    self.attachments.push(attachment);
                    added = true;
                }
                Err(problem) => problems.push(problem),
            }
        }
        if added {
            self.attachment_edits += 1;
            self.changed();
        }
        if !problems.is_empty() {
            self.notice = Some(format!("Not attached: {}.", problems.join("; ")));
        }
    }

    /// Takes the attachment at `at` off.
    pub fn remove_attachment(&mut self, at: usize) {
        if self.state == State::Editing && at < self.attachments.len() {
            self.attachments.remove(at);
            self.attachment_edits += 1;
            self.changed();
        }
    }

    /// The save timer a change asks for, to start once.
    pub fn take_save_timer(&mut self) -> Option<SaveTimer> {
        self.save_timer.take()
    }

    /// The typing stopped for [`SAVE_AFTER`]: the draft is saved, unless something changed since.
    pub fn save_timer_elapsed(&mut self, timer: SaveTimer) {
        if timer.edits == self.edits && self.state == State::Editing && self.needs_save() {
            self.save_wanted = true;
        }
    }

    /// `Ctrl+Enter`: puts the message in the outbox, once. Asked again while it is on its way,
    /// it does nothing.
    pub fn send(&mut self) {
        if self.state != State::Editing {
            return;
        }
        if [&self.to, &self.cc, &self.bcc]
            .iter()
            .all(|field| addresses(field).is_empty())
        {
            self.notice = Some("The message has no recipient: To, Cc or Bcc.".to_owned());
            return;
        }
        self.notice = None;
        self.state = State::Sending;
        self.send_wanted = true;
    }

    /// The window is asked to close: whether it may close now. A draft to save is saved first,
    /// and the window closes once the daemon saved it ([`Compose::done`]). A save that failed
    /// keeps the window open, and the next close discards what was typed.
    pub fn close(&mut self) -> bool {
        match self.state {
            State::Sent | State::Closed => return true,
            State::Sending => return false,
            State::Editing => {}
        }
        if self.close_failed || !self.needs_save() {
            self.state = State::Closed;
            return true;
        }
        self.closing = true;
        self.save_wanted = true;
        false
    }

    /// Whether the window is to close: the message is sent, or saved on closing.
    pub fn done(&self) -> bool {
        matches!(self.state, State::Sent | State::Closed)
    }

    /// The next request for the daemon: one at a time, built from the fields as they are now.
    pub fn take_request(&mut self) -> Option<Command> {
        if self.in_flight.is_some() {
            return None;
        }
        // A new message is saved first: a send asked again then names its draft, and the daemon
        // knows it is already on its way.
        let send = self.send_wanted && self.draft.is_some();
        if !send && !self.save_wanted && !self.send_wanted {
            return None;
        }
        self.save_wanted = false;
        if send {
            self.send_wanted = false;
        }
        self.in_flight = Some(InFlight {
            send,
            edits: self.edits,
            attachments: self.attachment_edits,
        });
        let attachments = (self.draft.is_none() || self.attachment_edits != self.attachments_saved)
            .then(|| {
                self.attachments
                    .iter()
                    .map(|a| Attached {
                        name: a.name.clone(),
                        data: a.data.clone(),
                    })
                    .collect()
            });
        let subject =
            (self.draft.is_some() || !self.subject.is_empty()).then(|| self.subject.clone());
        Some(Command::Draft(SaveDraft {
            account: self.account.clone(),
            draft: self.draft.clone(),
            to: Some(addresses(&self.to)),
            cc: Some(addresses(&self.cc)),
            bcc: Some(addresses(&self.bcc)),
            subject,
            body: Some(self.body.clone()),
            attachments,
            in_reply_to: self.in_reply_to.clone(),
            references: self.references.clone(),
            send,
            answers: if send { self.answers.clone() } else { None },
            forwards: if send { self.forwards.clone() } else { None },
        }))
    }

    /// The daemon's answer to the request taken last, or why it could not be asked.
    pub fn answered(&mut self, answer: Result<Answer, String>) {
        let Some(asked) = self.in_flight.take() else {
            return;
        };
        let refused = match answer {
            Ok(answer) if answer.ok => {
                if let Some(id) = answer.message_id {
                    self.draft = Some(id);
                }
                self.saved = asked.edits;
                self.attachments_saved = asked.attachments;
                if asked.send {
                    self.state = State::Sent;
                } else if self.closing && self.state == State::Editing {
                    // What was typed while this save was on its way is still to save.
                    if self.needs_save() {
                        self.save_wanted = true;
                    } else {
                        self.state = State::Closed;
                    }
                }
                if !self.closing && self.state != State::Sending {
                    self.notice = None;
                }
                return;
            }
            // The draft is already in the outbox: a send whose answer was lost, asked again.
            Ok(answer) if asked.send && answer.error.as_deref() == Some("busy") => {
                self.state = State::Sent;
                return;
            }
            Ok(answer) => {
                let error = answer.error.unwrap_or_default();
                match answer.message {
                    Some(message) => format!("refused ({error}): {message}"),
                    None => format!("refused ({error})"),
                }
            }
            Err(e) => format!("the daemon could not be asked ({e})"),
        };
        let what = if self.state == State::Sending {
            "Not sent"
        } else {
            "Not saved"
        };
        self.state = State::Editing;
        self.send_wanted = false;
        let then = if self.closing {
            self.closing = false;
            self.close_failed = true;
            " Close again to discard it."
        } else {
            ""
        };
        self.notice = Some(format!(
            "{what}, {refused}. What you wrote is kept in this window.{then}"
        ));
    }

    /// Whether something changed since the last save, and is worth a draft: a new message left
    /// empty is not.
    pub fn needs_save(&self) -> bool {
        self.edits != self.saved && (self.draft.is_some() || !self.is_empty())
    }

    fn is_empty(&self) -> bool {
        [&self.to, &self.cc, &self.bcc, &self.subject, &self.body]
            .iter()
            .all(|field| field.trim().is_empty())
            && self.attachments.is_empty()
    }

    fn changed(&mut self) {
        self.edits += 1;
        self.close_failed = false;
        self.save_timer = Some(SaveTimer {
            edits: self.edits,
            after: SAVE_AFTER,
        });
    }
}

/// The most bytes of one attachment: the message is held in memory, sent to the daemon and
/// written to its copy whole.
pub const MAX_ATTACHMENT: u64 = 25 * 1024 * 1024;

/// Reads the files to attach, now, as they may be gone by the time the message is sent. It
/// blocks on the disk: the window calls it off its own thread.
pub fn load(paths: Vec<PathBuf>) -> Vec<Result<Attachment, String>> {
    paths
        .into_iter()
        .map(|path| {
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .ok_or_else(|| format!("{} names no file", path.display()))?;
            let unreadable = |e: std::io::Error| format!("{} cannot be read ({e})", path.display());
            // Looked at before it is opened: opening a FIFO waits for a writer.
            let meta = std::fs::metadata(&path).map_err(unreadable)?;
            if !meta.is_file() {
                return Err(format!("{} is not a regular file", path.display()));
            }
            let too_big = || {
                format!(
                    "{} is larger than {} MiB",
                    path.display(),
                    MAX_ATTACHMENT >> 20
                )
            };
            if meta.len() > MAX_ATTACHMENT {
                return Err(too_big());
            }
            // One byte more than the cap shows a file that grew since it was looked at.
            let mut data = Vec::new();
            std::fs::File::open(&path)
                .and_then(|file| file.take(MAX_ATTACHMENT + 1).read_to_end(&mut data))
                .map_err(unreadable)?;
            if data.len() as u64 > MAX_ATTACHMENT {
                return Err(too_big());
            }
            Ok(Attachment { name, data })
        })
        .collect()
}

/// Recipients as a field shows them, `"Name" <address>` or `address`, comma-separated.
fn texts(list: &[Mailbox]) -> String {
    list.iter()
        .map(Mailbox::to_text)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The addresses of a field: split on commas and semicolons outside quotes and angle brackets,
/// each one trimmed, the empty ones left out. Whether each is an address is the daemon's to say.
pub fn addresses(field: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut current = String::new();
    let (mut quoted, mut angle) = (false, false);
    for c in field.chars() {
        match c {
            '"' if !angle => quoted = !quoted,
            '<' if !quoted => angle = true,
            '>' if !quoted => angle = false,
            ',' | ';' if !quoted && !angle => {
                found.push(std::mem::take(&mut current));
                continue;
            }
            _ => {}
        }
        current.push(c);
    }
    found.push(current);
    found
        .into_iter()
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
        .collect()
}
