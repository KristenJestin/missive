//! `missive outbox`: lists the messages of the outbox, and puts a `failed` or `uncertain` one
//! back in the queue or removes it, through the daemon, the only writer of the local copy (see
//! docs/cli.md).

use clap::{Args, Subcommand};
use missive_core::protocol::Command;

use crate::act::{self, ActError};
use crate::read;

#[derive(Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct OutboxArgs {
    #[command(subcommand)]
    action: Option<OutboxCommand>,
    /// One JSON array on standard output (see docs/cli.md).
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand)]
enum OutboxCommand {
    /// Send a failed or uncertain message again. Starts the daemon when none runs.
    Resend {
        /// Its number in the outbox, as missive outbox lists it.
        outbox: i64,
        /// Resend an uncertain message, which the server may have taken already: its recipients
        /// may then get it twice.
        #[arg(long)]
        yes: bool,
    },
    /// Remove a failed or uncertain message from the outbox, for good. Starts the daemon when
    /// none runs.
    Discard {
        /// Its number in the outbox, as missive outbox lists it.
        outbox: i64,
    },
}

pub async fn run(args: OutboxArgs) -> Result<(), ActError> {
    let (command, done) = match args.action {
        None => return read::outbox(args.json).map_err(ActError::Failed),
        Some(OutboxCommand::Resend { outbox, yes }) => {
            (Command::Resend { outbox, yes }, "queued to send again")
        }
        Some(OutboxCommand::Discard { outbox }) => (Command::Discard { outbox }, "discarded"),
    };
    let outbox = match &command {
        Command::Resend { outbox, .. } | Command::Discard { outbox } => *outbox,
        _ => unreachable!("only the outbox's requests are made here"),
    };
    let answer = act::ask(command).await?;
    if answer.ok {
        println!("Message {outbox} {done}.");
        return Ok(());
    }
    let message = answer.message.unwrap_or_default();
    Err(match answer.error.as_deref() {
        Some("no-such-message") => ActError::NotFound(message),
        Some("uncertain") => ActError::Failed(format!(
            "message {outbox} is uncertain: the server may have taken it already, and its \
             recipients would then get it twice. Resend it with --yes only if you know it did not \
             arrive"
        )),
        _ => ActError::Failed(message),
    })
}
