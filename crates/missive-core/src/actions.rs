//! What the user asks to change on the server: mark, flag, move and delete messages.
//!
//! An action is recorded first, in the queue of the local copy, in the same transaction as the
//! change it makes to the copy at once ([`crate::store::Store::queue`]); the daemon then applies
//! it on the server ([`apply`]) and reconciles. Until then the sync keeps the change: the flags
//! the server lists are shown with the waiting actions on top, a message waiting for a move stays
//! in the row the server still has it at, shown in its new folder, and a message waiting for a
//! permanent delete is not fetched again.
//!
//! On the server, every command is read through the same path as the sync's, and counts as done
//! only with its tagged `OK`:
//!
//! - read, unread, flagged, unflagged: `UID STORE <uids> +FLAGS (\Seen)` (or `-FLAGS`, or
//!   `\Flagged`), not silent: the flags it answers are written to the copy.
//! - move, and delete (a move to the trash): `UID MOVE` when the server has MOVE. Else, with
//!   UIDPLUS, `UID COPY`, then `UID STORE <uids> +FLAGS.SILENT (\Deleted)` and
//!   `UID EXPUNGE <uids>`, which removes those UIDs and no other message marked deleted. A server
//!   with neither refuses the move: a bare `EXPUNGE` would remove what another client marked.
//!   The new UIDs come from `COPYUID`: the message's row moves to them. Without it, the row
//!   leaves the copy and the target is synced at once, which brings the message back under its
//!   new UID.
//! - delete-permanently: `UID STORE <uids> +FLAGS.SILENT (\Deleted)`, then `UID EXPUNGE <uids>`;
//!   refused without UIDPLUS, for the same reason.
//!
//! A `NO` leaves the action in the queue, tried again at the next pass, and the actions after it
//! wait, so that they are applied in order; after [`MAX_ATTEMPTS`] it is refused. A cut
//! connection leaves it in the queue as it is. A refusal that will not change (the folder was
//! renumbered, the target does not exist, the server cannot move safely, a `BAD`) drops it and
//! takes its change back from the copy. A UID the server no longer has is not an error: the
//! command changes nothing for it, and the next sync removes it from the copy.
//!
//! A `UID COPY` whose answer was never read (the connection was cut) may or may not have
//! copied: before copying again, the target's UIDNEXT tells whether anything arrived there since,
//! and if so, each message is looked for there by its `Message-ID`. A message without one is
//! copied again: a copy twice is better than a message lost.

use std::collections::BTreeSet;

use async_imap::imap_proto::{MailboxDatum, Response, StatusAttribute};

use crate::connect::ImapSession;
use crate::copy::message_id;
use crate::store::{Action, Store};
use crate::sync::{self, Capabilities, CopyUid, Done, SyncError};

/// How many times an action is tried when the server answers `NO` before it is refused.
pub const MAX_ATTEMPTS: u32 = 5;

/// What an action does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Read,
    Unread,
    Flagged,
    Unflagged,
    Move,
    /// A move to the account's trash.
    Delete,
    DeletePermanently,
}

impl Kind {
    pub const ALL: [Kind; 7] = [
        Kind::Read,
        Kind::Unread,
        Kind::Flagged,
        Kind::Unflagged,
        Kind::Move,
        Kind::Delete,
        Kind::DeletePermanently,
    ];

    /// Its name in the queue and in the events.
    pub fn name(self) -> &'static str {
        match self {
            Kind::Read => "read",
            Kind::Unread => "unread",
            Kind::Flagged => "flagged",
            Kind::Unflagged => "unflagged",
            Kind::Move => "move",
            Kind::Delete => "delete",
            Kind::DeletePermanently => "delete-permanently",
        }
    }

    pub fn parse(name: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|kind| kind.name() == name)
    }

    /// The flag it adds (`true`) or removes, for those that change a flag.
    pub fn flag(self) -> Option<(&'static str, bool)> {
        match self {
            Kind::Read => Some(("\\Seen", true)),
            Kind::Unread => Some(("\\Seen", false)),
            Kind::Flagged => Some(("\\Flagged", true)),
            Kind::Unflagged => Some(("\\Flagged", false)),
            _ => None,
        }
    }

    /// Whether it takes the messages to another folder.
    pub fn moves(self) -> bool {
        matches!(self, Kind::Move | Kind::Delete)
    }
}

