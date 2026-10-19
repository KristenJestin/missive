//! The drafts still to be written to the server, or removed from it (schema version 7). See
//! [`crate::drafts`].

use mail_parser::{Address, MessageParser};
use rusqlite::{OptionalExtension, Transaction, params};

use super::{Mailbox, NewMessage, Store, insert, queue};
use crate::copy::MessageId;

/// A draft of the queue, as the daemon writes it to the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DraftRow {
    pub id: i64,
    /// Without its angle brackets.
    pub message_id: String,
    /// The version to append, its Bcc header included; `None` when the draft is only to be
    /// removed (it was sent).
    pub raw: Option<Vec<u8>>,
    /// See the schema: the row leaves only if it is still this version.
    pub version: i64,
    pub attempts: u32,
}

/// A draft found to be picked up again: its latest version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stored {
    /// `None` for a draft another client wrote without a Message-ID.
    pub message_id: Option<String>,
    pub raw: Vec<u8>,
}

impl Store {
    /// The draft `id` names in the account: a Message-ID (with or without its angle brackets),
    /// or a message identifier, as `list` prints it, of a message of the account's Drafts
    /// folder. Its latest version is the one waiting in the copy, else the newest the Drafts
    /// folder holds. `None` when there is no such draft, or when it was sent.
    pub fn find_draft(&self, account: i64, id: &str) -> rusqlite::Result<Option<Stored>> {
        let message_id = match MessageId::parse(id) {
            Some(parsed) => {
                let found: Option<(Option<String>, Vec<u8>)> = self
                    .connection
                    .query_row(
                        "SELECT message.message_id, message_raw.raw FROM message
                           JOIN message_raw ON message_raw.message = message.id
                           JOIN folder ON folder.id = message.folder
                           JOIN account ON account.key = folder.account
                          WHERE account.key = ?1 AND account.id = ?2 AND folder.id = ?3
                            AND folder.role = 'drafts' AND folder.uidvalidity = ?4
                            AND message.uid = ?5",
                        params![
                            account,
                            parsed.account,
                            parsed.folder,
                            parsed.uidvalidity,
                            parsed.uid
                        ],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                let Some((message_id, raw)) = found else {
                    return Ok(None);
                };
                match message_id {
                    Some(message_id) => message_id,
                    None => {
                        return Ok(Some(Stored {
                            message_id: None,
                            raw,
                        }));
                    }
                }
            }
            None => id.trim_start_matches('<').trim_end_matches('>').to_owned(),
        };
        let waiting: Option<Option<Vec<u8>>> = self
            .connection
            .query_row(
                "SELECT raw FROM draft WHERE account = ?1 AND message_id = ?2",
                params![account, message_id],
                |r| r.get(0),
            )
            .optional()?;
        let raw = match waiting {
            Some(Some(raw)) => Some(raw),
            // Sent: it is no longer a draft.
            Some(None) => None,
            None => self
                .connection
                .query_row(
                    "SELECT message_raw.raw FROM message
                       JOIN message_raw ON message_raw.message = message.id
                       JOIN folder ON folder.id = message.folder
                      WHERE folder.account = ?1 AND folder.role = 'drafts'
                        AND message.message_id = ?2
                      ORDER BY message.uid DESC LIMIT 1",
                    params![account, message_id],
                    |r| r.get(0),
                )
                .optional()?,
        };
        Ok(raw.map(|raw| Stored {
            message_id: Some(message_id),
            raw,
        }))
    }

    /// Records a new version of a draft, to be written to the server: it takes the place of the
    /// version waiting, if any.
    pub fn save_draft(
        &mut self,
        account: i64,
        message_id: &str,
        raw: &[u8],
    ) -> rusqlite::Result<()> {
        let parsed = MessageParser::default().parse(raw);
        let parsed = parsed.as_ref();
        self.connection.execute(
            "INSERT INTO draft (account, message_id, raw, date, subject, sender, recipients,
                                has_attachments, saved_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, unixepoch())
             ON CONFLICT (account, message_id) DO UPDATE SET
                 raw = excluded.raw, date = excluded.date, subject = excluded.subject,
                 sender = excluded.sender, recipients = excluded.recipients,
                 has_attachments = excluded.has_attachments, saved_at = excluded.saved_at,
                 version = draft.version + 1, attempts = 0, last_error = NULL",
            params![
                account,
                message_id,
                raw,
                parsed
                    .and_then(|m| m.date())
                    .map(|date| date.to_timestamp()),
                parsed.and_then(|m| m.subject()),
                mailboxes(parsed.and_then(|m| m.from())),
                mailboxes(parsed.and_then(|m| m.to())),
                parsed.is_some_and(crate::body::has_attachments),
            ],
        )?;
        Ok(())
    }

