//! What the daemon tells its watchers (`docs/events.md` is the documentation of record), and the
//! small queries on the local copy that tell it what to say.
//!
//! The queries live here, apart from [`crate::store`], for the events only. The unread counts are
//! not one of them: they come from [`Copy::unread`], the query `missive unread` answers with.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, params};
use serde::{Deserialize, Serialize};

use crate::copy::{Copy, CopyError, message_id};
use crate::protocol::PROTOCOL;

/// The name an unread count gives the folders that have no role.
const NO_ROLE: &str = "other";

/// One line of the event stream.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub enum Event {
    /// A message arrived in a folder that had been synced before.
    NewMessage(NewMessage),
    /// The unread counts changed.
    Unread(Unread),
    /// An account's connection state changed.
    State { account: String, state: String },
    /// A pass over every folder of an account began.
    SyncStarted { account: String },
    /// A pass over every folder of an account ended, with what it changed.
    SyncEnded {
        account: String,
        folders: usize,
        new: usize,
        removed: usize,
        flags: usize,
    },
    /// The watcher read too slowly: this many events were dropped for it.
    Lagged { missed: u64 },
    /// There is no daemon to listen to. Said by `missive watch`, never by the daemon.
    Gone,
}

#[derive(Serialize)]
struct Line<'a> {
    protocol: u32,
    #[serde(flatten)]
    event: &'a Event,
}

impl Event {
    /// The event as a line of the stream, without its `\n`.
    pub fn line(&self) -> String {
        serde_json::to_string(&Line {
            protocol: PROTOCOL,
            event: self,
        })
        .expect("an event serializes")
    }
}

/// A message that arrived. The sender and the subject are told only here, on the user's own
/// socket: the daemon's log never shows them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewMessage {
    pub account: String,
    /// The folder's role, `None` for a folder of the user's own.
    pub folder: Option<String>,
    /// See [`crate::copy`].
    pub message: String,
    pub sender: Option<Sender>,
    pub subject: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sender {
    pub name: Option<String>,
    pub address: Option<String>,
}

/// The unread counts of the whole copy.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unread {
    /// Everything that is unread but in the junk and trash folders, as in `missive unread`.
    pub total: u64,
    pub accounts: Vec<AccountUnread>,
}

