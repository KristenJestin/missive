//! `missive compose`: writes a message and gives it to the daemon, which saves it as a draft in
//! the account's Drafts folder or, with `--send`, puts it in the outbox and sends it as soon as
//! the account's server can be reached (see docs/cli.md). `--draft` picks a draft up again: the
//! fields given replace its own. The files to attach are read here, when the command runs: they
//! may be gone by the time the message is sent.

use std::io::Read;
use std::path::PathBuf;

use clap::Args;
use missive_core::account::AccountStore;
use missive_core::compose::{Attachment, Draft, Mailbox};
use missive_core::copy::MessageId;
use missive_core::protocol::{Answer, Attached, Command, SaveDraft, Send};
use serde::Serialize;

use crate::act::{self, ActError};

#[derive(Args)]
pub struct ComposeArgs {
    /// The account to send from, by identifier [default: the only account, or the draft's].
    #[arg(long)]
    account: Option<String>,
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
    /// Send the message (through the outbox, offline included); a draft picked up is removed once
    /// the server took it. Without it the message is saved as a draft.
    #[arg(long)]
    send: bool,
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
    // A message identifier names its account.
    let named = match (
        &args.account,
        args.draft.as_deref().and_then(MessageId::parse),
    ) {
        (None, Some(id)) => Some(id.account),
        (named, _) => named.clone(),
    };
    let account = account(named)?;
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
    let draft = Draft {
        to: mailboxes(&args.to)?,
        cc: mailboxes(&args.cc)?,
        bcc: mailboxes(&args.bcc)?,
        subject: args.subject,
        body: body.clone().unwrap_or_default(),
        attachments,
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
                to: args.to,
                cc: args.cc,
                bcc: args.bcc,
                subject: draft.subject,
                body: draft.body,
                attachments: attached(draft.attachments),
            },
            args.json,
        )
        .await;
    }

    // Picking a draft up again, only what was given changes; a new draft is all that is given.
    let given = |list: Vec<String>| (args.draft.is_none() || !list.is_empty()).then_some(list);
    let answer = act::ask(Command::Draft(SaveDraft {
        account,
        draft: args.draft.clone(),
        to: given(args.to),
        cc: given(args.cc),
        bcc: given(args.bcc),
        subject: draft.subject,
        body: if args.draft.is_none() {
            Some(draft.body)
        } else {
            body
        },
        attachments: (args.draft.is_none() || !args.attach.is_empty())
            .then(|| attached(draft.attachments)),
        send: args.send,
    }))
    .await?;
    let answer = answered(answer)?;
    let Some(message_id) = answer.message_id else {
        return Err(unreadable());
    };
    if args.send {
        let Some(outbox) = answer.outbox else {
            return Err(unreadable());
        };
        return queued(outbox, message_id, args.json);
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
fn answered(answer: Answer) -> Result<Answer, ActError> {
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
        Some("no-such-message") => ActError::NotFound(message),
        _ => ActError::Failed(message),
    })
}

fn unreadable() -> ActError {
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
fn account(named: Option<String>) -> Result<String, String> {
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
