//! `missive mark`, `missive move` and `missive delete`: they ask the daemon, which records the
//! action, shows it in the local copy at once, and applies it on the server when it can (see
//! docs/cli.md). They answer once the action is recorded, connected or not.

use clap::{ArgGroup, Args};
use missive_core::protocol::{Answer, Command, PROTOCOL, Request};
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::daemon;

/// The exit status when an identifier names no message, as for `missive show`.
pub const NOT_FOUND: u8 = 3;

#[derive(Args)]
#[command(group(ArgGroup::new("mark").required(true).args(["read", "unread", "flagged", "unflagged"])))]
pub struct MarkArgs {
    /// The messages, by identifier, as missive search and list print them.
    #[arg(required = true, value_name = "MESSAGE ID")]
    messages: Vec<String>,
    #[arg(long)]
    read: bool,
    #[arg(long)]
    unread: bool,
    #[arg(long)]
    flagged: bool,
    #[arg(long)]
    unflagged: bool,
    /// One JSON object on standard output (see docs/cli.md).
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
pub struct MoveArgs {
    /// The messages, by identifier, as missive search and list print them.
    #[arg(required = true, value_name = "MESSAGE ID")]
    messages: Vec<String>,
    /// The folder: a role (inbox, sent, drafts, trash, junk, archive) or a name, as missive
    /// unread shows it.
    #[arg(long, value_name = "ROLE OR FOLDER NAME")]
    to: String,
    /// One JSON object on standard output (see docs/cli.md).
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
pub struct DeleteArgs {
    /// The messages, by identifier, as missive search and list print them.
    #[arg(required = true, value_name = "MESSAGE ID")]
    messages: Vec<String>,
    /// Delete for good, rather than move to the trash: only for messages in the trash (or in an
    /// account that has none).
    #[arg(long)]
    permanently: bool,
    /// One JSON object on standard output (see docs/cli.md).
    #[arg(long)]
    json: bool,
}

/// Why a command failed.
pub enum ActError {
    /// An identifier names no message.
    NotFound(String),
    Failed(String),
}

impl<E: std::fmt::Display> From<E> for ActError {
    fn from(error: E) -> Self {
        ActError::Failed(error.to_string())
    }
}

pub async fn mark(args: MarkArgs) -> Result<(), ActError> {
    let mark_as = match (args.read, args.unread, args.flagged) {
        (true, _, _) => "read",
        (_, true, _) => "unread",
        (_, _, true) => "flagged",
        _ => "unflagged",
    };
    let count = args.messages.len();
    let command = Command::Mark {
        messages: args.messages,
        mark_as: mark_as.to_owned(),
    };
    let done = format!("marked {mark_as}");
    run(command, args.json, count, &done).await
}

pub async fn move_to(args: MoveArgs) -> Result<(), ActError> {
    let count = args.messages.len();
    let done = format!("moved to {}", args.to);
    let command = Command::Move {
        messages: args.messages,
        to: args.to,
    };
    run(command, args.json, count, &done).await
}

pub async fn delete(args: DeleteArgs) -> Result<(), ActError> {
    let count = args.messages.len();
    let done = if args.permanently {
        "deleted for good"
    } else {
        "moved to the trash"
    };
    let command = Command::Delete {
        messages: args.messages,
        permanently: args.permanently,
    };
    run(command, args.json, count, done).await
}

#[derive(Serialize)]
struct Recorded {
    messages: Vec<String>,
}

async fn run(command: Command, json: bool, asked: usize, done: &str) -> Result<(), ActError> {
    let answer = ask(command).await?;
    if !answer.ok {
        let message = answer.message.unwrap_or_else(|| {
            format!(
                "the daemon refused: {}",
                answer.error.clone().unwrap_or_default()
            )
        });
        return Err(match answer.error.as_deref() {
            Some("no-such-message") => ActError::NotFound(message),
            _ => ActError::Failed(message),
        });
    }
    let messages = answer.messages.unwrap_or_default();
    if json {
        println!("{}", serde_json::to_string(&Recorded { messages })?);
    } else {
        let count = messages.len().min(asked);
        let noun = if count == 1 { "message" } else { "messages" };
        println!("{count} {noun} {done}; the server is told as soon as the account is connected.");
    }
    Ok(())
}

/// Sends one request to the daemon, starting it when none answers, and reads its answer.
pub async fn ask(command: Command) -> Result<Answer, ActError> {
    let stream = daemon::connect().await.map_err(ActError::Failed)?;
    let (read, mut write) = stream.into_split();
    let mut line = serde_json::to_string(&Request {
        protocol: PROTOCOL,
        command,
    })?;
    line.push('\n');
    write.write_all(line.as_bytes()).await?;
    let mut line = String::new();
    BufReader::new(read).read_line(&mut line).await?;
    serde_json::from_str(&line).map_err(|_| {
        ActError::Failed("the daemon gave an answer this missive cannot read".to_owned())
    })
}
