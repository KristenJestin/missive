//! The queue of actions (schema version 4): what the user asked to change on the server, written
//! with its change to the copy, and what becomes of it once the server answered. See
//! [`crate::actions`].

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use super::Store;
use crate::actions::{Kind, Queued, Refused, Wanted, with_flag};
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
    pub attempts: u32,
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
        if moving.is_some() {
            return Ok(Err(Refused::new(
                "busy",
                format!(
                    "{id}: a move of this message waits for the server; ask again once it is \
                         applied (missive watch tells when)"
                ),
            )));
        }
        if queued.messages.contains(id) {
            continue;
        }
        queued.messages.push(id.clone());
        if !queued.accounts.contains(&parsed.account) {
            queued.accounts.push(parsed.account.clone());
        }
        match groups.iter_mut().find(|g| g.folder == parsed.folder) {
            Some(group) => group.uids.push(parsed.uid),
            None => groups.push(Group {
                account,
                account_id: parsed.account.clone(),
                folder: parsed.folder,
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
                if target == group.folder {
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
        actions.push((&*group, kind, target));
    }

    for (group, kind, target) in actions {
        transaction.execute(
            "INSERT INTO action (account, folder, uidvalidity, uids, kind, target)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                group.account,
                group.folder,
                group.uidvalidity,
                serde_json::to_string(&group.uids).expect("numbers serialize"),
                kind.name(),
                target
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
        self.connection
            .query_row(
                "SELECT action.id, action.kind, source.id, source.name, source.uidvalidity,
                        action.uidvalidity, action.uids, target.id, target.name,
                        target.uidvalidity, action.state, action.target_uidnext, action.copied,
                        action.attempts
                   FROM action
                   JOIN folder AS source ON source.id = action.folder
                   LEFT JOIN folder AS target ON target.id = action.target
                  WHERE action.account = ?1
                  ORDER BY action.id LIMIT 1",
                [account],
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

    /// Records how far a move without MOVE got: see the schema.
    pub fn copy_state(
        &self,
        action: i64,
        state: &str,
        target_uidnext: Option<u32>,
        copied: Option<&CopyUid>,
    ) -> rusqlite::Result<()> {
        self.connection.execute(
            "UPDATE action SET state = ?2, target_uidnext = coalesce(?3, target_uidnext),
                    copied = ?4 WHERE id = ?1",
            params![
                action,
                state,
                target_uidnext,
                copied.map(|c| serde_json::to_string(c).expect("numbers serialize"))
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
    pub fn applied_move(
        &mut self,
        action: &Action,
        copied: Option<&CopyUid>,
    ) -> rusqlite::Result<Vec<i64>> {
        let target = action.target.as_ref().expect("a move has a target");
        let transaction = self.connection.transaction()?;
        transaction.execute("DELETE FROM action WHERE id = ?1", [action.id])?;
        let uidvalidity: Option<u32> = transaction.query_row(
            "SELECT uidvalidity FROM folder WHERE id = ?1",
            [target.id],
            |r| r.get(0),
        )?;
        let copied = copied.filter(|c| Some(c.uidvalidity) == uidvalidity);
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
        transaction.commit()?;
        Ok(if resync { vec![target.id] } else { Vec::new() })
    }

    /// An action refused for good: it leaves the queue and its change leaves the copy. A message
    /// shown in its new folder is shown where the server has it again; flags are read again whole
    /// at the next sync of the folder (its HIGHESTMODSEQ is forgotten); a message deleted for
    /// good comes back with the next sync, which no longer skips it.
    pub fn refused(&mut self, action: &Action) -> rusqlite::Result<()> {
        let transaction = self.connection.transaction()?;
        transaction.execute("DELETE FROM action WHERE id = ?1", [action.id])?;
        if action.kind.flag().is_some() {
            transaction.execute(
                "UPDATE folder SET highest_modseq = NULL WHERE id = ?1",
                [action.folder.id],
            )?;
        } else if action.kind.moves() {
            for uid in &action.uids {
                transaction.execute(
                    "UPDATE message SET moving_to = NULL WHERE folder = ?1 AND uid = ?2",
                    params![action.folder.id, uid],
                )?;
            }
        }
        transaction.commit()
    }
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

        let resync = f
            .store
            .applied_move(
                &moved,
                Some(&CopyUid {
                    uidvalidity: 20,
                    pairs: vec![(2, 41), (3, 42)],
                }),
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

            let resync = f.store.applied_move(&moved, copied.as_ref()).unwrap();

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

        // A message whose move waits takes no other action until the server moved it.
        f.queue(
            std::slice::from_ref(&good),
            Wanted::Move("archive".to_owned()),
        )
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
}
