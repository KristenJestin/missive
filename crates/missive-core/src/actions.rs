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
//! - read, unread, flagged, unflagged, answered: `UID STORE <uids> +FLAGS (\Seen)` (or `-FLAGS`,
//!   or `\Flagged`, `\Answered`), not silent: the flags it answers are written to the copy.
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
//! A `NO`, a cut connection, an answer the sync's reader cannot use or a command that gets no
//! answer within its timeout leaves the action in the queue as it is, tried again at the next
//! pass, and the actions after it wait, so that they are applied in order; after
//! [`MAX_ATTEMPTS`] failures it is refused, so that an action that always fails does not hold the
//! queue for ever. A refusal that will not change (the folder was renumbered, the target does not
//! exist, the server cannot move safely, a `BAD` to the action's own command) drops it at once and
//! takes its change back from the copy. A UID the server no longer has is not an error: the
//! command changes nothing for it, and the next sync removes it from the copy.
//!
//! A removal whose `UID EXPUNGE` was refused takes its `\Deleted` back, so that no other
//! client's `EXPUNGE` removes the message later, but only from the messages that did not have it
//! before the removal (`UID SEARCH DELETED UID <uids>` asks which): a `\Deleted` another client
//! set stays.
//!
//! A removal records the `\Deleted` it sets itself before it sets it: when the connection is lost
//! before the flag is taken back, the next try still knows it is its own. An action refused for
//! good that leaves it there keeps it in [`Store::deleted_left`], and it is taken back at once,
//! then at the next connection, unless the folder was renumbered or deleted since. Until then, a move of those messages is refused: `UID MOVE` and
//! `UID COPY` keep the flags, and the copy in the target would keep a `\Deleted` nothing records.
//!
//! An action asked while a move of its messages waits is recorded on them as the server still has
//! them, apart from the messages that are not moving, and follows them to their new UIDs once the
//! move is applied, but only when the server told those UIDs (`COPYUID`) for exactly those
//! messages: a delete applied to a UID found by other means could remove another message. Without
//! them, it is refused. A permanent delete asked after a move to the trash is refused with that
//! move.
//!
//! A move refused once its `UID COPY` was made leaves the messages in both folders. Nothing is
//! removed to fix it: the copy shows both, the outcome says so, and the messages are recorded as
//! half moved ([`crate::copy::Copy::half_moved`]) until the user removes one of the two. A move
//! refused while the answer to its `UID COPY` is unknown is recorded the same way, as a maybe.
//!
//! A `UID COPY` whose answer was never read (the connection was cut) may or may not have
//! copied: before copying again, the target's UIDNEXT tells whether anything arrived there since,
//! and if so, each message is looked for there by its `Message-ID`, and a message found counts as
//! its copy only with the size and the INTERNALDATE the copy kept from the source (`UID COPY`
//! keeps both). A message without a Message-ID or an INTERNALDATE, or whose copy is not found, is
//! copied again: a copy twice is better than a message lost.

use std::collections::BTreeSet;
use std::time::Duration;

use async_imap::imap_proto::{
    AttributeValue, MailboxDatum, NameAttribute, Response, StatusAttribute,
};

use crate::connect::{ConnectError, ImapSession};
use crate::copy::message_id;
use crate::store::{Action, Dropped, Fingerprint, Store};
use crate::sync::{self, Capabilities, CopyUid, Done, SyncError};

/// How many times an action fails (a `NO`, a cut connection, a timeout…) before it is refused.
pub const MAX_ATTEMPTS: u32 = 5;

/// How long the server is given to answer each command of an action: one that never answers
/// would hold the worker for ever. Moving many large messages can take a server a while.
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// The slowest link an `APPEND` is waited for, in bytes per second (about 256 kbit/s).
const APPEND_RATE: u64 = 32 * 1024;

/// The longest an `APPEND` is waited for, whatever its size.
const APPEND_TIMEOUT_MAX: Duration = Duration::from_secs(15 * 60);

/// How long to wait for an `APPEND` of `size` bytes: the timeout of a command, and the time its
/// message takes on a slow link, up to a bound. A large message on a slow link would otherwise
/// time out at every pass and be uploaded again. A timeout asked beyond the bound is kept.
fn append_timeout(timeout: Duration, size: usize) -> Duration {
    let upload = Duration::from_secs(size as u64 / APPEND_RATE);
    (timeout + upload).min(APPEND_TIMEOUT_MAX.max(timeout))
}

