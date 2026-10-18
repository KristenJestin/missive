//! The outbox: a message to send is written there first, in the local copy, then submitted to
//! the account's SMTP server ([`crate::submit`]), and leaves it only once the server took it.
//!
//! Its states, each a row of the outbox (schema version 6):
//!
//! - `queued`: to send. A try that fails before the end of data (a `4xx`, a connection lost, a
//!   login refused, a certificate refused) leaves it `queued`: the server took nothing, so sending
//!   it again is safe. The daemon tries again later, from a backoff.
//! - `sending`: written just before the end of data is, and only when the message was written
//!   whole: what follows may reach the server. A daemon killed then leaves the row so; the next
//!   one makes it `uncertain` ([`crate::store::Store::sending_left`]).
//! - `sent`: the server answered `250` to the end of data, and the copy is still to be appended
//!   to the Sent folder. Without a copy to write (the account says its server keeps one, or it
//!   has no Sent folder) the message leaves the outbox at once.
//! - `failed`: the server refused it for good (`5xx` to `MAIL FROM`, a `RCPT TO`, `DATA` or the
//!   end of data). It stays, reported by `status`, and is never sent again.
//! - `uncertain`: the connection was lost (or the server stayed silent) after the end of data,
//!   before its answer. The server may have taken it: it stays, reported, and is never sent again
//!   by itself, since a message delivered twice is worse than a question.
//!
//! The copy in Sent is appended by the account's IMAP connection, through the verified write
//! path (`APPEND` counts only with its tagged `OK`), flagged `\Seen`, with the message's Bcc
//! recipients in it (see [`crate::compose`]). A try whose answer was lost may have appended it:
//! before trying again, the Sent folder is searched for its `Message-ID`. Missive never creates a
//! Sent folder. After [`MAX_ATTEMPTS`] failures the copy is given up (the message was sent).

use std::sync::Arc;
use std::time::Duration;

use rustls::ClientConfig;

use crate::account::{Account, Password};
use crate::actions::MAX_ATTEMPTS;
use crate::connect::{ConnectError, ImapSession};
use crate::store::Store;
use crate::submit::{Outgoing, Submitted, submit};
use crate::sync::{self, Done, SyncError};

/// What became of a try of the account's next message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Tried {
    /// The server took it.
    Sent {
        id: i64,
        message_id: String,
        /// `append`, `server` or `none`: see [`crate::events::Sent`].
        sent_copy: &'static str,
    },
    /// It stays to send: the server took nothing.
    Again { id: i64, reason: String },
    /// The login failed: it stays to send.
    Login { id: i64, error: ConnectError },
    Failed {
        id: i64,
        message_id: String,
        code: u16,
        step: &'static str,
    },
    Uncertain {
        id: i64,
        message_id: String,
        reason: String,
    },
}

/// Submits the account's first message to send, if there is one, and records what came of it.
pub async fn send_next(
    store: &mut Store,
    key: i64,
    account: &Account,
    password: &Password,
    tls: Arc<ClientConfig>,
) -> rusqlite::Result<Option<Tried>> {
    let Some(message) = store.next_to_send(key)? else {
        return Ok(None);
    };
    let id = message.id;
    let marking = &mut *store;
    let submitted = submit(
        &account.smtp,
        &account.user,
        password,
        tls,
        Outgoing {
            sender: &message.sender,
            recipients: &message.recipients,
            raw: &message.raw,
        },
        move || {
            marking
                .sending(id)
                .map_err(|e| format!("the outbox could not be written ({e})"))
        },
    )
    .await;
    let message_id = message.message_id;
    Ok(Some(match submitted {
        Ok(Submitted::Sent) => {
            let sent_copy = if !message.sent_copy {
                "server"
            } else if store.sent_folder(key)?.is_none() && store.knows_folders(key)? {
                "none"
            } else {
                "append"
            };
            store.sent(id, sent_copy == "append")?;
            Tried::Sent {
                id,
                message_id,
                sent_copy,
            }
        }
        Ok(Submitted::Again { reason }) => {
            store.send_again(id, &reason)?;
            Tried::Again { id, reason }
        }
        Ok(Submitted::Refused { code, step }) => {
            store.send_failed(id, &format!("the server answered {code} to {step}"))?;
            Tried::Failed {
                id,
                message_id,
                code,
                step,
            }
        }
        Ok(Submitted::Uncertain { reason }) => {
            store.send_uncertain(id, &reason)?;
            Tried::Uncertain {
                id,
                message_id,
                reason,
            }
        }
        Err(error) => {
            store.send_again(id, &error.to_string())?;
            Tried::Login { id, error }
        }
    }))
}

