//! The queue of actions (schema version 4): what the user asked to change on the server, written
//! with its change to the copy, and what becomes of it once the server answered. See
//! [`crate::actions`].

use std::collections::BTreeSet;

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use super::{NewMessage, Store};
use crate::actions::{Kind, Queued, Refused, Wanted, unsupported, with_flag};
use crate::copy::{MessageId, NotFound};
use crate::sync::CopyUid;

/// The roles a move can name a folder by.
const ROLES: [&str; 6] = ["inbox", "sent", "drafts", "trash", "junk", "archive"];

/// A folder an action names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionFolder {
    pub id: i64,
    /// Its name on the server.
    pub name: String,
    /// Its UIDVALIDITY in the copy now.
    pub uidvalidity: Option<u32>,
}

/// An action of the queue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Action {
    pub id: i64,
    pub kind: Kind,
    /// Where the server holds the messages.
    pub folder: ActionFolder,
    /// The UIDVALIDITY the UIDs belong to.
    pub uidvalidity: u32,
    /// In ascending order.
    pub uids: Vec<u32>,
    pub target: Option<ActionFolder>,
    /// `pending`, `copying` or `copied`: see the schema.
    pub state: String,
    pub target_uidnext: Option<u32>,
    pub copied: Option<CopyUid>,
    /// Whether `copied` is the server's COPYUID, not what was found again after a lost answer.
    pub copyuid: bool,
    pub attempts: u32,
}

/// What dropping an action left to tell.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Dropped {
    /// A move refused once its messages were copied: they are in both folders.
    pub copied: bool,
    /// The actions asked after it on its messages that cannot apply without it, to refuse too.
    pub behind: Vec<i64>,
}

/// What the copy kept of a message that a copy of it on the server has too: `UID COPY` keeps the
/// raw message, so its size, and the INTERNALDATE (RFC 3501, 6.4.7).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fingerprint {
    /// Without its angle brackets.
    pub message_id: Option<String>,
    pub size: i64,
    pub internal_date: Option<i64>,
}

/// The messages of one folder a request names.
struct Group {
    account: i64,
    account_id: String,
    folder: i64,
    /// The move that waits for these messages, if one does: an action asked after it follows
    /// the messages once it is applied, so it is not mixed with messages that are not moving.
    mover: Option<i64>,
    /// Where they are shown: the target of that move, else their folder.
    shown: i64,
    role: Option<String>,
    uidvalidity: u32,
    uids: Vec<u32>,
    /// The first identifier of the group, for the messages that name one.
    first: String,
}

impl Store {
    /// Records what a request asks for on the messages `ids` name: one action per folder, in the
    /// order of the request, each with its change to the copy, all in one transaction. Nothing is
    /// recorded when one identifier names no message, or when the request cannot apply to one
    /// of them: the answer says why.
    pub fn queue(
        &mut self,
        ids: &[String],
        wanted: &Wanted,
    ) -> rusqlite::Result<Result<Queued, Refused>> {
        let transaction = self.connection.transaction()?;
        let queued = queue_in(&transaction, ids, wanted)?;
        if queued.is_ok() {
            transaction.commit()?;
        }
        Ok(queued)
    }
}

