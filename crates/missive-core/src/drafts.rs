//! Drafts: a message not sent yet, kept in the account's Drafts folder, so that it can be picked
//! up again from Missive or another client.
//!
//! A draft is saved first in the local copy ([`crate::store::Store::save_draft`]), offline
//! included, and the copy shows it at once; the daemon then writes it to the server ([`write`]).
//! Every version of a draft keeps its Message-ID, which names it: that is how its versions are
//! found on the server, by this client or after a connection was cut. Each version gets a new
//! `Date`.
//!
//! Missive only ever replaces or removes a version it wrote itself: one that carries
//! [`OWN_DRAFT`]. A draft another client wrote, picked up again, becomes a new draft of
//! Missive's own, under a new Message-ID, and the original stays on the server as it is: Missive
//! reads a draft back without its HTML and most of its headers, and would otherwise replace the
//! only rich copy with a poorer one.
//!
//! On the server, every command goes through the sync's verified path, and counts only with its
//! tagged `OK`:
//!
//! 1. `SELECT` the Drafts folder, and `UID SEARCH HEADER Message-ID "<id>" HEADER
//!    X-Missive-Draft "1"`: the versions it already holds, one from an `APPEND` whose answer was
//!    lost included. A Message-ID that is not ASCII cannot be written as a quoted string, and a
//!    server answers `BAD` to it: it is not searched for, and only the versions the copy holds
//!    are known.
//! 2. `APPEND <drafts> (\Draft \Seen)` of the new version, given a timeout that grows with its
//!    size (a large draft on a slow link would otherwise time out at every pass). Its UID comes
//!    from `APPENDUID` (UIDPLUS), else from the same search, made again. A version tried before
//!    is first looked for among those found (`UID FETCH <uids> (UID BODY.PEEK[])`): one
//!    identical byte for byte landed from an `APPEND` whose answer was lost, and is taken as the
//!    new version instead of a second copy. So cut answers, however many in a row, never add
//!    more than one copy of a version, with or without UIDPLUS.
//!    When the version cannot be written to the copy (its UID is unknown, or the copy numbers
//!    the folder otherwise than the server), the folder is synced at once.
//! 3. Only then, the older versions are removed: `UID STORE <uids> +FLAGS.SILENT (\Deleted)` and
//!    `UID EXPUNGE <uids>`, which removes those UIDs and no other message marked deleted; an
//!    expunge refused takes the flag back. Without UIDPLUS there is no `UID EXPUNGE`: the older
//!    versions are left in place, and the user is told. Never a bare `EXPUNGE`.
//!
//! A draft whose message was sent is removed the same way, once the server took the message
//! (`missive compose --draft … --send`): a message refused or uncertain keeps its draft.
//!
//! A `NO` or a `BAD` keeps the version in the copy, and the older ones on the server: it is tried
//! again at the next pass, never dropped. An account without a Drafts folder keeps its drafts in
//! the copy (`status` says so): Missive never creates a folder on the server.

use std::collections::BTreeSet;
use std::time::Duration;

use async_imap::imap_proto::{AttributeValue, Response};

use crate::actions::{self, Server};
use crate::compose::OWN_DRAFT;
use crate::connect::ImapSession;
use crate::copy::message_id;
use crate::store::{DraftRow, NewMessage, Store};
use crate::sync::{self, Capabilities, Done, SyncError};

/// The flags of a draft on the server.
const FLAGS: &str = "\\Draft \\Seen";

/// What became of a draft.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Result {
    /// The version is in the Drafts folder, under the identifier `id` when its UID is known.
    Saved { id: Option<String>, left: Left },
    /// The draft is no longer in the Drafts folder: its message was sent.
    Removed { left: Left },
    /// It stays in the copy, tried again at the next pass: `reason` is `refused` (the server
    /// answered `NO` or `BAD`, or has no such folder) or `connection`.
    Retried { attempts: u32, reason: &'static str },
}

/// Older versions left on the server, and why.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Left {
    pub count: usize,
    pub why: Option<&'static str>,
}

/// A draft written to the server, for the events and the log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    /// Its row in the copy, which the log names it by.
    pub row: i64,
    /// Its Message-ID.
    pub draft: String,
    pub result: Result,
}

