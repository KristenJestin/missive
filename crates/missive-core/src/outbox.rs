//! The outbox: a message to send is written there first, in the local copy, then submitted to
//! the account's SMTP server ([`crate::submit`]), and leaves it only once the server took it.
//!
//! Its states, each a row of the outbox (schema version 6):
//!
//! - `queued`: to send. A try that fails before the end of data (a `4xx`, a connection lost, a
//!   login refused, a certificate refused) leaves it `queued`: the server took nothing, so sending
//!   it again is safe. It is tried again after a wait of its own, which holds back no other
//!   message ([`crate::store::Store::send_again`]).
//! - `claimed`: a worker took it, before its first command: no other worker takes it. Nothing
//!   was sent for it yet, so a daemon killed then leaves it to the next one, which puts it back
//!   in the queue ([`crate::store::Store::release_claimed`]). A worker that cannot write the
//!   outbox for five tries leaves it so until the next start: `status` calls a message claimed
//!   for more than [`CLAIM_STUCK_MS`] stuck.
//! - `sending`: written just before the end of data is, and only when the message was written
//!   whole: what follows may reach the server. A daemon killed then leaves the row so; the next
//!   one makes it `uncertain` ([`crate::store::Store::sending_left`]).
//! - `sent`: the server answered `250` to the end of data, and the copy is still to be appended
//!   to the Sent folder. Without a copy to write (the account says its server keeps one, or it
//!   has no Sent folder) the message leaves the outbox at once.
//! - `failed`: the server refused it for good (`5xx` to `MAIL FROM`, a `RCPT TO`, `DATA` or the
//!   end of data). It stays, reported by `status`, and is never sent again by itself.
//! - `uncertain`: the connection was lost (or the server stayed silent) after the end of data,
//!   before its answer. The server may have taken it: it stays, reported, and is never sent again
//!   by itself, since a message delivered twice is worse than a question.
//!
//! The user can put a `failed` or `uncertain` message back in the queue, or remove it
//! ([`crate::store::Store::resend`], [`crate::store::Store::discard`]): `missive outbox`. A
//! resent message is claimed as any other, so it goes once.
//!
//! After the server's `250`, recording that it was sent is tried again for a while when the copy
//! is busy: a row left `sending` would be reported `uncertain` by the next daemon, for a message
//! that was sent. A daemon that stops meanwhile finishes it, for a short while ([`record`] is
//! apart from [`submit_next`] for that).
//!
//! The copy in Sent is appended by the account's IMAP connection, through the verified write
//! path (`APPEND` counts only with its tagged `OK`), flagged `\Seen`, with the message's Bcc
//! recipients in it (see [`crate::compose`]). A try whose answer was lost, or that a killed daemon
//! left, may have appended it: each try is counted before its `APPEND`, and every try after the
//! first searches the Sent folder for its `Message-ID` first (a search that fails is not a try).
//! Missive never creates a Sent folder.
//! After [`MAX_ATTEMPTS`] tries the copy is given up (the message was sent).

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustls::ClientConfig;

use crate::account::{Account, Password};
use crate::actions::MAX_ATTEMPTS;
use crate::connect::{ConnectError, ImapSession};
use crate::store::Store;
use crate::submit::{Outgoing, Submitted, submit};
use crate::sync::{self, Done, SyncError};

/// How many times recording what came of a submission is tried, [`RECORD_PAUSE`] apart, when the
/// copy is busy: each try already waits for the copy as long as its busy timeout says.
const RECORD_TRIES: u32 = 5;
const RECORD_PAUSE: Duration = Duration::from_secs(1);

/// How long a message stays `claimed` before `status` calls it stuck, in milliseconds: more than
/// a submission can take (each command waits at most 5 minutes, the data 10).
pub const CLAIM_STUCK_MS: i64 = 60 * 60 * 1000;

