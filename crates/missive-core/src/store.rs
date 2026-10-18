//! The local copy: SQLite in `$XDG_DATA_HOME/missive/copy.sqlite` (or
//! `~/.local/share/missive/…`).
//!
//! The daemon is its only writer; `status`, `unread` and `search` read it directly, while the
//! daemon writes, since the copy is in WAL mode. Each write is one transaction, and no transaction
//! is ever held across a wait on the network: a process killed at any point leaves a copy that is
//! whole as of its last commit.
//!
//! The schema, version 2 (`PRAGMA user_version`). Version 1 is the copy itself:
//!
//! ```sql
#![doc = include_str!("store/schema-1.sql")]
//! ```
//!
//! Version 2 adds the full-text index of `missive search`, which is filled from the raw message
//! when a message is written (in the same transaction), and, when a copy of version 1 is opened,
//! for the messages it already holds:
//!
//! ```sql
#![doc = include_str!("store/schema-2.sql")]
//! ```
//!
//! The index is contentless: it keeps the words and where they are, not the text, so a message
//! costs its words and their places beside its raw message, and no text is stored twice. The text it is built from is read by `mail-parser`: the text parts of a message, and,
//! for a message with only HTML, that HTML reduced to its text (markup, styles and scripts
//! left out).

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use mail_parser::{Address, MessageParser};
use rusqlite::{Connection, Transaction, params};
use serde::Serialize;

/// The schema version this build writes. A copy written by a later version is refused rather
/// than misread.
pub const SCHEMA_VERSION: i64 = 2;

/// Each step brings the schema from the version before it: `MIGRATIONS[0]` from nothing to 1.
const MIGRATIONS: &[&str] = &[
    include_str!("store/schema-1.sql"),
    include_str!("store/schema-2.sql"),
];

/// The local copy, opened for writing.
pub struct Store {
    connection: Connection,
}

/// What the copy knows of a folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderRow {
    pub id: i64,
    pub uidvalidity: Option<u32>,
    pub highest_modseq: Option<u64>,
}

/// A message fetched from the server, ready to be written.
pub struct NewMessage {
    pub uid: u32,
    pub flags: String,
    /// The IMAP internal date, in seconds since the epoch.
    pub internal_date: Option<i64>,
    /// The size the server gives (`RFC822.SIZE`), in bytes.
    pub size: i64,
    pub raw: Vec<u8>,
}

#[derive(Serialize)]
struct Mailbox<'a> {
    name: Option<&'a str>,
    address: Option<&'a str>,
}