/// What [`write`] did.
#[derive(Debug, Default)]
pub struct Written {
    pub outcomes: Vec<Outcome>,
    /// The Drafts folder's row, when the copy has to catch up with it.
    pub resync: Option<i64>,
    /// A command got no answer in time: the connection has to be dropped.
    pub timed_out: bool,
}

const NO_UIDPLUS: &str = "the server has no UID EXPUNGE (UIDPLUS), so the older versions of the \
                          draft are left in the Drafts folder";
const NOT_REMOVED: &str = "the server refused to remove the older versions of the draft";

/// Writes the account's drafts to its Drafts folder, each command given `timeout`. A draft that
/// failed before is tried again only when `pass`. A connection error stops it, counted as a
/// failure of that draft, and is returned beside what was done before it.
pub async fn write(
    session: &mut ImapSession,
    store: &mut Store,
    key: i64,
    account_id: &str,
    capabilities: Capabilities,
    timeout: Duration,
    pass: bool,
) -> (Written, std::result::Result<(), SyncError>) {
    let mut written = Written::default();
    let mut server = Server::new(session, timeout);
    let result = each(
        &mut server,
        store,
        key,
        account_id,
        capabilities,
        pass,
        &mut written,
    )
    .await;
    written.timed_out = server.timed_out;
    (written, result)
}

async fn each(
    server: &mut Server<'_>,
    store: &mut Store,
    key: i64,
    account_id: &str,
    capabilities: Capabilities,
    pass: bool,
    written: &mut Written,
) -> std::result::Result<(), SyncError> {
    for draft in store.drafts(key)? {
        if draft.attempts > 0 && !pass {
            continue;
        }
        // Without a Drafts folder, the draft stays in the copy.
        let Some((folder, name, in_copy)) = store.drafts_folder(key)? else {
            return Ok(());
        };
        let retried = |store: &mut Store, why, reason| -> rusqlite::Result<Outcome> {
            Ok(Outcome {
                row: draft.id,
                draft: draft.message_id.clone(),
                result: Result::Retried {
                    attempts: store.draft_failed(draft.id, why)?,
                    reason,
                },
            })
        };
        let outcome = match one(
            server,
            store,
            &draft,
            (folder, &name, in_copy),
            capabilities,
        )
        .await
        {
            Ok(Step::Done {
                uidvalidity,
                new,
                left,
            }) => {
                // The version is not in the copy when its UID is unknown, or when the copy
                // numbers the folder otherwise than the server (`draft_written` writes nothing).
                if draft.raw.is_some() && (new.is_none() || in_copy != Some(uidvalidity)) {
                    written.resync = Some(folder);
                }
                Outcome {
                    row: draft.id,
                    draft: draft.message_id.clone(),
                    result: match draft.raw {
                        Some(_) => Result::Saved {
                            id: new.map(|uid| message_id(account_id, folder, uidvalidity, uid)),
                            left,
                        },
                        None => Result::Removed { left },
                    },
                }
            }
            Ok(Step::No(why)) => retried(store, why, "refused")?,
            Err(error @ SyncError::Connection(_)) => {
                written
                    .outcomes
                    .push(retried(store, "connection", "connection")?);
                return Err(error);
            }
            Err(error) => return Err(error),
        };
        written.outcomes.push(outcome);
    }
    Ok(())
}

/// The last of `uids` whose message is `raw`, byte for byte. A refused fetch finds none: the
/// version is then appended again, as before.
async fn same(
    server: &mut Server<'_>,
    uids: &BTreeSet<u32>,
    raw: &[u8],
) -> std::result::Result<Option<u32>, SyncError> {
    let set = sync::uid_set(&uids.iter().copied().collect::<Vec<_>>());
    let mut found = None;
    let done = server
        .run_done(&format!("UID FETCH {set} (UID BODY.PEEK[])"), |response| {
            if let Response::Fetch(_, attributes) = response
                && let Some(uid) = sync::uid(attributes)
                && uids.contains(&uid)
                && attributes.iter().any(|attribute| {
                    matches!(attribute, AttributeValue::BodySection {
                        section: None,
                        data: Some(data),
                        ..
                    } if data.as_ref() == raw)
                })
            {
                found = found.max(Some(uid));
            }
        })
        .await?;
    Ok(found.filter(|_| matches!(done, Done::Ok(_))))
}