/// Now, in milliseconds since the epoch: the outbox's clock.
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// What became of a try of the account's next message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Tried {
    /// The server took it.
    Sent {
        id: i64,
        message_id: String,
        /// `append`, `server` or `none`: see [`crate::events::Sent`].
        sent_copy: &'static str,
        /// Whether the message it answers was marked `\Answered`: the account's IMAP worker has
        /// that action to apply.
        answered: bool,
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

/// A message submitted, and what the server said: not recorded yet.
#[derive(Debug)]
pub struct Submission {
    id: i64,
    message_id: String,
    /// Whether a copy is to go in Sent (the server does not keep one itself).
    sent_copy: bool,
    submitted: Result<Submitted, ConnectError>,
}

/// Submits the account's first message whose time has come, if there is one, and records what
/// came of it.
pub async fn send_next(
    store: &mut Store,
    key: i64,
    account: &Account,
    password: &Password,
    tls: Arc<ClientConfig>,
) -> rusqlite::Result<Option<Tried>> {
    match submit_next(store, key, account, password, tls).await? {
        Some(submission) => record(store, key, submission).await.map(Some),
        None => Ok(None),
    }
}

/// The first half of [`send_next`]: claims the account's first message whose time has come, if
/// there is one, and submits it. A caller that stops its worker may drop this future; it must not
/// drop [`record`], which only the server's answer, known here, makes possible.
pub async fn submit_next(
    store: &mut Store,
    key: i64,
    account: &Account,
    password: &Password,
    tls: Arc<ClientConfig>,
) -> rusqlite::Result<Option<Submission>> {
    let Some(message) = store.claim_next(key, now())? else {
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
    Ok(Some(Submission {
        id,
        message_id: message.message_id,
        sent_copy: message.sent_copy,
        submitted,
    }))
}

/// The second half of [`send_next`]: records what came of a submission, a few local writes.
pub async fn record(
    store: &mut Store,
    key: i64,
    submission: Submission,
) -> rusqlite::Result<Tried> {
    let Submission {
        id,
        message_id,
        sent_copy: copy,
        submitted,
    } = submission;
    Ok(match submitted {
        Ok(Submitted::Sent) => {
            // A folder that cannot be read is not an answer: the copy is left to the IMAP worker,
            // which looks again. What was sent is recorded whatever these reads say.
            let sent_copy = if !copy {
                "server"
            } else if matches!(
                (store.sent_folder(key), store.knows_folders(key)),
                (Ok(None), Ok(true))
            ) {
                "none"
            } else {
                "append"
            };
            let answered = recorded(store, |store| store.sent(id, sent_copy == "append")).await?;
            Tried::Sent {
                id,
                message_id,
                sent_copy,
                answered,
            }
        }
        Ok(Submitted::Again { reason }) => {
            recorded(store, |store| store.send_again(id, &reason, now())).await?;
            Tried::Again { id, reason }
        }
        Ok(Submitted::Refused { code, step }) => {
            let why = format!("the server answered {code} to {step}");
            recorded(store, |store| store.send_failed(id, &why)).await?;
            Tried::Failed {
                id,
                message_id,
                code,
                step,
            }
        }
        Ok(Submitted::Uncertain { reason }) => {
            recorded(store, |store| store.send_uncertain(id, &reason)).await?;
            Tried::Uncertain {
                id,
                message_id,
                reason,
            }
        }
        Err(error) => {
            let why = error.to_string();
            recorded(store, |store| store.send_again(id, &why, now())).await?;
            Tried::Login { id, error }
        }
    })
}

/// Records what came of a submission, trying again while the copy is busy: the server's answer
/// is known only here.
///
/// The copy is lent to `record` mutably: a shared borrow held across the pause would make the
/// future not `Send`, the copy not being `Sync`.
async fn recorded<T>(
    store: &mut Store,
    mut record: impl FnMut(&mut Store) -> rusqlite::Result<T>,
) -> rusqlite::Result<T> {
    let mut tries = 1;
    loop {
        match record(store) {
            Err(_) if tries < RECORD_TRIES => {
                tries += 1;
                tokio::time::sleep(RECORD_PAUSE).await;
            }
            done => return done,
        }
    }
}

/// What became of a copy for Sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Copied {
    /// It is in Sent: appended now, or found there from a try whose answer was lost.
    Appended { id: i64 },
    /// It stays, tried again later.
    Retried { id: i64, attempts: u32 },
    /// It will never be in Sent: `why`, for people.
    GivenUp {
        id: i64,
        message_id: String,
        why: &'static str,
    },
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
                    message_id: copy.message_id,
                    why: "the account has no Sent folder",
                });
            }
            continue;
        };
        let Some(quoted) = sync::quote(&name) else {
            store.copy_done(id)?;
            copies.copied.push(Copied::GivenUp {
                id,
                message_id: copy.message_id,
                why: "the Sent folder's name cannot be written in a command",
            });
            continue;
        };
        // Only an append started counts: a search that fails (the connection dropped) says
        // nothing of the copy, and the next pass searches again.
        if copy.attempts > 0 {
            let searched =
                tokio::time::timeout(timeout, already_there(session, &name, &copy.message_id))
                    .await;
            match searched {
                Ok(Ok(true)) => {
                    store.copy_done(id)?;
                    copies.copied.push(Copied::Appended { id });
                    copies.resync = Some(folder);
                    continue;
                }
                Ok(Ok(false)) => {}
                Ok(Err(error)) => return Err(error),
                Err(_) => {
                    copies.timed_out = true;
                    return Err(SyncError::Connection(ConnectError::Unreachable(
                        "the IMAP server did not answer SEARCH in time".to_owned(),
                    )));
                }
            }
        }
        let attempts = store.copy_trying(id)?;
        let tried =
            tokio::time::timeout(timeout, sync::append(session, &quoted, "\\Seen", &copy.raw))
                .await;
        let done = match tried {
            Ok(Ok(done)) => done,
            Ok(Err(error)) => {
                failed(store, id, copy.message_id, attempts, copies)?;
                return Err(error);
            }
            Err(_) => {
                failed(store, id, copy.message_id, attempts, copies)?;
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
                    message_id: copy.message_id,
                    why: "the server has no such Sent folder",
                });
            }
            Done::No { .. } | Done::Bad => {
                failed(store, id, copy.message_id, attempts, copies)?;
            }
        }
    }
    Ok(())
}

/// A copy that failed at its `attempts`-th try: given up after [`MAX_ATTEMPTS`].
fn failed(
    store: &mut Store,
    id: i64,
    message_id: String,
    attempts: u32,
    copies: &mut Copies,
) -> rusqlite::Result<()> {
    if attempts >= MAX_ATTEMPTS {
        store.copy_done(id)?;
        copies.copied.push(Copied::GivenUp {
            id,
            message_id,
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