impl Store {
    /// `$XDG_DATA_HOME/missive/copy.sqlite`, or `$HOME/.local/share/missive/copy.sqlite`.
    pub fn default_path() -> io::Result<PathBuf> {
        let data = match std::env::var_os("XDG_DATA_HOME").map(PathBuf::from) {
            Some(path) if path.is_absolute() => path,
            _ => match std::env::var_os("HOME") {
                Some(home) if !home.is_empty() => PathBuf::from(home).join(".local/share"),
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "neither XDG_DATA_HOME nor HOME is set",
                    ));
                }
            },
        };
        Ok(data.join("missive").join("copy.sqlite"))
    }

    /// Opens the copy, creating it or bringing its schema up to date.
    pub fn open(path: &Path) -> rusqlite::Result<Store> {
        let mut connection = Connection::open(path)?;
        // Another connection (another account's, or a reader's) may hold the lock for a moment.
        connection.busy_timeout(Duration::from_secs(10))?;
        let mode: String = connection.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(rusqlite::Error::InvalidParameterName(format!(
                "the copy could not be put in WAL mode (it is in {mode} mode)"
            )));
        }
        connection.pragma_update(None, "foreign_keys", true)?;
        migrate(&mut connection)?;
        Ok(Store { connection })
    }

    /// The number of an account in the copy, which logs name it by; the account is added the
    /// first time it is seen.
    pub fn account(&self, id: &str) -> rusqlite::Result<i64> {
        self.connection.execute(
            "INSERT INTO account (id, state) VALUES (?1, 'syncing') ON CONFLICT (id) DO NOTHING",
            [id],
        )?;
        self.connection
            .query_row("SELECT key FROM account WHERE id = ?1", [id], |r| r.get(0))
    }

    /// Records an account's connection state, and when `synced` that a complete sync just ended.
    pub fn set_state(&self, account: i64, state: &str, synced: bool) -> rusqlite::Result<()> {
        if synced {
            self.connection.execute(
                "UPDATE account SET state = ?2, last_sync = unixepoch() WHERE key = ?1",
                params![account, state],
            )?;
        } else {
            self.connection.execute(
                "UPDATE account SET state = ?2 WHERE key = ?1",
                params![account, state],
            )?;
        }
        Ok(())
    }

    /// A folder of an account, added the first time it is seen; its role follows the server's.
    pub fn folder(
        &self,
        account: i64,
        name: &str,
        role: Option<&str>,
    ) -> rusqlite::Result<FolderRow> {
        self.connection.execute(
            "INSERT INTO folder (account, name, role) VALUES (?1, ?2, ?3)
             ON CONFLICT (account, name) DO UPDATE SET role = excluded.role",
            params![account, name, role],
        )?;
        self.connection.query_row(
            "SELECT id, uidvalidity, highest_modseq FROM folder WHERE account = ?1 AND name = ?2",
            params![account, name],
            |r| {
                Ok(FolderRow {
                    id: r.get(0)?,
                    uidvalidity: r.get(1)?,
                    highest_modseq: r.get::<_, Option<i64>>(2)?.map(|modseq| modseq as u64),
                })
            },
        )
    }

    /// The UIDs in the copy of a folder, with their flags.
    pub fn flags(&self, folder: i64) -> rusqlite::Result<HashMap<u32, String>> {
        let mut statement = self
            .connection
            .prepare_cached("SELECT uid, flags FROM message WHERE folder = ?1")?;
        let rows = statement.query_map([folder], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect()
    }

    /// Empties a folder whose UIDVALIDITY changed: its UIDs name other messages now, or none.
    pub fn rebuild(&mut self, folder: i64, uidvalidity: u32) -> rusqlite::Result<()> {
        let transaction = self.connection.transaction()?;
        transaction.execute("DELETE FROM message WHERE folder = ?1", [folder])?;
        transaction.execute(
            "UPDATE folder SET uidvalidity = ?2, highest_modseq = NULL WHERE id = ?1",
            params![folder, uidvalidity],
        )?;
        transaction.commit()
    }

    /// Applies what a complete listing of the folder said, in one transaction: the messages it no
    /// longer holds, the flags that changed, and the HIGHESTMODSEQ those flags are up to date
    /// with (`None` without CONDSTORE).
    pub fn apply_listing(
        &mut self,
        folder: i64,
        removed: &[u32],
        flags: &[(u32, String)],
        highest_modseq: Option<u64>,
    ) -> rusqlite::Result<()> {
        let transaction = self.connection.transaction()?;
        {
            let mut remove =
                transaction.prepare_cached("DELETE FROM message WHERE folder = ?1 AND uid = ?2")?;
            for uid in removed {
                remove.execute(params![folder, uid])?;
            }
            let mut update = transaction
                .prepare_cached("UPDATE message SET flags = ?3 WHERE folder = ?1 AND uid = ?2")?;
            for (uid, flags) in flags {
                update.execute(params![folder, uid, flags])?;
            }
        }
        transaction.execute(
            "UPDATE folder SET highest_modseq = ?2 WHERE id = ?1",
            params![folder, highest_modseq.map(|modseq| modseq as i64)],
        )?;
        transaction.commit()
    }

    /// Writes a batch of new messages, in one transaction.
    pub fn insert(&mut self, folder: i64, messages: &[NewMessage]) -> rusqlite::Result<()> {
        let transaction = self.connection.transaction()?;
        for message in messages {
            insert(&transaction, folder, message)?;
        }
        transaction.commit()
    }
}

