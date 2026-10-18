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
//! 3. The messages of the listing missing from the copy, newest first, fetched whole in batches
//!    of at most `batch` messages and [`BATCH_BYTES`] bytes, by the sizes the server gives
//!    (`RFC822.SIZE`, asked first, for the missing UIDs only, in commands of bounded length); a
//!    message larger than that comes alone, and so does one the server gave no size for (a
//!    `NO` keeps the sizes that came before it), so that a refusal lands on it alone.
//!
//! The sync point is the set of UIDs in the copy itself, written with the messages: a sync cut at
//! any point (a lost connection, a killed daemon) leaves whole batches behind, and the next one
//! fetches what is still missing. Nothing is removed but what a complete listing no longer holds.
//!
//! A listing is complete only when the command that gave it ended with a tagged `OK`: the answers
//! of `LIST`, `EXAMINE`, `UID SEARCH` and `UID FETCH` are read here, not by `async-imap`, which
//! takes the end of an answer for its success whatever its status, and a connection closed in
//! the middle of an answer for its end. A `NO` leaves the folder as it is until the next pass, and
//! so does an answer a sync cannot use although the command ended with `OK` (an `EXAMINE` without
//! UIDVALIDITY or EXISTS; a listing shorter than `EXISTS` said, less the messages the server
//! expunged meanwhile): one folder never stops the others. A `BAD`, a `BYE` or a closed
//! connection is a connection error.

use std::collections::{BTreeSet, HashMap};

use async_imap::imap_proto::{
    AttributeValue, MailboxDatum, NameAttribute, RequestId, Response, ResponseCode, Status,
};
use async_imap::types::Flag;

use crate::connect::{ConnectError, ImapSession, imap_error};
use crate::store::{FolderRow, NewMessage, Store};

/// The most bytes of messages fetched, and held in memory, at a time: 32 MiB.
pub const BATCH_BYTES: u64 = 32 * 1024 * 1024;