/// What an action does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Read,
    Unread,
    Flagged,
    Unflagged,
    /// Set by a reply once it is sent, never asked for with `mark`.
    Answered,
    /// Set by a forward once it is sent, never asked for with `mark`. A keyword (`$Forwarded`,
    /// RFC 5788), which a server may not keep: see [`Kind::keyword`].
    Forwarded,
    Move,
    /// A move to the account's trash.
    Delete,
    DeletePermanently,
}

impl Kind {
    pub const ALL: [Kind; 9] = [
        Kind::Read,
        Kind::Unread,
        Kind::Flagged,
        Kind::Unflagged,
        Kind::Answered,
        Kind::Forwarded,
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
            Kind::Answered => "answered",
            Kind::Forwarded => "forwarded",
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
            Kind::Answered => Some(("\\Answered", true)),
            Kind::Forwarded => Some(("$Forwarded", true)),
            _ => None,
        }
    }

    /// Whether the flag it sets is a keyword, which a folder keeps only when it says so
    /// (`\*` in its PERMANENTFLAGS, RFC 3501 6.3.1) rather than a system flag.
    pub fn keyword(self) -> bool {
        matches!(self, Kind::Forwarded)
    }

    /// Whether it takes the messages to another folder.
    pub fn moves(self) -> bool {
        matches!(self, Kind::Move | Kind::Delete)
    }
}

/// What a request asks for, before it is checked against the copy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Wanted {
    /// Read, Unread, Flagged, Unflagged or Answered.
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
    /// `no-such-message`, `no-such-folder`, `no-trash`, `in-trash`, `not-in-trash`, `busy` or
    /// `unsupported`.
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
    /// The numbers of the actions recorded, one per folder, as their `action` events give them.
    pub actions: Vec<i64>,
}

/// Why a server with or without MOVE and UIDPLUS cannot apply an action of this kind safely, if
/// it cannot: when the copy knows the server's capabilities, the request is refused at once
/// (`unsupported`), else the action is refused when it is tried.
pub fn unsupported(kind: Kind, moves: bool, uidplus: bool) -> Option<&'static str> {
    if kind == Kind::DeletePermanently && !uidplus {
        return Some(
            "the server has no UID EXPUNGE (UIDPLUS): deleting for good would also remove the \
             messages other clients marked deleted",
        );
    }
    if kind.moves() && !moves && !uidplus {
        return Some(
            "the server has neither MOVE nor UIDPLUS: moving would also remove the messages \
             other clients marked deleted",
        );
    }
    None
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
    /// (the connection was lost, the answer could not be used, or none came in time).
    Retried {
        attempts: u32,
        reason: &'static str,
    },
    /// It was dropped and its change taken back from the copy. `reason` is `renumbered`,
    /// `no-such-folder`, `unsupported`, `deleted-left`, `refused` or `connection` (it failed that way
    /// [`MAX_ATTEMPTS`] times). `copied`: a move the server refused once it had copied the
    /// messages, which are in both folders now; nothing is removed to fix it but by the user.
    /// `maybe_copied`: a move refused while the answer to its copy was unknown, whose messages
    /// may be in both folders.
    Refused {
        reason: &'static str,
        message: String,
        copied: bool,
        maybe_copied: bool,
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
    /// Whether a command got no answer in time: what the connection says next cannot be trusted,
    /// so it has to be dropped.
    pub timed_out: bool,
    /// How many messages got back the `\Deleted` a refused action had left on them.
    pub taken_back: usize,
}

/// The session, each of whose commands is given [`COMMAND_TIMEOUT`] (or the timeout asked).
pub(crate) struct Server<'a> {
    session: &'a mut ImapSession,
    timeout: Duration,
    /// Whether a command got no answer in time.
    pub(crate) timed_out: bool,
    /// Whether the folder last selected keeps keywords.
    keywords: bool,
}

impl<'a> Server<'a> {
    pub(crate) fn new(session: &'a mut ImapSession, timeout: Duration) -> Server<'a> {
        Server {
            session,
            timeout,
            timed_out: false,
            keywords: true,
        }
    }
}

impl Server<'_> {
    /// A command's result, or the error of one that got no answer in time.
    fn timed<T>(
        &mut self,
        result: std::result::Result<std::result::Result<T, SyncError>, tokio::time::error::Elapsed>,
    ) -> std::result::Result<T, SyncError> {
        result.unwrap_or_else(|_| {
            self.timed_out = true;
            Err(SyncError::Connection(ConnectError::Unreachable(
                "the IMAP server did not answer an action's command in time".to_owned(),
            )))
        })
    }