/// What became of a copy for Sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Copied {
    /// It is in Sent: appended now, or found there from a try whose answer was lost.
    Appended { id: i64 },
    /// It stays, tried again later.
    Retried { id: i64, attempts: u32 },
    /// It will never be in Sent: `why`, for the log.
    GivenUp { id: i64, why: &'static str },
}

/// What [`copy_sent`] did.
#[derive(Debug, Default)]
pub struct Copies {
    pub copied: Vec<Copied>,
    /// The Sent folder's row, when a copy went in: it is to be synced.
    pub resync: Option<i64>,
    /// A command got no answer in time: the connection has to be dropped.
    pub timed_out: bool,
}

/// Appends to Sent the copy of every message the server took, each command given `timeout`. A
/// connection error stops it, counted as a failure of that copy, and is returned beside what was
/// done before it.
pub async fn copy_sent(
    session: &mut ImapSession,
    store: &mut Store,
    key: i64,
    timeout: Duration,
) -> (Copies, Result<(), SyncError>) {
    let mut copies = Copies::default();
    let result = copy_each(session, store, key, timeout, &mut copies).await;
    (copies, result)
}

async fn copy_each(
    session: &mut ImapSession,
    store: &mut Store,
    key: i64,
    timeout: Duration,
    copies: &mut Copies,
) -> Result<(), SyncError> {
    for copy in store.sent_copies(key)? {
        let id = copy.id;
        let Some((folder, name)) = store.sent_folder(key)? else {
            if store.knows_folders(key)? {
                store.copy_done(id)?;
                copies.copied.push(Copied::GivenUp {
                    id,
                    why: "the account has no Sent folder",
                });
            }
            continue;
        };
        let Some(quoted) = sync::quote(&name) else {
            store.copy_done(id)?;
            copies.copied.push(Copied::GivenUp {
                id,
                why: "the Sent folder's name cannot be written in a command",
            });
            continue;
        };
        let tried = tokio::time::timeout(timeout, async {
            if copy.attempts > 0 && already_there(session, &name, &copy.message_id).await? {
                return Ok(Done::Ok(None));
            }
            sync::append(session, &quoted, "\\Seen", &copy.raw).await
        })
        .await;
        let done = match tried {
            Ok(Ok(done)) => done,
            Ok(Err(error)) => {
                failed(store, id, copies)?;
                return Err(error);
            }
            Err(_) => {
                failed(store, id, copies)?;
                copies.timed_out = true;
                return Err(SyncError::Connection(ConnectError::Unreachable(
                    "the IMAP server did not answer APPEND in time".to_owned(),
                )));
            }
        };
        match done {
            Done::Ok(_) => {
                store.copy_done(id)?;
                copies.copied.push(Copied::Appended { id });
                copies.resync = Some(folder);
            }
            Done::No { try_create: true } => {
                store.copy_done(id)?;
                copies.copied.push(Copied::GivenUp {
                    id,
                    why: "the server has no such Sent folder",
                });
            }
            Done::No { .. } | Done::Bad => failed(store, id, copies)?,
        }
    }
    Ok(())
}

/// Counts a failed copy, and gives it up after [`MAX_ATTEMPTS`].
fn failed(store: &mut Store, id: i64, copies: &mut Copies) -> rusqlite::Result<()> {
    let attempts = store.copy_failed(id)?;
    if attempts >= MAX_ATTEMPTS {
        store.copy_done(id)?;
        copies.copied.push(Copied::GivenUp {
            id,
            why: "appending it failed too many times",
        });
    } else {
        copies.copied.push(Copied::Retried { id, attempts });
    }
    Ok(())
}

/// Whether Sent holds a message with this Message-ID: a try whose answer was lost may have
/// appended it.
async fn already_there(
    session: &mut ImapSession,
    name: &str,
    message_id: &str,
) -> Result<bool, SyncError> {
    if sync::examine_uidvalidity(session, name).await?.is_none() {
        return Ok(false);
    }
    let Some(quoted) = sync::quote(&format!("<{message_id}>")) else {
        return Ok(false);
    };
    Ok(
        sync::search(session, &format!("HEADER Message-ID {quoted}"))
            .await?
            .is_some_and(|found| !found.is_empty()),
    )
}
