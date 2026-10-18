//! `missive mailto <uri>`: a `mailto:` link, as the system hands it to its default handler, saved
//! as a draft in the account's Drafts folder (see docs/cli.md). Nothing is opened: the window
//! that will show the composition does not exist yet.

use clap::Args;
use missive_core::mailto;
use missive_core::protocol::{Command, SaveDraft};
use serde::Serialize;

use crate::act::{self, ActError};
use crate::compose;

#[derive(Args)]
pub struct MailtoArgs {
    /// The link: mailto:name@example.org?subject=Hello (RFC 6068).
    #[arg(value_name = "URI")]
    uri: String,
    /// The account to write from, by identifier [default: the only account].
    #[arg(long)]
    account: Option<String>,
    /// One JSON object on standard output (see docs/cli.md).
    #[arg(long)]
    json: bool,
}

#[derive(Serialize)]
struct Saved {
    id: String,
}

pub async fn run(args: MailtoArgs) -> Result<(), ActError> {
    let link = mailto::parse(&args.uri)?;
    let account = compose::account(args.account)?;
    let addresses = |list: Vec<missive_core::compose::Mailbox>| -> Vec<String> {
        list.into_iter()
            .map(|m| match m.name {
                Some(name) => format!("{name} <{}>", m.address),
                None => m.address,
            })
            .collect()
    };
    let answer = act::ask(Command::Draft(SaveDraft {
        account,
        to: Some(addresses(link.to)),
        cc: Some(addresses(link.cc)),
        bcc: Some(addresses(link.bcc)),
        subject: link.subject,
        body: Some(link.body.unwrap_or_default()),
        in_reply_to: link.in_reply_to,
        ..SaveDraft::default()
    }))
    .await?;
    let Some(id) = compose::answered(answer)?.message_id else {
        return Err(compose::unreadable());
    };
    if args.json {
        println!("{}", serde_json::to_string(&Saved { id })?);
    } else {
        println!("Draft {id} is saved: missive list --folder drafts shows it.");
    }
    Ok(())
}