fn insert(transaction: &Transaction, folder: i64, message: &NewMessage) -> rusqlite::Result<()> {
    let parsed = MessageParser::default().parse(&message.raw);
    let parsed = parsed.as_ref();
    let mailboxes = |address: Option<&Address>| {
        let list: Vec<Mailbox> = address
            .into_iter()
            .flat_map(|address| address.iter())
            .map(|mailbox| Mailbox {
                name: mailbox.name(),
                address: mailbox.address(),
            })
            .collect();
        serde_json::to_string(&list).expect("names and addresses serialize")
    };
    let mut statement = transaction.prepare_cached(
        "INSERT INTO message (folder, uid, flags, internal_date, size, date, subject, sender,
             recipients, cc, message_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         ON CONFLICT (folder, uid) DO UPDATE SET flags = excluded.flags
         RETURNING id",
    )?;
    let id: i64 = statement.query_row(
        params![
            folder,
            message.uid,
            message.flags,
            message.internal_date,
            message.size,
            parsed
                .and_then(|m| m.date())
                .map(|date| date.to_timestamp()),
            parsed.and_then(|m| m.subject()),
            mailboxes(parsed.and_then(|m| m.from())),
            mailboxes(parsed.and_then(|m| m.to())),
            mailboxes(parsed.and_then(|m| m.cc())),
            parsed.and_then(|m| m.message_id()),
        ],
        |r| r.get(0),
    )?;
    transaction
        .prepare_cached(
            "INSERT INTO message_raw (message, raw) VALUES (?1, ?2)
             ON CONFLICT (message) DO NOTHING",
        )?
        .execute(params![id, message.raw])?;
    index(transaction, id, parsed)
}

/// Puts a message in the full-text index, in place of what the index held for it.
fn index(
    transaction: &Transaction,
    id: i64,
    parsed: Option<&mail_parser::Message>,
) -> rusqlite::Result<()> {
    let words = |address: Option<&Address>| {
        let mut words = String::new();
        for mailbox in address.into_iter().flat_map(|address| address.iter()) {
            for part in [mailbox.name(), mailbox.address()].into_iter().flatten() {
                words.push_str(part);
                words.push(' ');
            }
        }
        words
    };
    let mut recipients = words(parsed.and_then(|m| m.to()));
    recipients.push_str(&words(parsed.and_then(|m| m.cc())));
    let mut body = String::new();
    if let Some(parsed) = parsed {
        for part in 0..parsed.text_body_count() {
            body.extend(parsed.body_text(part).as_deref());
            body.push('\n');
        }
    }
    transaction
        .prepare_cached("DELETE FROM message_search WHERE rowid = ?1")?
        .execute([id])?;
    transaction
        .prepare_cached(
            "INSERT INTO message_search (rowid, sender, recipients, subject, body)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?
        .execute(params![
            id,
            words(parsed.and_then(|m| m.from())),
            recipients,
            parsed.and_then(|m| m.subject()),
            body,
        ])?;
    Ok(())
}

/// Indexes the messages a copy of version 1 already holds.
fn index_existing(transaction: &Transaction) -> rusqlite::Result<()> {
    let mut statement =
        transaction.prepare("SELECT message, raw FROM message_raw ORDER BY message")?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let raw: Vec<u8> = row.get(1)?;
        let parsed = MessageParser::default().parse(&raw);
        index(transaction, row.get(0)?, parsed.as_ref())?;
    }
    Ok(())
}

