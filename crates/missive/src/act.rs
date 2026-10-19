//! `missive mark`, `missive move` and `missive delete`: they ask the daemon, which records the
//! action, shows it in the local copy at once, and applies it on the server when it can (see
//! docs/cli.md). They answer once the action is recorded, connected or not; with `--wait`, once
//! the server applied or refused it, or when it has to stay in the queue (the account is offline,
//! or the wait is over).

use std::collections::HashMap;
use std::time::Duration;

use clap::{ArgGroup, Args};
use missive_core::client;
use missive_core::copy::MessageId;
use missive_core::events::ActionEvent;
use missive_core::protocol::{Answer, Command, PROTOCOL, Request, socket_path};
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::time::Instant;

/// The exit status when an identifier names no message, as for `missive show`.
pub const NOT_FOUND: u8 = 3;

/// The exit status of `--wait` when an action is still in the queue.
pub const QUEUED: u8 = 4;

/// The exit status of `--wait` when the server refused an action.
pub const REFUSED: u8 = 5;

/// The states in which an account's queue waits for a connection: `--wait` stops waiting.
const OFFLINE: [&str; 4] = [
    "no-login",
    "auth-refused",
    "certificate-refused",
    "unreachable",
];

/// `--wait` and how long it waits.
#[derive(Args)]
pub struct WaitArgs {
    /// Answer once the server applied or refused the actions, or when they have to stay in the
    /// queue: the account is offline, or the wait is over.
    #[arg(long)]
    wait: bool,
    /// How long --wait waits, in seconds.
    #[arg(long, value_name = "SECONDS", default_value_t = 60, requires = "wait")]
    timeout: u64,
}

impl WaitArgs {
    fn wait(&self) -> Option<Duration> {
        self.wait.then(|| Duration::from_secs(self.timeout))
    }
}

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
    #[command(flatten)]
    wait: WaitArgs,
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
    #[command(flatten)]
    wait: WaitArgs,
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
    #[command(flatten)]
    wait: WaitArgs,
}

/// Why a command failed.
pub enum ActError {
    /// An identifier names no message.
    NotFound(String),
    Failed(String),
    /// Told already: exit with this status.
    Exit(u8),
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
    run(command, args.json, count, &done, args.wait.wait()).await
}

pub async fn move_to(args: MoveArgs) -> Result<(), ActError> {
    let count = args.messages.len();
    let done = format!("moved to {}", args.to);
    let command = Command::Move {
        messages: args.messages,
        to: args.to,
    };
    run(command, args.json, count, &done, args.wait.wait()).await
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
    run(command, args.json, count, done, args.wait.wait()).await
}

#[derive(Serialize)]
struct Recorded {
    messages: Vec<String>,
    /// With `--wait`: `applied`, `refused` (one of the actions at least) or `queued` (one at
    /// least is still in the queue, and none was refused).
    #[serde(skip_serializing_if = "Option::is_none")]
    outcome: Option<&'static str>,
    /// With `--wait`: what became of each action.
    #[serde(skip_serializing_if = "Option::is_none")]
    actions: Option<Vec<Waited>>,
}

/// What became of an action, as `--wait` tells it.
#[derive(Clone, Debug, Serialize)]
struct Waited {
    id: i64,
    /// `applied`, `refused` or `queued`.
    outcome: &'static str,
    /// Why it was refused (as the `action` event says), or why it is still queued: the state of
    /// an offline account (`unreachable`…), `timeout` or `gone` (the daemon stopped).
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    copied: Option<bool>,
}

async fn run(
    command: Command,
    json: bool,
    asked: usize,
    done: &str,
    wait: Option<Duration>,
) -> Result<(), ActError> {
    let (answer, waited) = match wait {
        Some(timeout) => {
            let (answer, waited) = ask_and_wait(command, timeout).await?;
            (answer, Some(waited))
        }
        None => (ask(command).await?, None),
    };
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
    let outcome = waited.as_ref().map(|waited| {
        if waited.iter().any(|w| w.outcome == "refused") {
            "refused"
        } else if waited.iter().any(|w| w.outcome == "queued") {
            "queued"
        } else {
            "applied"
        }
    });
    let count = messages.len().min(asked);
    if json {
        let recorded = Recorded {
            messages,
            outcome,
            actions: waited.clone(),
        };
        println!("{}", serde_json::to_string(&recorded)?);
    } else {
        let noun = if count == 1 { "message" } else { "messages" };
        match &waited {
            None => println!(
                "{count} {noun} {done}; the server is told as soon as the account is connected."
            ),
            Some(waited) => {
                let mut said = Vec::new();
                for w in waited {
                    match w.outcome {
                        "refused" => said.push(format!(
                            "refused by the server: {}",
                            w.message.as_deref().unwrap_or("no reason given")
                        )),
                        "queued" => said.push(match w.reason.as_deref() {
                            Some("timeout") => {
                                "still queued: the server has not applied it yet".to_owned()
                            }
                            Some("gone") => "still queued: the daemon stopped".to_owned(),
                            Some(state) => format!("still queued: the account is {state}"),
                            None => "still queued".to_owned(),
                        }),
                        _ => {}
                    }
                }
                said.dedup();
                if said.is_empty() {
                    println!("{count} {noun} {done}; applied on the server.");
                } else {
                    println!("{count} {noun} {done}; {}.", said.join("; "));
                }
            }
        }
    }
    match outcome {
        Some("refused") => Err(ActError::Exit(REFUSED)),
        Some("queued") => Err(ActError::Exit(QUEUED)),
        _ => Ok(()),
    }
}

