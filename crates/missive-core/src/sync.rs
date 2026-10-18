//! Bringing the local copy of an account up to date with its IMAP server, read only.
//!
//! Only commands that change nothing on the server are sent: `LIST`, `EXAMINE` (never `SELECT`),
//! `UID SEARCH`, `UID FETCH` of `FLAGS`, `INTERNALDATE`, `RFC822.SIZE` and `BODY.PEEK[]` (never
//! `BODY[]` or `RFC822`, which mark a message as read), `ENABLE CONDSTORE`, `IDLE`, `NOOP` and
//! `LOGOUT`.
//!
//! A folder is synced in three steps, each one written to the copy in its own transaction:
//!
//! 1. `EXAMINE`. When its UIDVALIDITY is not the one the copy has, the server renumbered the
//!    folder: the copy of that folder is emptied, and refilled by the steps below.
//! 2. A complete listing of its UIDs: with CONDSTORE, `UID SEARCH ALL` and the flags changed since
//!    the HIGHESTMODSEQ in the copy (`UID FETCH 1:* (FLAGS) (CHANGEDSINCE …)`); without it,
//!    `UID FETCH 1:* (FLAGS)`, which gives both. A message of the copy missing from that listing
//!    was expunged: it leaves the copy, in the same transaction as the flag changes and the new
//!    HIGHESTMODSEQ.
//! 3. The messages of the listing missing from the copy, newest first, fetched whole in batches.
//!
//! The sync point is the set of UIDs in the copy itself, written with the messages: a sync cut at
//! any point (a lost connection, a killed daemon) leaves whole batches behind, and the next one
//! fetches what is still missing. Nothing is removed but what a complete listing no longer holds.

use std::collections::{BTreeSet, HashMap};

use async_imap::types::{Fetch, Flag, NameAttribute};
use futures_util::TryStreamExt;

use crate::connect::{ConnectError, ImapSession, imap_error};
use crate::store::{FolderRow, NewMessage, Store};

/// Why a sync stopped.
#[derive(Debug)]
pub enum SyncError {
    /// The connection failed or the server answered something unexpected: the account is
    /// reconnected later.
    Connection(ConnectError),
    /// The local copy could not be written.
    Store(rusqlite::Error),
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncError::Connection(error) => write!(f, "{error}"),
            SyncError::Store(error) => write!(f, "the local copy could not be written ({error})"),
        }
    }
}

impl From<async_imap::error::Error> for SyncError {
    fn from(error: async_imap::error::Error) -> Self {
        SyncError::Connection(imap_error(error))
    }
}

impl From<rusqlite::Error> for SyncError {
    fn from(error: rusqlite::Error) -> Self {
        SyncError::Store(error)
    }
}

type Result<T> = std::result::Result<T, SyncError>;

/// A folder to sync: its name on the server, its role, and its row in the copy.
#[derive(Clone, Debug)]
pub struct Folder {
    pub name: String,
    pub role: Option<&'static str>,
    pub row: FolderRow,
}

/// What a sync changed in the copy: counts only, never what the mail says.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Changes {
    pub new: usize,
    pub removed: usize,
    pub flags: usize,
    pub rebuilt: usize,
}

impl std::ops::AddAssign for Changes {
    fn add_assign(&mut self, other: Changes) {
        self.new += other.new;
        self.removed += other.removed;
        self.flags += other.flags;
        self.rebuilt += other.rebuilt;
    }
}

/// What the server can do that a sync uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    pub condstore: bool,
    pub idle: bool,
}

/// Reads the server's capabilities, and asks it to send HIGHESTMODSEQ and MODSEQ from now on
/// when it can.
pub async fn prepare(session: &mut ImapSession) -> Result<Capabilities> {
    let capabilities = session.capabilities().await?;
    let condstore = capabilities.has_str("CONDSTORE") && capabilities.has_str("ENABLE");
    if condstore {
        session.run_command_and_check_ok("ENABLE CONDSTORE").await?;
    }
    Ok(Capabilities {
        condstore,
        idle: capabilities.has_str("IDLE"),
    })
}

/// The account's folders that can hold messages, the inbox first, each added to the copy. The
/// store is taken `&mut` although only read: it is not `Sync`, and the future must be `Send`.
pub async fn folders(
    session: &mut ImapSession,
    store: &mut Store,
    account: i64,
) -> Result<Vec<Folder>> {
    let names: Vec<_> = session
        .list(Some(""), Some("*"))
        .await?
        .try_collect()
        .await?;
    let mut folders = Vec::new();
    for name in &names {
        let attributes = name.attributes();
        if attributes
            .iter()
            .any(|a| matches!(a, NameAttribute::NoSelect))
            || attributes.iter().any(
                |a| matches!(a, NameAttribute::Extension(e) if e.eq_ignore_ascii_case("\\NonExistent")),
            )
        {
            continue;
        }
        let role = if name.name().eq_ignore_ascii_case("INBOX") {
            Some("inbox")
        } else {
            attributes.iter().find_map(|a| match a {
                NameAttribute::Sent => Some("sent"),
                NameAttribute::Drafts => Some("drafts"),
                NameAttribute::Trash => Some("trash"),
                NameAttribute::Junk => Some("junk"),
                NameAttribute::Archive => Some("archive"),
                _ => None,
            })
        };
        let row = store.folder(account, name.name(), role)?;
        folders.push(Folder {
            name: name.name().to_owned(),
            role,
            row,
        });
    }
    // Stable: the others stay in the server's order.
    folders.sort_by_key(|folder| folder.role != Some("inbox"));
    Ok(folders)
}

