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
//! A move refused once its `UID COPY` was made leaves the messages in both folders. Nothing is
//! removed to fix it: the copy shows both, the outcome says so, and the messages are recorded as
//! half moved ([`crate::copy::Copy::half_moved`]) until the user removes one of the two.
//!
//! A `UID COPY` whose answer was never read (the connection was cut) may or may not have
//! copied: before copying again, the target's UIDNEXT tells whether anything arrived there since,
//! and if so, each message is looked for there by its `Message-ID`, and a message found counts as
//! its copy only with the size and the INTERNALDATE the copy kept from the source (`UID COPY`
//! keeps both). A message without a Message-ID or an INTERNALDATE, or whose copy is not found, is
//! copied again: a copy twice is better than a message lost.

use std::collections::BTreeSet;
use std::time::Duration;

use async_imap::imap_proto::{AttributeValue, MailboxDatum, Response, StatusAttribute};

use crate::connect::{ConnectError, ImapSession};
use crate::copy::message_id;
use crate::store::{Action, Fingerprint, Store};
use crate::sync::{self, Capabilities, CopyUid, Done, SyncError};

/// How many times an action fails (a `NO`, a cut connection, a timeout…) before it is refused.
pub const MAX_ATTEMPTS: u32 = 5;

/// How long the server is given to answer each command of an action: one that never answers
/// would hold the worker for ever. Moving many large messages can take a server a while.
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// What an action does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Read,
    Unread,
    Flagged,
    Unflagged,
    /// Set by a reply once it is sent, never asked for with `mark`.
    Answered,
    Move,
    /// A move to the account's trash.
    Delete,
    DeletePermanently,
}

impl Kind {
    pub const ALL: [Kind; 8] = [
        Kind::Read,
        Kind::Unread,
        Kind::Flagged,
        Kind::Unflagged,
        Kind::Answered,
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
    /// `no-such-folder`, `unsupported`, `refused` or `connection` (it failed that way
    /// [`MAX_ATTEMPTS`] times). `copied`: a move the server refused once it had copied the
    /// messages, which are in both folders now; nothing is removed to fix it but by the user.
    Refused {
        reason: &'static str,
        message: String,
        copied: bool,
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
}

/// The session, each of whose commands is given [`COMMAND_TIMEOUT`] (or the timeout asked).
pub(crate) struct Server<'a> {
    session: &'a mut ImapSession,
    timeout: Duration,
    /// Whether a command got no answer in time.
    pub(crate) timed_out: bool,
}

impl<'a> Server<'a> {
    pub(crate) fn new(session: &'a mut ImapSession, timeout: Duration) -> Server<'a> {
        Server {
            session,
            timeout,
            timed_out: false,
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
        self.timed(result)
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
        let result =
            tokio::time::timeout(self.timeout, sync::append(self.session, name, flags, raw)).await;
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
                    let copied = refuse(store, &action, applied)?;
                    applied.outcomes.push(outcome(refused(
                        "connection",
                        format!(
                            "it failed {attempts} times: the connection was lost, the server's \
                             answer could not be used, or none came in time"
                        ),
                        copied,
                    )));
                }
                return Err(error);
            }
            Err(error) => return Err(error),
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
                let copied = refuse(store, &action, applied)?;
                refused(
                    "refused",
                    format!("the server refused it {attempts} times ({why})"),
                    copied,
                )
            }
            Step::Refused(reason, message) => {
                let copied = refuse(store, &action, applied)?;
                refused(reason, message.to_owned(), copied)
            }
        };
        applied.outcomes.push(outcome(result));
    }
}

/// Drops an action and takes its change back from the copy; the folder is synced again, and the
/// target too when the move had copied the messages there. Returns whether it had.
fn refuse(
    store: &mut Store,
    action: &Action,
    applied: &mut Applied,
) -> std::result::Result<bool, SyncError> {
    let copied = store.refused(action)?;
    applied.resync.insert(action.folder.id);
    if copied && let Some(target) = &action.target {
        applied.resync.insert(target.id);
    }
    Ok(copied)
}

/// What a refused action says, for people: `why`, and for a move that had copied the messages,
/// where they are now.
fn refused(reason: &'static str, why: String, copied: bool) -> Result {
    let message = if copied {
        format!(
            "{why}; the server had copied the messages to the target first, so they are in both \
             folders now: nothing was removed, delete or move one of the two"
        )
    } else {
        why
    };
    Result::Refused {
        reason,
        message,
        copied,
    }
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
    server: &mut Server<'_>,
    store: &mut Store,
    action: &Action,
    capabilities: Capabilities,
) -> std::result::Result<Step, SyncError> {
    if action.folder.uidvalidity != Some(action.uidvalidity) {
        return Ok(RENUMBERED);
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
                Step::Applied { resync: Vec::new() }
            }
            Done::No { .. } => Step::No("store"),
            Done::Bad => BAD,
        });
    }
    if action.kind == Kind::DeletePermanently {
        return Ok(match remove(server, &action.uids).await? {
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
        let done = server
            .run_done(&format!("UID MOVE {set} {target_name}"), |response| {
                if let Some(copy) = sync::copy_uid(response) {
                    untagged = Some(copy);
                }
            })
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
    copy_then_remove(server, store, action, &target_name).await
}

/// `UID STORE +FLAGS.SILENT (\Deleted)`, then `UID EXPUNGE` of the same UIDs. An expunge the
/// server refused takes the flag back (best effort), but only from the messages that did not
/// have it before: left there, another client's `EXPUNGE` would remove messages the copy shows,
/// once the action is refused, and a `\Deleted` another client set is not ours to clear. Which
/// ones had it is asked first (`UID SEARCH DELETED UID <set>`); an answer refused means the
/// removal is not started.
pub(crate) async fn remove(
    server: &mut Server<'_>,
    uids: &[u32],
) -> std::result::Result<Done, SyncError> {
    let set = sync::uid_set(uids);
    let Some(marked) = server.search(&format!("DELETED UID {set}")).await? else {
        return Ok(Done::No { try_create: false });
    };
    let done = server
        .run_done(
            &format!("UID STORE {set} +FLAGS.SILENT (\\Deleted)"),
            |_| {},
        )
        .await?;
    if !matches!(done, Done::Ok(_)) {
        return Ok(done);
    }
    let done = server
        .run_done(&format!("UID EXPUNGE {set}"), |_| {})
        .await?;
    let ours: Vec<u32> = uids
        .iter()
        .copied()
        .filter(|uid| !marked.contains(uid))
        .collect();
    if !matches!(done, Done::Ok(_)) && !ours.is_empty() {
        server
            .run_done(
                &format!(
                    "UID STORE {} -FLAGS.SILENT (\\Deleted)",
                    sync::uid_set(&ours)
                ),
                |_| {},
            )
            .await?;
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
        store.copy_state(action.id, "copying", Some(uidnext), None)?;
        match copy(server, &action.uids, target_name).await? {
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
    Ok(match remove(server, &action.uids).await? {
        Done::Ok(_) => Step::Applied {
            resync: store.applied_move(&action, action.copied.as_ref())?,
        },
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
    fn a_refused_move_that_copied_says_the_messages_are_in_both_folders() {
        let Result::Refused { message, .. } =
            refused("refused", "the server said no".to_owned(), true)
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
