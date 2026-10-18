//! Reading the local copy: what `missive status`, `unread` and `search` answer from.
//!
//! The copy is opened read only and never created: a reader that finds none says so. It touches
//! neither the network nor the keyring, and works while the daemon is down or writing (the copy
//! is in WAL mode, so a read does not wait for a write).
//!
//! A message is named by its [`Hit::id`], `<account>/<folder id>/<uidvalidity>/<uid>`: the account
//! as the configuration names it (an identifier has no `/`), the folder's number in the copy, the
//! UIDVALIDITY its UIDs belong to, and the UID. The row number of a message in the copy would be
//! shorter, but it is lost when a folder is rebuilt after the server renumbered it; with this
//! one, a later `open` can tell "that message is gone" from "that is another message now" by
//! comparing the UIDVALIDITY.

use std::path::Path;
use std::time::Duration;

use rusqlite::types::Value;
use rusqlite::{Connection, OpenFlags, params};
use serde::{Deserialize, Serialize};

use crate::store::SCHEMA_VERSION;

/// The first schema version that has the search index.
const SEARCH_VERSION: i64 = 2;

/// Why the copy could not be read.
#[derive(Debug)]
pub enum CopyError {
    Sqlite(rusqlite::Error),
    /// The copy was written by a later Missive than this one.
    Later(i64),
    /// The copy is older than the search index: the daemon adds it the next time it starts.
    NotIndexed,
}

impl std::fmt::Display for CopyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CopyError::Sqlite(error) => write!(f, "the local copy could not be read ({error})"),
            CopyError::Later(version) => write!(
                f,
                "the local copy has schema version {version}, written by a later Missive than \
                 this one (which knows up to {SCHEMA_VERSION})"
            ),
            CopyError::NotIndexed => write!(
                f,
                "the local copy has no search index yet: it is built when the daemon next \
                 starts (missive sync starts it)"
            ),
        }
    }
}

impl From<rusqlite::Error> for CopyError {
    fn from(error: rusqlite::Error) -> Self {
        CopyError::Sqlite(error)
    }
}

type Result<T> = std::result::Result<T, CopyError>;

/// The local copy, opened for reading.
pub struct Copy {
    connection: Connection,
    version: i64,
}

/// An account as the copy knows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountRow {
    pub id: String,
    /// The connection state the daemon recorded last; `syncing` stays there if the daemon was
    /// killed meanwhile, which only the caller can tell.
    pub state: String,
    /// When the last complete sync ended, in seconds since the epoch.
    pub last_sync: Option<i64>,
    pub messages: u64,
}

/// A folder of an account.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FolderRef {
    pub id: i64,
    pub name: String,
    /// `inbox`, `sent`, `drafts`, `trash`, `junk` or `archive`; `None` for a folder of the user's.
    pub role: Option<String>,
}

/// The unread messages of a folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderUnread {
    pub account: String,
    pub folder: FolderRef,
    pub unread: u64,
}

/// A name and an address, as a header gave them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Person {
    pub name: Option<String>,
    pub address: Option<String>,
}

/// What a search asks for.
#[derive(Clone, Debug)]
pub struct Search {
    /// Words, each cut at its spaces; a message has to hold every one of them (anywhere in its
    /// sender, recipients, subject or body text). Each is taken literally, never as search syntax.
    pub terms: Vec<String>,
    /// The accounts to look in, by identifier.
    pub accounts: Vec<String>,
    /// Only the folders with this role.
    pub role: Option<String>,
    pub limit: usize,
}

/// A message a search found.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Hit {
    /// See the module's documentation.
    pub id: String,
    pub account: String,
    pub folder: FolderRef,
    pub uidvalidity: Option<u32>,
    pub uid: u32,
    /// The message's `Date`, else the server's internal date, in seconds since the epoch.
    pub date: Option<i64>,
    pub subject: Option<String>,
    pub from: Vec<Person>,
    pub to: Vec<Person>,
    pub unread: bool,
}