/// Subscribes to the daemon's events, then sends the request on the same connection (its answer
/// comes among the events), and follows the actions it recorded until each is applied or
/// refused, or until it has to stay in the queue: an account of the request is offline, the
/// daemon stopped, or `timeout` is over. The numbers of the actions name them for ever, so an
/// event that came before the answer is kept and matched once the answer gives them.
async fn ask_and_wait(
    command: Command,
    timeout: Duration,
) -> Result<(Answer, Vec<Waited>), ActError> {
    let deadline = Instant::now() + timeout;
    let accounts: Vec<String> = match &command {
        Command::Mark { messages, .. }
        | Command::Move { messages, .. }
        | Command::Delete { messages, .. } => messages
            .iter()
            .filter_map(|id| MessageId::parse(id))
            .map(|id| id.account)
            .collect(),
        _ => Vec::new(),
    };
    let socket = socket_path()?;
    let program = std::env::current_exe()?;
    let stream = client::connect(&socket, &program)
        .await
        .map_err(ActError::Failed)?;
    let (read, mut write) = stream.into_split();
    let mut lines = String::new();
    for command in [Command::Subscribe, command] {
        lines.push_str(&serde_json::to_string(&Request {
            protocol: PROTOCOL,
            command,
        })?);
        lines.push('\n');
    }
    write.write_all(lines.as_bytes()).await?;
    let mut read = BufReader::new(read).lines();
    let mut answer: Option<Answer> = None;
    let mut ended: HashMap<i64, Waited> = HashMap::new();
    let mut states: HashMap<String, String> = HashMap::new();
    let stopped = loop {
        if let Some(answer) = &answer {
            let ids = answer.actions.as_deref().unwrap_or_default();
            if !answer.ok || ids.iter().all(|id| ended.contains_key(id)) {
                break None;
            }
            if let Some(state) = accounts
                .iter()
                .find_map(|a| states.get(a).filter(|s| OFFLINE.contains(&s.as_str())))
            {
                break Some(state.clone());
            }
        }
        let line = match tokio::time::timeout_at(deadline, read.next_line()).await {
            Err(_) => break Some("timeout".to_owned()),
            Ok(Ok(Some(line))) => line,
            Ok(_) => break Some("gone".to_owned()),
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        match (value["answer"].as_str(), value["event"].as_str()) {
            (Some("subscribe"), _) => {
                if value["ok"] != true {
                    return Err(ActError::Failed(
                        "the daemon refused to tell what became of the actions".to_owned(),
                    ));
                }
            }
            (Some(_), _) => {
                answer = Some(serde_json::from_value(value).map_err(|_| {
                    "the daemon gave an answer this missive cannot read".to_owned()
                })?);
            }
            (None, Some("state")) => {
                if let (Some(account), Some(state)) =
                    (value["account"].as_str(), value["state"].as_str())
                {
                    states.insert(account.to_owned(), state.to_owned());
                }
            }
            (None, Some("action")) => {
                let Ok(event) = serde_json::from_value::<ActionEvent>(value) else {
                    continue;
                };
                let outcome = match event.outcome.as_str() {
                    "applied" => "applied",
                    "refused" => "refused",
                    _ => continue,
                };
                ended.insert(
                    event.id,
                    Waited {
                        id: event.id,
                        outcome,
                        reason: event.reason,
                        message: event.message,
                        copied: event.copied,
                    },
                );
            }
            _ => {}
        }
    };
    let Some(answer) = answer else {
        return Err(ActError::Failed(match stopped.as_deref() {
            Some("timeout") => "the daemon did not answer in time".to_owned(),
            _ => "the daemon closed the connection before it answered".to_owned(),
        }));
    };
    let waited = answer
        .actions
        .iter()
        .flatten()
        .map(|id| {
            ended.remove(id).unwrap_or_else(|| Waited {
                id: *id,
                outcome: "queued",
                reason: stopped.clone(),
                message: None,
                copied: None,
            })
        })
        .collect();
    Ok((answer, waited))
}

/// Sends one request to the daemon, starting it when none answers, and reads its answer.
pub async fn ask(command: Command) -> Result<Answer, ActError> {
    let socket = socket_path()?;
    let program = std::env::current_exe()?;
    client::ask(&socket, &program, command)
        .await
        .map_err(ActError::Failed)
}
