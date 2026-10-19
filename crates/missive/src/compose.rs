//! `missive compose`: writes a message and gives it to the daemon, which saves it as a draft in
//! the account's Drafts folder or, with `--send`, puts it in the outbox and sends it as soon as
//! the account's server can be reached (see docs/cli.md). `--draft` picks a draft up again: the
//! fields given replace its own. The files to attach are read here, when the command runs: they
//! may be gone by the time the message is sent.
//!
//! `--reply`, `--reply-all` and `--forward` start from a message of the local copy: what to put in
//! the message (recipients, subject, quote, thread, the original attached) is worked out here
//! from the copy, by `missive_core::reply`, and what the user gives replaces it. The daemon then
//! gets an ordinary message to save or send.

use std::io::Read;
use std::path::PathBuf;

use clap::{ArgGroup, Args};
use missive_core::account::AccountStore;
use missive_core::actions::ORIGINAL_GONE;
use missive_core::compose::{Attachment, Draft, Mailbox};
use missive_core::copy::MessageId;
use missive_core::draft_lock;
use missive_core::protocol::{Answer, Attached, Command, SaveDraft, Send};
use missive_core::reply::{self, Answering, Mode};
use serde::Serialize;

use crate::act::{self, ActError};
use crate::read::{self, ShowError};

#[derive(Args)]
#[command(group(ArgGroup::new("answering").args(["reply", "reply_all", "forward"])))]
pub struct ComposeArgs {
    /// The account to send from, by identifier [default: the account of the message answered or
    /// forwarded, or of the draft, else the only account].
    #[arg(long)]
    account: Option<String>,
    /// Reply to this message's sender: its identifier, as missive search and list print it. The
    /// recipients, subject, quote and thread are the message's; the fields given replace them.
    #[arg(long, value_name = "MESSAGE ID", conflicts_with = "draft")]
    reply: Option<String>,
    /// Reply to this message's sender and to everyone it went to, but the account itself.
    #[arg(long, value_name = "MESSAGE ID", conflicts_with = "draft")]
    reply_all: Option<String>,
    /// Forward this message: it goes along as an attachment, untouched.
    #[arg(long, value_name = "MESSAGE ID", conflicts_with = "draft")]
    forward: Option<String>,
    /// The draft to pick up again: its identifier as compose printed it, or a message
    /// identifier of the Drafts folder, as missive list prints it. The fields given replace its
    /// own; the others are kept.
    #[arg(long, value_name = "DRAFT ID")]
    draft: Option<String>,
    /// A recipient: name@example.org, or "Name <name@example.org>".
    #[arg(long, value_name = "ADDRESS")]
    to: Vec<String>,
    /// A recipient in copy.
    #[arg(long, value_name = "ADDRESS")]
    cc: Vec<String>,
    /// A recipient in blind copy: the server is told, the message sent does not name them.
    #[arg(long, value_name = "ADDRESS")]
    bcc: Vec<String>,
    /// With --draft: takes every recipient (To) off the draft.
    #[arg(long, requires = "draft", conflicts_with = "to")]
    no_to: bool,
    /// With --draft: takes every recipient in copy off the draft.
    #[arg(long, requires = "draft", conflicts_with = "cc")]
    no_cc: bool,
    /// With --draft: takes every recipient in blind copy off the draft.
    #[arg(long, requires = "draft", conflicts_with = "bcc")]
    no_bcc: bool,
    #[arg(long)]
    subject: Option<String>,
    /// The text of the message, from a file (UTF-8).
    #[arg(long, value_name = "PATH", conflicts_with = "body")]
    body_file: Option<PathBuf>,
    /// The text of the message; `-` reads it from standard input (UTF-8).
    #[arg(long, value_name = "TEXT")]
    body: Option<String>,
    /// A file to attach; its media type is guessed from its name.
    #[arg(long, value_name = "PATH")]
    attach: Vec<PathBuf>,
    /// With --draft: takes every attachment off the draft.
    #[arg(long, requires = "draft", conflicts_with = "attach")]
    no_attach: bool,
    /// Send the message (through the outbox, offline included); a draft picked up is removed once
    /// the server took it. Without it the message is saved as a draft.
    #[arg(long)]
    send: bool,
    /// Open no compose window, even when a display is set: the draft is only saved.
    #[arg(long)]
    no_window: bool,
    /// One JSON object on standard output (see docs/cli.md).
    #[arg(long)]
    json: bool,
}

#[derive(Serialize)]
struct Queued {
    outbox: i64,
    message_id: String,
}

#[derive(Serialize)]
struct Saved {
    id: String,
}