impl Unread {
    /// The counts of the copy, from the query `missive unread` uses. An account the daemon has
    /// not listed the folders of yet is there with a zero and no role.
    pub fn of(copy: &Copy) -> Result<Unread, CopyError> {
        let mut unread = Unread::default();
        for account in copy.accounts()? {
            unread.accounts.push(AccountUnread {
                id: account.id,
                total: 0,
                roles: BTreeMap::new(),
            });
        }
        for folder in copy.unread()? {
            let Some(account) = unread.accounts.iter_mut().find(|a| a.id == folder.account) else {
                continue;
            };
            if folder.counted() {
                account.total += folder.unread;
                unread.total += folder.unread;
            }
            *account
                .roles
                .entry(folder.folder.role.unwrap_or_else(|| NO_ROLE.to_owned()))
                .or_default() += folder.unread;
        }
        Ok(unread)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountUnread {
    pub id: String,
    /// As [`Unread::total`], for this account.
    pub total: u64,
    /// Unread messages by the role of their folder (`other` for the user's own folders), for each
    /// role the account has a folder of.
    pub roles: BTreeMap<String, u64>,
}

/// A read-only connection to the local copy, as `status` or `search` open it: the daemon writes
/// while it reads, since the copy is in WAL mode.
pub struct Reader(Connection);

impl Reader {
    pub fn open(path: &Path) -> rusqlite::Result<Reader> {
        let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        connection.busy_timeout(Duration::from_secs(10))?;
        Ok(Reader(connection))
    }

    /// Every account of the copy and its connection state, by identifier.
    pub fn states(&self) -> rusqlite::Result<Vec<(String, String)>> {
        self.0
            .prepare("SELECT id, state FROM account ORDER BY id")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect()
    }

    /// Whether a complete sync of the account ever ended.
    pub fn completed(&self, account: i64) -> rusqlite::Result<bool> {
        self.0.query_row(
            "SELECT last_sync IS NOT NULL FROM account WHERE key = ?1",
            [account],
            |r| r.get(0),
        )
    }

    /// The highest UID of a folder in the copy, 0 when it holds none.
    pub fn last_uid(&self, folder: i64) -> rusqlite::Result<u32> {
        self.0.query_row(
            "SELECT coalesce(max(uid), 0) FROM message WHERE folder = ?1",
            [folder],
            |r| r.get(0),
        )
    }

    /// The messages of a folder with a UID above `after`, oldest first. A server numbers new
    /// messages above every one it numbered before, so these are what arrived since `after` was
    /// the highest UID.
    pub fn arrived(
        &self,
        account: &str,
        role: Option<&str>,
        folder: i64,
        after: u32,
    ) -> rusqlite::Result<Vec<NewMessage>> {
        self.0
            .prepare(
                "SELECT folder.uidvalidity, message.uid, message.sender, message.subject \
                 FROM message JOIN folder ON folder.id = message.folder \
                 WHERE message.folder = ?1 AND message.uid > ?2 ORDER BY message.uid",
            )?
            .query_map(params![folder, after], |r| {
                let sender: String = r.get(2)?;
                Ok(NewMessage {
                    account: account.to_owned(),
                    folder: role.map(str::to_owned),
                    message: message_id(
                        account,
                        folder,
                        r.get::<_, Option<u32>>(0)?.unwrap_or(0),
                        r.get(1)?,
                    ),
                    sender: serde_json::from_str::<Vec<Sender>>(&sender)
                        .ok()
                        .and_then(|senders| senders.into_iter().next()),
                    subject: r.get(3)?,
                })
            })?
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{NewMessage as Stored, Store};

    fn message(uid: u32, flags: &str) -> Stored {
        let raw =
            format!("From: Carol <carol@example.com>\r\nSubject: Number {uid}\r\n\r\nHello.\r\n")
                .into_bytes();
        Stored {
            uid,
            flags: flags.to_owned(),
            internal_date: None,
            size: raw.len() as i64,
            raw,
        }
    }

    fn copy() -> (tempfile::TempDir, Store, Reader, Copy) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("copy.sqlite");
        let store = Store::open(&path).unwrap();
        let reader = Reader::open(&path).unwrap();
        let copy = Copy::open(&path).unwrap().unwrap();
        (dir, store, reader, copy)
    }

    #[test]
    fn an_event_is_one_line_with_its_protocol_and_its_name() {
        let event = Event::State {
            account: "alice".to_owned(),
            state: "ready".to_owned(),
        };
        assert_eq!(
            event.line(),
            r#"{"protocol":1,"event":"state","account":"alice","state":"ready"}"#
        );
        assert_eq!(Event::Gone.line(), r#"{"protocol":1,"event":"gone"}"#);
        assert_eq!(
            Event::SyncStarted {
                account: "alice".to_owned()
            }
            .line(),
            r#"{"protocol":1,"event":"sync-started","account":"alice"}"#
        );
        assert_eq!(
            Event::Lagged { missed: 3 }.line(),
            r#"{"protocol":1,"event":"lagged","missed":3}"#
        );
    }

    #[test]
    fn a_new_message_event_names_the_message_by_account_folder_validity_and_uid() {
        assert_eq!(message_id("alice", 4, 17, 99), "alice/4/17/99");
        let event = Event::NewMessage(NewMessage {
            account: "alice".to_owned(),
            folder: None,
            message: message_id("alice", 4, 17, 99),
            sender: None,
            subject: None,
        });
        assert_eq!(
            event.line(),
            r#"{"protocol":1,"event":"new-message","account":"alice","folder":null,"message":"alice/4/17/99","sender":null,"subject":null}"#
        );
    }

    #[test]
    fn unread_is_every_message_without_seen_or_deleted_by_account_and_role() {
        let (_dir, mut store, _reader, copy) = copy();
        let alice = store.account("alice").unwrap();
        let bob = store.account("bob").unwrap();
        let inbox = store.folder(alice, "INBOX", Some("inbox")).unwrap().id;
        let junk = store.folder(alice, "Spam", Some("junk")).unwrap().id;
        let own = store.folder(alice, "Mine", None).unwrap().id;
        store.folder(bob, "INBOX", Some("inbox")).unwrap();
        store
            .insert(
                inbox,
                &[
                    message(1, ""),
                    message(2, "\\Seen"),
                    message(3, "\\Flagged"),
                    message(4, "\\Deleted"),
                    message(5, "\\seen \\Flagged"),
                ],
            )
            .unwrap();
        store
            .insert(junk, &[message(1, ""), message(2, "")])
            .unwrap();
        store.insert(own, &[message(1, "$Label1")]).unwrap();

        let unread = Unread::of(&copy).unwrap();

        assert_eq!(unread.total, 3);
        assert_eq!(unread.accounts.len(), 2);
        assert_eq!(unread.accounts[0].id, "alice");
        assert_eq!(unread.accounts[0].total, 3);
        assert_eq!(
            unread.accounts[0].roles,
            BTreeMap::from([
                ("inbox".to_owned(), 2),
                ("junk".to_owned(), 2),
                ("other".to_owned(), 1)
            ])
        );
        assert_eq!(unread.accounts[1].id, "bob");
        assert_eq!(unread.accounts[1].total, 0);
        assert_eq!(
            unread.accounts[1].roles,
            BTreeMap::from([("inbox".to_owned(), 0)])
        );
    }

    #[test]
    fn an_account_without_a_folder_yet_has_no_role_in_its_counts() {
        let (_dir, store, _reader, copy) = copy();
        store.account("alice").unwrap();
        let unread = Unread::of(&copy).unwrap();
        assert_eq!(unread.accounts.len(), 1);
        assert!(unread.accounts[0].roles.is_empty());
        assert_eq!(unread.total, 0);
    }

    #[test]
    fn what_arrived_is_what_is_above_the_highest_uid_known_before() {
        let (_dir, mut store, reader, _copy) = copy();
        let alice = store.account("alice").unwrap();
        let inbox = store.folder(alice, "INBOX", Some("inbox")).unwrap().id;
        store.rebuild(inbox, 17).unwrap();
        assert_eq!(reader.last_uid(inbox).unwrap(), 0);
        store
            .insert(inbox, &[message(3, ""), message(4, "")])
            .unwrap();
        assert_eq!(reader.last_uid(inbox).unwrap(), 4);
        store
            .insert(inbox, &[message(5, ""), message(6, "")])
            .unwrap();

        let arrived = reader.arrived("alice", Some("inbox"), inbox, 4).unwrap();

        assert_eq!(arrived.len(), 2);
        assert_eq!(arrived[0].message, format!("alice/{inbox}/17/5"));
        assert_eq!(arrived[0].subject.as_deref(), Some("Number 5"));
        assert_eq!(arrived[0].folder.as_deref(), Some("inbox"));
        assert_eq!(
            arrived[0].sender,
            Some(Sender {
                name: Some("Carol".to_owned()),
                address: Some("carol@example.com".to_owned())
            })
        );
        assert_eq!(arrived[1].subject.as_deref(), Some("Number 6"));
    }

    #[test]
    fn an_account_has_completed_a_sync_once_its_last_sync_is_recorded() {
        let (_dir, store, reader, _copy) = copy();
        let alice = store.account("alice").unwrap();
        assert!(!reader.completed(alice).unwrap());
        store.set_state(alice, "syncing", false).unwrap();
        assert!(!reader.completed(alice).unwrap());
        store.set_state(alice, "ready", true).unwrap();
        assert!(reader.completed(alice).unwrap());
        assert_eq!(
            reader.states().unwrap(),
            [("alice".to_owned(), "ready".to_owned())]
        );
    }
}