/// What one try of a draft came to.
enum Step {
    /// Written (or removed): the UID of the new version when known.
    Done {
        uidvalidity: u32,
        new: Option<u32>,
        left: Left,
    },
    /// The server refused a command; why, for the copy.
    No(&'static str),
}

async fn one(
    server: &mut Server<'_>,
    store: &mut Store,
    draft: &DraftRow,
    (folder, name, in_copy): (i64, &str, Option<u32>),
    capabilities: Capabilities,
) -> std::result::Result<Step, SyncError> {
    let Some(quoted) = sync::quote(name) else {
        return Ok(Step::No("folder-name"));
    };
    let Some(uidvalidity) = server.select(name).await? else {
        return Ok(Step::No("select"));
    };
    // With its angle brackets: `3@` does not match `13@`. A quoted string holds ASCII only, and a
    // server answers `BAD` to more, at every pass: such a Message-ID (another client's) is not
    // looked for, and the versions the copy holds stand for what the server holds.
    let search = Some(&draft.message_id)
        .filter(|id| id.is_ascii())
        .and_then(|id| sync::quote(&format!("<{id}>")))
        .map(|id| format!("HEADER Message-ID {id} HEADER {OWN_DRAFT} \"1\""));
    let mut old = BTreeSet::new();
    if let Some(search) = &search {
        let Some(found) = server.search(search).await? else {
            return Ok(Step::No("search"));
        };
        old.extend(found);
    }
    if in_copy == Some(uidvalidity) {
        old.extend(store.draft_uids(folder, &draft.message_id)?);
    }

    // A version tried before may be there already, from an `APPEND` whose answer was lost: it is
    // taken as the new one rather than appended again, which a flaky link would repeat.
    let mut new = match &draft.raw {
        Some(raw) if draft.attempts > 0 && !old.is_empty() => same(server, &old, raw).await?,
        _ => None,
    };
    if let Some(raw) = draft.raw.as_ref().filter(|_| new.is_none()) {
        let done = server.append(&quoted, FLAGS, raw).await?;
        match done {
            Done::Ok(_) => {}
            Done::No { try_create: true } => return Ok(Step::No("no-such-folder")),
            Done::No { .. } => return Ok(Step::No("append")),
            // Kept all the same: a draft is never dropped.
            Done::Bad => return Ok(Step::No("append-bad")),
        }
        new = match sync::appended(&done) {
            Some((validity, uid)) if validity == uidvalidity => Some(uid),
            _ => match &search {
                Some(search) => server
                    .search(search)
                    .await?
                    .and_then(|found| found.into_iter().filter(|uid| !old.contains(uid)).max()),
                None => None,
            },
        };
    }

    let old: Vec<u32> = old.into_iter().filter(|uid| Some(*uid) != new).collect();
    let (removed, left) = if old.is_empty() {
        (Vec::new(), Left::default())
    } else if !capabilities.uidplus {
        (
            Vec::new(),
            Left {
                count: old.len(),
                why: Some(NO_UIDPLUS),
            },
        )
    } else {
        match actions::remove(server, &old).await? {
            Done::Ok(_) => (old, Left::default()),
            // A removal only is tried again; a new version is in, and the older ones are found
            // again by the next save.
            _ if draft.raw.is_none() => return Ok(Step::No("expunge")),
            _ => (
                Vec::new(),
                Left {
                    count: old.len(),
                    why: Some(NOT_REMOVED),
                },
            ),
        }
    };
    let message = match (&draft.raw, new) {
        (Some(raw), Some(uid)) => Some(NewMessage {
            uid,
            flags: FLAGS.to_owned(),
            internal_date: None,
            size: raw.len() as i64,
            raw: raw.clone(),
        }),
        _ => None,
    };
    store.draft_written(draft, folder, uidvalidity, &removed, message)?;
    Ok(Step::Done {
        uidvalidity,
        new,
        left,
    })
}
