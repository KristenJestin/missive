//! The outbox (schema versions 6, 8 and 9): the messages to send, and where each one is between
//! its writing and its server's answer. See [`crate::outbox`].

use rusqlite::{OptionalExtension, params};

use super::Store;
use crate::actions::{Kind, Wanted};
use crate::compose::Built;

/// A message of the outbox to send.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToSend {
    pub id: i64,
    pub sender: String,
    pub recipients: Vec<String>,
    pub message_id: String,
    pub raw: Vec<u8>,
    pub sent_copy: bool,
}

/// A message the server took, whose copy is to be appended to Sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SentCopy {
    pub id: i64,
    pub message_id: String,
    /// The message as it goes in Sent: its Bcc header, then the message sent.
    pub raw: Vec<u8>,
    /// How many times appending it was started so far: a try a killed daemon left without its
    /// answer counts.
    pub attempts: u32,
}

/// The longest wait before a message is tried again, in milliseconds.
const RETRY_MAX: i64 = 5 * 60 * 1000;

/// A message that may have reached the server: see [`Store::sending_left`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Left {
    pub id: i64,
    pub message_id: String,
}

impl Store {
    /// Puts a message in the account's outbox; returns its number there. A message sent from a
    /// draft names it (its Message-ID): the draft is removed once the server took the message.
    pub fn enqueue(
        &self,
        account: i64,
        built: &Built,
        sent_copy: bool,
        draft: Option<&str>,
    ) -> rusqlite::Result<i64> {
        self.insert_outbox(account, built, sent_copy, draft, None)
    }

    /// Puts a reply in the account's outbox. Once the server took it, the message it answers
    /// (`answers`, its identifier) is marked `\Answered`, through the queue of actions.
    pub fn enqueue_reply(
        &self,
        account: i64,
        built: &Built,
        sent_copy: bool,
        answers: &str,
    ) -> rusqlite::Result<i64> {
        self.insert_outbox(account, built, sent_copy, None, Some(answers))
    }