/// The most bytes of a UID set in the commands that ask the sizes: a command line stays well
/// under the 8000 bytes servers have to take (RFC 7162), however scattered the UIDs are.
const SET_BYTES: usize = 900;

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
            names.push((unescape(&list.name), role));
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
/// `None` when the folder cannot be synced now: the server refused to examine or to list it (it
/// was deleted since it was listed, say), or answered with something no sync can use (no
/// UIDVALIDITY or EXISTS, a listing shorter than the folder), or its name cannot be sent. The
/// copy of that folder is left as it is, and the other folders are synced all the same; only a
/// `BAD` or a lost connection is an error.
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
        if !complete(listing.len(), mailbox.exists, expunged) {
            return Ok(None);
        }
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
        if !complete(flags.len(), mailbox.exists, expunged) {
            return Ok(None);
        }
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
    if missing.is_empty() {
        return Ok(Some(changes));
    }
    // The sizes of what is missing, in commands of a bounded length. A refusal (`NO`) keeps the
    // sizes that came before it: a message the server cannot read must not hold the others back.
    let mut sizes = HashMap::new();
    for set in uid_sets(&missing, SET_BYTES) {
        run(
            session,
            &format!("UID FETCH {set} (UID RFC822.SIZE)"),
            |response| {
                if let Response::Fetch(_, attributes) = response
                    && let Some(uid) = uid(attributes)
                    && let Some(size) = attributes.iter().find_map(|a| match a {
                        AttributeValue::Rfc822Size(size) => Some(*size),
                        _ => None,
                    })
                {
                    sizes.insert(uid, size);
                }
            },
        )
        .await?;
    }
    // One left without a size is fetched alone, so that a refusal lands on it alone.
    let missing: Vec<(u32, Option<u32>)> = missing
        .iter()
        .map(|uid| (*uid, sizes.get(uid).copied()))
        .collect();
    for uids in batches(&missing, batch, BATCH_BYTES) {
        let uids = &uids[..];
        let without_size = uids.iter().any(|uid| !sizes.contains_key(uid));
        let mut messages = Vec::new();
        let command = format!(
            "UID FETCH {} (UID FLAGS INTERNALDATE BODY.PEEK[])",
            uid_set(uids)
        );
        // Each answer is dropped once its message is taken out of it: a batch is held once.
        let ended = run(session, &command, |response| {
            if let Response::Fetch(_, attributes) = response
                && let Some(message) = new_message(attributes, &sizes)
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
        // A message expunged meanwhile is simply not there; the next listing tells. But one the
        // server did not size is told apart from it by nothing, so it is counted: the log says
        // that a message never came.
        if ended? == Ended::No || without_size {
            changes.refused += uids.len().saturating_sub(messages.len());
        }
    }
    Ok(Some(changes))
}

/// Cuts the messages to fetch, `(uid, size)` in the order given, into batches of at most `count`
/// messages and `bytes` bytes. A message larger than `bytes`, or whose size the server did not
/// give, makes a batch of its own.
fn batches(messages: &[(u32, Option<u32>)], count: usize, bytes: u64) -> Vec<Vec<u32>> {
    let mut batches = Vec::new();
    let mut current = Vec::new();
    let mut held = 0;
    for &(uid, size) in messages {
        let Some(size) = size else {
            batches.extend((!current.is_empty()).then(|| std::mem::take(&mut current)));
            batches.push(vec![uid]);
            held = 0;
            continue;
        };
        let size = u64::from(size);
        if !current.is_empty() && (current.len() >= count.max(1) || held + size > bytes) {
            batches.push(std::mem::take(&mut current));
            held = 0;
        }
        current.push(uid);
        held += size;
    }
    if !current.is_empty() {
        batches.push(current);
    }
    batches
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

/// Reads the next response on the connection and hands it to `each`, as [`run`] does; a
/// connection closed is an error.
async fn read<T>(session: &mut ImapSession, each: impl FnOnce(&Response<'_>) -> T) -> Result<T> {
    let Some(response) = session
        .read_response()
        .await
        .map_err(async_imap::error::Error::Io)?
    else {
        return Err(async_imap::error::Error::ConnectionLost.into());
    };
    Ok(each(response.parsed()))
}

/// `IDLE` in the selected folder, once the server started it: see [`idle`].
pub struct Idle {
    tag: RequestId,
    /// The server told of a change before it started IDLE.
    news: bool,
}

/// Sends `IDLE` and reads its answer until the server starts it (its `+`). `None` when the
/// server answered it with its tagged answer instead (`NO` or `BAD`, with or without text): the
/// command is over, the session is as it was, and does without IDLE. The answer is read here,
/// like the others: `async-imap`'s `Handle::init` panics on a `BAD` without text.
pub async fn idle(session: &mut ImapSession) -> Result<Option<Idle>> {
    let tag = session.run_command("IDLE").await?;
    let mut news = false;
    loop {
        let answer = read(session, |response| match response {
            Response::Continue { .. } => Some(true),
            Response::Done { tag: done, .. } if *done == tag => Some(false),
            response => {
                news |= is_news(response);
                None
            }
        })
        .await?;
        match answer {
            Some(true) => return Ok(Some(Idle { tag, news })),
            Some(false) => return Ok(None),
            None => {}
        }
    }
}

impl Idle {
    /// Waits until the server tells of a change in the folder; its keepalives (`* OK …`) are
    /// not one. It can be dropped at any point and called again: a response is taken from the
    /// connection only whole, and one that tells a change is returned at once.
    pub async fn news(&mut self, session: &mut ImapSession) -> Result<()> {
        if std::mem::take(&mut self.news) {
            return Ok(());
        }
        loop {
            let ended = read(session, |response| match response {
                Response::Done { tag, .. } if *tag == self.tag => Some(true),
                response => is_news(response).then_some(false),
            })
            .await?;
            match ended {
                Some(true) => {
                    return Err(SyncError::protocol("the IMAP server ended IDLE on its own"));
                }
                Some(false) => return Ok(()),
                None => {}
            }
        }
    }

    /// Ends IDLE (`DONE`) and reads until its tagged `OK`. What the server tells meanwhile is
    /// left to the sync that follows.
    pub async fn done(self, session: &mut ImapSession) -> Result<()> {
        session.run_command_untagged("DONE").await?;
        loop {
            let ended = read(session, |response| match response {
                Response::Done { tag, status, .. } if *tag == self.tag => {
                    Some(matches!(status, Status::Ok))
                }
                _ => None,
            })
            .await?;
            match ended {
                Some(true) => return Ok(()),
                Some(false) => return Err(SyncError::protocol("the IMAP server did not end IDLE")),
                None => {}
            }
        }
    }
}

/// Whether a response during IDLE tells of a change: anything but a keepalive.
fn is_news(response: &Response<'_>) -> bool {
    !matches!(
        response,
        Response::Data {
            status: Status::Ok,
            ..
        } | Response::Continue { .. }
    )
}

/// What a sync needs from `EXAMINE`.
struct Examined {
    uidvalidity: u32,
    exists: u32,
    highest_modseq: Option<u64>,
}

/// `EXAMINE`: `None` when the server refused it, or when its answer holds no UIDVALIDITY or
/// EXISTS, which every server must send: such an answer is not used.
async fn examine(session: &mut ImapSession, name: &str) -> Result<Option<Examined>> {
    let Some(name) = quote(name) else {
        return Ok(None);
    };
    let (mut uidvalidity, mut exists, mut highest_modseq) = (None, None, None);
    let ended = run(
        session,
        &format!("EXAMINE {name}"),
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
    Ok(uidvalidity
        .zip(exists)
        .map(|(uidvalidity, exists)| Examined {
            uidvalidity,
            exists,
            highest_modseq,
        }))
}

/// Whether a listing of `listed` messages can be complete, when `EXAMINE` said `exists` and the
/// server expunged `expunged` while listing them.
fn complete(listed: usize, exists: u32, expunged: u32) -> bool {
    listed as u64 + u64::from(expunged) >= u64::from(exists)
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

/// A whole message from a `FETCH` answer, `None` without its UID or its body. Its size is the
/// one the server gave in `sizes`, else the length of its body.
fn new_message(attributes: &[AttributeValue<'_>], sizes: &HashMap<u32, u32>) -> Option<NewMessage> {
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
    let uid = uid(attributes)?;
    let raw = raw?;
    Some(NewMessage {
        uid,
        flags: flags(attributes),
        internal_date,
        size: sizes
            .get(&uid)
            .map_or(raw.len() as i64, |size| i64::from(*size)),
        raw,
    })
}

/// The name `LIST` gave, without the escapes of its quoted string (`\\` and `\"`): `imap-proto`
/// hands the string over as it was written.
fn unescape(name: &str) -> String {
    let mut unescaped = String::with_capacity(name.len());
    let mut chars = name.chars();
    while let Some(c) = chars.next() {
        unescaped.push(if c == '\\' {
            chars.next().unwrap_or(c)
        } else {
            c
        });
    }
    unescaped
}

/// A folder name as a quoted string; `None` for a name no command can carry.
fn quote(name: &str) -> Option<String> {
    if name.contains(['\r', '\n']) {
        return None;
    }
    Some(format!(
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

/// Runs of consecutive UIDs as ranges, in ascending order: `2`, `5:9`.
fn uid_runs(uids: &[u32]) -> Vec<String> {
    let mut sorted = uids.to_vec();
    sorted.sort_unstable();
    let mut runs: Vec<String> = Vec::new();
    let mut index = 0;
    while index < sorted.len() {
        let start = sorted[index];
        let mut end = start;
        while index + 1 < sorted.len() && sorted[index + 1] == end + 1 {
            index += 1;
            end = sorted[index];
        }
        runs.push(if start == end {
            start.to_string()
        } else {
            format!("{start}:{end}")
        });
        index += 1;
    }
    runs
}

/// A UID set for a command: runs of consecutive UIDs as ranges, `2,5:9`.
fn uid_set(uids: &[u32]) -> String {
    uid_runs(uids).join(",")
}

/// The UIDs as sets of at most `limit` bytes each (a single run is never cut).
fn uid_sets(uids: &[u32], limit: usize) -> Vec<String> {
    let mut sets = Vec::new();
    let mut current = String::new();
    for run in uid_runs(uids) {
        if !current.is_empty() && current.len() + 1 + run.len() > limit {
            sets.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(',');
        }
        current.push_str(&run);
    }
    sets.extend((!current.is_empty()).then_some(current));
    sets
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
        assert!(complete(3, 3, 0));
        assert!(complete(2, 3, 1));
        // New mail since EXAMINE makes a listing longer, never shorter.
        assert!(complete(4, 3, 0));
        assert!(!complete(2, 3, 0));
        assert!(!complete(0, 20000, 0));
    }

    #[test]
    fn a_folder_name_is_quoted_and_escaped() {
        assert_eq!(quote("INBOX").unwrap(), "\"INBOX\"");
        assert_eq!(quote("a \"b\" \\c").unwrap(), "\"a \\\"b\\\" \\\\c\"");
        assert!(quote("a\r\nb").is_none());
    }

    #[test]
    fn a_listed_folder_name_loses_the_escapes_of_its_quoted_string() {
        assert_eq!(unescape("INBOX"), "INBOX");
        assert_eq!(unescape("a\\\"b\\\\c"), "a\"b\\c");
        // What quote() writes, unescape() reads back.
        let name = "a \"b\" \\c\\";
        let quoted = quote(name).unwrap();
        assert_eq!(unescape(&quoted[1..quoted.len() - 1]), name);
    }

    #[test]
    fn batches_are_cut_by_count_and_by_size_and_a_big_message_comes_alone() {
        let messages = [
            (9, Some(10)),
            (8, Some(10)),
            (7, Some(10)),
            (6, Some(10)),
            (5, Some(100)),
            (4, Some(10)),
            (3, Some(30)),
        ];
        assert_eq!(
            batches(&messages, 3, 50),
            [vec![9, 8, 7], vec![6], vec![5], vec![4, 3]]
        );
        assert_eq!(batches(&messages[..2], 50, 20), [vec![9, 8]]);
        assert_eq!(batches(&[(1, Some(21))], 50, 20), [vec![1]]);
        assert!(batches(&[], 50, 20).is_empty());
    }

    #[test]
    fn a_message_without_a_size_comes_alone_where_it_stands() {
        let messages = [
            (9, Some(10)),
            (8, None),
            (7, Some(10)),
            (6, Some(10)),
            (5, None),
            (4, None),
        ];
        assert_eq!(
            batches(&messages, 50, 1000),
            [vec![9], vec![8], vec![7, 6], vec![5], vec![4]]
        );
    }

    #[test]
    fn a_uid_set_writes_runs_as_ranges() {
        assert_eq!(uid_set(&[9, 8, 7, 3, 1, 2, 12]), "1:3,7:9,12");
        assert_eq!(uid_set(&[4]), "4");
    }

    #[test]
    fn uid_sets_are_cut_under_a_length_and_hold_every_uid_once() {
        let scattered: Vec<u32> = (1..=500).map(|n| n * 2).collect();
        let sets = uid_sets(&scattered, 100);
        assert!(sets.len() > 1);
        assert!(sets.iter().all(|set| set.len() <= 100), "{sets:?}");
        assert_eq!(sets.join(",").split(',').count(), 500);
        // A run is one piece, however long the limit.
        assert_eq!(uid_sets(&[3, 4, 5, 6, 7, 8, 9, 10], 3), ["3:10"]);
        assert_eq!(uid_sets(&[9, 1, 2, 7], 100), ["1:2,7,9"]);
        assert!(uid_sets(&[], 100).is_empty());
    }
}