    pub(crate) async fn run_done(
        &mut self,
        command: &str,
        each: impl FnMut(&Response<'_>),
    ) -> std::result::Result<Done, SyncError> {
        let result =
            tokio::time::timeout(self.timeout, sync::run_done(self.session, command, each)).await;
        self.timed(result)
    }

    pub(crate) async fn select(
        &mut self,
        name: &str,
    ) -> std::result::Result<Option<u32>, SyncError> {
        let result = tokio::time::timeout(self.timeout, sync::select(self.session, name)).await;
        let selected = self.timed(result)?;
        self.keywords = selected.is_none_or(|(_, keywords)| keywords);
        Ok(selected.map(|(uidvalidity, _)| uidvalidity))
    }

    async fn examine_uidvalidity(
        &mut self,
        name: &str,
    ) -> std::result::Result<Option<u32>, SyncError> {
        let result =
            tokio::time::timeout(self.timeout, sync::examine_uidvalidity(self.session, name)).await;
        self.timed(result)
    }

    pub(crate) async fn search(
        &mut self,
        criteria: &str,
    ) -> std::result::Result<Option<Vec<u32>>, SyncError> {
        let result = tokio::time::timeout(self.timeout, sync::search(self.session, criteria)).await;
        self.timed(result)
    }

    /// [`sync::append`]; `name` is quoted already.
    pub(crate) async fn append(
        &mut self,
        name: &str,
        flags: &str,
        raw: &[u8],
    ) -> std::result::Result<Done, SyncError> {
        let timeout = append_timeout(self.timeout, raw.len());
        let result =
            tokio::time::timeout(timeout, sync::append(self.session, name, flags, raw)).await;
        self.timed(result)
    }
}

/// Applies the account's queue on the server, in order, until it is empty or an action has to
/// wait. Each command is given `timeout`. A connection error stops it, counted as a failure of
/// the action it happened in, and is returned beside what was done before it.
pub async fn apply(
    session: &mut ImapSession,
    store: &mut Store,
    account: i64,
    account_id: &str,
    capabilities: Capabilities,
    timeout: Duration,
) -> (Applied, std::result::Result<(), SyncError>) {
    let mut applied = Applied::default();
    let mut server = Server::new(session, timeout);
    let result = round(
        &mut server,
        store,
        account,
        account_id,
        capabilities,
        &mut applied,
    )
    .await;
    applied.timed_out = server.timed_out;
    (applied, result)
}

async fn round(
    server: &mut Server<'_>,
    store: &mut Store,
    account: i64,
    account_id: &str,
    capabilities: Capabilities,
    applied: &mut Applied,
) -> std::result::Result<(), SyncError> {
    take_back(server, store, account, applied).await?;
    loop {
        let Some(action) = store.next_action(account)? else {
            return Ok(());
        };
        let messages = messages(account_id, &action);
        let outcome = |result| Outcome {
            action: action.id,
            kind: action.kind,
            messages: messages.clone(),
            result,
        };
        let result = match one(server, store, &action, capabilities).await {
            Ok(result) => result,
            Err(error @ SyncError::Connection(_)) => {
                // Counted like a `NO`: an action whose command always ends the connection, or
                // gets an answer the reader cannot use, must not stop the account for ever.
                let attempts = store.retried(action.id, "connection")?;
                if attempts < MAX_ATTEMPTS {
                    applied.outcomes.push(outcome(Result::Retried {
                        attempts,
                        reason: "connection",
                    }));
                } else {
                    let dropped = refuse(store, &action, applied)?;
                    applied.outcomes.push(outcome(refused(
                        "connection",
                        format!(
                            "it failed {attempts} times: the connection was lost, the server's \
                             answer could not be used, or none came in time"
                        ),
                        &dropped,
                    )));
                    refuse_behind(store, account_id, &dropped.behind, NOT_MOVED, applied)?;
                }
                return Err(error);
            }
            Err(error) => return Err(error),
        };
        let mut behind = (Vec::new(), NOT_MOVED);
        let mut left = 0;
        let result = match result {
            Step::Applied { resync, moved } => {
                applied.resync.extend(resync);
                behind = (moved, MOVED);
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
                let dropped = refuse(store, &action, applied)?;
                behind.0 = dropped.behind.clone();
                left = dropped.deleted_left;
                refused(
                    "refused",
                    format!("the server refused it {attempts} times ({why})"),
                    &dropped,
                )
            }
            Step::Refused(reason, message) => {
                let dropped = refuse(store, &action, applied)?;
                behind.0 = dropped.behind.clone();
                left = dropped.deleted_left;
                refused(reason, message.to_owned(), &dropped)
            }
            Step::Skipped => {
                // The copy took its flag at once: it takes it back at once too, the server
                // having been told nothing, and the folder is not read again for it.
                store.skipped(&action)?;
                continue;
            }
        };
        applied.outcomes.push(outcome(result));
        refuse_behind(store, account_id, &behind.0, behind.1, applied)?;
        // Before anything else is asked of its messages: a move would carry the flag along.
        if left > 0 {
            take_back(server, store, account, applied).await?;
        }
    }
}

/// The identifiers of an action's messages, as they were when it was asked.
fn messages(account_id: &str, action: &Action) -> Vec<String> {
    action
        .uids
        .iter()
        .map(|uid| message_id(account_id, action.folder.id, action.uidvalidity, *uid))
        .collect()
}

/// Why an action asked after a move is refused when the move was applied but the server did not
/// say where its messages went (no COPYUID, or a copy found again rather than told).
const MOVED: (&str, &str) = (
    "moved",
    "a move before it took the messages to another folder, and the server did not tell their \
     new UIDs (COPYUID): nothing was done to them there",
);

/// Why a permanent delete asked after a move is refused when that move was.
const NOT_MOVED: (&str, &str) = (
    "not-moved",
    "the move before it was refused, so the messages are not where it was asked: nothing was \
     deleted",
);

/// Refuses the actions asked after a move that cannot apply now, each with its outcome.
fn refuse_behind(
    store: &mut Store,
    account_id: &str,
    ids: &[i64],
    (reason, why): (&'static str, &str),
    applied: &mut Applied,
) -> std::result::Result<(), SyncError> {
    for id in ids {
        // A refusal before may have taken it already.
        let Some(action) = store.action(*id)? else {
            continue;
        };
        // What was asked after it names the same messages: it is in `ids` already.
        let dropped = refuse(store, &action, applied)?;
        applied.outcomes.push(Outcome {
            action: action.id,
            kind: action.kind,
            messages: messages(account_id, &action),
            result: refused(reason, why.to_owned(), &dropped),
        });
    }
    Ok(())
}

/// Drops an action and takes its change back from the copy; the folder is synced again, and the
/// target too when the move had copied the messages there.
fn refuse(
    store: &mut Store,
    action: &Action,
    applied: &mut Applied,
) -> std::result::Result<Dropped, SyncError> {
    let dropped = store.refused(action)?;
    applied.resync.insert(action.folder.id);
    if (dropped.copied || dropped.maybe_copied)
        && let Some(target) = &action.target
    {
        applied.resync.insert(target.id);
    }
    Ok(dropped)
}

/// What a refused action says, for people: `why`, for a move that had copied the messages,
/// where they are now, and the `\Deleted` flags it could not take back.
fn refused(reason: &'static str, why: String, dropped: &Dropped) -> Result {
    let mut message = why;
    if dropped.copied {
        message.push_str(
            "; the server had copied the messages to the target first, so they are in both \
             folders now: nothing was removed, delete or move one of the two",
        );
    }
    if dropped.maybe_copied {
        message.push_str(
            "; the server's answer to the copy to the target was lost, so the messages may be in \
             both folders now: nothing was removed, look in the target",
        );
    }
    if dropped.deleted_left > 0 {
        message.push_str(&format!(
            "; {} of them keep the \\Deleted flag Missive set on the server, which it takes \
             back at the next connection",
            dropped.deleted_left
        ));
    }
    Result::Refused {
        reason,
        message,
        copied: dropped.copied,
        maybe_copied: dropped.maybe_copied,
    }
}

/// Takes back the `\Deleted` flags refused actions left on the server ([`Store::deleted_left`]),
/// unless the folder was renumbered since: its UIDs then name other messages. One the server
/// refuses now is tried again at the next connection.
async fn take_back(
    server: &mut Server<'_>,
    store: &mut Store,
    account: i64,
    applied: &mut Applied,
) -> std::result::Result<(), SyncError> {
    for left in store.deleted_left(account)? {
        if left.folder.uidvalidity != Some(left.uidvalidity) {
            store.taken_back(left.id)?;
            continue;
        }
        match server.select(&left.folder.name).await? {
            None => {
                // A folder deleted since has none of its messages left to take the flag from.
                // But a server that lists nothing at all, not even the inbox every account has
                // (a bridge still syncing), does not tell that it is gone.
                if listed(server, &left.folder.name).await? == Some(false)
                    && listed(server, "INBOX").await? == Some(true)
                {
                    store.taken_back(left.id)?;
                }
                continue;
            }
            Some(uidvalidity) if uidvalidity != left.uidvalidity => {
                store.taken_back(left.id)?;
                continue;
            }
            Some(_) => {}
        }
        let done = server
            .run_done(
                &format!(
                    "UID STORE {} -FLAGS.SILENT (\\Deleted)",
                    sync::uid_set(&left.uids)
                ),
                |_| {},
            )
            .await?;
        if matches!(done, Done::Ok(_)) {
            store.taken_back(left.id)?;
            applied.taken_back += left.uids.len();
        }
    }
    Ok(())
}

/// Whether the server lists a folder (`LIST "" <name>`); `None` when it cannot tell.
async fn listed(
    server: &mut Server<'_>,
    name: &str,
) -> std::result::Result<Option<bool>, SyncError> {
    let Some(quoted) = sync::quote(name) else {
        return Ok(None);
    };
    let mut found = false;
    let done = server
        .run_done(&format!("LIST \"\" {quoted}"), |response| {
            if let Response::MailboxData(MailboxDatum::List(list)) = response
                && sync::unescape(&list.name) == name
                && !list.name_attributes.iter().any(|a| {
                    matches!(a, NameAttribute::Extension(e) if e.eq_ignore_ascii_case("\\NonExistent"))
                })
            {
                found = true;
            }
        })
        .await?;
    Ok(matches!(done, Done::Ok(_)).then_some(found))
}

/// What one try of an action came to.
enum Step {
    /// `moved`: the actions asked after a move that could not follow its messages.
    Applied {
        resync: Vec<i64>,
        moved: Vec<i64>,
    },
    /// The server answered `NO`; why, for the queue.
    No(&'static str),
    /// The server cannot keep what the action sets: dropped, with no outcome to tell.
    Skipped,
    Refused(&'static str, &'static str),
}

const RENUMBERED: Step = Step::Refused(
    "renumbered",
    "the server renumbered the folder (its UIDVALIDITY changed), so its UIDs name other messages",
);
const NO_SUCH_FOLDER: Step =
    Step::Refused("no-such-folder", "the server has no such target folder");
const BAD: Step = Step::Refused("refused", "the server refused the command as malformed");
const DELETED_LEFT: Step = Step::Refused(
    "deleted-left",
    "a refused action left the \\Deleted flag Missive set on these messages and it could not be \
     taken back yet: moving them would carry it to the target, where any client's EXPUNGE would \
     remove them, so nothing was moved",
);

async fn one(
    server: &mut Server<'_>,
    store: &mut Store,
    action: &Action,
    capabilities: Capabilities,
) -> std::result::Result<Step, SyncError> {
    if action.folder.uidvalidity != Some(action.uidvalidity) {
        return Ok(RENUMBERED);
    }
    if action
        .target
        .as_ref()
        .is_some_and(|t| t.id == action.folder.id)
    {
        store.settled(action)?;
        return Ok(Step::Applied {
            resync: Vec::new(),
            moved: Vec::new(),
        });
    }
    // `UID MOVE` and `UID COPY` keep the flags, and the target keeps no record of this one.
    if action.kind.moves()
        && store.deleted_left_on(action.folder.id, action.uidvalidity, &action.uids)?
    {
        return Ok(DELETED_LEFT);
    }
    // Refused before anything is sent.
    if let Some(why) = unsupported(action.kind, capabilities.moves, capabilities.uidplus) {
        return Ok(Step::Refused("unsupported", why));
    }
    match server.select(&action.folder.name).await? {
        None => return Ok(Step::No("select")),
        Some(uidvalidity) if uidvalidity != action.uidvalidity => return Ok(RENUMBERED),
        Some(_) => {}
    }
    // A server that keeps no keywords is not asked for one: it would refuse, or take it and
    // forget it. Nothing is said, the message just is not marked.
    if action.kind.keyword() && !server.keywords {
        return Ok(Step::Skipped);
    }
    let set = sync::uid_set(&action.uids);
    if let Some((flag, add)) = action.kind.flag() {
        let sign = if add { '+' } else { '-' };
        let mut fetched = Vec::new();
        let done = server
            .run_done(
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
                Step::Applied {
                    resync: Vec::new(),
                    moved: Vec::new(),
                }
            }
            Done::No { .. } => Step::No("store"),
            Done::Bad => BAD,
        });
    }
    if action.kind == Kind::DeletePermanently {
        return Ok(match removing(server, store, action).await? {
            Done::Ok(_) => {
                store.applied_delete(action)?;
                Step::Applied {
                    resync: Vec::new(),
                    moved: Vec::new(),
                }
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
        // A server may tell the new UIDs in several untagged COPYUIDs, one per batch it moved.
        let mut untagged: Option<std::result::Result<CopyUid, ()>> = None;
        let done = server
            .run_done(&format!("UID MOVE {set} {target_name}"), |response| {
                if let Some(copy) = sync::copy_uid(response) {
                    untagged = Some(match untagged.take() {
                        None => Ok(copy),
                        // Parts that disagree tell nothing to be trusted.
                        Some(Ok(all)) => sync::merged(all, copy).ok_or(()),
                        Some(Err(())) => Err(()),
                    });
                }
            })
            .await?;
        return Ok(match done {
            Done::Ok(tagged) => {
                let copied = match untagged {
                    Some(untagged) => untagged.ok(),
                    None => tagged,
                };
                let (resync, moved) = store.applied_move(action, copied.as_ref(), true)?;
                Step::Applied { resync, moved }
            }
            Done::No { try_create: true } => NO_SUCH_FOLDER,
            Done::No { .. } => Step::No("move"),
            Done::Bad => BAD,
        });
    }
    copy_then_remove(server, store, action, &target_name).await
}

/// [`remove_marking`] for an action.
async fn removing(
    server: &mut Server<'_>,
    store: &mut Store,
    action: &Action,
) -> std::result::Result<Done, SyncError> {
    // Held as `&mut`: a `&Store` cannot cross an await of a task that may move between threads.
    remove_marking(server, &action.uids, &action.marked_deleted, move |uids| {
        Ok(store.marked_deleted(action.id, uids)?)
    })
    .await
}

/// `UID STORE +FLAGS.SILENT (\Deleted)`, then `UID EXPUNGE` of the same UIDs. An expunge the
/// server refused takes the flag back (best effort), but only from the messages that did not
/// have it before: left there, another client's `EXPUNGE` would remove messages the copy shows,
/// once the action is refused, and a `\Deleted` another client set is not ours to clear. Which
/// ones had it is asked first (`UID SEARCH DELETED UID <set>`); an answer refused means the
/// removal is not started.
///
/// `mark` records the UIDs whose flag is Missive's own, before it is set and once it is taken
/// back: a connection lost before the flag is taken back leaves it on the server, and the next
/// try, which then finds it set, must still know it is Missive's own. `earlier` are the UIDs an
/// earlier try recorded.
pub(crate) async fn remove_marking(
    server: &mut Server<'_>,
    uids: &[u32],
    earlier: &[u32],
    mut mark: impl FnMut(&[u32]) -> std::result::Result<(), SyncError>,
) -> std::result::Result<Done, SyncError> {
    let set = sync::uid_set(uids);
    let Some(marked) = server.search(&format!("DELETED UID {set}")).await? else {
        return Ok(Done::No { try_create: false });
    };
    let ours: Vec<u32> = uids
        .iter()
        .copied()
        .filter(|uid| !marked.contains(uid) || earlier.contains(uid))
        .collect();
    mark(&ours)?;
    let done = server
        .run_done(
            &format!("UID STORE {set} +FLAGS.SILENT (\\Deleted)"),
            |_| {},
        )
        .await?;
    if !matches!(done, Done::Ok(_)) {
        // Nothing was set: only what an earlier try set is still there.
        let still: Vec<u32> = ours
            .iter()
            .copied()
            .filter(|uid| marked.contains(uid))
            .collect();
        mark(&still)?;
        return Ok(done);
    }
    let done = server
        .run_done(&format!("UID EXPUNGE {set}"), |_| {})
        .await?;
    if !matches!(done, Done::Ok(_)) && !ours.is_empty() {
        let taken_back = server
            .run_done(
                &format!(
                    "UID STORE {} -FLAGS.SILENT (\\Deleted)",
                    sync::uid_set(&ours)
                ),
                |_| {},
            )
            .await?;
        if matches!(taken_back, Done::Ok(_)) {
            mark(&[])?;
        }
    }
    Ok(done)
}

/// A move without MOVE: see the module's documentation. The source folder is selected.
async fn copy_then_remove(
    server: &mut Server<'_>,
    store: &mut Store,
    action: &Action,
    target_name: &str,
) -> std::result::Result<Step, SyncError> {
    let mut action = action.clone();
    if action.state == "copying" {
        match recover_copy(server, store, &action, target_name).await? {
            Recovered::NotCopied => {
                store.copy_state(action.id, "pending", None, None, false)?;
                action.state = "pending".to_owned();
            }
            Recovered::Copied(copied) => {
                // Found again, not told by the server: not proof enough for what follows.
                store.copy_state(action.id, "copied", None, copied.as_ref(), false)?;
                action.state = "copied".to_owned();
                action.copied = copied;
                action.copyuid = false;
            }
            Recovered::Unknown => return Ok(Step::No("status")),
        }
        // Looking in the target selected it.
        match server.select(&action.folder.name).await? {
            None => return Ok(Step::No("select")),
            Some(uidvalidity) if uidvalidity != action.uidvalidity => return Ok(RENUMBERED),
            Some(_) => {}
        }
    }
    if action.state == "pending" {
        let Some(uidnext) = uid_next(server, target_name).await? else {
            return Ok(Step::No("status"));
        };
        // Written before the COPY is sent: if its answer never comes, the next try knows where
        // to look for what it copied.
        store.copy_state(action.id, "copying", Some(uidnext), None, false)?;
        match copy(server, &action.uids, target_name).await? {
            Done::Ok(copied) => {
                store.copy_state(action.id, "copied", None, copied.as_ref(), true)?;
                action.copyuid = copied.is_some();
                action.copied = copied;
            }
            Done::No { try_create: true } => return Ok(NO_SUCH_FOLDER),
            Done::No { .. } => {
                store.copy_state(action.id, "pending", None, None, false)?;
                return Ok(Step::No("copy"));
            }
            Done::Bad => return Ok(BAD),
        }
    }
    // Copied: only the removal from the source is left, which can be tried again as it is.
    Ok(match removing(server, store, &action).await? {
        Done::Ok(_) => {
            let (resync, moved) =
                store.applied_move(&action, action.copied.as_ref(), action.copyuid)?;
            Step::Applied { resync, moved }
        }
        Done::No { .. } => Step::No("expunge"),
        Done::Bad => BAD,
    })
}

/// `UID COPY`, with the `COPYUID` of its answer.
async fn copy(
    server: &mut Server<'_>,
    uids: &[u32],
    target_name: &str,
) -> std::result::Result<Done, SyncError> {
    let mut untagged = None;
    let done = server
        .run_done(
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
    server: &mut Server<'_>,
    store: &mut Store,
    action: &Action,
    target_name: &str,
) -> std::result::Result<Recovered, SyncError> {
    let Some(before) = action.target_uidnext else {
        return Ok(Recovered::NotCopied);
    };
    let Some(now) = uid_next(server, target_name).await? else {
        return Ok(Recovered::Unknown);
    };
    // Nothing arrived in the target since: the COPY did nothing.
    if now <= before {
        return Ok(Recovered::NotCopied);
    }
    let target = action.target.as_ref().expect("a move has a target");
    let Some(uidvalidity) = server.examine_uidvalidity(&target.name).await? else {
        return Ok(Recovered::Unknown);
    };
    let fingerprints = store.fingerprints(action.folder.id, &action.uids)?;
    let mut found = CopyUid {
        uidvalidity,
        pairs: Vec::new(),
    };
    let mut missing = Vec::new();
    for (uid, fingerprint) in action.uids.iter().zip(fingerprints) {
        let new = match fingerprint {
            Some(fingerprint) => {
                let taken: Vec<u32> = found.pairs.iter().map(|(_, new)| *new).collect();
                find_copy(server, before, &fingerprint, &taken).await?
            }
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
    match server.select(&action.folder.name).await? {
        Some(uidvalidity) if uidvalidity == action.uidvalidity => {}
        _ => return Ok(Recovered::Unknown),
    }
    let Done::Ok(again) = copy(server, &missing, target_name).await? else {
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

/// The UID of a copy of the message in the target, which is examined, from `before` on: a message
/// with its `Message-ID`, its size and its INTERNALDATE, which `UID COPY` keeps. The same
/// Message-ID alone is not enough: the same message received again, or another message that
/// reuses it, may have arrived there meanwhile, and trusting it would remove the original
/// without a copy. `taken` are the UIDs already found for other messages of the action.
async fn find_copy(
    server: &mut Server<'_>,
    before: u32,
    fingerprint: &Fingerprint,
    taken: &[u32],
) -> std::result::Result<Option<u32>, SyncError> {
    // The copy keeps a Message-ID without its angle brackets; with them, `3@` does not match
    // `13@`.
    let Some(quoted) = fingerprint
        .message_id
        .as_ref()
        .and_then(|id| sync::quote(&format!("<{id}>")))
    else {
        return Ok(None);
    };
    // Without its INTERNALDATE, nothing can tell a copy from another message.
    let Some(internal_date) = fingerprint.internal_date else {
        return Ok(None);
    };
    let Some(candidates) = server
        .search(&format!("UID {before}:* HEADER Message-ID {quoted}"))
        .await?
    else {
        return Ok(None);
    };
    let candidates: Vec<u32> = candidates
        .into_iter()
        .filter(|new| *new >= before && !taken.contains(new))
        .collect();
    if candidates.is_empty() {
        return Ok(None);
    }
    let mut same = Vec::new();
    let done = server
        .run_done(
            &format!(
                "UID FETCH {} (UID RFC822.SIZE INTERNALDATE)",
                sync::uid_set(&candidates)
            ),
            |response| {
                if let Response::Fetch(_, attributes) = response
                    && let Some(uid) = sync::uid(attributes)
                    && candidates.contains(&uid)
                {
                    let mut size = None;
                    let mut date = None;
                    for attribute in attributes.iter() {
                        match attribute {
                            AttributeValue::Rfc822Size(value) => size = Some(i64::from(*value)),
                            AttributeValue::InternalDate(value) => {
                                date = sync::parse_internal_date(value);
                            }
                            _ => {}
                        }
                    }
                    if size == Some(fingerprint.size) && date == Some(internal_date) {
                        same.push(uid);
                    }
                }
            },
        )
        .await?;
    if !matches!(done, Done::Ok(_)) {
        return Ok(None);
    }
    Ok(same.into_iter().min())
}

/// The UIDNEXT of a folder, from `STATUS`. `None` when the server refused it.
async fn uid_next(
    server: &mut Server<'_>,
    name: &str,
) -> std::result::Result<Option<u32>, SyncError> {
    let mut found = None;
    let done = server
        .run_done(&format!("STATUS {name} (UIDNEXT)"), |response| {
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
    fn an_append_is_given_a_timeout_that_grows_with_its_size_up_to_a_bound() {
        let base = Duration::from_secs(30);
        // A small message gets the usual timeout.
        assert_eq!(append_timeout(base, 1_000), base);
        // A large one on a slow link gets longer, in proportion.
        let ten = append_timeout(base, 10 * 1024 * 1024);
        let twenty = append_timeout(base, 20 * 1024 * 1024);
        assert!(ten > base * 4, "{ten:?}");
        assert!(twenty > ten, "{twenty:?}");
        // Bounded: a connection that never answers is not waited for for ever.
        assert_eq!(append_timeout(base, usize::MAX), APPEND_TIMEOUT_MAX);
        // A timeout asked beyond the bound is kept.
        let long = Duration::from_secs(3600);
        assert_eq!(append_timeout(long, 100 * 1024 * 1024), long);
    }

    #[test]
    fn a_refused_move_that_copied_says_the_messages_are_in_both_folders() {
        let dropped = Dropped {
            copied: true,
            ..Dropped::default()
        };
        let Result::Refused { message, .. } =
            refused("refused", "the server said no".to_owned(), &dropped)
        else {
            panic!("a refusal")
        };
        assert_eq!(
            message,
            "the server said no; the server had copied the messages to the target first, so \
             they are in both folders now: nothing was removed, delete or move one of the two"
        );
    }

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