pub async fn run(args: ComposeArgs) -> Result<(), ActError> {
    let (mode, id) = match (&args.reply, &args.reply_all, &args.forward) {
        (Some(id), _, _) => (Some(Some(Mode::Reply)), Some(id)),
        (_, Some(id), _) => (Some(Some(Mode::ReplyAll)), Some(id)),
        (_, _, Some(id)) => (Some(None), Some(id)),
        _ => (None, None),
    };
    let stored = match id {
        Some(id) => Some(read::message(id).map_err(|e| match e {
            ShowError::NotFound(why) => ActError::NotFound(why),
            ShowError::Failed(why) => ActError::Failed(why),
        })?),
        None => None,
    };
    // A message identifier names its account.
    let named = match (
        &args.account,
        args.draft.as_deref().and_then(MessageId::parse),
        &stored,
    ) {
        (None, Some(id), _) => Some(id.account),
        (None, None, Some(stored)) => Some(stored.hit.account.clone()),
        (named, _, _) => named.clone(),
    };
    let account = account(named)?;
    if let Some(draft) = &args.draft {
        refuse_if_open(&account, draft)?;
    }
    let answering = match (stored, mode) {
        (Some(stored), Some(mode)) => Some(answering(&account, stored, mode)?),
        _ => None,
    };
    let body = match (args.body_file, args.body) {
        (Some(path), _) => Some(
            std::fs::read(&path)
                .map_err(|e| format!("cannot read {}: {e}", path.display()))
                .and_then(|bytes| {
                    String::from_utf8(bytes)
                        .map_err(|_| format!("{} is not UTF-8 text", path.display()))
                })?,
        ),
        (None, Some(body)) if body == "-" => {
            let mut bytes = Vec::new();
            std::io::stdin()
                .read_to_end(&mut bytes)
                .map_err(|e| format!("cannot read standard input: {e}"))?;
            Some(
                String::from_utf8(bytes)
                    .map_err(|_| "standard input is not UTF-8 text".to_owned())?,
            )
        }
        (None, body) => body,
    };
    let mut attachments = Vec::new();
    for path in &args.attach {
        let data =
            std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .ok_or_else(|| format!("{} names no file", path.display()))?;
        attachments.push(Attachment { name, data });
    }
    // What the user gives replaces what the message answered suggests.
    let a = answering.unwrap_or_default();
    if let Some(notice) = &a.notice {
        eprintln!("{notice}");
    }
    attachments.splice(0..0, a.attachment);
    let texts = |list: Vec<Mailbox>| -> Vec<String> { list.iter().map(Mailbox::to_text).collect() };
    let to = if args.to.is_empty() {
        texts(a.to)
    } else {
        args.to
    };
    let cc = if args.cc.is_empty() {
        texts(a.cc)
    } else {
        args.cc
    };
    let subject = args.subject.or(a.subject);
    let text = match &a.quote {
        Some(quote) => reply::with_quote(body.as_deref().unwrap_or_default(), quote),
        None => body.clone().unwrap_or_default(),
    };
    let draft = Draft {
        to: mailboxes(&to)?,
        cc: mailboxes(&cc)?,
        bcc: mailboxes(&args.bcc)?,
        subject,
        body: text,
        html: None,
        inline: Vec::new(),
        attachments,
        in_reply_to: a.in_reply_to,
        references: a.references,
    };
    // What can be told here is told before the daemon is asked; a draft picked up again is
    // checked by the daemon, once its own fields are known.
    match (args.send, &args.draft) {
        (true, None) => draft.check()?,
        _ => draft.check_fields()?,
    }
    let attached = |attachments: Vec<Attachment>| -> Vec<Attached> {
        attachments
            .into_iter()
            .map(|a| Attached {
                name: a.name,
                data: a.data,
            })
            .collect()
    };
    if args.send && args.draft.is_none() {
        return send(
            Send {
                account,
                to,
                cc,
                bcc: args.bcc,
                subject: draft.subject,
                body: draft.body,
                attachments: attached(draft.attachments),
                in_reply_to: draft.in_reply_to.first().cloned(),
                references: draft.references,
                answers: a.answers,
                forwards: a.forwards,
            },
            args.json,
        )
        .await;
    }

    // Picking a draft up again, only what was given changes (an empty list takes the field off:
    // `--no-cc`…); a new draft is all that is given.
    let given = |list: Vec<String>, cleared: bool| match (cleared, &args.draft) {
        (true, _) => Some(Vec::new()),
        (false, None) => Some(list),
        (false, Some(_)) => (!list.is_empty()).then_some(list),
    };
    let answer = act::ask(Command::Draft(SaveDraft {
        account: account.clone(),
        draft: args.draft.clone(),
        to: given(to, args.no_to),
        cc: given(cc, args.no_cc),
        bcc: given(args.bcc, args.no_bcc),
        subject: draft.subject,
        body: if args.draft.is_none() {
            Some(draft.body)
        } else {
            body
        },
        attachments: (args.draft.is_none() || !args.attach.is_empty() || args.no_attach)
            .then(|| attached(draft.attachments)),
        in_reply_to: draft.in_reply_to.first().cloned(),
        references: draft.references,
        send: args.send,
        answers: a.answers,
        forwards: a.forwards,
    }))
    .await?;
    let answer = answered(answer)?;
    let Some(message_id) = answer.message_id else {
        return Err(unreadable());
    };
    if let Some(kept) = answer.kept {
        eprintln!(
            "Draft {kept} was written by another client: it is kept as it is in the Drafts \
             folder, beside the new one (delete it yourself if you no longer want it)."
        );
    }
    if answer.html_dropped {
        eprintln!("The draft had an HTML part, which is dropped: the text you gave replaces it.");
    }
    if args.send {
        let Some(outbox) = answer.outbox else {
            return Err(unreadable());
        };
        return queued(outbox, message_id, args.json);
    }
    // Saved first: the window shows the draft, and nothing is lost if it cannot start.
    if !args.no_window {
        crate::window::compose(&account, &message_id);
    }
    if args.json {
        println!("{}", serde_json::to_string(&Saved { id: message_id })?);
    } else {
        println!(
            "Draft {message_id} is saved: it is written to the account's Drafts folder as soon \
             as the server can be reached (missive compose --draft {message_id} picks it up \
             again)."
        );
    }
    Ok(())
}