impl Copy {
    /// Opens the copy read only. `None` when there is none yet: no file, or one the daemon has
    /// created and not yet given its schema.
    pub fn open(path: &Path) -> Result<Option<Copy>> {
        let connection = match Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ) {
            Ok(connection) => connection,
            Err(rusqlite::Error::SqliteFailure(error, _))
                if error.code == rusqlite::ErrorCode::CannotOpen && !path.exists() =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        };
        connection.busy_timeout(Duration::from_secs(10))?;
        let version: i64 = connection.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(CopyError::Later(version));
        }
        if version == 0 {
            return Ok(None);
        }
        Ok(Some(Copy {
            connection,
            version,
        }))
    }

    /// Every account the copy holds, by identifier.
    pub fn accounts(&self) -> Result<Vec<AccountRow>> {
        let mut statement = self.connection.prepare_cached(
            "SELECT account.id, account.state, account.last_sync,
                    (SELECT count(*) FROM message JOIN folder ON folder.id = message.folder
                      WHERE folder.account = account.key)
               FROM account ORDER BY account.id",
        )?;
        let rows = statement.query_map([], |r| {
            Ok(AccountRow {
                id: r.get(0)?,
                state: r.get(1)?,
                last_sync: r.get(2)?,
                messages: r.get::<_, i64>(3)? as u64,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The unread messages of every folder of every account: those without `\Seen`, and not
    /// marked `\Deleted` (they wait for an expunge). A flag is matched as a whole word.
    /// By account, then folder: the inbox first, the other roles, then the user's own folders by
    /// name.
    pub fn unread(&self) -> Result<Vec<FolderUnread>> {
        let mut statement = self.connection.prepare_cached(
            "SELECT account.id, folder.id, folder.name, folder.role,
                    (SELECT count(*) FROM message
                      WHERE message.folder = folder.id
                        AND ' ' || message.flags || ' ' NOT LIKE '% \\Seen %'
                        AND ' ' || message.flags || ' ' NOT LIKE '% \\Deleted %')
               FROM folder JOIN account ON account.key = folder.account",
        )?;
        let rows = statement.query_map([], |r| {
            Ok(FolderUnread {
                account: r.get(0)?,
                folder: FolderRef {
                    id: r.get(1)?,
                    name: r.get(2)?,
                    role: r.get(3)?,
                },
                unread: r.get::<_, i64>(4)? as u64,
            })
        })?;
        let mut folders: Vec<FolderUnread> = rows.collect::<rusqlite::Result<_>>()?;
        folders.sort_by(|a, b| {
            (&a.account, role_rank(&a.folder), &a.folder.name).cmp(&(
                &b.account,
                role_rank(&b.folder),
                &b.folder.name,
            ))
        });
        Ok(folders)
    }

    /// The messages that hold every word of `search`, newest first.
    pub fn search(&self, search: &Search) -> Result<Vec<Hit>> {
        if self.version < SEARCH_VERSION {
            return Err(CopyError::NotIndexed);
        }
        let Some(expression) = match_expression(&search.terms) else {
            return Ok(Vec::new());
        };
        let accounts = serde_json::to_string(&search.accounts).expect("strings serialize");
        let mut statement = self.connection.prepare_cached(
            "SELECT account.id, folder.id, folder.name, folder.role, folder.uidvalidity,
                    message.uid, coalesce(message.date, message.internal_date), message.subject,
                    message.sender, message.recipients, message.flags
               FROM message_search
               JOIN message ON message.id = message_search.rowid
               JOIN folder ON folder.id = message.folder
               JOIN account ON account.key = folder.account
              WHERE message_search MATCH ?1
                AND account.id IN (SELECT value FROM json_each(?2))
                AND (?3 IS NULL OR folder.role = ?3)
              ORDER BY coalesce(message.date, message.internal_date) DESC, message.id DESC
              LIMIT ?4",
        )?;
        let role = search.role.clone().map_or(Value::Null, Value::Text);
        let rows = statement.query_map(
            params![expression, accounts, role, search.limit as i64],
            |r| {
                let account: String = r.get(0)?;
                let folder = FolderRef {
                    id: r.get(1)?,
                    name: r.get(2)?,
                    role: r.get(3)?,
                };
                let uidvalidity: Option<u32> = r.get(4)?;
                let uid: u32 = r.get(5)?;
                let people = |column: usize| -> rusqlite::Result<Vec<Person>> {
                    let json: String = r.get(column)?;
                    Ok(serde_json::from_str(&json).unwrap_or_default())
                };
                let flags: String = r.get(10)?;
                Ok(Hit {
                    id: format!("{account}/{}/{}/{uid}", folder.id, uidvalidity.unwrap_or(0)),
                    account,
                    folder,
                    uidvalidity,
                    uid,
                    date: r.get(6)?,
                    subject: r.get(7)?,
                    from: people(8)?,
                    to: people(9)?,
                    unread: !flags
                        .split(' ')
                        .any(|flag| flag.eq_ignore_ascii_case("\\Seen")),
                })
            },
        )?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

fn role_rank(folder: &FolderRef) -> usize {
    ["inbox", "drafts", "sent", "archive", "junk", "trash"]
        .iter()
        .position(|role| Some(*role) == folder.role.as_deref())
        .unwrap_or(6)
}

/// The words as an FTS5 query: each one a quoted string, so that what the user typed (`AND`,
/// `NEAR(`, `col:`, `*`, `-`, a quote…) is a word to find and never syntax. `None` when there is
/// nothing to look for.
fn match_expression(terms: &[String]) -> Option<String> {
    let quoted: Vec<String> = terms
        .iter()
        .flat_map(|term| term.split_whitespace())
        .filter(|term| term.chars().any(char::is_alphanumeric))
        .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
        .collect();
    (!quoted.is_empty()).then(|| quoted.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{NewMessage, Store};

    fn message(uid: u32, flags: &str, date: &str, subject: &str, body: &str) -> NewMessage {
        let raw = format!(
            "From: Carol <carol@example.com>\r\nTo: Dave <dave@example.net>\r\n\
             Subject: {subject}\r\nDate: {date}\r\nContent-Type: text/plain; charset=utf-8\r\n\
             Content-Transfer-Encoding: 8bit\r\n\r\n{body}\r\n"
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

    fn open() -> (tempfile::TempDir, Store, Copy) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("copy.sqlite");
        let store = Store::open(&path).unwrap();
        let copy = Copy::open(&path).unwrap().unwrap();
        (dir, store, copy)
    }

    fn search(copy: &Copy, terms: &[&str]) -> Vec<u32> {
        copy.search(&Search {
            terms: terms.iter().map(|term| (*term).to_owned()).collect(),
            accounts: vec!["alice".to_owned()],
            role: None,
            limit: 100,
        })
        .unwrap()
        .iter()
        .map(|hit| hit.uid)
        .collect()
    }

    #[test]
    fn no_copy_is_none_and_is_not_created() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("copy.sqlite");
        assert!(Copy::open(&path).unwrap().is_none());
        assert!(!path.exists());
        // A file created and not yet given its schema is no copy yet either.
        drop(Connection::open(&path).unwrap());
        assert!(Copy::open(&path).unwrap().is_none());
    }

    #[test]
    fn a_copy_written_by_a_later_version_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("copy.sqlite");
        Connection::open(&path)
            .unwrap()
            .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        let error = Copy::open(&path).err().unwrap();
        assert!(error.to_string().contains("later Missive"), "{error}");
    }

    #[test]
    fn a_copy_is_read_while_the_store_holds_it_open_and_writes() {
        let (_dir, mut store, copy) = open();
        let account = store.account("alice").unwrap();
        let folder = store.folder(account, "INBOX", Some("inbox")).unwrap().id;
        store
            .insert(
                folder,
                &[message(1, "", "Sat, 17 Oct 2026 09:30:00 +0000", "A", "b")],
            )
            .unwrap();
        assert_eq!(copy.accounts().unwrap()[0].messages, 1);
        store
            .insert(
                folder,
                &[message(2, "", "Sat, 17 Oct 2026 09:31:00 +0000", "A", "b")],
            )
            .unwrap();
        assert_eq!(copy.accounts().unwrap()[0].messages, 2);
    }

    #[test]
    fn accounts_say_their_state_last_sync_and_message_count() {
        let (_dir, mut store, copy) = open();
        let alice = store.account("alice").unwrap();
        let bob = store.account("bob").unwrap();
        let inbox = store.folder(alice, "INBOX", Some("inbox")).unwrap().id;
        let other = store.folder(alice, "Other", None).unwrap().id;
        store
            .insert(
                inbox,
                &[message(1, "", "Sat, 17 Oct 2026 09:30:00 +0000", "A", "b")],
            )
            .unwrap();
        store
            .insert(
                other,
                &[message(1, "", "Sat, 17 Oct 2026 09:30:00 +0000", "A", "b")],
            )
            .unwrap();
        store.set_state(alice, "ready", true).unwrap();
        store.set_state(bob, "auth-refused", false).unwrap();
        let accounts = copy.accounts().unwrap();
        assert_eq!(accounts.len(), 2);
        assert_eq!(accounts[0].id, "alice");
        assert_eq!(accounts[0].state, "ready");
        assert!(accounts[0].last_sync.is_some());
        assert_eq!(accounts[0].messages, 2);
        assert_eq!(
            accounts[1],
            AccountRow {
                id: "bob".to_owned(),
                state: "auth-refused".to_owned(),
                last_sync: None,
                messages: 0
            }
        );
    }

    #[test]
    fn unread_counts_the_messages_without_seen_by_folder_inbox_first() {
        let (_dir, mut store, copy) = open();
        let alice = store.account("alice").unwrap();
        let date = "Sat, 17 Oct 2026 09:30:00 +0000";
        let zebra = store.folder(alice, "Zebra", None).unwrap().id;
        let trash = store.folder(alice, "Trash", Some("trash")).unwrap().id;
        let inbox = store.folder(alice, "INBOX", Some("inbox")).unwrap().id;
        let apple = store.folder(alice, "Apple", None).unwrap().id;
        let sent = store.folder(alice, "Sent", Some("sent")).unwrap().id;
        store
            .insert(
                inbox,
                &[
                    message(1, "\\Seen", date, "A", "b"),
                    message(2, "\\Flagged", date, "A", "b"),
                    message(3, "", date, "A", "b"),
                    // A flag that holds the word is not the word.
                    message(4, "$NotSeen \\Unseen", date, "A", "b"),
                    message(5, "\\Flagged \\Seen", date, "A", "b"),
                    // Waiting to be expunged: not counted.
                    message(6, "\\Deleted", date, "A", "b"),
                    message(7, "\\Deleted \\Seen", date, "A", "b"),
                ],
            )
            .unwrap();
        store
            .insert(sent, &[message(1, "\\Seen", date, "A", "b")])
            .unwrap();
        store
            .insert(apple, &[message(1, "", date, "A", "b")])
            .unwrap();
        store
            .insert(zebra, &[message(1, "", date, "A", "b")])
            .unwrap();
        store
            .insert(trash, &[message(1, "", date, "A", "b")])
            .unwrap();
        store.account("bob").unwrap();

        let unread = copy.unread().unwrap();
        let summary: Vec<(&str, &str, u64)> = unread
            .iter()
            .map(|f| (f.account.as_str(), f.folder.name.as_str(), f.unread))
            .collect();
        assert_eq!(
            summary,
            [
                ("alice", "INBOX", 3),
                ("alice", "Sent", 0),
                ("alice", "Trash", 1),
                ("alice", "Apple", 1),
                ("alice", "Zebra", 1),
            ]
        );
    }

    #[test]
    fn search_finds_every_word_anywhere_newest_first() {
        let (_dir, mut store, copy) = open();
        let alice = store.account("alice").unwrap();
        let inbox = store.folder(alice, "INBOX", Some("inbox")).unwrap().id;
        store
            .insert(
                inbox,
                &[
                    message(
                        1,
                        "",
                        "Mon, 12 Oct 2026 09:00:00 +0000",
                        "Plan",
                        "walnut grove",
                    ),
                    message(2, "", "Wed, 14 Oct 2026 09:00:00 +0000", "Plan", "walnut"),
                    message(3, "", "Tue, 13 Oct 2026 09:00:00 +0000", "Walnut", "grove"),
                ],
            )
            .unwrap();
        assert_eq!(search(&copy, &["walnut"]), [2, 3, 1]);
        assert_eq!(search(&copy, &["walnut", "grove"]), [3, 1]);
        assert_eq!(search(&copy, &["WALNUT", "Plan"]), [2, 1]);
        assert!(search(&copy, &["walnut", "absent"]).is_empty());
    }

    #[test]
    fn what_the_user_types_is_never_search_syntax() {
        let (_dir, mut store, copy) = open();
        let alice = store.account("alice").unwrap();
        let inbox = store.folder(alice, "INBOX", Some("inbox")).unwrap().id;
        store
            .insert(
                inbox,
                &[message(
                    1,
                    "",
                    "Mon, 12 Oct 2026 09:00:00 +0000",
                    "Plan",
                    "this and that or the other, not near; \"quoted\" star",
                )],
            )
            .unwrap();
        for hostile in [
            "AND",
            "OR",
            "NOT",
            "NEAR",
            "NEAR(a b)",
            "\"",
            "\"quoted",
            "sta*",
            "-star",
            "body:star",
            "{body}:star",
            "^star",
            "(",
            ")",
            "a OR b",
            "'; DROP TABLE message; --",
        ] {
            copy.search(&Search {
                terms: vec![hostile.to_owned()],
                accounts: vec!["alice".to_owned()],
                role: None,
                limit: 10,
            })
            .unwrap_or_else(|e| panic!("{hostile:?}: {e}"));
        }
        // They are words to find: these are in the message.
        assert_eq!(search(&copy, &["and", "or", "not", "near"]), [1]);
        assert_eq!(search(&copy, &["\"quoted\""]), [1]);
        // And they find nothing they are not in.
        assert!(search(&copy, &["star OR absent"]).is_empty());
        assert_eq!(search(&copy, &["this  star"]), [1]);
        assert!(search(&copy, &["sta*"]).is_empty());
        // Nothing to look for finds nothing.
        assert!(search(&copy, &["!!!", "-"]).is_empty());
        assert!(search(&copy, &[]).is_empty());
        assert_eq!(copy.accounts().unwrap()[0].messages, 1);
    }

    #[test]
    fn search_can_be_narrowed_to_accounts_and_a_folder_role_and_limited() {
        let (_dir, mut store, copy) = open();
        let date = |day: u32| format!("Mon, {day} Oct 2026 09:00:00 +0000");
        let alice = store.account("alice").unwrap();
        let bob = store.account("bob").unwrap();
        let inbox = store.folder(alice, "INBOX", Some("inbox")).unwrap().id;
        let sent = store.folder(alice, "Sent", Some("sent")).unwrap().id;
        let bobs = store.folder(bob, "INBOX", Some("inbox")).unwrap().id;
        store
            .insert(
                inbox,
                &[
                    message(1, "", &date(10), "Plan", "walnut"),
                    message(2, "\\Seen", &date(11), "Plan", "walnut"),
                ],
            )
            .unwrap();
        store
            .insert(sent, &[message(1, "", &date(12), "Plan", "walnut")])
            .unwrap();
        store
            .insert(bobs, &[message(1, "", &date(13), "Plan", "walnut")])
            .unwrap();
        let search = |accounts: &[&str], role: Option<&str>, limit| {
            copy.search(&Search {
                terms: vec!["walnut".to_owned()],
                accounts: accounts.iter().map(|a| (*a).to_owned()).collect(),
                role: role.map(str::to_owned),
                limit,
            })
            .unwrap()
        };
        let ids = |hits: Vec<Hit>| hits.into_iter().map(|hit| hit.id).collect::<Vec<_>>();
        let uidvalidity = "0";
        assert_eq!(
            ids(search(&["alice", "bob"], None, 10)),
            [
                format!("bob/{bobs}/{uidvalidity}/1"),
                format!("alice/{sent}/{uidvalidity}/1"),
                format!("alice/{inbox}/{uidvalidity}/2"),
                format!("alice/{inbox}/{uidvalidity}/1"),
            ]
        );
        assert_eq!(ids(search(&["alice"], None, 2)).len(), 2);
        assert_eq!(ids(search(&["alice"], Some("inbox"), 10)).len(), 2);
        assert_eq!(ids(search(&["alice", "bob"], Some("sent"), 10)).len(), 1);
        assert!(search(&[], None, 10).is_empty());
        // What a hit says.
        let hit = &search(&["alice"], Some("inbox"), 1)[0];
        assert_eq!(hit.uid, 2);
        assert_eq!(hit.subject.as_deref(), Some("Plan"));
        assert_eq!(hit.from[0].address.as_deref(), Some("carol@example.com"));
        assert_eq!(hit.to[0].name.as_deref(), Some("Dave"));
        assert_eq!(hit.date, Some(1_791_709_200));
        assert!(!hit.unread);
        assert!(search(&["alice"], Some("inbox"), 10)[1].unread);
    }

    #[test]
    fn a_hit_names_the_uidvalidity_its_uid_belongs_to() {
        let (_dir, mut store, copy) = open();
        let alice = store.account("alice").unwrap();
        let inbox = store.folder(alice, "INBOX", Some("inbox")).unwrap().id;
        store.rebuild(inbox, 77).unwrap();
        store
            .insert(
                inbox,
                &[message(5, "", "Mon, 12 Oct 2026 09:00:00 +0000", "P", "b")],
            )
            .unwrap();
        let hits = copy
            .search(&Search {
                terms: vec!["carol".to_owned()],
                accounts: vec!["alice".to_owned()],
                role: None,
                limit: 10,
            })
            .unwrap();
        assert_eq!(hits[0].id, format!("alice/{inbox}/77/5"));
        assert_eq!(hits[0].uidvalidity, Some(77));
    }

    #[test]
    fn a_copy_without_the_index_still_counts_but_cannot_search() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("copy.sqlite");
        {
            let connection = Connection::open(&path).unwrap();
            connection
                .execute_batch(include_str!("store/schema-1.sql"))
                .unwrap();
            connection.pragma_update(None, "user_version", 1).unwrap();
            connection
                .execute_batch("INSERT INTO account (key, id, state) VALUES (1, 'alice', 'ready')")
                .unwrap();
        }
        let copy = Copy::open(&path).unwrap().unwrap();
        assert_eq!(copy.accounts().unwrap().len(), 1);
        assert!(copy.unread().unwrap().is_empty());
        let error = copy
            .search(&Search {
                terms: vec!["a".to_owned()],
                accounts: vec!["alice".to_owned()],
                role: None,
                limit: 10,
            })
            .unwrap_err();
        assert!(matches!(error, CopyError::NotIndexed));
    }
}