/// What a request asks for, before it is checked against the copy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Wanted {
    /// Read, Unread, Flagged or Unflagged.
    Mark(Kind),
    /// To a folder named by its role (`inbox`, `archive`…) or by its name on the server.
    Move(String),
    Delete {
        permanently: bool,
    },
}

/// Why a request was refused before anything was recorded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refused {
    /// `no-such-message`, `no-such-folder`, `no-trash`, `in-trash`, `not-in-trash` or `busy`.
    pub error: &'static str,
    /// For people.
    pub message: String,
}

impl Refused {
    pub fn new(error: &'static str, message: impl Into<String>) -> Refused {
        Refused {
            error,
            message: message.into(),
        }
    }
}

/// What a request recorded.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Queued {
    /// The identifiers of the messages the actions are on, in the order of the request.
    pub messages: Vec<String>,
    /// The accounts the actions are for, by identifier: their workers have work.
    pub accounts: Vec<String>,
}

/// A message's flags, as the copy keeps them, with `flag` added or removed.
pub fn with_flag(flags: &str, flag: &str, add: bool) -> String {
    let mut list: Vec<&str> = flags
        .split(' ')
        .filter(|f| !f.is_empty() && !f.eq_ignore_ascii_case(flag))
        .collect();
    if add {
        list.push(flag);
    }
    list.sort_unstable();
    list.dedup();
    list.join(" ")
}

/// What became of an action at the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Result {
    Applied,
    /// It stays in the queue: `reason` is `refused` (the server answered `NO`) or `connection`
    /// (the connection was lost before the server answered).
    Retried {
        attempts: u32,
        reason: &'static str,
    },
    /// It was dropped and its change taken back from the copy. `reason` is `renumbered`,
    /// `no-such-folder`, `unsupported` or `refused`.
    Refused {
        reason: &'static str,
        message: String,
    },
}

/// An action tried on the server, for the events and the log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    /// Its row in the queue.
    pub action: i64,
    pub kind: Kind,
    /// The identifiers of its messages, as they were when it was asked.
    pub messages: Vec<String>,
    pub result: Result,
}

/// What a round of [`apply`] did.
#[derive(Debug, Default)]
pub struct Applied {
    pub outcomes: Vec<Outcome>,
    /// The folders to sync now, by row: a target whose new messages are not in the copy yet, a
    /// folder whose change was taken back.
    pub resync: BTreeSet<i64>,
}

/// Applies the account's queue on the server, in order, until it is empty or an action has to
/// wait. A connection error stops it, and is returned beside what was done before it.
pub async fn apply(
    session: &mut ImapSession,
    store: &mut Store,
    account: i64,
    account_id: &str,
    capabilities: Capabilities,
) -> (Applied, std::result::Result<(), SyncError>) {
    let mut applied = Applied::default();
    let result = round(
        session,
        store,
        account,
        account_id,
        capabilities,
        &mut applied,
    )
    .await;
    (applied, result)
}

async fn round(
    session: &mut ImapSession,
    store: &mut Store,
    account: i64,
    account_id: &str,
    capabilities: Capabilities,
    applied: &mut Applied,
) -> std::result::Result<(), SyncError> {
    loop {
        let Some(action) = store.next_action(account)? else {
            return Ok(());
        };
        let messages: Vec<String> = action
            .uids
            .iter()
            .map(|uid| message_id(account_id, action.folder.id, action.uidvalidity, *uid))
            .collect();
        let outcome = |result| Outcome {
            action: action.id,
            kind: action.kind,
            messages: messages.clone(),
            result,
        };
        let result = match one(session, store, &action, capabilities).await {
            Ok(result) => result,
            Err(error) => {
                if matches!(error, SyncError::Connection(_)) {
                    applied.outcomes.push(outcome(Result::Retried {
                        attempts: action.attempts,
                        reason: "connection",
                    }));
                }
                return Err(error);
            }
        };
        let result = match result {
            Step::Applied { resync } => {
                applied.resync.extend(resync);
                Result::Applied
            }
            Step::No(why) => {
                let attempts = store.retried(action.id, why)?;
                if attempts < MAX_ATTEMPTS {
                    applied.outcomes.push(outcome(Result::Retried {
                        attempts,
                        reason: "refused",
                    }));
                    // The actions after it wait, so that they are applied in order.
                    return Ok(());
                }
                refuse(store, &action, applied)?;
                Result::Refused {
                    reason: "refused",
                    message: format!("the server refused it {attempts} times ({why})"),
                }
            }
            Step::Refused(reason, message) => {
                refuse(store, &action, applied)?;
                Result::Refused {
                    reason,
                    message: message.to_owned(),
                }
            }
        };
        applied.outcomes.push(outcome(result));
    }
}