/// Refuses to change a draft that is open in a compose window, before anything is saved: the
/// window does not know of a new version, and its next save would overwrite it. It never waits
/// for the window and never takes its lock. The draft is named by its `Message-ID`, or by the
/// identifier of its message in the copy, whose `Message-ID` is the one its window locked.
fn refuse_if_open(account: &str, draft: &str) -> Result<(), ActError> {
    let key = match read::message(draft) {
        Ok(stored) => missive_core::compose::draft_message_id(&stored.raw),
        // The name the daemon finds the draft by: its brackets go, as `find_draft` strips them.
        Err(_) if MessageId::parse(draft).is_none() => Some(
            draft
                .trim()
                .trim_start_matches('<')
                .trim_end_matches('>')
                .to_owned(),
        ),
        Err(_) => None,
    };
    if key.is_some_and(|key| draft_lock::is_held(account, &key)) {
        eprintln!(
            "missive: the draft {draft} is open in a compose window: nothing was changed or sent. \
             Work on it there, or close the window first."
        );
        return Err(ActError::Exit(act::OPEN_IN_WINDOW));
    }
    Ok(())
}

/// Works out a reply (`Some(mode)`) or a forward (`None`) to `stored`, written from `account`.
fn answering(
    account: &str,
    stored: missive_core::copy::Stored,
    mode: Option<Mode>,
) -> Result<Answering, ActError> {
    let address = AccountStore::from_env()
        .and_then(|store| store.get(account))
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no account {account} (missive account list shows them)"))?
        .address;
    Ok(reply::answering(
        &stored.hit.id,
        &stored.raw,
        stored.hit.subject.as_deref(),
        mode,
        &address,
    )?)
}

fn mailboxes(list: &[String]) -> Result<Vec<Mailbox>, String> {
    list.iter().map(|text| Mailbox::parse(text)).collect()
}

/// Sends a new message, through the outbox.
async fn send(message: Send, json: bool) -> Result<(), ActError> {
    let answer = answered(act::ask(Command::Send(message)).await?)?;
    let (Some(outbox), Some(message_id)) = (answer.outbox, answer.message_id) else {
        return Err(unreadable());
    };
    queued(outbox, message_id, json)
}

/// The answer of a daemon that did what was asked, else why it did not.
pub(crate) fn answered(answer: Answer) -> Result<Answer, ActError> {
    if answer.ok {
        return Ok(answer);
    }
    let message = answer.message.unwrap_or_else(|| {
        format!(
            "the daemon refused: {}",
            answer.error.clone().unwrap_or_default()
        )
    });
    Err(match answer.error.as_deref() {
        Some("no-such-message" | ORIGINAL_GONE) => ActError::NotFound(message),
        _ => ActError::Failed(message),
    })
}

pub(crate) fn unreadable() -> ActError {
    ActError::Failed("the daemon gave an answer this missive cannot read".to_owned())
}

/// Tells that a message is in the outbox.
fn queued(outbox: i64, message_id: String, json: bool) -> Result<(), ActError> {
    if json {
        println!("{}", serde_json::to_string(&Queued { outbox, message_id })?);
    } else {
        println!(
            "Message {outbox} is in the outbox: it is sent as soon as the server can be reached \
             (missive watch tells when, missive status shows what waits)."
        );
    }
    Ok(())
}

/// The account named, or the only one.
pub(crate) fn account(named: Option<String>) -> Result<String, String> {
    let listing = AccountStore::from_env()
        .and_then(|store| store.list())
        .map_err(|e| e.to_string())?;
    let ids: Vec<String> = listing.accounts.into_iter().map(|(id, _)| id).collect();
    match named {
        Some(id) if ids.contains(&id) => Ok(id),
        Some(id) => Err(format!("no account {id} (missive account list shows them)")),
        None => match &ids[..] {
            [id] => Ok(id.clone()),
            [] => Err("there is no account to send from (missive account add)".to_owned()),
            _ => Err("there are several accounts: say which one with --account".to_owned()),
        },
    }
}
