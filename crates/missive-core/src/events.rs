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
use crate::store::SkippedFolder;

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
        /// The folders the pass left as they were, none counted in `folders`.
        skipped: Vec<SkippedFolder>,
    },
    /// An action was applied on the server, is tried again later, or was refused.
    Action(ActionEvent),
    /// The server took a message of the outbox.
    Sent(Sent),
    /// The server refused a message of the outbox for good: it is not sent again.
    SendFailed(SendFailed),
    /// The connection was lost after a message's end of data, before the server's answer: it
    /// may have been sent, and is not sent again.
    SendUncertain(SendUncertain),
    /// A draft was written to the Drafts folder, removed from it, or is tried again later.
    Draft(DraftEvent),
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

/// What became of an action at the server (`docs/events.md`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionEvent {
    pub account: String,
    /// `read`, `unread`, `flagged`, `unflagged`, `move`, `delete` or `delete-permanently`.
    pub action: String,
    /// `applied`, `retried` or `refused`.
    pub outcome: String,
    /// The identifiers of the messages, as they were when the action was asked.
    pub messages: Vec<String>,
    /// For `retried` and `refused`: why, for programs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// For `retried`: how many times the server refused it so far.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempts: Option<u32>,
    /// For `refused`: why, for people.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// What became of a draft at the server (`docs/events.md`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftEvent {
    pub account: String,
    /// The draft's `Message-ID`, without angle brackets, as `missive compose --json` printed it.
    pub draft: String,
    /// `saved`, `removed` (its message was sent) or `retried`.
    pub outcome: String,
    /// For `saved`: the identifier of the version in the Drafts folder, when its UID is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// For `saved` and `removed`: how many older versions stay in the Drafts folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub left: Option<usize>,
    /// For `retried`: `refused` or `connection`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// For `retried`: how many times it failed so far.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempts: Option<u32>,
    /// Why older versions stay, for people.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// A message the server took (`docs/events.md`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sent {
    pub account: String,
    /// Its number in the outbox, as `missive compose --json` printed it.
    pub outbox: i64,
    /// Its `Message-ID`, without angle brackets.
    pub message_id: String,
    /// What becomes of its copy in Sent: `append` (Missive appends it), `server` (the account
    /// says its server keeps one) or `none` (the account has no Sent folder).
    pub sent_copy: String,
}

/// A message the server refused for good (`docs/events.md`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendFailed {
    pub account: String,
    pub outbox: i64,
    pub message_id: String,
    /// The server's reply code (`5xx`).
    pub code: u16,
    /// `MAIL FROM`, `RCPT TO`, `DATA` or `end of data`.
    pub step: String,
    /// For people.
    pub message: String,
}

/// A message that may or may not have been sent (`docs/events.md`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendUncertain {
    pub account: String,
    pub outbox: i64,
    pub message_id: String,
    /// For people.
    pub message: String,
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

    /// The first `limit` messages of a folder with a UID above `after`, oldest first. A server
    /// numbers new messages above every one it numbered before, so these are what arrived since
    /// `after` was the highest UID.
    pub fn arrived(
        &self,
        account: &str,
        role: Option<&str>,
        folder: i64,
        after: u32,
        limit: usize,
    ) -> rusqlite::Result<Vec<NewMessage>> {
        self.0
            .prepare(
                "SELECT folder.uidvalidity, message.uid, message.sender, message.subject \
                 FROM message JOIN folder ON folder.id = message.folder \
                 WHERE message.folder = ?1 AND message.uid > ?2 ORDER BY message.uid LIMIT ?3",
            )?
            .query_map(params![folder, after, limit as i64], |r| {
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
    fn an_action_event_says_its_outcome_and_only_the_fields_it_has() {
        let applied = Event::Action(ActionEvent {
            account: "alice".to_owned(),
            action: "read".to_owned(),
            outcome: "applied".to_owned(),
            messages: vec!["alice/4/17/99".to_owned()],
            reason: None,
            attempts: None,
            message: None,
        });
        assert_eq!(
            applied.line(),
            r#"{"protocol":1,"event":"action","account":"alice","action":"read","outcome":"applied","messages":["alice/4/17/99"]}"#
        );
        let retried = Event::Action(ActionEvent {
            account: "alice".to_owned(),
            action: "move".to_owned(),
            outcome: "retried".to_owned(),
            messages: vec!["alice/4/17/99".to_owned()],
            reason: Some("refused".to_owned()),
            attempts: Some(2),
            message: None,
        });
        assert_eq!(
            retried.line(),
            r#"{"protocol":1,"event":"action","account":"alice","action":"move","outcome":"retried","messages":["alice/4/17/99"],"reason":"refused","attempts":2}"#
        );
    }

    #[test]
    fn the_send_events_read_as_documented() {
        let sent = Event::Sent(Sent {
            account: "alice".to_owned(),
            outbox: 7,
            message_id: "1f@example.org".to_owned(),
            sent_copy: "append".to_owned(),
        });
        assert_eq!(
            sent.line(),
            r#"{"protocol":1,"event":"sent","account":"alice","outbox":7,"message_id":"1f@example.org","sent_copy":"append"}"#
        );
        let failed = Event::SendFailed(SendFailed {
            account: "alice".to_owned(),
            outbox: 7,
            message_id: "1f@example.org".to_owned(),
            code: 550,
            step: "RCPT TO".to_owned(),
            message: "refused".to_owned(),
        });
        assert_eq!(
            failed.line(),
            r#"{"protocol":1,"event":"send-failed","account":"alice","outbox":7,"message_id":"1f@example.org","code":550,"step":"RCPT TO","message":"refused"}"#
        );
        let uncertain = Event::SendUncertain(SendUncertain {
            account: "alice".to_owned(),
            outbox: 7,
            message_id: "1f@example.org".to_owned(),
            message: "lost".to_owned(),
        });
        assert_eq!(
            uncertain.line(),
            r#"{"protocol":1,"event":"send-uncertain","account":"alice","outbox":7,"message_id":"1f@example.org","message":"lost"}"#
        );
    }

    #[test]
    fn the_draft_event_reads_as_documented() {
        let saved = Event::Draft(DraftEvent {
            account: "alice".to_owned(),
            draft: "1f@example.org".to_owned(),
            outcome: "saved".to_owned(),
            id: Some("alice/3/7/2".to_owned()),
            left: Some(0),
            reason: None,
            attempts: None,
            message: None,
        });
        assert_eq!(
            saved.line(),
            r#"{"protocol":1,"event":"draft","account":"alice","draft":"1f@example.org","outcome":"saved","id":"alice/3/7/2","left":0}"#
        );
        let retried = Event::Draft(DraftEvent {
            account: "alice".to_owned(),
            draft: "1f@example.org".to_owned(),
            outcome: "retried".to_owned(),
            id: None,
            left: None,
            reason: Some("refused".to_owned()),
            attempts: Some(2),
            message: None,
        });
        assert_eq!(
            retried.line(),
            r#"{"protocol":1,"event":"draft","account":"alice","draft":"1f@example.org","outcome":"retried","reason":"refused","attempts":2}"#
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

        let arrived = reader
            .arrived("alice", Some("inbox"), inbox, 4, 20)
            .unwrap();

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
        let first = reader.arrived("alice", Some("inbox"), inbox, 4, 1).unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].subject.as_deref(), Some("Number 5"));
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