/// [`Store::queue`] inside a transaction the caller commits, when the request was recorded.
pub(super) fn queue_in(
    transaction: &Transaction,
    ids: &[String],
    wanted: &Wanted,
) -> rusqlite::Result<Result<Queued, Refused>> {
    let mut groups: Vec<Group> = Vec::new();
    let mut queued = Queued::default();
    for id in ids {
        let Some(parsed) = MessageId::parse(id) else {
            return Ok(Err(not_found(id, NotFound::Invalid)));
        };
        let folder: Option<(i64, Option<u32>, Option<String>, String)> = transaction
            .query_row(
                "SELECT account.key, folder.uidvalidity, folder.role, folder.name FROM folder
                       JOIN account ON account.key = folder.account
                      WHERE account.id = ?1 AND folder.id = ?2",
                params![parsed.account, parsed.folder],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let Some((account, uidvalidity, role, name)) = folder else {
            return Ok(Err(not_found(id, NotFound::Unknown)));
        };
        if uidvalidity.unwrap_or(0) != parsed.uidvalidity {
            return Ok(Err(not_found(id, NotFound::Renumbered(name))));
        }
        let moving: Option<Option<i64>> = transaction
            .query_row(
                "SELECT moving_to FROM message WHERE folder = ?1 AND uid = ?2",
                params![parsed.folder, parsed.uid],
                |r| r.get(0),
            )
            .optional()?;
        let Some(moving) = moving else {
            return Ok(Err(not_found(id, NotFound::Unknown)));
        };
        let (mover, shown, role) = match moving {
            None => (None, parsed.folder, role),
            Some(target) => {
                // Without UIDPLUS the server does not say where the move takes the message, so
                // what is asked after it could not follow it.
                let uidplus: Option<bool> = transaction.query_row(
                    "SELECT can_uidplus FROM account WHERE key = ?1",
                    [account],
                    |r| r.get(0),
                )?;
                if uidplus == Some(false) {
                    return Ok(Err(Refused::new(
                        "busy",
                        format!(
                            "{id}: a move of this message waits for the server, which will not \
                             tell its new UID (it has no UIDPLUS); ask again once it is applied \
                             (missive watch tells when)"
                        ),
                    )));
                }
                let mover = last_move(transaction, parsed.folder, parsed.uidvalidity, parsed.uid)?;
                let role: Option<String> = transaction.query_row(
                    "SELECT role FROM folder WHERE id = ?1",
                    [target],
                    |r| r.get(0),
                )?;
                (mover, target, role)
            }
        };
        if queued.messages.contains(id) {
            continue;
        }
        queued.messages.push(id.clone());
        if !queued.accounts.contains(&parsed.account) {
            queued.accounts.push(parsed.account.clone());
        }
        match groups
            .iter_mut()
            .find(|g| g.folder == parsed.folder && g.mover == mover)
        {
            Some(group) => group.uids.push(parsed.uid),
            None => groups.push(Group {
                account,
                account_id: parsed.account.clone(),
                folder: parsed.folder,
                mover,
                shown,
                role,
                uidvalidity: parsed.uidvalidity,
                uids: vec![parsed.uid],
                first: id.clone(),
            }),
        }
    }

    // Every group is checked before anything is written.
    let mut actions = Vec::new();
    for group in &mut groups {
        group.uids.sort_unstable();
        let trash = folder_with_role(transaction, group.account, "trash")?;
        let in_trash = group.role.as_deref() == Some("trash");
        let (kind, target) = match wanted {
            Wanted::Mark(kind) => (*kind, None),
            Wanted::Move(to) => {
                let Some(target) = find_folder(transaction, group.account, to)? else {
                    return Ok(Err(Refused::new(
                        "no-such-folder",
                        format!(
                            "the account {} has no folder {to} (a role, or a name as missive \
                                 unread shows it)",
                            group.account_id
                        ),
                    )));
                };
                if target == group.shown {
                    // Already there: nothing to do.
                    continue;
                }
                (Kind::Move, Some(target))
            }
            Wanted::Delete { permanently: false } => {
                if in_trash {
                    return Ok(Err(Refused::new(
                        "in-trash",
                        format!(
                            "{}: already in the trash; missive delete --permanently deletes \
                                 it for good",
                            group.first
                        ),
                    )));
                }
                let Some(trash) = trash else {
                    return Ok(Err(Refused::new(
                        "no-trash",
                        format!(
                            "the account {} has no trash folder to move messages to; missive \
                                 delete --permanently deletes them for good",
                            group.account_id
                        ),
                    )));
                };
                (Kind::Delete, Some(trash))
            }
            Wanted::Delete { permanently: true } => {
                if trash.is_some() && !in_trash {
                    return Ok(Err(Refused::new(
                        "not-in-trash",
                        format!(
                            "{}: not in the trash; missive delete moves it there, and only \
                                 from there does --permanently delete for good",
                            group.first
                        ),
                    )));
                }
                (Kind::DeletePermanently, None)
            }
        };
        let capabilities: (Option<bool>, Option<bool>) = transaction.query_row(
            "SELECT can_move, can_uidplus FROM account WHERE key = ?1",
            [group.account],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if let (Some(moves), Some(uidplus)) = capabilities
            && let Some(why) = unsupported(kind, moves, uidplus)
        {
            return Ok(Err(Refused::new(
                "unsupported",
                format!("{}: {why}", group.first),
            )));
        }
        actions.push((&*group, kind, target));
    }

    for (group, kind, target) in actions {
        let id: i64 = transaction.query_row(
            "UPDATE action_sequence
                SET last = max(last, (SELECT coalesce(max(id), 0) FROM action)) + 1
             RETURNING last",
            [],
            |r| r.get(0),
        )?;
        queued.actions.push(id);
        transaction.execute(
            "INSERT INTO action (id, account, folder, uidvalidity, uids, kind, target)
                 VALUES (?7, ?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                group.account,
                group.folder,
                group.uidvalidity,
                serde_json::to_string(&group.uids).expect("numbers serialize"),
                kind.name(),
                target,
                id
            ],
        )?;
        for uid in &group.uids {
            if let Some((flag, add)) = kind.flag() {
                let flags: String = transaction.query_row(
                    "SELECT flags FROM message WHERE folder = ?1 AND uid = ?2",
                    params![group.folder, uid],
                    |r| r.get(0),
                )?;
                transaction.execute(
                    "UPDATE message SET flags = ?3 WHERE folder = ?1 AND uid = ?2",
                    params![group.folder, uid, with_flag(&flags, flag, add)],
                )?;
            } else if kind.moves() {
                transaction.execute(
                    "UPDATE message SET moving_to = ?3 WHERE folder = ?1 AND uid = ?2",
                    params![group.folder, uid, target],
                )?;
            } else {
                transaction.execute(
                    "DELETE FROM message WHERE folder = ?1 AND uid = ?2",
                    params![group.folder, uid],
                )?;
            }
        }
    }
    Ok(Ok(queued))
}

impl Store {
    /// The account's first action, the one to apply next.
    pub fn next_action(&self, account: i64) -> rusqlite::Result<Option<Action>> {
        self.action_where("action.account = ?1 ORDER BY action.id LIMIT 1", account)
    }

    /// The action of this number, while it is in the queue.
    pub fn action(&self, id: i64) -> rusqlite::Result<Option<Action>> {
        self.action_where("action.id = ?1", id)
    }

    fn action_where(&self, condition: &str, value: i64) -> rusqlite::Result<Option<Action>> {
        self.connection
            .query_row(
                &format!(
                    "SELECT action.id, action.kind, source.id, source.name, source.uidvalidity,
                            action.uidvalidity, action.uids, target.id, target.name,
                            target.uidvalidity, action.state, action.target_uidnext,
                            action.copied, action.attempts, action.copyuid
                       FROM action
                       JOIN folder AS source ON source.id = action.folder
                       LEFT JOIN folder AS target ON target.id = action.target
                      WHERE {condition}"
                ),
                [value],
                |r| {
                    let kind: String = r.get(1)?;
                    let uids: String = r.get(6)?;
                    let target = match r.get::<_, Option<i64>>(7)? {
                        Some(id) => Some(ActionFolder {
                            id,
                            name: r.get(8)?,
                            uidvalidity: r.get(9)?,
                        }),
                        None => None,
                    };
                    let copied: Option<String> = r.get(12)?;
                    Ok(Action {
                        id: r.get(0)?,
                        // Written by this module only.
                        kind: Kind::parse(&kind).expect("a known action"),
                        folder: ActionFolder {
                            id: r.get(2)?,
                            name: r.get(3)?,
                            uidvalidity: r.get(4)?,
                        },
                        uidvalidity: r.get(5)?,
                        uids: serde_json::from_str(&uids).unwrap_or_default(),
                        target,
                        state: r.get(10)?,
                        target_uidnext: r.get(11)?,
                        copied: copied.and_then(|c| serde_json::from_str(&c).ok()),
                        copyuid: r.get(14)?,
                        attempts: r.get(13)?,
                    })
                },
            )
            .optional()
    }

    /// Counts a `NO` to an action, which stays in the queue; returns how many it had.
    pub fn retried(&self, action: i64, why: &str) -> rusqlite::Result<u32> {
        self.connection.query_row(
            "UPDATE action SET attempts = attempts + 1, last_error = ?2 WHERE id = ?1
             RETURNING attempts",
            params![action, why],
            |r| r.get(0),
        )
    }

    /// Records how far a move without MOVE got: see the schema. `copyuid`: whether `copied` is
    /// what the server said.
    pub fn copy_state(
        &self,
        action: i64,
        state: &str,
        target_uidnext: Option<u32>,
        copied: Option<&CopyUid>,
        copyuid: bool,
    ) -> rusqlite::Result<()> {
        self.connection.execute(
            "UPDATE action SET state = ?2, target_uidnext = coalesce(?3, target_uidnext),
                    copied = ?4, copyuid = ?5 WHERE id = ?1",
            params![
                action,
                state,
                target_uidnext,
                copied.map(|c| serde_json::to_string(c).expect("numbers serialize")),
                copyuid && copied.is_some()
            ],
        )?;
        Ok(())
    }

    /// What tells each of a folder's `uids` from another message, in their order (`None` for a
    /// UID the copy does not have).
    pub fn fingerprints(
        &self,
        folder: i64,
        uids: &[u32],
    ) -> rusqlite::Result<Vec<Option<Fingerprint>>> {
        let mut statement = self.connection.prepare_cached(
            "SELECT message_id, size, internal_date FROM message WHERE folder = ?1 AND uid = ?2",
        )?;
        uids.iter()
            .map(|uid| {
                statement
                    .query_row(params![folder, uid], |r| {
                        Ok(Fingerprint {
                            message_id: r.get(0)?,
                            size: r.get(1)?,
                            internal_date: r.get(2)?,
                        })
                    })
                    .optional()
            })
            .collect()
    }

    /// A flag action applied: it leaves the queue, and the flags the server answered are written,
    /// with the actions still waiting on top.
    pub fn applied_flags(
        &mut self,
        action: &Action,
        fetched: &[(u32, String)],
    ) -> rusqlite::Result<()> {
        let transaction = self.connection.transaction()?;
        transaction.execute("DELETE FROM action WHERE id = ?1", [action.id])?;
        let pending = pending_flags(&transaction, action.folder.id)?;
        for (uid, flags) in fetched {
            transaction.execute(
                "UPDATE message SET flags = ?3 WHERE folder = ?1 AND uid = ?2",
                params![action.folder.id, uid, overlay(flags, *uid, &pending)],
            )?;
        }
        transaction.commit()
    }

    /// A permanent delete applied: it leaves the queue; its messages already left the copy.
    pub fn applied_delete(&mut self, action: &Action) -> rusqlite::Result<()> {
        self.connection
            .execute("DELETE FROM action WHERE id = ?1", [action.id])?;
        Ok(())
    }

    /// A move applied: it leaves the queue, and each message's row goes to its new UID in the
    /// target when `copied` gives it (and the target in the copy has the UIDVALIDITY it names).
    /// Else the row leaves the copy, and the target is returned, to be synced now: its sync
    /// brings the message under its new UID.
    ///
    /// The actions asked after it on its messages follow them to their new UIDs, but only when
    /// `copyuid` says the server gave them (COPYUID) for exactly those messages: a UID found
    /// again by other means could name another message, which a delete would remove. A message
    /// the server did not move (it no longer had it) leaves them. Returns the folders to sync,
    /// and the actions that could not follow, to refuse.
    pub fn applied_move(
        &mut self,
        action: &Action,
        copied: Option<&CopyUid>,
        copyuid: bool,
    ) -> rusqlite::Result<(Vec<i64>, Vec<i64>)> {
        let target = action.target.as_ref().expect("a move has a target");
        let transaction = self.connection.transaction()?;
        transaction.execute("DELETE FROM action WHERE id = ?1", [action.id])?;
        let uidvalidity: Option<u32> = transaction.query_row(
            "SELECT uidvalidity FROM folder WHERE id = ?1",
            [target.id],
            |r| r.get(0),
        )?;
        let source_uidvalidity: Option<u32> = transaction.query_row(
            "SELECT uidvalidity FROM folder WHERE id = ?1",
            [action.folder.id],
            |r| r.get(0),
        )?;
        let copied = copied
            .filter(|c| Some(c.uidvalidity) == uidvalidity)
            .filter(|_| source_uidvalidity == Some(action.uidvalidity));
        let mut resync = false;
        for uid in &action.uids {
            let new = copied.and_then(|c| c.pairs.iter().find(|(old, _)| old == uid).map(|p| p.1));
            let taken = match new {
                Some(new) => transaction
                    .query_row(
                        "SELECT 1 FROM message WHERE folder = ?1 AND uid = ?2",
                        params![target.id, new],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some(),
                None => true,
            };
            if let (Some(new), false) = (new, taken) {
                transaction.execute(
                    "UPDATE message SET folder = ?3, uid = ?4, moving_to = NULL
                      WHERE folder = ?1 AND uid = ?2",
                    params![action.folder.id, uid, target.id, new],
                )?;
            } else {
                transaction.execute(
                    "DELETE FROM message WHERE folder = ?1 AND uid = ?2",
                    params![action.folder.id, uid],
                )?;
                resync = true;
            }
        }
        let mut behind = Vec::new();
        for (id, _, uids) in later(&transaction, action)? {
            // Asked on messages that were all moving with this one (see `queue_in`).
            let new: BTreeSet<u32> = match copied.filter(|_| copyuid) {
                Some(copied) => uids
                    .iter()
                    .filter_map(|uid| copied.pairs.iter().find(|(old, _)| old == uid))
                    .map(|(_, new)| *new)
                    .collect(),
                None => BTreeSet::new(),
            };
            if new.is_empty() {
                behind.push(id);
                continue;
            }
            let new: Vec<u32> = new.into_iter().collect();
            transaction.execute(
                "UPDATE action SET folder = ?2, uidvalidity = ?3, uids = ?4 WHERE id = ?1",
                params![
                    id,
                    target.id,
                    copied.map(|c| c.uidvalidity),
                    serde_json::to_string(&new).expect("numbers serialize")
                ],
            )?;
            for uid in new {
                moving_again(&transaction, target.id, uid)?;
            }
        }
        transaction.commit()?;
        Ok((if resync { vec![target.id] } else { Vec::new() }, behind))
    }

    /// A move whose messages are where it takes them already (a move back, asked while the
    /// move before it waited, which the server then refused): it leaves the queue, and nothing
    /// is sent.
    pub fn settled(&mut self, action: &Action) -> rusqlite::Result<()> {
        let transaction = self.connection.transaction()?;
        transaction.execute("DELETE FROM action WHERE id = ?1", [action.id])?;
        for uid in &action.uids {
            moving_again(&transaction, action.folder.id, *uid)?;
        }
        transaction.commit()
    }

    /// An action refused for good: it leaves the queue and its change leaves the copy. A message
    /// shown in its new folder is shown where the server has it again; flags are read again whole
    /// at the next sync of the folder (its HIGHESTMODSEQ is forgotten); a message deleted for
    /// good comes back with the next sync, which no longer skips it.
    ///
    /// A move refused once the server had copied its messages (without MOVE) leaves them in both
    /// folders: each is recorded as half moved, and its copy written to the target in the copy
    /// when its new UID is known.
    ///
    /// A permanent delete asked after a move on its messages is refused with it: it was asked of
    /// a message shown in the trash, which the server never moved there.
    pub fn refused(&mut self, action: &Action) -> rusqlite::Result<Dropped> {
        let transaction = self.connection.transaction()?;
        let row: Option<(i64, String, Option<String>)> = transaction
            .query_row(
                "DELETE FROM action WHERE id = ?1 RETURNING account, state, copied",
                [action.id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let mut dropped = Dropped::default();
        if action.kind.flag().is_some() {
            transaction.execute(
                "UPDATE folder SET highest_modseq = NULL WHERE id = ?1",
                [action.folder.id],
            )?;
        } else if action.kind.moves() {
            for uid in &action.uids {
                moving_again(&transaction, action.folder.id, *uid)?;
            }
            dropped.behind = later(&transaction, action)?
                .into_iter()
                .filter(|(_, kind, _)| *kind == Kind::DeletePermanently)
                .map(|(id, _, _)| id)
                .collect();
            if let Some((account, state, copied)) = row
                && state == "copied"
            {
                let copied: Option<CopyUid> = copied.and_then(|c| serde_json::from_str(&c).ok());
                half_move(&transaction, account, action, copied.as_ref())?;
                dropped.copied = true;
            }
        }
        transaction.commit()?;
        Ok(dropped)
    }
}

/// Whether the half move `half_move` names still has its message in both folders, as far as the
/// copy knows: once the user removed one of them, it is settled.
pub(crate) const HALF_MOVE_LEFT: &str = "
    EXISTS (SELECT 1 FROM message JOIN folder ON folder.id = message.folder
             WHERE message.folder = half_move.folder AND message.uid = half_move.uid
               AND folder.uidvalidity = half_move.uidvalidity)
    AND (half_move.target_uid IS NULL
         OR EXISTS (SELECT 1 FROM message JOIN folder ON folder.id = message.folder
                     WHERE message.folder = half_move.target AND message.uid = half_move.target_uid
                       AND folder.uidvalidity = half_move.target_uidvalidity))";

/// Records the messages of a move refused after their copy as half moved, and writes each copy
/// whose UID `copied` gives to the target in the copy (unless the target has another
/// UIDVALIDITY there, or a message at that UID already, or the source folder has another
/// UIDVALIDITY than the action's: its UIDs then name other messages, and the next sync of the
/// target finds the copy).
fn half_move(
    transaction: &Transaction,
    account: i64,
    action: &Action,
    copied: Option<&CopyUid>,
) -> rusqlite::Result<()> {
    let target = action.target.as_ref().expect("a move has a target");
    let uidvalidity: Option<u32> = transaction.query_row(
        "SELECT uidvalidity FROM folder WHERE id = ?1",
        [target.id],
        |r| r.get(0),
    )?;
    let source_uidvalidity: Option<u32> = transaction.query_row(
        "SELECT uidvalidity FROM folder WHERE id = ?1",
        [action.folder.id],
        |r| r.get(0),
    )?;
    let copied = copied
        .filter(|c| Some(c.uidvalidity) == uidvalidity)
        .filter(|_| source_uidvalidity == Some(action.uidvalidity));
    // Those the user settled since are let go.
    transaction.execute(
        &format!("DELETE FROM half_move WHERE account = ?1 AND NOT ({HALF_MOVE_LEFT})"),
        [account],
    )?;
    for uid in &action.uids {
        let new = copied.and_then(|c| c.pairs.iter().find(|(old, _)| old == uid).map(|p| p.1));
        if let Some(new) = new {
            let source: Option<NewMessage> = transaction
                .query_row(
                    "SELECT message.flags, message.internal_date, message.size, message_raw.raw
                       FROM message JOIN message_raw ON message_raw.message = message.id
                      WHERE message.folder = ?1 AND message.uid = ?2",
                    params![action.folder.id, uid],
                    |r| {
                        Ok(NewMessage {
                            uid: new,
                            flags: r.get(0)?,
                            internal_date: r.get(1)?,
                            size: r.get(2)?,
                            raw: r.get(3)?,
                        })
                    },
                )
                .optional()?;
            let taken = transaction
                .query_row(
                    "SELECT 1 FROM message WHERE folder = ?1 AND uid = ?2",
                    params![target.id, new],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if let (Some(source), false) = (source, taken) {
                super::insert(transaction, target.id, &source)?;
            }
        }
        transaction.execute(
            "INSERT INTO half_move (account, folder, uidvalidity, uid, target, target_uidvalidity,
                                    target_uid)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                account,
                action.folder.id,
                action.uidvalidity,
                uid,
                target.id,
                new.and(copied.map(|c| c.uidvalidity)),
                new
            ],
        )?;
    }
    Ok(())
}

/// The latest move or delete waiting for a message, which an action asked on it now comes after.
fn last_move(
    connection: &Connection,
    folder: i64,
    uidvalidity: u32,
    uid: u32,
) -> rusqlite::Result<Option<i64>> {
    connection
        .query_row(
            "SELECT action.id FROM action, json_each(action.uids)
              WHERE action.folder = ?1 AND action.uidvalidity = ?2
                AND action.kind IN ('move', 'delete') AND json_each.value = ?3
              ORDER BY action.id DESC LIMIT 1",
            params![folder, uidvalidity, uid],
            |r| r.get(0),
        )
        .optional()
}

/// Shows a message in the target of the latest move still waiting for it, or in its folder.
fn moving_again(connection: &Connection, folder: i64, uid: u32) -> rusqlite::Result<()> {
    connection.execute(
        "UPDATE message SET moving_to = (
             SELECT action.target FROM action JOIN folder ON folder.id = action.folder,
                    json_each(action.uids)
              WHERE action.folder = ?1 AND action.uidvalidity = folder.uidvalidity
                AND action.kind IN ('move', 'delete') AND json_each.value = ?2
              ORDER BY action.id DESC LIMIT 1)
          WHERE folder = ?1 AND uid = ?2",
        params![folder, uid],
    )?;
    Ok(())
}

/// The actions asked after `action` on some of its messages, with their kind and UIDs, in order.
fn later(connection: &Connection, action: &Action) -> rusqlite::Result<Vec<(i64, Kind, Vec<u32>)>> {
    let mut statement = connection.prepare_cached(
        "SELECT id, kind, uids FROM action
          WHERE folder = ?1 AND uidvalidity = ?2 AND id > ?3 ORDER BY id",
    )?;
    let rows = statement.query_map(
        params![action.folder.id, action.uidvalidity, action.id],
        |r| {
            let kind: String = r.get(1)?;
            let uids: String = r.get(2)?;
            Ok((
                r.get(0)?,
                Kind::parse(&kind).expect("a known action"),
                serde_json::from_str(&uids).unwrap_or_default(),
            ))
        },
    )?;
    let mut later = Vec::new();
    for row in rows {
        let (id, kind, uids): (i64, Kind, Vec<u32>) = row?;
        if uids.iter().any(|uid| action.uids.contains(uid)) {
            later.push((id, kind, uids));
        }
    }
    Ok(later)
}

fn not_found(id: &str, why: NotFound) -> Refused {
    Refused::new("no-such-message", format!("{id}: {why}"))
}

/// The account's first folder with this role.
fn folder_with_role(
    connection: &Connection,
    account: i64,
    role: &str,
) -> rusqlite::Result<Option<i64>> {
    connection
        .query_row(
            "SELECT id FROM folder WHERE account = ?1 AND role = ?2 ORDER BY id LIMIT 1",
            params![account, role],
            |r| r.get(0),
        )
        .optional()
}

/// The folder a move names: by its role, else by its name.
fn find_folder(connection: &Connection, account: i64, to: &str) -> rusqlite::Result<Option<i64>> {
    if ROLES.contains(&to)
        && let Some(id) = folder_with_role(connection, account, to)?
    {
        return Ok(Some(id));
    }
    connection
        .query_row(
            "SELECT id FROM folder WHERE account = ?1 AND name = ?2",
            params![account, to],
            |r| r.get(0),
        )
        .optional()
}

/// The flag actions waiting on a folder, in order, for the UIDVALIDITY it has now: once it has
/// another, their UIDs name other messages.
pub(super) fn pending_flags(
    connection: &Connection,
    folder: i64,
) -> rusqlite::Result<Vec<(Kind, Vec<u32>)>> {
    let mut statement = connection.prepare_cached(
        "SELECT action.kind, action.uids FROM action
           JOIN folder ON folder.id = action.folder
          WHERE action.folder = ?1 AND action.uidvalidity = folder.uidvalidity
            AND action.kind IN ('read', 'unread', 'flagged', 'unflagged', 'answered')
          ORDER BY action.id",
    )?;
    let rows = statement.query_map([folder], |r| {
        let kind: String = r.get(0)?;
        let uids: String = r.get(1)?;
        Ok((
            Kind::parse(&kind).expect("a known action"),
            serde_json::from_str(&uids).unwrap_or_default(),
        ))
    })?;
    rows.collect()
}

/// The flags the server gave a message, with the waiting actions on top.
pub(super) fn overlay(flags: &str, uid: u32, pending: &[(Kind, Vec<u32>)]) -> String {
    let mut flags = flags.to_owned();
    for (kind, uids) in pending {
        if let Some((flag, add)) = kind.flag()
            && uids.contains(&uid)
        {
            flags = with_flag(&flags, flag, add);
        }
    }
    flags
}

/// Whether a permanent delete of the message waits: the sync must not bring it back.
pub(super) fn deleting(transaction: &Transaction, folder: i64, uid: u32) -> rusqlite::Result<bool> {
    transaction
        .prepare_cached(
            "SELECT EXISTS (SELECT 1 FROM action JOIN folder ON folder.id = action.folder,
                                   json_each(action.uids)
                             WHERE action.folder = ?1 AND action.uidvalidity = folder.uidvalidity
                               AND action.kind = 'delete-permanently'
                               AND json_each.value = ?2)",
        )?
        .query_row(params![folder, uid], |r| r.get(0))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::copy::{Copy, List, message_id};
    use crate::store::NewMessage;

    fn message(uid: u32, flags: &str) -> NewMessage {
        let raw = format!(
            "From: Carol <carol@example.com>\r\nSubject: Number {uid}\r\n\
             Message-ID: <{uid}@example.com>\r\n\r\nHello.\r\n"
        )
        .into_bytes();
        NewMessage {
            uid,
            flags: flags.to_owned(),
            internal_date: Some(1_790_000_000),
            size: raw.len() as i64,
            raw,
        }
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        store: Store,
        copy: Copy,
        inbox: i64,
        archive: i64,
        trash: i64,
    }

    /// Alice's inbox (UIDVALIDITY 10) holds UIDs 1 (seen), 2 and 3; her archive (20) and trash
    /// (30) are empty.
    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("copy.sqlite");
        let mut store = Store::open(&path).unwrap();
        let alice = store.account("alice").unwrap();
        let inbox = store.folder(alice, "INBOX", Some("inbox")).unwrap().id;
        let archive = store.folder(alice, "Archive", Some("archive")).unwrap().id;
        let trash = store.folder(alice, "Trash", Some("trash")).unwrap().id;
        store.rebuild(inbox, 10).unwrap();
        store.rebuild(archive, 20).unwrap();
        store.rebuild(trash, 30).unwrap();
        store
            .insert(
                inbox,
                &[message(1, "\\Seen"), message(2, ""), message(3, "")],
            )
            .unwrap();
        let copy = Copy::open(&path).unwrap().unwrap();
        Fixture {
            _dir: dir,
            store,
            copy,
            inbox,
            archive,
            trash,
        }
    }

    impl Fixture {
        fn id(&self, folder: i64, uid: u32) -> String {
            let uidvalidity = [(self.inbox, 10), (self.archive, 20), (self.trash, 30)]
                .into_iter()
                .find(|(f, _)| *f == folder)
                .unwrap()
                .1;
            message_id("alice", folder, uidvalidity, uid)
        }

        fn queue(&mut self, ids: &[String], wanted: Wanted) -> Result<Queued, Refused> {
            self.store.queue(ids, &wanted).unwrap()
        }

        fn flags(&self, folder: i64) -> HashMap<u32, String> {
            self.store.flags(folder).unwrap()
        }

        fn actions(&self) -> i64 {
            self.store
                .connection
                .query_row("SELECT count(*) FROM action", [], |r| r.get(0))
                .unwrap()
        }

        /// `(folder shown, id)` of every message, as `missive list` gives them.
        fn listed(&self) -> Vec<(String, String)> {
            let mut listed: Vec<(String, String)> = self
                .copy
                .list(&List {
                    accounts: vec!["alice".to_owned()],
                    role: None,
                    name: None,
                    unread: false,
                    limit: 100,
                })
                .unwrap()
                .into_iter()
                .map(|m| (m.hit.folder.name, m.hit.id))
                .collect();
            listed.sort();
            listed
        }

        fn next(&self) -> Action {
            self.store.next_action(1).unwrap().unwrap()
        }
    }

    #[test]
    fn marking_read_changes_the_copy_at_once_and_queues_one_action_per_folder() {
        let mut f = fixture();
        f.store.insert(f.archive, &[message(7, "")]).unwrap();
        let ids = [f.id(f.inbox, 2), f.id(f.archive, 7), f.id(f.inbox, 3)];

        let queued = f.queue(&ids, Wanted::Mark(Kind::Read)).unwrap();

        assert_eq!(queued.messages, ids);
        assert_eq!(queued.accounts, ["alice"]);
        assert_eq!(f.flags(f.inbox)[&2], "\\Seen");
        assert_eq!(f.flags(f.inbox)[&3], "\\Seen");
        assert_eq!(f.flags(f.archive)[&7], "\\Seen");
        assert_eq!(f.actions(), 2);
        let first = f.next();
        assert_eq!(
            (first.kind, first.folder.id, first.uidvalidity, first.uids),
            (Kind::Read, f.inbox, 10, vec![2, 3])
        );
    }

    #[test]
    fn a_sync_while_a_mark_waits_does_not_undo_it() {
        let mut f = fixture();
        let ids = [f.id(f.inbox, 2)];
        f.queue(&ids, Wanted::Mark(Kind::Read)).unwrap();
        f.queue(&ids, Wanted::Mark(Kind::Flagged)).unwrap();

        // The server still lists 2 unread; another client flagged 3 meanwhile.
        f.store
            .apply_listing(
                f.inbox,
                &[],
                &[(2, String::new()), (3, "\\Answered".to_owned())],
                Some(9),
            )
            .unwrap();

        assert_eq!(f.flags(f.inbox)[&2], "\\Flagged \\Seen");
        assert_eq!(f.flags(f.inbox)[&3], "\\Answered");
    }

    #[test]
    fn once_applied_the_flags_the_server_answered_are_written_with_the_later_actions_on_top() {
        let mut f = fixture();
        let ids = [f.id(f.inbox, 2)];
        f.queue(&ids, Wanted::Mark(Kind::Read)).unwrap();
        f.queue(&ids, Wanted::Mark(Kind::Flagged)).unwrap();
        let read = f.next();

        f.store
            .applied_flags(&read, &[(2, "$Label \\Seen".to_owned())])
            .unwrap();

        assert_eq!(f.flags(f.inbox)[&2], "$Label \\Flagged \\Seen");
        assert_eq!(f.next().kind, Kind::Flagged);
    }

    #[test]
    fn a_refused_mark_leaves_the_queue_and_its_folder_is_read_again_whole() {
        let mut f = fixture();
        f.store.apply_listing(f.inbox, &[], &[], Some(9)).unwrap();
        f.queue(&[f.id(f.inbox, 2)], Wanted::Mark(Kind::Read))
            .unwrap();
        let read = f.next();

        f.store.refused(&read).unwrap();

        assert_eq!(f.actions(), 0);
        let row = f.store.folder(1, "INBOX", Some("inbox")).unwrap();
        assert_eq!(row.highest_modseq, None);
        // The next listing gives the server's flags, with nothing on top any more.
        f.store
            .apply_listing(f.inbox, &[], &[(2, String::new())], Some(9))
            .unwrap();
        assert_eq!(f.flags(f.inbox)[&2], "");
    }

    #[test]
    fn a_waiting_move_shows_the_message_in_its_target_under_the_identifier_of_its_source() {
        let mut f = fixture();
        let id = f.id(f.inbox, 2);

        f.queue(
            std::slice::from_ref(&id),
            Wanted::Move("archive".to_owned()),
        )
        .unwrap();

        assert_eq!(
            f.listed(),
            [
                ("Archive".to_owned(), id.clone()),
                ("INBOX".to_owned(), f.id(f.inbox, 1)),
                ("INBOX".to_owned(), f.id(f.inbox, 3)),
            ]
        );
        let shown = f.copy.message(&id).unwrap().unwrap();
        assert_eq!(shown.hit.folder.name, "Archive");
        // The unread counts follow it.
        let unread: Vec<(String, u64)> = f
            .copy
            .unread()
            .unwrap()
            .into_iter()
            .map(|u| (u.folder.name, u.unread))
            .collect();
        assert_eq!(
            unread,
            [
                ("INBOX".to_owned(), 1),
                ("Archive".to_owned(), 1),
                ("Trash".to_owned(), 0)
            ]
        );
        // The server still lists it in the inbox: a sync keeps it where it is shown.
        f.store
            .apply_listing(f.inbox, &[], &[(2, "\\Flagged".to_owned())], None)
            .unwrap();
        f.store.insert(f.inbox, &[message(2, "")]).unwrap();
        assert_eq!(f.listed()[0], ("Archive".to_owned(), id));
        assert_eq!(f.flags(f.inbox).len(), 3);
    }

    #[test]
    fn an_applied_move_takes_the_row_to_its_new_uid_without_fetching_it_again() {
        let mut f = fixture();
        f.queue(
            &[f.id(f.inbox, 2), f.id(f.inbox, 3)],
            Wanted::Move("Archive".to_owned()),
        )
        .unwrap();
        let moved = f.next();

        let (resync, _) = f
            .store
            .applied_move(
                &moved,
                Some(&CopyUid {
                    uidvalidity: 20,
                    pairs: vec![(2, 41), (3, 42)],
                }),
                true,
            )
            .unwrap();

        assert!(resync.is_empty());
        assert_eq!(f.actions(), 0);
        assert_eq!(
            f.listed(),
            [
                ("Archive".to_owned(), f.id(f.archive, 41)),
                ("Archive".to_owned(), f.id(f.archive, 42)),
                ("INBOX".to_owned(), f.id(f.inbox, 1)),
            ]
        );
        let raw = f.copy.message(&f.id(f.archive, 41)).unwrap().unwrap().raw;
        assert_eq!(raw, message(2, "").raw);
    }

    #[test]
    fn an_applied_move_without_its_new_uids_leaves_the_copy_and_asks_for_the_target_to_be_synced() {
        for copied in [
            None,
            // A COPYUID of another UIDVALIDITY than the copy knows the target by.
            Some(CopyUid {
                uidvalidity: 99,
                pairs: vec![(2, 41)],
            }),
        ] {
            let mut f = fixture();
            f.queue(&[f.id(f.inbox, 2)], Wanted::Move("archive".to_owned()))
                .unwrap();
            let moved = f.next();

            let (resync, _) = f.store.applied_move(&moved, copied.as_ref(), true).unwrap();

            assert_eq!(resync, [f.archive]);
            assert_eq!(f.listed().len(), 2);
            assert!(f.flags(f.archive).is_empty());
        }
    }

    #[test]
    fn a_refused_move_shows_the_message_where_the_server_has_it_again() {
        let mut f = fixture();
        let id = f.id(f.inbox, 2);
        f.queue(
            std::slice::from_ref(&id),
            Wanted::Delete { permanently: false },
        )
        .unwrap();
        assert_eq!(f.listed()[2], ("Trash".to_owned(), id.clone()));
        let moved = f.next();
        assert_eq!(moved.kind, Kind::Delete);
        assert_eq!(moved.target.as_ref().unwrap().id, f.trash);

        f.store.refused(&moved).unwrap();

        assert_eq!(f.listed()[1], ("INBOX".to_owned(), id));
        assert_eq!(f.actions(), 0);
    }

    #[test]
    fn a_message_deleted_for_good_is_not_fetched_again_while_the_delete_waits() {
        let mut f = fixture();
        f.store
            .insert(f.trash, &[message(5, ""), message(6, "")])
            .unwrap();
        f.queue(&[f.id(f.trash, 5)], Wanted::Delete { permanently: true })
            .unwrap();
        assert_eq!(f.flags(f.trash).into_keys().collect::<Vec<_>>(), [6]);

        // The server still has it.
        f.store.insert(f.trash, &[message(5, "")]).unwrap();
        assert_eq!(f.flags(f.trash).into_keys().collect::<Vec<_>>(), [6]);

        // Refused: the next sync brings it back.
        let deleted = f.next();
        f.store.refused(&deleted).unwrap();
        f.store.insert(f.trash, &[message(5, "")]).unwrap();
        assert_eq!(f.flags(f.trash).len(), 2);
    }

    #[test]
    fn a_folder_renumbered_while_an_action_waits_drops_its_effect_on_the_new_uids() {
        let mut f = fixture();
        f.queue(&[f.id(f.inbox, 2)], Wanted::Mark(Kind::Read))
            .unwrap();
        f.store.rebuild(f.inbox, 11).unwrap();
        f.store.insert(f.inbox, &[message(2, "")]).unwrap();
        f.store
            .apply_listing(f.inbox, &[], &[(2, String::new())], None)
            .unwrap();
        assert_eq!(f.flags(f.inbox)[&2], "");
    }

    #[test]
    fn what_cannot_apply_is_refused_and_nothing_is_recorded() {
        let mut f = fixture();
        f.store.insert(f.trash, &[message(5, "")]).unwrap();
        let good = f.id(f.inbox, 2);
        let cases = [
            (
                vec![good.clone(), "alice/1".to_owned()],
                Wanted::Mark(Kind::Read),
                "no-such-message",
            ),
            (
                vec![good.clone(), f.id(f.inbox, 9)],
                Wanted::Mark(Kind::Read),
                "no-such-message",
            ),
            (
                vec![message_id("alice", f.inbox, 11, 2)],
                Wanted::Mark(Kind::Read),
                "no-such-message",
            ),
            (
                vec![good.clone()],
                Wanted::Move("Nowhere".to_owned()),
                "no-such-folder",
            ),
            (
                vec![good.clone()],
                Wanted::Delete { permanently: true },
                "not-in-trash",
            ),
            (
                vec![good.clone(), f.id(f.trash, 5)],
                Wanted::Delete { permanently: false },
                "in-trash",
            ),
        ];
        for (ids, wanted, error) in cases {
            let refused = f.queue(&ids, wanted.clone()).unwrap_err();
            assert_eq!(
                refused.error, error,
                "{ids:?} {wanted:?}: {}",
                refused.message
            );
        }
        assert_eq!(f.actions(), 0);
        assert_eq!(f.flags(f.inbox)[&2], "");

        // A message whose move waits takes no other action until the server moved it, when the
        // server is known not to tell where it moved it.
        f.queue(
            std::slice::from_ref(&good),
            Wanted::Move("archive".to_owned()),
        )
        .unwrap();
        f.store
            .connection
            .execute("UPDATE account SET can_move = 1, can_uidplus = 0", [])
            .unwrap();
        let refused = f
            .queue(std::slice::from_ref(&good), Wanted::Mark(Kind::Read))
            .unwrap_err();
        assert_eq!(refused.error, "busy");
        assert_eq!(f.actions(), 1);
    }

    #[test]
    fn without_a_trash_delete_is_refused_and_delete_permanently_is_accepted_anywhere() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("copy.sqlite")).unwrap();
        let bob = store.account("bob").unwrap();
        let inbox = store.folder(bob, "INBOX", Some("inbox")).unwrap().id;
        store.rebuild(inbox, 5).unwrap();
        store
            .insert(inbox, &[message(1, ""), message(2, "")])
            .unwrap();
        let id = |uid| message_id("bob", inbox, 5, uid);

        let refused = store
            .queue(&[id(1)], &Wanted::Delete { permanently: false })
            .unwrap()
            .unwrap_err();
        assert_eq!(refused.error, "no-trash");
        assert!(
            refused.message.contains("--permanently"),
            "{}",
            refused.message
        );

        store
            .queue(&[id(1)], &Wanted::Delete { permanently: true })
            .unwrap()
            .unwrap();
        assert_eq!(
            store.flags(inbox).unwrap().into_keys().collect::<Vec<_>>(),
            [2]
        );
    }

    #[test]
    fn a_move_to_the_folder_a_message_is_in_records_nothing() {
        let mut f = fixture();
        let queued = f
            .queue(&[f.id(f.inbox, 2)], Wanted::Move("inbox".to_owned()))
            .unwrap();
        assert_eq!(queued.messages.len(), 1);
        assert_eq!(f.actions(), 0);
    }

    #[test]
    fn a_move_refused_after_its_copy_is_half_moved_until_one_of_the_two_is_gone() {
        let mut f = fixture();
        f.queue(
            &[f.id(f.inbox, 2), f.id(f.inbox, 3)],
            Wanted::Move("archive".to_owned()),
        )
        .unwrap();
        let moved = f.next();
        // The COPY told the new UID of 2 only.
        f.store
            .copy_state(
                moved.id,
                "copied",
                None,
                Some(&CopyUid {
                    uidvalidity: 20,
                    pairs: vec![(2, 41)],
                }),
                true,
            )
            .unwrap();

        assert!(f.store.refused(&moved).unwrap().copied);

        let half = |f: &Fixture| -> Vec<(String, Option<String>)> {
            f.copy
                .half_moved("alice")
                .unwrap()
                .into_iter()
                .map(|h| {
                    assert_eq!((h.folder.as_str(), h.target.as_str()), ("inbox", "archive"));
                    (h.message, h.copy)
                })
                .collect()
        };
        assert_eq!(
            half(&f),
            [
                (f.id(f.inbox, 2), Some(f.id(f.archive, 41))),
                (f.id(f.inbox, 3), None),
            ]
        );
        // Shown where the server has them: both in the inbox, and the copy of 2 in the archive.
        assert_eq!(
            f.listed(),
            [
                ("Archive".to_owned(), f.id(f.archive, 41)),
                ("INBOX".to_owned(), f.id(f.inbox, 1)),
                ("INBOX".to_owned(), f.id(f.inbox, 2)),
                ("INBOX".to_owned(), f.id(f.inbox, 3)),
            ]
        );

        // The copy of 2 removed, and 3 removed from the inbox: nothing is left half moved.
        f.store.apply_listing(f.archive, &[41], &[], None).unwrap();
        f.store.apply_listing(f.inbox, &[3], &[], None).unwrap();
        assert!(half(&f).is_empty());
    }

    #[test]
    fn a_move_refused_before_its_copy_is_not_half_moved() {
        let mut f = fixture();
        f.queue(&[f.id(f.inbox, 2)], Wanted::Move("archive".to_owned()))
            .unwrap();
        let moved = f.next();
        assert!(!f.store.refused(&moved).unwrap().copied);
        assert!(f.copy.half_moved("alice").unwrap().is_empty());
    }
}
