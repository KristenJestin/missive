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
//!
//! A listing is complete only when the command that gave it ended with a tagged `OK`: the answers
//! of `LIST`, `EXAMINE`, `UID SEARCH` and `UID FETCH` are read here, not by `async-imap`, which
//! takes the end of an answer for its success whatever its status, and a connection closed in
//! the middle of an answer for its end. A `NO` leaves the folder as it is until the next pass; a
//! `BAD`, a `BYE` or a closed connection is a connection error. A listing that ended with `OK`
//! but holds fewer messages than `EXISTS` said, less those the server expunged meanwhile, is a
//! connection error too.

use std::collections::{BTreeSet, HashMap};

use async_imap::imap_proto::{
    AttributeValue, MailboxDatum, NameAttribute, Response, ResponseCode, Status,
};
use async_imap::types::Flag;

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

impl SyncError {
    /// The server answered something a sync cannot use.
    fn protocol(what: &str) -> SyncError {
        SyncError::Connection(ConnectError::Unreachable(what.to_owned()))
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
    /// New messages the server refused to send (`NO`): they are asked for again at the next pass.
    pub refused: usize,
}

impl std::ops::AddAssign for Changes {
    fn add_assign(&mut self, other: Changes) {
        self.new += other.new;
        self.removed += other.removed;
        self.flags += other.flags;
        self.rebuilt += other.rebuilt;
        self.refused += other.refused;
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
    let mut names = Vec::new();
    let ended = run(session, "LIST \"\" \"*\"", |response| {
        if let Response::MailboxData(MailboxDatum::List(list)) = response {
            let attributes = &list.name_attributes;
            let selectable = !attributes.iter().any(|a| {
                matches!(a, NameAttribute::NoSelect)
                    || matches!(a, NameAttribute::Extension(e) if e.eq_ignore_ascii_case("\\NonExistent"))
            });
            if !selectable {
                return;
            }
            let role = if list.name.eq_ignore_ascii_case("INBOX") {
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
            names.push((list.name.to_string(), role));
        }
    })
    .await?;
    if ended == Ended::No {
        return Err(SyncError::protocol(
            "the IMAP server refused to list the folders",
        ));
    }
    let mut folders = Vec::new();
    for (name, role) in names {
        let row = store.folder(account, &name, role)?;
        folders.push(Folder { name, role, row });
    }
    // Stable: the others stay in the server's order.
    folders.sort_by_key(|folder| folder.role != Some("inbox"));
    Ok(folders)
}

/// Syncs one folder (see the module's documentation), fetching new messages `batch` at a time.
/// `None` when the server refused to examine or to list it (it was deleted since it was listed,
/// say): the copy of that folder is left as it is.
pub async fn folder(
    session: &mut ImapSession,
    store: &mut Store,
    folder: &mut Folder,
    batch: usize,
) -> Result<Option<Changes>> {
    let Some(mailbox) = examine(session, &folder.name).await? else {
        return Ok(None);
    };
    let mut changes = Changes::default();
    if folder.row.uidvalidity != Some(mailbox.uidvalidity) {
        if folder.row.uidvalidity.is_some() {
            changes.rebuilt = 1;
        }
        store.rebuild(folder.row.id, mailbox.uidvalidity)?;
        folder.row.uidvalidity = Some(mailbox.uidvalidity);
        folder.row.highest_modseq = None;
    }

    let local = store.flags(folder.row.id)?;
    // EXISTS is taken at its word only from an EXAMINE that ended with OK.
    let (listing, server_flags) = if mailbox.exists == 0 {
        (BTreeSet::new(), HashMap::new())
    } else if let Some(highest) = mailbox.highest_modseq {
        let mut listing = BTreeSet::new();
        let mut expunged = 0;
        let ended = run(session, "UID SEARCH ALL", |response| match response {
            Response::MailboxData(MailboxDatum::Search(uids)) => listing.extend(uids),
            Response::Expunge(_) => expunged += 1,
            _ => {}
        })
        .await?;
        if ended == Ended::No {
            return Ok(None);
        }
        complete(listing.len(), mailbox.exists, expunged)?;
        let flags = match folder.row.highest_modseq {
            _ if local.is_empty() => Some(HashMap::new()),
            Some(since) if since == highest => Some(HashMap::new()),
            Some(since) if since < highest => {
                fetch_flags(session, &format!("(FLAGS) (CHANGEDSINCE {since})"))
                    .await?
                    .map(|(flags, _)| flags)
            }
            // Never seen with CONDSTORE, or a server that went back: everything is checked.
            _ => fetch_flags(session, "(FLAGS)")
                .await?
                .map(|(flags, _)| flags),
        };
        let Some(flags) = flags else {
            return Ok(None);
        };
        (listing, flags)
    } else {
        let Some((flags, expunged)) = fetch_flags(session, "(FLAGS)").await? else {
            return Ok(None);
        };
        complete(flags.len(), mailbox.exists, expunged)?;
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
        let mut messages = Vec::new();
        let command = format!(
            "UID FETCH {} (UID FLAGS INTERNALDATE RFC822.SIZE BODY.PEEK[])",
            uid_set(uids)
        );
        let ended = run(session, &command, |response| {
            if let Response::Fetch(_, attributes) = response
                && let Some(message) = new_message(attributes)
                && uids.contains(&message.uid)
            {
                messages.push(message);
            }
        })
        .await;
        // The whole messages that came are kept, however the command ended. One that did not
        // come is still missing from the copy: the next pass asks for it again.
        changes.new += messages.len();
        store.insert(folder.row.id, &messages)?;
        // A message expunged meanwhile is simply not there; the next listing tells.
        if ended? == Ended::No {
            changes.refused += uids.len() - messages.len();
        }
    }
    Ok(Some(changes))
}

/// How a command the server answered ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ended {
    Ok,
    No,
}

/// Sends `command` and hands each response to `each` until the command's tagged answer. Only
/// that answer says whether what came before is whole: an `OK` or a `NO` is returned, and a
/// `BAD`, or a connection closed first (after a `BYE`, say), is an error.
async fn run(
    session: &mut ImapSession,
    command: &str,
    mut each: impl FnMut(&Response<'_>),
) -> Result<Ended> {
    let tag = session.run_command(command).await?;
    loop {
        let Some(response) = session
            .read_response()
            .await
            .map_err(async_imap::error::Error::Io)?
        else {
            return Err(async_imap::error::Error::ConnectionLost.into());
        };
        match response.parsed() {
            Response::Done {
                tag: done, status, ..
            } if *done == tag => {
                return match status {
                    Status::Ok => Ok(Ended::Ok),
                    Status::No => Ok(Ended::No),
                    _ => Err(SyncError::protocol(
                        "the IMAP server refused a command as malformed",
                    )),
                };
            }
            parsed => each(parsed),
        }
    }
}

/// What a sync needs from `EXAMINE`.
struct Examined {
    uidvalidity: u32,
    exists: u32,
    highest_modseq: Option<u64>,
}

/// `EXAMINE`: `None` when the server refused it. An answer without UIDVALIDITY or EXISTS, which
/// every server must send, is not used.
async fn examine(session: &mut ImapSession, name: &str) -> Result<Option<Examined>> {
    let (mut uidvalidity, mut exists, mut highest_modseq) = (None, None, None);
    let ended = run(
        session,
        &format!("EXAMINE {}", quote(name)?),
        |response| match response {
            Response::Data {
                status: Status::Ok,
                outcome,
            } => match outcome.code {
                Some(ResponseCode::UidValidity(value)) => uidvalidity = Some(value),
                Some(ResponseCode::HighestModSeq(value)) => highest_modseq = Some(value),
                _ => {}
            },
            Response::MailboxData(MailboxDatum::Exists(value)) => exists = Some(*value),
            _ => {}
        },
    )
    .await?;
    if ended == Ended::No {
        return Ok(None);
    }
    match (uidvalidity, exists) {
        (Some(uidvalidity), Some(exists)) => Ok(Some(Examined {
            uidvalidity,
            exists,
            highest_modseq,
        })),
        _ => Err(SyncError::protocol(
            "the IMAP server examined a folder without its UIDVALIDITY or EXISTS",
        )),
    }
}

/// Whether a listing of `listed` messages can be complete, when `EXAMINE` said `exists` and the
/// server expunged `expunged` while listing them.
fn complete(listed: usize, exists: u32, expunged: u32) -> Result<()> {
    if listed as u64 + u64::from(expunged) < u64::from(exists) {
        return Err(SyncError::protocol(
            "the IMAP server listed fewer messages than the folder holds",
        ));
    }
    Ok(())
}

/// The flags of the folder's messages, by UID, from `UID FETCH 1:* <query>`, with the number of
/// messages expunged meanwhile. `None` when the server refused it.
async fn fetch_flags(
    session: &mut ImapSession,
    query: &str,
) -> Result<Option<(HashMap<u32, String>, u32)>> {
    let mut found = HashMap::new();
    let mut expunged = 0;
    let ended = run(
        session,
        &format!("UID FETCH 1:* {query}"),
        |response| match response {
            Response::Fetch(_, attributes) => {
                if let Some(uid) = uid(attributes) {
                    found.insert(uid, flags(attributes));
                }
            }
            Response::Expunge(_) => expunged += 1,
            _ => {}
        },
    )
    .await?;
    Ok((ended == Ended::Ok).then_some((found, expunged)))
}

fn uid(attributes: &[AttributeValue<'_>]) -> Option<u32> {
    attributes.iter().find_map(|a| match a {
        AttributeValue::Uid(uid) => Some(*uid),
        _ => None,
    })
}

/// A whole message from a `FETCH` answer, `None` without its UID or its body.
fn new_message(attributes: &[AttributeValue<'_>]) -> Option<NewMessage> {
    let mut internal_date = None;
    let mut raw = None;
    for attribute in attributes {
        match attribute {
            AttributeValue::InternalDate(value) => internal_date = parse_internal_date(value),
            AttributeValue::BodySection {
                section: None,
                data: Some(data),
                ..
            } => raw = Some(data.to_vec()),
            _ => {}
        }
    }
    Some(NewMessage {
        uid: uid(attributes)?,
        flags: flags(attributes),
        internal_date,
        raw: raw?,
    })
}

/// A folder name as a quoted string; a name no command can carry is refused.
fn quote(name: &str) -> Result<String> {
    if name.contains(['\r', '\n']) {
        return Err(SyncError::protocol(
            "the IMAP server listed a folder name no command can carry",
        ));
    }
    Ok(format!(
        "\"{}\"",
        name.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// An IMAP internal date, `17-Oct-2026 09:30:00 +0200` (the day may be one digit after a space),
/// in seconds since the epoch.
fn parse_internal_date(text: &str) -> Option<i64> {
    let mut parts = text.trim_start().split(' ');
    let mut date = parts.next()?.split('-');
    let mut time = parts.next()?.split(':');
    let zone = parts.next()?;
    let day: i64 = date.next()?.parse().ok()?;
    let month = date.next()?;
    let month = MONTHS.iter().position(|m| m.eq_ignore_ascii_case(month))? as i64 + 1;
    let year: i64 = date.next()?.parse().ok()?;
    let mut clock = [0i64; 3];
    for value in &mut clock {
        *value = time.next()?.parse().ok()?;
    }
    let sign = match zone.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let zone: i64 = zone.get(1..5)?.parse().ok()?;
    let offset = sign * ((zone / 100) * 3600 + (zone % 100) * 60);
    Some(
        days_from_civil(year, month, day) * 86400 + clock[0] * 3600 + clock[1] * 60 + clock[2]
            - offset,
    )
}

/// Days since 1970-01-01 of a date of the proleptic Gregorian calendar (Howard Hinnant's
/// algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// A message's flags as the copy keeps them: IMAP spelling, sorted, `\Recent` left out (it
/// belongs to one session, not to the message).
fn flags(attributes: &[AttributeValue<'_>]) -> String {
    let mut flags: Vec<String> = attributes
        .iter()
        .filter_map(|a| match a {
            AttributeValue::Flags(flags) => Some(flags),
            _ => None,
        })
        .flatten()
        .filter_map(|flag| {
            let flag = Flag::from(flag.as_ref());
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
    fn an_internal_date_reads_as_seconds_since_the_epoch() {
        assert_eq!(
            parse_internal_date("17-Oct-2026 09:30:00 +0200"),
            Some(1_792_222_200)
        );
        assert_eq!(
            parse_internal_date(" 1-Jan-2026 00:00:00 +0000"),
            Some(1_767_225_600)
        );
        assert_eq!(
            parse_internal_date("29-Feb-2024 23:59:59 -0130"),
            Some(1_709_251_199 + 5400)
        );
        assert_eq!(parse_internal_date("17-Foo-2026 09:30:00 +0200"), None);
        assert_eq!(parse_internal_date("17-Oct-2026"), None);
    }

    #[test]
    fn a_listing_shorter_than_exists_less_the_expunged_is_refused() {
        assert!(complete(3, 3, 0).is_ok());
        assert!(complete(2, 3, 1).is_ok());
        // New mail since EXAMINE makes a listing longer, never shorter.
        assert!(complete(4, 3, 0).is_ok());
        assert!(complete(2, 3, 0).is_err());
        assert!(complete(0, 20000, 0).is_err());
    }

    #[test]
    fn a_folder_name_is_quoted_and_escaped() {
        assert_eq!(quote("INBOX").unwrap(), "\"INBOX\"");
        assert_eq!(quote("a \"b\" \\c").unwrap(), "\"a \\\"b\\\" \\\\c\"");
        assert!(quote("a\r\nb").is_err());
    }

    #[test]
    fn a_uid_set_writes_runs_as_ranges() {
        assert_eq!(uid_set(&[9, 8, 7, 3, 1, 2, 12]), "1:3,7:9,12");
        assert_eq!(uid_set(&[4]), "4");
    }
}