    /// Whether a message of the outbox sent from this draft is still to be sent.
    pub fn draft_sending(&self, account: i64, message_id: &str) -> rusqlite::Result<bool> {
        self.connection.query_row(
            "SELECT EXISTS (SELECT 1 FROM outbox WHERE account = ?1 AND draft = ?2
                              AND state IN ('queued', 'claimed', 'sending'))",
            params![account, message_id],
            |r| r.get(0),
        )
    }

    /// Whether a message with this Message-ID was already sent: the copy's Sent folder holds it,
    /// or the outbox sent it or does not know whether the server took it. Without UIDPLUS a
    /// sent draft's last version stays in the Drafts folder, and sending it again would send
    /// the same message twice.
    pub fn already_sent(&self, account: i64, message_id: &str) -> rusqlite::Result<bool> {
        self.connection.query_row(
            "SELECT EXISTS (SELECT 1 FROM outbox WHERE account = ?1 AND message_id = ?2
                              AND state IN ('sent', 'uncertain'))
                 OR EXISTS (SELECT 1 FROM message JOIN folder ON folder.id = message.folder
                             WHERE folder.account = ?1 AND folder.role = 'sent'
                               AND message.message_id = ?2)",
            params![account, message_id],
            |r| r.get(0),
        )
    }

    /// The account's drafts to write or remove, oldest first.
    pub fn drafts(&self, account: i64) -> rusqlite::Result<Vec<DraftRow>> {
        self.connection
            .prepare(
                "SELECT id, message_id, raw, version, attempts
                   FROM draft WHERE account = ?1 ORDER BY id",
            )?
            .query_map([account], |r| {
                Ok(DraftRow {
                    id: r.get(0)?,
                    message_id: r.get(1)?,
                    raw: r.get(2)?,
                    version: r.get(3)?,
                    attempts: r.get(4)?,
                })
            })?
            .collect()
    }

    /// The account's folder with the role `drafts`: its row, name and UIDVALIDITY.
    pub fn drafts_folder(
        &self,
        account: i64,
    ) -> rusqlite::Result<Option<(i64, String, Option<u32>)>> {
        self.connection
            .query_row(
                "SELECT id, name, uidvalidity FROM folder WHERE account = ?1 AND role = 'drafts'
                  ORDER BY id LIMIT 1",
                [account],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
    }

    /// The UIDs the copy holds of a draft in a folder, by its Message-ID: only the versions
    /// Missive wrote ([`crate::compose::is_own_draft`]).
    pub fn draft_uids(&self, folder: i64, message_id: &str) -> rusqlite::Result<Vec<u32>> {
        let found: Vec<(u32, Vec<u8>)> = self
            .connection
            .prepare_cached(
                "SELECT message.uid, message_raw.raw FROM message
                   JOIN message_raw ON message_raw.message = message.id
                  WHERE message.folder = ?1 AND message.message_id = ?2",
            )?
            .query_map(params![folder, message_id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(found
            .into_iter()
            .filter(|(_, raw)| crate::compose::is_own_draft(raw))
            .map(|(uid, _)| uid)
            .collect())
    }

    /// Writing a draft failed this time: it stays. Returns how many times it failed.
    pub fn draft_failed(&self, id: i64, why: &str) -> rusqlite::Result<u32> {
        self.connection.query_row(
            "UPDATE draft SET attempts = attempts + 1, last_error = ?2 WHERE id = ?1
             RETURNING attempts",
            params![id, why],
            |r| r.get(0),
        )
    }

    /// A draft written to the server (or removed from it), in one transaction: its row leaves,
    /// unless another version was saved meanwhile; the versions removed from the folder leave
    /// the copy; and the version appended, when its UID is known (`new`) and the copy's folder
    /// has the UIDVALIDITY the server gave, is written to the copy at once. Returns whether the
    /// row left.
    pub fn draft_written(
        &mut self,
        draft: &DraftRow,
        folder: i64,
        uidvalidity: u32,
        removed: &[u32],
        new: Option<NewMessage>,
    ) -> rusqlite::Result<bool> {
        let transaction = self.connection.transaction()?;
        let left = transaction.execute(
            "DELETE FROM draft WHERE id = ?1 AND version = ?2",
            params![draft.id, draft.version],
        )? > 0;
        let current: Option<u32> = transaction.query_row(
            "SELECT uidvalidity FROM folder WHERE id = ?1",
            [folder],
            |r| r.get(0),
        )?;
        if current == Some(uidvalidity) {
            for uid in removed {
                transaction.execute(
                    "DELETE FROM message WHERE folder = ?1 AND uid = ?2",
                    params![folder, uid],
                )?;
            }
            if let Some(new) = new
                && !queue::deleting(&transaction, folder, new.uid)?
            {
                insert(&transaction, folder, &new)?;
            }
        }
        transaction.commit()?;
        Ok(left)
    }
}

/// The draft of a message of the outbox the server took is to be removed from the Drafts
/// folder: its row is now a removal, whatever version waited.
pub(super) fn remove_draft(
    transaction: &Transaction,
    account: i64,
    message_id: &str,
) -> rusqlite::Result<()> {
    transaction.execute(
        "INSERT INTO draft (account, message_id, raw, saved_at) VALUES (?1, ?2, NULL, unixepoch())
         ON CONFLICT (account, message_id) DO UPDATE SET
             raw = NULL, version = draft.version + 1, attempts = 0, last_error = NULL",
        params![account, message_id],
    )?;
    Ok(())
}

fn mailboxes(address: Option<&Address>) -> String {
    let list: Vec<Mailbox> = address
        .into_iter()
        .flat_map(|address| address.iter())
        .map(|mailbox| Mailbox {
            name: mailbox.name(),
            address: mailbox.address(),
        })
        .collect();
    serde_json::to_string(&list).expect("names and addresses serialize")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compose::Built;
    use crate::copy::message_id;

    fn raw(subject: &str) -> Vec<u8> {
        format!(
            "X-Missive-Draft: 1\r\nFrom: alice@example.org\r\nTo: Bob <bob@example.com>\r\n\
             Subject: {subject}\r\nMessage-ID: <d1@example.org>\r\n\r\nHello\r\n"
        )
        .into_bytes()
    }

    fn open() -> (tempfile::TempDir, Store, i64) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("copy.sqlite")).unwrap();
        let account = store.account("alice").unwrap();
        (dir, store, account)
    }

    /// The account's Drafts folder, examined under UIDVALIDITY 7, holding `subject` at UID 1.
    fn on_server(store: &mut Store, account: i64, subject: &str) -> i64 {
        let folder = store.folder(account, "Drafts", Some("drafts")).unwrap().id;
        store.rebuild(folder, 7).unwrap();
        store
            .insert(
                folder,
                &[NewMessage {
                    uid: 1,
                    flags: "\\Draft \\Seen".to_owned(),
                    internal_date: None,
                    size: 10,
                    raw: raw(subject),
                }],
            )
            .unwrap();
        folder
    }

    #[test]
    fn a_draft_is_found_by_its_message_id_or_its_identifier_the_waiting_version_first() {
        let (_dir, mut store, account) = open();
        let folder = on_server(&mut store, account, "On the server");
        let found = |store: &Store, id: &str| store.find_draft(account, id).unwrap();

        let by_id = found(&store, "<d1@example.org>").unwrap();
        assert_eq!(by_id.raw, raw("On the server"));
        let identifier = message_id("alice", folder, 7, 1);
        let by_identifier = found(&store, &identifier).unwrap();
        assert_eq!(by_identifier.message_id.as_deref(), Some("d1@example.org"));

        store
            .save_draft(account, "d1@example.org", &raw("Waiting"))
            .unwrap();
        assert_eq!(found(&store, "d1@example.org").unwrap().raw, raw("Waiting"));
        assert_eq!(found(&store, &identifier).unwrap().raw, raw("Waiting"));
        assert_eq!(found(&store, "other@example.org"), None);
        // Another account's identifier, or a message outside the Drafts folder, is no draft.
        assert_eq!(found(&store, &message_id("bob", folder, 7, 1)), None);
        let inbox = store.folder(account, "INBOX", Some("inbox")).unwrap().id;
        assert_eq!(found(&store, &message_id("alice", inbox, 0, 1)), None);
    }

    #[test]
    fn a_version_of_another_client_under_the_same_message_id_is_never_one_to_remove() {
        let (_dir, mut store, account) = open();
        let folder = on_server(&mut store, account, "Missive's");
        let foreign = String::from_utf8(raw("Another client's"))
            .unwrap()
            .replace("X-Missive-Draft: 1\r\n", "");
        store
            .insert(
                folder,
                &[NewMessage {
                    uid: 2,
                    flags: "\\Draft".to_owned(),
                    internal_date: None,
                    size: 10,
                    raw: foreign.into_bytes(),
                }],
            )
            .unwrap();

        assert_eq!(store.draft_uids(folder, "d1@example.org").unwrap(), [1]);
    }

    #[test]
    fn a_version_saved_while_the_last_one_was_written_stays_to_write() {
        let (_dir, mut store, account) = open();
        let folder = on_server(&mut store, account, "First");
        store
            .save_draft(account, "d1@example.org", &raw("Second"))
            .unwrap();
        let writing = store.drafts(account).unwrap().remove(0);
        store
            .save_draft(account, "d1@example.org", &raw("Third"))
            .unwrap();

        let left = store
            .draft_written(&writing, folder, 7, &[1], None)
            .unwrap();

        assert!(!left);
        let waiting = store.drafts(account).unwrap();
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0].raw, Some(raw("Third")));
        assert_eq!(waiting[0].version, writing.version + 1);
        // The version removed from the server left the copy all the same.
        assert!(
            store
                .draft_uids(folder, "d1@example.org")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_draft_written_leaves_the_queue_and_its_new_version_is_in_the_copy() {
        let (_dir, mut store, account) = open();
        let folder = on_server(&mut store, account, "First");
        store
            .save_draft(account, "d1@example.org", &raw("Second"))
            .unwrap();
        let writing = store.drafts(account).unwrap().remove(0);
        let new = NewMessage {
            uid: 2,
            flags: "\\Draft \\Seen".to_owned(),
            internal_date: None,
            size: 10,
            raw: raw("Second"),
        };

        assert!(
            store
                .draft_written(&writing, folder, 7, &[1], Some(new))
                .unwrap()
        );

        assert!(store.drafts(account).unwrap().is_empty());
        assert_eq!(store.draft_uids(folder, "d1@example.org").unwrap(), [2]);
    }

    #[test]
    fn a_draft_whose_message_was_sent_becomes_a_removal_and_is_no_draft_any_more() {
        let (_dir, mut store, account) = open();
        on_server(&mut store, account, "First");
        store
            .save_draft(account, "d1@example.org", &raw("Second"))
            .unwrap();
        let built = Built {
            sender: "alice@example.org".to_owned(),
            recipients: vec!["bob@example.com".to_owned()],
            message_id: "d1@example.org".to_owned(),
            raw: raw("Second"),
            bcc_header: None,
        };
        let outbox = store
            .enqueue(account, &built, false, Some("d1@example.org"))
            .unwrap();
        assert!(store.draft_sending(account, "d1@example.org").unwrap());

        store.sent(outbox, false).unwrap();

        assert!(!store.draft_sending(account, "d1@example.org").unwrap());
        let removal = store.drafts(account).unwrap();
        assert_eq!(removal.len(), 1);
        assert_eq!(removal[0].raw, None);
        assert_eq!(store.find_draft(account, "d1@example.org").unwrap(), None);
    }
}