fn migrate(connection: &mut Connection) -> rusqlite::Result<()> {
    let transaction = connection.transaction()?;
    let version: i64 = transaction.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version > SCHEMA_VERSION {
        return Err(rusqlite::Error::InvalidParameterName(format!(
            "the copy has schema version {version}, written by a later Missive than this one \
             (which knows up to {SCHEMA_VERSION})"
        )));
    }
    for (index, step) in MIGRATIONS.iter().enumerate().skip(version as usize) {
        transaction.execute_batch(step)?;
        if index == 1 {
            index_existing(&transaction)?;
        }
        transaction.pragma_update(None, "user_version", index as i64 + 1)?;
    }
    transaction.commit()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(uid: u32, flags: &str) -> NewMessage {
        let raw: Vec<u8> = format!(
            "From: Carol <carol@example.com>\r\nTo: dave@example.net, Eve <eve@example.org>\r\n\
                 Subject: Number {uid}\r\nDate: Sat, 17 Oct 2026 09:30:00 +0200\r\n\
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

    fn open() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("copy.sqlite")).unwrap();
        (dir, store)
    }

    #[test]
    fn a_new_copy_gets_the_current_schema_in_wal_mode() {
        let (dir, _store) = open();
        let reader = Connection::open(dir.path().join("copy.sqlite")).unwrap();
        let version: i64 = reader
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        let mode: String = reader
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        assert_eq!(mode, "wal");
        // Opening it again changes nothing.
        drop(Store::open(&dir.path().join("copy.sqlite")).unwrap());
    }

    #[test]
    fn a_copy_written_by_a_later_version_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("copy.sqlite");
        Connection::open(&path)
            .unwrap()
            .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        let error = Store::open(&path).err().unwrap();
        assert!(error.to_string().contains("later Missive"), "{error}");
    }

    #[test]
    fn a_message_is_stored_raw_with_its_envelope() {
        let (_dir, mut store) = open();
        let account = store.account("alice").unwrap();
        let folder = store.folder(account, "INBOX", Some("inbox")).unwrap();
        store.insert(folder.id, &[message(7, "\\Seen")]).unwrap();
        let row: (String, String, String, i64, Vec<u8>, i64) = store
            .connection
            .query_row(
                "SELECT subject, sender, recipients, date, raw, size FROM message \
                 JOIN message_raw ON message_raw.message = message.id",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(row.0, "Number 7");
        assert_eq!(row.1, r#"[{"name":"Carol","address":"carol@example.com"}]"#);
        assert_eq!(
            row.2,
            r#"[{"name":null,"address":"dave@example.net"},{"name":"Eve","address":"eve@example.org"}]"#
        );
        assert_eq!(row.3, 1_792_222_200);
        assert_eq!(row.4, message(7, "").raw);
        assert_eq!(row.5, row.4.len() as i64);
    }

    #[test]
    fn a_listing_removes_updates_and_moves_the_sync_point_together() {
        let (_dir, mut store) = open();
        let account = store.account("alice").unwrap();
        let folder = store.folder(account, "INBOX", Some("inbox")).unwrap().id;
        store
            .insert(folder, &[message(1, ""), message(2, ""), message(3, "")])
            .unwrap();
        store
            .apply_listing(folder, &[2], &[(3, "\\Seen".to_owned())], Some(42))
            .unwrap();
        let flags = store.flags(folder).unwrap();
        assert_eq!(
            flags,
            HashMap::from([(1, String::new()), (3, "\\Seen".to_owned())])
        );
        assert_eq!(
            store.folder(account, "INBOX", Some("inbox")).unwrap(),
            FolderRow {
                id: folder,
                uidvalidity: None,
                highest_modseq: Some(42),
            }
        );
    }

    fn raws(store: &Store) -> i64 {
        store
            .connection
            .query_row("SELECT count(*) FROM message_raw", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn a_message_leaves_with_its_raw_message_and_a_replayed_batch_keeps_one() {
        let (_dir, mut store) = open();
        let account = store.account("alice").unwrap();
        let folder = store.folder(account, "INBOX", Some("inbox")).unwrap().id;
        store
            .insert(folder, &[message(1, ""), message(2, "")])
            .unwrap();
        // The same batch again, as after a kill before the sync point was known to move.
        store.insert(folder, &[message(2, "\\Seen")]).unwrap();
        assert_eq!(raws(&store), 2);

        store.apply_listing(folder, &[1], &[], None).unwrap();
        assert_eq!(raws(&store), 1);
        store.rebuild(folder, 9).unwrap();
        assert_eq!(raws(&store), 0);
    }

    #[test]
    fn a_rebuild_empties_only_that_folder() {
        let (_dir, mut store) = open();
        let account = store.account("alice").unwrap();
        let inbox = store.folder(account, "INBOX", Some("inbox")).unwrap().id;
        let other = store.folder(account, "Other", None).unwrap().id;
        store.insert(inbox, &[message(1, "")]).unwrap();
        store.insert(other, &[message(1, "")]).unwrap();
        store.rebuild(other, 7).unwrap();
        assert_eq!(store.flags(inbox).unwrap().len(), 1);
        assert!(store.flags(other).unwrap().is_empty());
        assert_eq!(
            store.folder(account, "Other", None).unwrap().uidvalidity,
            Some(7)
        );
    }

    #[test]
    fn a_failed_batch_writes_nothing() {
        let (_dir, mut store) = open();
        let account = store.account("alice").unwrap();
        let inbox = store.folder(account, "INBOX", Some("inbox")).unwrap().id;
        // The second message names a folder that does not exist: the whole batch is refused.
        let result = store.insert(inbox, &[message(1, "")]).and_then(|()| {
            let transaction = store.connection.transaction()?;
            insert(&transaction, inbox, &message(2, ""))?;
            insert(&transaction, inbox + 100, &message(3, ""))?;
            transaction.commit()
        });
        assert!(result.is_err());
        assert_eq!(
            store.flags(inbox).unwrap().into_keys().collect::<Vec<_>>(),
            [1]
        );
    }

    #[test]
    fn an_account_keeps_its_number_and_its_state() {
        let (_dir, store) = open();
        let alice = store.account("alice").unwrap();
        let bob = store.account("bob").unwrap();
        assert_ne!(alice, bob);
        assert_eq!(store.account("alice").unwrap(), alice);
        store.set_state(bob, "auth-refused", false).unwrap();
        store.set_state(alice, "ready", true).unwrap();
        let rows: Vec<(String, String, Option<i64>)> = store
            .connection
            .prepare("SELECT id, state, last_sync FROM account ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(rows[0].1, "ready");
        assert!(rows[0].2.is_some());
        assert_eq!(rows[1], ("bob".to_owned(), "auth-refused".to_owned(), None));
    }

    fn mail(uid: u32, headers: &str, body: &str) -> NewMessage {
        let raw = format!(
            "From: Carol <carol@example.com>\r\nTo: dave@example.net\r\nSubject: Hello\r\n\
             {headers}\r\n{body}"
        )
        .into_bytes();
        NewMessage {
            uid,
            flags: String::new(),
            internal_date: Some(1_790_000_000),
            size: raw.len() as i64,
            raw,
        }
    }

    /// The UIDs of the messages the index finds for one word.
    fn found(store: &Store, word: &str) -> Vec<u32> {
        let mut statement = store
            .connection
            .prepare(
                "SELECT message.uid FROM message_search \
                 JOIN message ON message.id = message_search.rowid \
                 WHERE message_search MATCH ?1 ORDER BY message.uid",
            )
            .unwrap();
        statement
            .query_map([format!("\"{word}\"")], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    #[test]
    fn a_message_is_indexed_by_sender_recipients_subject_and_body() {
        let (_dir, mut store) = open();
        let account = store.account("alice").unwrap();
        let folder = store.folder(account, "INBOX", Some("inbox")).unwrap().id;
        let raw = "From: Zoé Sender <zoe@example.com>\r\n\
                   To: Dave Recipient <dave@example.net>\r\n\
                   Cc: erin.copied@example.org\r\n\
                   Subject: Quarterly plan\r\n\r\nThe walnut is in the body.\r\n";
        let message = NewMessage {
            uid: 1,
            flags: String::new(),
            internal_date: None,
            size: raw.len() as i64,
            raw: raw.as_bytes().to_vec(),
        };
        store.insert(folder, &[message]).unwrap();
        for word in [
            "zoe",
            "sender",
            "recipient",
            "copied",
            "quarterly",
            "walnut",
        ] {
            assert_eq!(found(&store, word), [1], "{word}");
        }
        assert!(found(&store, "absent").is_empty());
    }

    #[test]
    fn the_text_of_an_html_only_message_is_indexed_without_its_markup() {
        let (_dir, mut store) = open();
        let account = store.account("alice").unwrap();
        let folder = store.folder(account, "INBOX", Some("inbox")).unwrap().id;
        let html = mail(
            1,
            "Content-Type: text/html; charset=utf-8\r\n",
            "<html><head><style>.hidden { color: red }</style></head>\
             <body><p>The <b>marmot</b> sleeps.</p><script>var secret = 1;</script></body></html>",
        );
        store.insert(folder, &[html]).unwrap();
        assert_eq!(found(&store, "marmot"), [1]);
        assert!(found(&store, "body").is_empty());
        assert!(found(&store, "html").is_empty());
        assert!(found(&store, "hidden").is_empty());
        assert!(found(&store, "secret").is_empty());
    }

    #[test]
    fn the_text_part_of_an_alternative_message_and_an_encoded_body_are_indexed() {
        let (_dir, mut store) = open();
        let account = store.account("alice").unwrap();
        let folder = store.folder(account, "INBOX", Some("inbox")).unwrap().id;
        let alternative = mail(
            1,
            "MIME-Version: 1.0\r\nContent-Type: multipart/alternative; boundary=\"b\"\r\n",
            "--b\r\nContent-Type: text/plain\r\n\r\nThe plain gazelle.\r\n--b\r\n\
             Content-Type: text/html\r\n\r\n<p>The html gazelle.</p>\r\n--b--\r\n",
        );
        let encoded = mail(
            2,
            "Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n",
            // "Crème brûlée" in UTF-8, base64.
            "Q3LDqG1lIGJyw7tsw6ll\r\n",
        );
        store.insert(folder, &[alternative, encoded]).unwrap();
        assert_eq!(found(&store, "gazelle"), [1]);
        assert_eq!(found(&store, "brulee"), [2]);
    }

    #[test]
    fn case_and_accents_do_not_matter_in_the_index() {
        let (_dir, mut store) = open();
        let account = store.account("alice").unwrap();
        let folder = store.folder(account, "INBOX", Some("inbox")).unwrap().id;
        let accented = mail(
            1,
            "Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n",
            "Les Éléphants d'Élodie\r\n",
        );
        store.insert(folder, &[accented]).unwrap();
        for word in ["éléphants", "ELEPHANTS", "elephants", "Élodie", "elodie"] {
            assert_eq!(found(&store, word), [1], "{word}");
        }
    }

    #[test]
    fn a_replayed_batch_indexes_a_message_once() {
        let (_dir, mut store) = open();
        let account = store.account("alice").unwrap();
        let folder = store.folder(account, "INBOX", Some("inbox")).unwrap().id;
        store.insert(folder, &[message(1, "")]).unwrap();
        store.insert(folder, &[message(1, "\\Seen")]).unwrap();
        assert_eq!(found(&store, "hello"), [1]);
        let rows: i64 = store
            .connection
            .query_row(
                "SELECT count(*) FROM message_search WHERE message_search MATCH '\"hello\"'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 1);
    }

    #[test]
    fn a_message_leaves_the_index_however_it_leaves_the_copy() {
        let (_dir, mut store) = open();
        let account = store.account("alice").unwrap();
        let inbox = store.folder(account, "INBOX", Some("inbox")).unwrap().id;
        let other = store.folder(account, "Other", None).unwrap().id;
        store
            .insert(inbox, &[message(1, ""), message(2, "")])
            .unwrap();
        store.insert(other, &[message(3, "")]).unwrap();
        assert_eq!(found(&store, "hello"), [1, 2, 3]);

        // Expunged on the server.
        store.apply_listing(inbox, &[1], &[], None).unwrap();
        assert_eq!(found(&store, "hello"), [2, 3]);
        // A folder renumbered by the server.
        store.rebuild(inbox, 9).unwrap();
        assert_eq!(found(&store, "hello"), [3]);
        // A folder deleted outright.
        store
            .connection
            .execute("DELETE FROM folder WHERE id = ?1", [other])
            .unwrap();
        assert!(found(&store, "hello").is_empty());
    }

    #[test]
    fn opening_a_copy_of_schema_1_indexes_the_messages_it_holds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("copy.sqlite");
        {
            let connection = Connection::open(&path).unwrap();
            connection.execute_batch(MIGRATIONS[0]).unwrap();
            connection.pragma_update(None, "user_version", 1).unwrap();
            connection
                .execute_batch(
                    "INSERT INTO account (key, id, state) VALUES (1, 'alice', 'ready');
                     INSERT INTO folder (id, account, name, role) VALUES (1, 1, 'INBOX', 'inbox');
                     INSERT INTO message (id, folder, uid, flags, size, subject, sender,
                         recipients, cc)
                     VALUES (1, 1, 1, '', 0, 'Old', '[]', '[]', '[]');",
                )
                .unwrap();
            let old = message(1, "");
            connection
                .execute(
                    "INSERT INTO message_raw (message, raw) VALUES (1, ?1)",
                    [old.raw],
                )
                .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(found(&store, "hello"), [1]);
        assert_eq!(found(&store, "carol"), [1]);
    }
}