/// Drops an action and takes its change back from the copy; the folder is synced again.
fn refuse(
    store: &mut Store,
    action: &Action,
    applied: &mut Applied,
) -> std::result::Result<(), SyncError> {
    store.refused(action)?;
    applied.resync.insert(action.folder.id);
    Ok(())
}

/// What one try of an action came to.
enum Step {
    Applied {
        resync: Vec<i64>,
    },
    /// The server answered `NO`; why, for the queue.
    No(&'static str),
    Refused(&'static str, &'static str),
}

const RENUMBERED: Step = Step::Refused(
    "renumbered",
    "the server renumbered the folder (its UIDVALIDITY changed), so its UIDs name other messages",
);
const NO_SUCH_FOLDER: Step =
    Step::Refused("no-such-folder", "the server has no such target folder");
const BAD: Step = Step::Refused("refused", "the server refused the command as malformed");

async fn one(
    session: &mut ImapSession,
    store: &mut Store,
    action: &Action,
    capabilities: Capabilities,
) -> std::result::Result<Step, SyncError> {
    if action.folder.uidvalidity != Some(action.uidvalidity) {
        return Ok(RENUMBERED);
    }
    // Refused before anything is sent.
    if action.kind == Kind::DeletePermanently && !capabilities.uidplus {
        return Ok(Step::Refused(
            "unsupported",
            "the server has no UID EXPUNGE (UIDPLUS): deleting for good would also remove the \
             messages other clients marked deleted",
        ));
    }
    if action.kind.moves() && !capabilities.moves && !capabilities.uidplus {
        return Ok(Step::Refused(
            "unsupported",
            "the server has neither MOVE nor UIDPLUS: moving would also remove the messages \
             other clients marked deleted",
        ));
    }
    match sync::select(session, &action.folder.name).await? {
        None => return Ok(Step::No("select")),
        Some(uidvalidity) if uidvalidity != action.uidvalidity => return Ok(RENUMBERED),
        Some(_) => {}
    }
    let set = sync::uid_set(&action.uids);
    if let Some((flag, add)) = action.kind.flag() {
        let sign = if add { '+' } else { '-' };
        let mut fetched = Vec::new();
        let done = sync::run_done(
            session,
            &format!("UID STORE {set} {sign}FLAGS ({flag})"),
            |response| {
                if let Response::Fetch(_, attributes) = response
                    && let Some(uid) = sync::uid(attributes)
                    && action.uids.contains(&uid)
                {
                    fetched.push((uid, sync::flags(attributes)));
                }
            },
        )
        .await?;
        return Ok(match done {
            Done::Ok(_) => {
                store.applied_flags(action, &fetched)?;
                Step::Applied { resync: Vec::new() }
            }
            Done::No { .. } => Step::No("store"),
            Done::Bad => BAD,
        });
    }
    if action.kind == Kind::DeletePermanently {
        return Ok(match remove(session, &set).await? {
            Done::Ok(_) => {
                store.applied_delete(action)?;
                Step::Applied { resync: Vec::new() }
            }
            Done::No { .. } => Step::No("expunge"),
            Done::Bad => BAD,
        });
    }
    let target = action.target.as_ref().expect("a move has a target");
    let Some(target_name) = sync::quote(&target.name) else {
        return Ok(NO_SUCH_FOLDER);
    };
    if capabilities.moves {
        let mut untagged = None;
        let done = sync::run_done(
            session,
            &format!("UID MOVE {set} {target_name}"),
            |response| {
                if let Some(copy) = sync::copy_uid(response) {
                    untagged = Some(copy);
                }
            },
        )
        .await?;
        return Ok(match done {
            Done::Ok(tagged) => Step::Applied {
                resync: store.applied_move(action, untagged.or(tagged).as_ref())?,
            },
            Done::No { try_create: true } => NO_SUCH_FOLDER,
            Done::No { .. } => Step::No("move"),
            Done::Bad => BAD,
        });
    }
    copy_then_remove(session, store, action, &set, &target_name).await
}

/// `UID STORE +FLAGS.SILENT (\Deleted)`, then `UID EXPUNGE` of the same UIDs.
async fn remove(session: &mut ImapSession, set: &str) -> std::result::Result<Done, SyncError> {
    let done = sync::run_done(
        session,
        &format!("UID STORE {set} +FLAGS.SILENT (\\Deleted)"),
        |_| {},
    )
    .await?;
    if !matches!(done, Done::Ok(_)) {
        return Ok(done);
    }
    sync::run_done(session, &format!("UID EXPUNGE {set}"), |_| {}).await
}

/// A move without MOVE: see the module's documentation. The source folder is selected.
async fn copy_then_remove(
    session: &mut ImapSession,
    store: &mut Store,
    action: &Action,
    set: &str,
    target_name: &str,
) -> std::result::Result<Step, SyncError> {
    let mut action = action.clone();
    if action.state == "copying" {
        match recover_copy(session, store, &action, target_name).await? {
            Recovered::NotCopied => {
                store.copy_state(action.id, "pending", None, None)?;
                action.state = "pending".to_owned();
            }
            Recovered::Copied(copied) => {
                store.copy_state(action.id, "copied", None, copied.as_ref())?;
                action.state = "copied".to_owned();
                action.copied = copied;
            }
            Recovered::Unknown => return Ok(Step::No("status")),
        }
        // Looking in the target selected it.
        match sync::select(session, &action.folder.name).await? {
            None => return Ok(Step::No("select")),
            Some(uidvalidity) if uidvalidity != action.uidvalidity => return Ok(RENUMBERED),
            Some(_) => {}
        }
    }
    if action.state == "pending" {
        let Some(uidnext) = uid_next(session, target_name).await? else {
            return Ok(Step::No("status"));
        };
        // Written before the COPY is sent: if its answer never comes, the next try knows where
        // to look for what it copied.
        store.copy_state(action.id, "copying", Some(uidnext), None)?;
        match copy(session, &action.uids, target_name).await? {
            Done::Ok(copied) => {
                store.copy_state(action.id, "copied", None, copied.as_ref())?;
                action.copied = copied;
            }
            Done::No { try_create: true } => return Ok(NO_SUCH_FOLDER),
            Done::No { .. } => {
                store.copy_state(action.id, "pending", None, None)?;
                return Ok(Step::No("copy"));
            }
            Done::Bad => return Ok(BAD),
        }
    }
    // Copied: only the removal from the source is left, which can be tried again as it is.
    Ok(match remove(session, set).await? {
        Done::Ok(_) => Step::Applied {
            resync: store.applied_move(&action, action.copied.as_ref())?,
        },
        Done::No { .. } => Step::No("expunge"),
        Done::Bad => BAD,
    })
}

/// `UID COPY`, with the `COPYUID` of its answer.
async fn copy(
    session: &mut ImapSession,
    uids: &[u32],
    target_name: &str,
) -> std::result::Result<Done, SyncError> {
    let mut untagged = None;
    let done = sync::run_done(
        session,
        &format!("UID COPY {} {target_name}", sync::uid_set(uids)),
        |response| {
            if let Some(copy) = sync::copy_uid(response) {
                untagged = Some(copy);
            }
        },
    )
    .await?;
    Ok(match done {
        Done::Ok(tagged) => Done::Ok(tagged.or(untagged)),
        other => other,
    })
}

enum Recovered {
    NotCopied,
    /// Copied, with the new UIDs found, when they are known.
    Copied(Option<CopyUid>),
    /// The target could not be read now.
    Unknown,
}

/// Finds out what a `UID COPY` whose answer never came did, and copies again what it did not.
async fn recover_copy(
    session: &mut ImapSession,
    store: &mut Store,
    action: &Action,
    target_name: &str,
) -> std::result::Result<Recovered, SyncError> {
    let Some(before) = action.target_uidnext else {
        return Ok(Recovered::NotCopied);
    };
    let Some(now) = uid_next(session, target_name).await? else {
        return Ok(Recovered::Unknown);
    };
    // Nothing arrived in the target since: the COPY did nothing.
    if now <= before {
        return Ok(Recovered::NotCopied);
    }
    let target = action.target.as_ref().expect("a move has a target");
    let Some(uidvalidity) = sync::examine_uidvalidity(session, &target.name).await? else {
        return Ok(Recovered::Unknown);
    };
    let message_ids = store.message_ids(action.folder.id, &action.uids)?;
    let mut found = CopyUid {
        uidvalidity,
        pairs: Vec::new(),
    };
    let mut missing = Vec::new();
    for (uid, message_id) in action.uids.iter().zip(message_ids) {
        // The copy keeps a Message-ID without its angle brackets; with them, `3@` does not
        // match `13@`.
        let new = match message_id.and_then(|id| sync::quote(&format!("<{id}>"))) {
            Some(quoted) => sync::search(
                session,
                &format!("UID {before}:* HEADER Message-ID {quoted}"),
            )
            .await?
            .and_then(|uids| uids.into_iter().find(|new| *new >= before)),
            None => None,
        };
        match new {
            Some(new) => found.pairs.push((*uid, new)),
            None => missing.push(*uid),
        }
    }
    if missing.is_empty() {
        return Ok(Recovered::Copied(Some(found)));
    }
    // What was not found is copied now: it is not in the target, or has no Message-ID to be
    // found by.
    match sync::select(session, &action.folder.name).await? {
        Some(uidvalidity) if uidvalidity == action.uidvalidity => {}
        _ => return Ok(Recovered::Unknown),
    }
    let Done::Ok(again) = copy(session, &missing, target_name).await? else {
        return Ok(Recovered::Unknown);
    };
    match again {
        Some(again) if again.uidvalidity == found.uidvalidity => {
            found.pairs.extend(again.pairs);
            Ok(Recovered::Copied(Some(found)))
        }
        // Copied, but where is not told: the target's sync brings them.
        _ => Ok(Recovered::Copied(None)),
    }
}

/// The UIDNEXT of a folder, from `STATUS`. `None` when the server refused it.
async fn uid_next(
    session: &mut ImapSession,
    name: &str,
) -> std::result::Result<Option<u32>, SyncError> {
    let mut found = None;
    let done = sync::run_done(session, &format!("STATUS {name} (UIDNEXT)"), |response| {
        if let Response::MailboxData(MailboxDatum::Status { status, .. }) = response {
            for attribute in status {
                if let StatusAttribute::UidNext(next) = attribute {
                    found = Some(*next);
                }
            }
        }
    })
    .await?;
    Ok(match done {
        Done::Ok(_) => found,
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_flag_is_added_or_removed_and_the_flags_stay_sorted_once_each() {
        assert_eq!(with_flag("", "\\Seen", true), "\\Seen");
        assert_eq!(with_flag("\\Flagged", "\\Seen", true), "\\Flagged \\Seen");
        assert_eq!(
            with_flag("\\Seen \\Flagged", "\\Seen", true),
            "\\Flagged \\Seen"
        );
        assert_eq!(with_flag("$Label \\Seen", "\\Seen", false), "$Label");
        assert_eq!(with_flag("\\Flagged", "\\Seen", false), "\\Flagged");
    }

    #[test]
    fn a_kind_reads_back_its_name() {
        for kind in Kind::ALL {
            assert_eq!(Kind::parse(kind.name()), Some(kind));
        }
        assert_eq!(Kind::parse("dance"), None);
    }
}
