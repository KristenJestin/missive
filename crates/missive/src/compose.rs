//! `missive compose`: writes a message and, with `--send`, gives it to the daemon, which puts it
//! in the outbox and sends it as soon as the account's server can be reached (see docs/cli.md).
//! The files to attach are read here, when the command runs: they may be gone by the time the
//! message is sent.

use std::io::Read;
use std::path::PathBuf;

use clap::Args;
use missive_core::account::AccountStore;
use missive_core::compose::{Attachment, Draft, Mailbox};
use missive_core::protocol::{Attached, Command, Send};
use serde::Serialize;

use crate::act::{self, ActError};

#[derive(Args)]
pub struct ComposeArgs {
    /// The account to send from, by identifier [default: the only account].
    #[arg(long)]
    account: Option<String>,
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
    /// Send the message (through the outbox, offline included). Without it the message is only
    /// checked: drafts come in a later version.
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

pub async fn run(args: ComposeArgs) -> Result<(), ActError> {
    let account = account(args.account)?;
    let mailboxes = |list: &[String]| -> Result<Vec<Mailbox>, String> {
        list.iter().map(|text| Mailbox::parse(text)).collect()
    };
    let body = match (args.body_file, args.body) {
        (Some(path), _) => std::fs::read(&path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))
            .and_then(|bytes| {
                String::from_utf8(bytes)
                    .map_err(|_| format!("{} is not UTF-8 text", path.display()))
            })?,
        (None, Some(body)) if body == "-" => {
            let mut bytes = Vec::new();
            std::io::stdin()
                .read_to_end(&mut bytes)
                .map_err(|e| format!("cannot read standard input: {e}"))?;
            String::from_utf8(bytes).map_err(|_| "standard input is not UTF-8 text".to_owned())?
        }
        (None, Some(body)) => body,
        (None, None) => String::new(),
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
        body,
        attachments,
    };
    draft.check()?;
    if !args.send {
        return Err(ActError::Failed(
            "the message is valid, but saving drafts comes in a later version of Missive: add \
             --send to send it now"
                .to_owned(),
        ));
    }

    let answer = act::ask(Command::Send(Send {
        account,
        to: args.to,
        cc: args.cc,
        bcc: args.bcc,
        subject: draft.subject,
        body: draft.body,
        attachments: draft
            .attachments
            .into_iter()
            .map(|a| Attached {
                name: a.name,
                data: a.data,
            })
            .collect(),
    }))
    .await?;
    if !answer.ok {
        return Err(ActError::Failed(answer.message.unwrap_or_else(|| {
            format!("the daemon refused: {}", answer.error.unwrap_or_default())
        })));
    }
    let (Some(outbox), Some(message_id)) = (answer.outbox, answer.message_id) else {
        return Err(ActError::Failed(
            "the daemon gave an answer this missive cannot read".to_owned(),
        ));
    };
    if args.json {
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