    fn insert_outbox(
        &self,
        account: i64,
        built: &Built,
        sent_copy: bool,
        draft: Option<&str>,
        answers: Option<&str>,
    ) -> rusqlite::Result<i64> {
        self.connection.query_row(
            "INSERT INTO outbox (account, sender, recipients, message_id, raw, bcc_header,
                                 sent_copy, queued_at, draft, answers)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, unixepoch(), ?8, ?9)
             RETURNING id",
            params![
                account,
                built.sender,
                serde_json::to_string(&built.recipients).expect("strings serialize"),
                built.message_id,
                built.raw,
                built.bcc_header,
                sent_copy,
                draft,
                answers
            ],
            |r| r.get(0),
        )
    }

    /// The messages a daemon left `sending` when it stopped (killed, most likely): they become
    /// `uncertain`, since their end of data may have reached the server.
    pub fn sending_left(&self, account: i64) -> rusqlite::Result<Vec<Left>> {
        self.connection
            .prepare(
                "UPDATE outbox SET state = 'uncertain',
                        last_error = 'the daemon stopped while the message was being sent'
                  WHERE account = ?1 AND state = 'sending'
                  RETURNING id, message_id",
            )?
            .query_map([account], |r| {
                Ok(Left {
                    id: r.get(0)?,
                    message_id: r.get(1)?,
                })
            })?
            .collect()
    }

    /// The messages a daemon left `claimed` when it stopped: nothing of them was sent yet, so
    /// they go back to the queue. Only at the daemon's start, when no worker can hold a claim.
    pub fn release_claimed(&self) -> rusqlite::Result<usize> {
        self.connection.execute(
            "UPDATE outbox SET state = 'queued' WHERE state = 'claimed'",
            [],
        )
    }

    /// When the account's next message to send may be tried, in milliseconds since the epoch (0:
    /// at once); `None` when it has none.
    pub fn next_due(&self, account: i64) -> rusqlite::Result<Option<i64>> {
        self.connection.query_row(
            "SELECT min(next_attempt_at) FROM outbox WHERE account = ?1 AND state = 'queued'",
            [account],
            |r| r.get(0),
        )
    }

    /// Claims the account's first message to send whose time has come at `now` (milliseconds
    /// since the epoch), before any command is sent for it: in one statement, so that two workers
    /// of the same account (an old one whose account was removed and added again, and its
    /// successor) never both take it.
    pub fn claim_next(&self, account: i64, now: i64) -> rusqlite::Result<Option<ToSend>> {
        self.connection
            .query_row(
                "UPDATE outbox SET state = 'claimed'
                  WHERE id = (SELECT id FROM outbox WHERE account = ?1 AND state = 'queued'
                                                      AND next_attempt_at <= ?2
                               ORDER BY id LIMIT 1)
                    AND state = 'queued'
                  RETURNING id, sender, recipients, message_id, raw, sent_copy",
                params![account, now],
                |r| {
                    let recipients: String = r.get(2)?;
                    Ok(ToSend {
                        id: r.get(0)?,
                        sender: r.get(1)?,
                        recipients: serde_json::from_str(&recipients).unwrap_or_default(),
                        message_id: r.get(3)?,
                        raw: r.get(4)?,
                        sent_copy: r.get(5)?,
                    })
                },
            )
            .optional()
    }

    /// Records that a message's end of data is about to be written; an error, and the end not
    /// written, when the message is no longer claimed (its account was removed meanwhile).
    pub fn sending(&self, id: i64) -> rusqlite::Result<()> {
        self.connection.query_row(
            "UPDATE outbox SET state = 'sending' WHERE id = ?1 AND state = 'claimed'
             RETURNING id",
            [id],
            |_| Ok(()),
        )
    }

    /// A try that failed before the end of data, at `now`: the message stays to send, after a
    /// wait of its own (1 second, then twice as long at each try, up to 5 minutes).
    pub fn send_again(&self, id: i64, why: &str, now: i64) -> rusqlite::Result<()> {
        self.connection.execute(
            "UPDATE outbox SET state = 'queued', attempts = attempts + 1, last_error = ?2,
                    next_attempt_at = ?3 + min(1000 << min(attempts, 9), ?4)
              WHERE id = ?1",
            params![id, why, now, RETRY_MAX],
        )?;
        Ok(())
    }

    /// The server refused the message for good.
    pub fn send_failed(&self, id: i64, why: &str) -> rusqlite::Result<()> {
        self.set(id, "failed", Some(why))
    }

    /// The connection was lost after the end of data.
    pub fn send_uncertain(&self, id: i64, why: &str) -> rusqlite::Result<()> {
        self.set(id, "uncertain", Some(why))
    }

    /// The server took the message: it leaves the outbox, or waits for its copy in Sent when
    /// `copy`. The draft it was sent from, if any, is to be removed, and the message it answers,
    /// if any, is to be marked `\Answered`, in the same transaction. Whether an action was
    /// recorded for the latter: the original may be gone, or being moved, by now, and then
    /// there is nothing to mark.
    pub fn sent(&mut self, id: i64, copy: bool) -> rusqlite::Result<bool> {
        let transaction = self.connection.transaction()?;
        let row: Option<(i64, Option<String>, Option<String>)> = transaction
            .query_row(
                "SELECT account, draft, answers FROM outbox WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let mut answered = false;
        if let Some((account, draft, answers)) = row {
            if let Some(draft) = draft {
                super::drafts::remove_draft(&transaction, account, &draft)?;
            }
            if let Some(answers) = answers {
                answered = super::queue::queue_in(
                    &transaction,
                    &[answers],
                    &Wanted::Mark(Kind::Answered),
                )?
                .is_ok();
            }
        }
        if copy {
            transaction.execute("UPDATE outbox SET state = 'sent' WHERE id = ?1", [id])?;
        } else {
            transaction.execute("DELETE FROM outbox WHERE id = ?1", [id])?;
        }
        transaction.commit()?;
        Ok(answered)
    }

    fn set(&self, id: i64, state: &str, why: Option<&str>) -> rusqlite::Result<()> {
        self.connection.execute(
            "UPDATE outbox SET state = ?2, last_error = coalesce(?3, last_error) WHERE id = ?1",
            params![id, state, why],
        )?;
        Ok(())
    }

    /// The account's folder with the role `sent`, by row and name, when the copy knows one.
    pub fn sent_folder(&self, account: i64) -> rusqlite::Result<Option<(i64, String)>> {
        self.connection
            .query_row(
                "SELECT id, name FROM folder WHERE account = ?1 AND role = 'sent'
                  ORDER BY id LIMIT 1",
                [account],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
    }

    /// Whether the copy knows any folder of the account: until it does, whether the account has
    /// a Sent folder is not known.
    pub fn knows_folders(&self, account: i64) -> rusqlite::Result<bool> {
        self.connection.query_row(
            "SELECT EXISTS (SELECT 1 FROM folder WHERE account = ?1)",
            [account],
            |r| r.get(0),
        )
    }

    /// The messages sent whose copy is to be appended to Sent, oldest first.
    pub fn sent_copies(&self, account: i64) -> rusqlite::Result<Vec<SentCopy>> {
        self.connection
            .prepare(
                "SELECT id, message_id, bcc_header, raw, copy_attempts
                   FROM outbox WHERE account = ?1 AND state = 'sent' ORDER BY id",
            )?
            .query_map([account], |r| {
                let mut raw: Vec<u8> = r.get::<_, Option<Vec<u8>>>(2)?.unwrap_or_default();
                raw.extend(r.get::<_, Vec<u8>>(3)?);
                Ok(SentCopy {
                    id: r.get(0)?,
                    message_id: r.get(1)?,
                    raw,
                    attempts: r.get(4)?,
                })
            })?
            .collect()
    }

    /// A copy about to be appended; returns how many times appending it was started, this one
    /// included. Counted before the `APPEND`, so that a daemon killed during it leaves the next
    /// one to look in Sent first.
    pub fn copy_trying(&self, id: i64) -> rusqlite::Result<u32> {
        self.connection.query_row(
            "UPDATE outbox SET copy_attempts = copy_attempts + 1 WHERE id = ?1
             RETURNING copy_attempts",
            [id],
            |r| r.get(0),
        )
    }

    /// The copy is in Sent, or will never be: the message leaves the outbox.
    pub fn copy_done(&self, id: i64) -> rusqlite::Result<()> {
        self.connection
            .execute("DELETE FROM outbox WHERE id = ?1 AND state = 'sent'", [id])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn built(n: u32) -> Built {
        Built {
            sender: "alice@example.org".to_owned(),
            recipients: vec![
                "bob@example.com".to_owned(),
                "hidden@example.net".to_owned(),
            ],
            message_id: format!("{n}@example.org"),
            raw: format!("Subject: {n}\r\n\r\nHello\r\n").into_bytes(),
            bcc_header: (n == 1).then(|| b"Bcc: <hidden@example.net>\r\n".to_vec()),
        }
    }

    fn open() -> (tempfile::TempDir, Store, i64) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("copy.sqlite")).unwrap();
        let account = store.account("alice").unwrap();
        (dir, store, account)
    }

    fn states(store: &Store) -> Vec<(i64, String)> {
        store
            .connection
            .prepare("SELECT id, state FROM outbox ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    #[test]
    fn messages_are_sent_in_order_and_one_that_failed_waits_no_more() {
        let (_dir, store, account) = open();
        let one = store.enqueue(account, &built(1), true, None).unwrap();
        let two = store.enqueue(account, &built(2), false, None).unwrap();

        let next = store.claim_next(account, 0).unwrap().unwrap();
        assert_eq!(next.id, one);
        assert_eq!(next.recipients, built(1).recipients);
        assert_eq!(next.raw, built(1).raw);
        assert!(next.sent_copy);

        store.send_again(one, "451", 0).unwrap();
        assert_eq!(store.claim_next(account, 1000).unwrap().unwrap().id, one);
        store.send_failed(one, "554").unwrap();
        assert_eq!(store.claim_next(account, 0).unwrap().unwrap().id, two);
        store.send_uncertain(two, "cut").unwrap();
        assert_eq!(store.claim_next(account, 0).unwrap(), None);
        assert_eq!(
            states(&store),
            [(one, "failed".to_owned()), (two, "uncertain".to_owned())]
        );
    }

    #[test]
    fn a_message_tried_again_waits_for_its_time_and_the_others_do_not() {
        let (_dir, store, account) = open();
        let one = store.enqueue(account, &built(1), true, None).unwrap();
        let two = store.enqueue(account, &built(2), true, None).unwrap();
        let now = 1_790_000_000_000;
        assert_eq!(store.next_due(account).unwrap(), Some(0));

        assert_eq!(store.claim_next(account, now).unwrap().unwrap().id, one);
        store.send_again(one, "451", now).unwrap();
        // The second goes while the first waits: 1 second, then twice as long each time.
        assert_eq!(store.next_due(account).unwrap(), Some(0));
        assert_eq!(store.claim_next(account, now).unwrap().unwrap().id, two);
        store.send_again(two, "451", now).unwrap();
        assert_eq!(store.claim_next(account, now + 999).unwrap(), None);
        assert_eq!(store.next_due(account).unwrap(), Some(now + 1000));
        assert_eq!(
            store.claim_next(account, now + 1000).unwrap().unwrap().id,
            one
        );
        store.send_again(one, "451", now + 1000).unwrap();
        assert_eq!(
            store.claim_next(account, now + 1000).unwrap().unwrap().id,
            two
        );
        store.send_failed(two, "554").unwrap();
        assert_eq!(store.next_due(account).unwrap(), Some(now + 3000));
        assert_eq!(store.claim_next(account, now + 2999).unwrap(), None);
        assert_eq!(
            store.claim_next(account, now + 3000).unwrap().unwrap().id,
            one
        );
        // Never more than 5 minutes.
        for _ in 0..20 {
            store.send_again(one, "451", now).unwrap();
            store.claim_next(account, now + 300_000).unwrap().unwrap();
        }
        store.send_again(one, "451", now).unwrap();
        assert_eq!(store.next_due(account).unwrap(), Some(now + 300_000));
    }

    #[test]
    fn a_message_is_claimed_by_one_worker_only() {
        let (dir, store, account) = open();
        let other = Store::open(&dir.path().join("copy.sqlite")).unwrap();
        let one = store.enqueue(account, &built(1), true, None).unwrap();

        assert_eq!(store.claim_next(account, 0).unwrap().unwrap().id, one);
        assert_eq!(other.claim_next(account, 0).unwrap(), None);
        assert_eq!(states(&store), [(one, "claimed".to_owned())]);
        // Only the row's claim lets its end of data go.
        let two = store.enqueue(account, &built(2), true, None).unwrap();
        assert!(store.sending(two).is_err());
        store.sending(one).unwrap();
        assert_eq!(
            states(&store),
            [(one, "sending".to_owned()), (two, "queued".to_owned())]
        );
    }

    #[test]
    fn a_claimed_message_found_at_start_goes_back_to_the_queue() {
        let (_dir, store, account) = open();
        let one = store.enqueue(account, &built(1), true, None).unwrap();
        let two = store.enqueue(account, &built(2), true, None).unwrap();
        store.claim_next(account, 0).unwrap();
        store.claim_next(account, 0).unwrap();
        store.sending(two).unwrap();

        assert_eq!(store.release_claimed().unwrap(), 1);

        assert_eq!(
            states(&store),
            [(one, "queued".to_owned()), (two, "sending".to_owned())]
        );
    }

    #[test]
    fn a_message_left_sending_by_a_daemon_that_stopped_becomes_uncertain() {
        let (_dir, store, account) = open();
        let one = store.enqueue(account, &built(1), true, None).unwrap();
        let two = store.enqueue(account, &built(2), true, None).unwrap();
        store.claim_next(account, 0).unwrap();
        store.sending(one).unwrap();

        let left = store.sending_left(account).unwrap();

        assert_eq!(
            left,
            [Left {
                id: one,
                message_id: "1@example.org".to_owned()
            }]
        );
        assert_eq!(
            states(&store),
            [(one, "uncertain".to_owned()), (two, "queued".to_owned())]
        );
        assert_eq!(store.claim_next(account, 0).unwrap().unwrap().id, two);
    }

    #[test]
    fn a_message_sent_leaves_the_outbox_once_its_copy_is_in_sent() {
        let (_dir, mut store, account) = open();
        let one = store.enqueue(account, &built(1), true, None).unwrap();
        let two = store.enqueue(account, &built(2), false, None).unwrap();
        store.sent(one, true).unwrap();
        store.sent(two, false).unwrap();
        assert_eq!(states(&store), [(one, "sent".to_owned())]);

        let copies = store.sent_copies(account).unwrap();
        assert_eq!(copies.len(), 1);
        assert_eq!(
            copies[0].raw,
            b"Bcc: <hidden@example.net>\r\nSubject: 1\r\n\r\nHello\r\n"
        );
        assert_eq!(store.copy_trying(one).unwrap(), 1);
        assert_eq!(store.sent_copies(account).unwrap()[0].attempts, 1);
        store.copy_done(one).unwrap();
        assert!(states(&store).is_empty());
    }

    /// The account's inbox holds UID 1; returns the inbox row and the message's identifier.
    fn with_a_message(store: &mut Store, account: i64) -> (i64, String) {
        let inbox = store.folder(account, "INBOX", Some("inbox")).unwrap().id;
        store.rebuild(inbox, 10).unwrap();
        let raw = b"From: carol@example.com\r\nSubject: Hi\r\n\r\nHello.\r\n".to_vec();
        store
            .insert(
                inbox,
                &[crate::store::NewMessage {
                    uid: 1,
                    flags: "\\Seen".to_owned(),
                    internal_date: Some(1_790_000_000),
                    size: raw.len() as i64,
                    raw,
                }],
            )
            .unwrap();
        (inbox, crate::copy::message_id("alice", inbox, 10, 1))
    }

    fn actions(store: &Store) -> Vec<String> {
        store
            .connection
            .prepare("SELECT kind FROM action ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    #[test]
    fn a_reply_marks_the_original_answered_only_once_it_is_sent() {
        let (_dir, mut store, account) = open();
        let (inbox, original) = with_a_message(&mut store, account);
        let reply = store
            .enqueue_reply(account, &built(1), false, &original)
            .unwrap();

        // Waiting to be sent, it marks nothing.
        assert!(actions(&store).is_empty());
        assert_eq!(store.flags(inbox).unwrap()[&1], "\\Seen");

        assert!(store.sent(reply, false).unwrap());
        assert_eq!(actions(&store), ["answered"]);
        assert_eq!(store.flags(inbox).unwrap()[&1], "\\Answered \\Seen");
        assert!(states(&store).is_empty());
    }

    #[test]
    fn a_message_sent_that_answers_nothing_marks_nothing() {
        let (_dir, mut store, account) = open();
        with_a_message(&mut store, account);
        let one = store.enqueue(account, &built(1), false, None).unwrap();
        assert!(!store.sent(one, false).unwrap());
        assert!(actions(&store).is_empty());
    }

    #[test]
    fn a_reply_sent_whose_original_is_gone_is_sent_all_the_same() {
        let (_dir, mut store, account) = open();
        let (inbox, original) = with_a_message(&mut store, account);
        let reply = store
            .enqueue_reply(account, &built(1), true, &original)
            .unwrap();
        store.rebuild(inbox, 11).unwrap();

        assert!(!store.sent(reply, true).unwrap());
        assert!(actions(&store).is_empty());
        assert_eq!(states(&store), [(reply, "sent".to_owned())]);
    }
}