/// Syncs one folder (see the module's documentation), fetching new messages `batch` at a time.
/// `None` when the server refused to examine it (it was deleted since it was listed, say).
pub async fn folder(
    session: &mut ImapSession,
    store: &mut Store,
    folder: &mut Folder,
    batch: usize,
) -> Result<Option<Changes>> {
    let mailbox = match session.examine(&folder.name).await {
        Ok(mailbox) => mailbox,
        Err(async_imap::error::Error::No(_)) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut changes = Changes::default();
    let uidvalidity = mailbox.uid_validity.ok_or_else(|| {
        SyncError::Connection(ConnectError::Unreachable(
            "the IMAP server gave no UIDVALIDITY".to_owned(),
        ))
    })?;
    if folder.row.uidvalidity != Some(uidvalidity) {
        if folder.row.uidvalidity.is_some() {
            changes.rebuilt = 1;
        }
        store.rebuild(folder.row.id, uidvalidity)?;
        folder.row.uidvalidity = Some(uidvalidity);
        folder.row.highest_modseq = None;
    }

    let local = store.flags(folder.row.id)?;
    let (listing, server_flags) = if mailbox.exists == 0 {
        (BTreeSet::new(), HashMap::new())
    } else if let Some(highest) = mailbox.highest_modseq {
        let listing: BTreeSet<u32> = session.uid_search("ALL").await?.into_iter().collect();
        let flags = match folder.row.highest_modseq {
            _ if local.is_empty() => HashMap::new(),
            Some(since) if since == highest => HashMap::new(),
            Some(since) if since < highest => {
                fetch_flags(session, &format!("(FLAGS) (CHANGEDSINCE {since})")).await?
            }
            // Never seen with CONDSTORE, or a server that went back: everything is checked.
            _ => fetch_flags(session, "(FLAGS)").await?,
        };
        (listing, flags)
    } else {
        let flags = fetch_flags(session, "(FLAGS)").await?;
        (flags.keys().copied().collect(), flags)
    };

    let removed: Vec<u32> = local
        .keys()
        .filter(|uid| !listing.contains(uid))
        .copied()
        .collect();
    let changed: Vec<(u32, String)> = server_flags
        .into_iter()
        .filter(|(uid, flags)| local.get(uid).is_some_and(|local| local != flags))
        .collect();
    changes.removed = removed.len();
    changes.flags = changed.len();
    store.apply_listing(folder.row.id, &removed, &changed, mailbox.highest_modseq)?;
    folder.row.highest_modseq = mailbox.highest_modseq;

    let missing: Vec<u32> = listing
        .iter()
        .rev()
        .filter(|uid| !local.contains_key(uid))
        .copied()
        .collect();
    for uids in missing.chunks(batch.max(1)) {
        let fetches: Vec<Fetch> = session
            .uid_fetch(
                uid_set(uids),
                "(UID FLAGS INTERNALDATE RFC822.SIZE BODY.PEEK[])",
            )
            .await?
            .try_collect()
            .await?;
        // A message expunged meanwhile is simply not there; the next listing tells.
        let messages: Vec<NewMessage> = fetches
            .iter()
            .filter_map(|fetch| {
                let uid = fetch.uid.filter(|uid| uids.contains(uid))?;
                Some(NewMessage {
                    uid,
                    flags: flags(fetch),
                    internal_date: fetch.internal_date().map(|date| date.timestamp()),
                    raw: fetch.body()?.to_vec(),
                })
            })
            .collect();
        changes.new += messages.len();
        store.insert(folder.row.id, &messages)?;
    }
    Ok(Some(changes))
}

/// The flags of the folder's messages, by UID, from `UID FETCH 1:* <query>`.
async fn fetch_flags(session: &mut ImapSession, query: &str) -> Result<HashMap<u32, String>> {
    let fetches: Vec<Fetch> = session.uid_fetch("1:*", query).await?.try_collect().await?;
    Ok(fetches
        .iter()
        .filter_map(|fetch| Some((fetch.uid?, flags(fetch))))
        .collect())
}

/// A message's flags as the copy keeps them: IMAP spelling, sorted, `\Recent` left out (it
/// belongs to one session, not to the message).
fn flags(fetch: &Fetch) -> String {
    let mut flags: Vec<String> = fetch
        .flags()
        .filter_map(|flag| {
            Some(match flag {
                Flag::Seen => "\\Seen".to_owned(),
                Flag::Answered => "\\Answered".to_owned(),
                Flag::Flagged => "\\Flagged".to_owned(),
                Flag::Deleted => "\\Deleted".to_owned(),
                Flag::Draft => "\\Draft".to_owned(),
                Flag::Recent | Flag::MayCreate => return None,
                Flag::Custom(name) => name.into_owned(),
            })
        })
        .collect();
    flags.sort();
    flags.dedup();
    flags.join(" ")
}

/// A UID set for a command: runs of consecutive UIDs as ranges, `5:9,2`.
fn uid_set(uids: &[u32]) -> String {
    let mut sorted = uids.to_vec();
    sorted.sort_unstable();
    let mut parts: Vec<String> = Vec::new();
    let mut index = 0;
    while index < sorted.len() {
        let start = sorted[index];
        let mut end = start;
        while index + 1 < sorted.len() && sorted[index + 1] == end + 1 {
            index += 1;
            end = sorted[index];
        }
        parts.push(if start == end {
            start.to_string()
        } else {
            format!("{start}:{end}")
        });
        index += 1;
    }
    parts.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_uid_set_writes_runs_as_ranges() {
        assert_eq!(uid_set(&[9, 8, 7, 3, 1, 2, 12]), "1:3,7:9,12");
        assert_eq!(uid_set(&[4]), "4");
    }
}
