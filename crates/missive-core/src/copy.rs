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
//! comparing the UIDVALIDITY: [`Copy::message`] does, and says which of the two it is.
//!
//! A message whose move waits for the server is shown in the folder it goes to (its folder, its
//! role, its counts), while its identifier still names the folder the server has it in, where
//! an action on it applies: the identifier changes once the server moved it, as it does for a
//! message another client moves.

use std::path::Path;
use std::time::Duration;

use rusqlite::types::Value;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::store::{HALF_MOVE_LEFT, SCHEMA_VERSION, SkippedFolder};

/// Roles whose unread messages are left out of every total: nobody wants a badge for spam.
const NOT_COUNTED: [&str; 2] = ["trash", "junk"];

/// The identifier of a message: see the module's documentation. One function, for the hits of a
/// search and for the events alike.
pub fn message_id(account: &str, folder: i64, uidvalidity: u32, uid: u32) -> String {
    format!("{account}/{folder}/{uidvalidity}/{uid}")
}

/// The first schema version that has the search index.
const SEARCH_VERSION: i64 = 2;

/// The first schema version that says whether a message has an attachment.
const ATTACHMENTS_VERSION: i64 = 3;

/// The first schema version that has the queue of actions, and messages waiting for a move.
const QUEUE_VERSION: i64 = 4;

/// The first schema version that records the folders a pass skipped.
const SKIPPED_VERSION: i64 = 5;

/// The first schema version that has the outbox.
const OUTBOX_VERSION: i64 = 6;

/// The first schema version that has the drafts waiting for the server.
const DRAFTS_VERSION: i64 = 7;

/// The first schema version that records when a queued message is tried next (and, for a claimed
/// one, when it was claimed).
const RETRY_VERSION: i64 = 9;

/// The first schema version that records the moves refused after their copy.
const HALF_MOVE_VERSION: i64 = 10;

/// The first schema version that counts the searches of Sent that failed.
const SENT_SEARCH_VERSION: i64 = 12;

/// The first schema version that keeps the `\Deleted` flags a refused action left.
const DELETED_LEFT_VERSION: i64 = 15;

/// A message identifier read back, strictly: exactly what [`message_id`] writes. A number written
/// another way (`+1`, `01`, ` 1`) names no message, so that one message has one identifier.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageId {
    pub account: String,
    pub folder: i64,
    /// `0` for a folder that had not been examined when the identifier was made.
    pub uidvalidity: u32,
    pub uid: u32,
}

impl MessageId {
    pub fn parse(id: &str) -> Option<MessageId> {
        let parts: Vec<&str> = id.split('/').collect();
        let [account, folder, uidvalidity, uid] = parts[..] else {
            return None;
        };
        // `parse` accepts a sign and leading zeros; writing the number back refuses them.
        fn number<T: std::str::FromStr + ToString>(text: &str) -> Option<T> {
            let number: T = text.parse().ok()?;
            (number.to_string() == text).then_some(number)
        }
        (!account.is_empty()).then_some(())?;
        Some(MessageId {
            account: account.to_owned(),
            folder: number(folder)?,
            uidvalidity: number(uidvalidity)?,
            uid: number(uid)?,
        })
    }
}

/// Why the copy could not be read.
#[derive(Debug)]
pub enum CopyError {
    Sqlite(rusqlite::Error),
    /// The copy was written by a later Missive than this one.
    Later(i64),
    /// The copy is older than the search index: the daemon adds it the next time it starts.
    NotIndexed,
    /// The copy is older than the attachment marker: the daemon adds it the next time it starts.
    NoAttachmentMarker,
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
            CopyError::NoAttachmentMarker => write!(
                f,
                "the local copy has no attachment marker yet: it is added when the daemon next \
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
    /// The folders the last complete pass left as they were.
    pub skipped: Vec<SkippedFolder>,
}

/// A message of an account's outbox, as `missive status` shows it: never its content.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Outgoing {
    /// Its number in the outbox.
    pub id: i64,
    /// `queued`, `sending`, `sent` (its copy in Sent is still to be written), `failed` or
    /// `uncertain`: see [`crate::outbox`].
    pub state: String,
    pub message_id: String,
    /// When it was written, in seconds since the epoch.
    pub queued_at: i64,
    /// The tries that failed before the end of data.
    pub attempts: u32,
    pub last_error: Option<String>,
    /// `claimed` for longer than a submission takes: its worker could not put it back in the
    /// queue, and it stays so until the next start of the daemon.
    pub stuck: bool,
    /// For a `sent` message: how many times looking for its copy in Sent failed so far; at five
    /// the copy is given up.
    pub copy_search_failures: u32,
}

/// A draft of an account kept in the local copy until the server holds it, as `missive status`
/// shows it: never its content.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WaitingDraft {
    /// Its Message-ID, which names it.
    pub id: String,
    /// `waiting` (to write to the Drafts folder), `removing` (its message was sent; to remove
    /// from the Drafts folder) or `no-drafts-folder` (the account has none: it stays in the copy).
    pub state: String,
    /// When its version was saved, in seconds since the epoch.
    pub saved_at: i64,
    /// The tries that failed, and the last error.
    pub attempts: u32,
    pub last_error: Option<String>,
}

/// A message whose move the server refused after it had copied it: it is in both folders until
/// the user removes one of them (`missive status`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct HalfMoved {
    /// Its identifier, in the folder it was to leave.
    pub message: String,
    /// The identifier of its copy in the target, when the server told its UID.
    pub copy: Option<String>,
    /// The folder it was to leave and its target: their roles, else their names.
    pub folder: String,
    pub target: String,
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

impl FolderUnread {
    /// Whether the folder's unread messages add up in a total (an account's, or all of them):
    /// all but the junk and trash folders'. The folder still lists its own count.
    pub fn counted(&self) -> bool {
        !self
            .folder
            .role
            .as_deref()
            .is_some_and(|role| NOT_COUNTED.contains(&role))
    }
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

/// What a list asks for.
#[derive(Clone, Debug)]
pub struct List {
    /// The accounts to look in, by identifier.
    pub accounts: Vec<String>,
    /// The folders to list: those with this role or this name (the server's, as `unread` shows
    /// it). Both `None`: every folder.
    pub role: Option<String>,
    pub name: Option<String>,
    /// Only the messages without `\Seen`, and not marked `\Deleted`: those `unread` counts.
    pub unread: bool,
    pub limit: usize,
}

/// A message of a list: a search hit, and what a list shows more.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Listed {
    #[serde(flatten)]
    pub hit: Hit,
    pub flagged: bool,
    pub answered: bool,
    /// The size the server gave (`RFC822.SIZE`), in bytes.
    pub size: u64,
    pub has_attachments: bool,
}

/// A message as the copy holds it, found by its identifier.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stored {
    pub hit: Hit,
    pub flagged: bool,
    pub answered: bool,
    pub size: u64,
    pub cc: Vec<Person>,
    pub message_id: Option<String>,
    /// The message exactly as the server sent it.
    pub raw: Vec<u8>,
}

/// Why an identifier names no message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NotFound {
    /// It is not an identifier at all.
    Invalid,
    /// The account, the folder or the UID is not in the copy: never was, or the message is gone.
    Unknown,
    /// The folder is in the copy under another UIDVALIDITY: the server renumbered it, so the
    /// UID of the identifier may now name another message, which is not shown.
    Renumbered(String),
}

impl std::fmt::Display for NotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NotFound::Invalid => write!(
                f,
                "not a message identifier (expected <account>/<folder>/<uidvalidity>/<uid>, as \
                 missive search and list print it)"
            ),
            NotFound::Unknown => write!(f, "no such message in the local copy"),
            NotFound::Renumbered(folder) => write!(
                f,
                "no such message: the server renumbered the folder {folder} since this \
                 identifier was made (its UIDVALIDITY changed); missive list gives the current ones"
            ),
        }
    }
}

fn has_flag(flags: &str, flag: &str) -> bool {
    flags.split(' ').any(|f| f.eq_ignore_ascii_case(flag))
}

/// The columns of [`hit`], in this order, from [`Copy::from`]: `folder` is the folder a message
/// is shown in, `home` the one the server has it in.
const HIT_COLUMNS: &str = "account.id, folder.id, folder.name, folder.role, home.uidvalidity,
    message.uid, coalesce(message.date, message.internal_date), message.subject,
    message.sender, message.recipients, message.flags, home.id";

/// A hit from a row that starts with [`HIT_COLUMNS`]. A copy written before raw 8-bit headers were
/// decoded holds U+FFFD for their bytes: the hit is then read again from the raw message, which
/// the copy keeps as it came (no migration; only the rows that need it cost a second read).
fn hit(connection: &Connection, r: &rusqlite::Row) -> rusqlite::Result<(Hit, String)> {
    let account: String = r.get(0)?;
    let folder = FolderRef {
        id: r.get(1)?,
        name: r.get(2)?,
        role: r.get(3)?,
    };
    let uidvalidity: Option<u32> = r.get(4)?;
    let uid: u32 = r.get(5)?;
    let home: i64 = r.get(11)?;
    let people = |column: usize| -> rusqlite::Result<Vec<Person>> {
        let json: String = r.get(column)?;
        Ok(serde_json::from_str(&json).unwrap_or_default())
    };
    let flags: String = r.get(10)?;
    let mut hit = Hit {
        id: message_id(&account, home, uidvalidity.unwrap_or(0), uid),
        account,
        folder,
        uidvalidity,
        uid,
        date: r.get(6)?,
        subject: r.get(7)?,
        from: people(8)?,
        to: people(9)?,
        unread: !has_flag(&flags, "\\Seen"),
    };
    if damaged(
        hit.subject
            .iter()
            .map(String::as_str)
            .chain(names(&[&hit.from, &hit.to])),
    ) {
        let raw = connection
            .query_row(
                "SELECT raw FROM message_raw JOIN message ON message.id = message_raw.message
                  WHERE message.folder = ?1 AND message.uid = ?2",
                params![home, uid],
                |r| r.get::<_, Vec<u8>>(0),
            )
            .ok();
        if let Some(envelope) = raw.as_deref().map(envelope) {
            hit.subject = envelope.subject;
            hit.from = envelope.from;
            hit.to = envelope.to;
        }
    }
    Ok((hit, flags))
}

/// The headers of a message that are stored apart, as the message says them.
struct Envelope {
    subject: Option<String>,
    from: Vec<Person>,
    to: Vec<Person>,
    cc: Vec<Person>,
}

fn envelope(raw: &[u8]) -> Envelope {
    let raw = crate::header::decoded(raw);
    let parsed = mail_parser::MessageParser::default().parse(raw.as_ref());
    let people = |address: Option<&mail_parser::Address>| -> Vec<Person> {
        address
            .into_iter()
            .flat_map(|address| address.iter())
            .map(|mailbox| Person {
                name: mailbox.name().map(str::to_owned),
                address: mailbox.address().map(str::to_owned),
            })
            .collect()
    };
    Envelope {
        subject: parsed.as_ref().and_then(|m| m.subject()).map(str::to_owned),
        from: people(parsed.as_ref().and_then(|m| m.from())),
        to: people(parsed.as_ref().and_then(|m| m.to())),
        cc: people(parsed.as_ref().and_then(|m| m.cc())),
    }
}

fn names<'a>(lists: &[&'a [Person]]) -> impl Iterator<Item = &'a str> {
    lists
        .iter()
        .flat_map(|list| list.iter())
        .filter_map(|person| person.name.as_deref())
        .collect::<Vec<_>>()
        .into_iter()
}

/// Whether a text holds the character a parser puts in place of a byte it cannot read.
fn damaged<'a>(mut texts: impl Iterator<Item = &'a str>) -> bool {
    texts.any(|text| text.contains('\u{fffd}'))
}

impl Copy {
    /// The tables the queries of messages read from, for [`HIT_COLUMNS`].
    fn from(&self) -> String {
        format!(
            "message
               JOIN folder AS home ON home.id = message.folder
               JOIN folder ON folder.id = {}
               JOIN account ON account.key = folder.account",
            self.shown()
        )
    }

    /// The folder a message is shown in: where a move that waits takes it, else its own.
    fn shown(&self) -> &'static str {
        if self.version >= QUEUE_VERSION {
            "coalesce(message.moving_to, message.folder)"
        } else {
            "message.folder"
        }
    }

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
        let skipped = if self.version >= SKIPPED_VERSION {
            "account.skipped"
        } else {
            "'[]'"
        };
        let mut statement = self.connection.prepare_cached(&format!(
            "SELECT account.id, account.state, account.last_sync,
                    (SELECT count(*) FROM message JOIN folder ON folder.id = message.folder
                      WHERE folder.account = account.key), {skipped}
               FROM account ORDER BY account.id"
        ))?;
        let rows = statement.query_map([], |r| {
            Ok(AccountRow {
                id: r.get(0)?,
                state: r.get(1)?,
                last_sync: r.get(2)?,
                messages: r.get::<_, i64>(3)? as u64,
                skipped: serde_json::from_str(&r.get::<_, String>(4)?).unwrap_or_default(),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The messages of an account left in both folders by a move refused after their copy,
    /// oldest first; empty in a copy older than them.
    pub fn half_moved(&self, account: &str) -> Result<Vec<HalfMoved>> {
        if self.version < HALF_MOVE_VERSION {
            return Ok(Vec::new());
        }
        let mut statement = self.connection.prepare_cached(&format!(
            "SELECT half_move.folder, half_move.uidvalidity, half_move.uid, half_move.target,
                    half_move.target_uidvalidity, half_move.target_uid,
                    coalesce(source.role, source.name), coalesce(target.role, target.name)
               FROM half_move JOIN account ON account.key = half_move.account
                    JOIN folder AS source ON source.id = half_move.folder
                    JOIN folder AS target ON target.id = half_move.target
              WHERE account.id = ?1 AND {HALF_MOVE_LEFT}
              ORDER BY half_move.id"
        ))?;
        let rows = statement.query_map([account], |r| {
            let copy = match (r.get::<_, Option<u32>>(4)?, r.get::<_, Option<u32>>(5)?) {
                (Some(uidvalidity), Some(uid)) => {
                    Some(message_id(account, r.get(3)?, uidvalidity, uid))
                }
                _ => None,
            };
            Ok(HalfMoved {
                message: message_id(account, r.get(0)?, r.get(1)?, r.get(2)?),
                copy,
                folder: r.get(6)?,
                target: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The messages an action refused for good left marked `\Deleted` on the server, by their
    /// identifiers, oldest first: Missive set the flag itself, and takes it back at the next
    /// connection. Empty in a copy older than them.
    pub fn deleted_left(&self, account: &str) -> Result<Vec<String>> {
        if self.version < DELETED_LEFT_VERSION {
            return Ok(Vec::new());
        }
        let mut statement = self.connection.prepare_cached(
            "SELECT deleted_left.folder, deleted_left.uidvalidity, json_each.value
               FROM deleted_left JOIN account ON account.key = deleted_left.account,
                    json_each(deleted_left.uids)
              WHERE account.id = ?1 ORDER BY deleted_left.id, json_each.key",
        )?;
        let rows = statement.query_map([account], |r| {
            Ok(message_id(account, r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The outbox of an account, oldest first; empty in a copy older than the outbox.
    pub fn outbox(&self, account: &str) -> Result<Vec<Outgoing>> {
        if self.version < OUTBOX_VERSION {
            return Ok(Vec::new());
        }
        // A claimed message holds the time of its claim where a queued one holds its next try.
        let claimed_at = if self.version >= RETRY_VERSION {
            "outbox.next_attempt_at"
        } else {
            "NULL"
        };
        let failures = if self.version >= SENT_SEARCH_VERSION {
            "outbox.copy_search_failures"
        } else {
            "0"
        };
        let mut statement = self.connection.prepare(&format!(
            "SELECT outbox.id, outbox.state, outbox.message_id, outbox.queued_at,
                    outbox.attempts, outbox.last_error, {claimed_at}, {failures}
               FROM outbox JOIN account ON account.key = outbox.account
              WHERE account.id = ?1 ORDER BY outbox.id"
        ))?;
        let limit = crate::outbox::now() - crate::outbox::CLAIM_STUCK_MS;
        let rows = statement.query_map([account], |r| {
            let state: String = r.get(1)?;
            let claimed_at: Option<i64> = r.get(6)?;
            Ok(Outgoing {
                id: r.get(0)?,
                // 0 is no claim time: a daemon older than the claim time left the one its message
                // was queued with, which says nothing of when it was claimed.
                stuck: state == "claimed" && claimed_at.is_some_and(|at| 0 < at && at < limit),
                state,
                message_id: r.get(2)?,
                queued_at: r.get(3)?,
                attempts: r.get(4)?,
                last_error: r.get(5)?,
                copy_search_failures: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The drafts of an account kept in the copy until the server holds them, oldest first;
    /// empty in a copy older than the drafts.
    pub fn drafts(&self, account: &str) -> Result<Vec<WaitingDraft>> {
        if self.version < DRAFTS_VERSION {
            return Ok(Vec::new());
        }
        let mut statement = self.connection.prepare_cached(
            "SELECT draft.message_id, draft.raw IS NULL, draft.saved_at, draft.attempts,
                    draft.last_error,
                    EXISTS (SELECT 1 FROM folder WHERE folder.account = account.key
                                                   AND folder.role = 'drafts'),
                    EXISTS (SELECT 1 FROM folder WHERE folder.account = account.key)
               FROM draft JOIN account ON account.key = draft.account
              WHERE account.id = ?1 ORDER BY draft.id",
        )?;
        let rows = statement.query_map([account], |r| {
            let (removing, has_drafts, knows_folders): (bool, bool, bool) =
                (r.get(1)?, r.get(5)?, r.get(6)?);
            let state = if knows_folders && !has_drafts {
                "no-drafts-folder"
            } else if removing {
                "removing"
            } else {
                "waiting"
            };
            Ok(WaitingDraft {
                id: r.get(0)?,
                state: state.to_owned(),
                saved_at: r.get(2)?,
                attempts: r.get(3)?,
                last_error: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The unread messages of every folder of every account: those without `\Seen`, and not
    /// marked `\Deleted` (they wait for an expunge). A flag is matched as a whole word. The
    /// counts of [`FolderUnread::counted`] folders are the ones that add up in a total.
    /// By account, then folder: the inbox first, the other roles, then the user's own folders by
    /// name.
    pub fn unread(&self) -> Result<Vec<FolderUnread>> {
        const UNREAD: &str = "' ' || message.flags || ' ' NOT LIKE '% \\Seen %'
            AND ' ' || message.flags || ' ' NOT LIKE '% \\Deleted %'";
        // The messages of the folder, less those shown elsewhere, plus those shown here: each
        // count reads an index (the moves that wait are few), not every row. The `+` keeps the
        // planner off the index by folder, which would read every row of the folder.
        let moving = if self.version >= QUEUE_VERSION {
            format!(
                "- (SELECT count(*) FROM message
                     WHERE message.moving_to IS NOT NULL AND +message.folder = folder.id
                       AND {UNREAD})
                 + (SELECT count(*) FROM message
                     WHERE message.moving_to = folder.id AND {UNREAD})"
            )
        } else {
            String::new()
        };
        let mut statement = self.connection.prepare_cached(&format!(
            "SELECT account.id, folder.id, folder.name, folder.role,
                    (SELECT count(*) FROM message
                      WHERE message.folder = folder.id AND {UNREAD}) {moving}
               FROM folder JOIN account ON account.key = folder.account"
        ))?;
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
        let mut statement = self.connection.prepare_cached(&format!(
            "SELECT {HIT_COLUMNS}
               FROM message_search
               JOIN {}
              WHERE message.id = message_search.rowid
                AND message_search MATCH ?1
                AND account.id IN (SELECT value FROM json_each(?2))
                AND (?3 IS NULL OR folder.role = ?3)
              ORDER BY coalesce(message.date, message.internal_date) DESC, message.id DESC
              LIMIT ?4",
            self.from()
        ))?;
        let role = search.role.clone().map_or(Value::Null, Value::Text);
        let rows = statement.query_map(
            params![expression, accounts, role, search.limit as i64],
            |r| Ok(hit(&self.connection, r)?.0),
        )?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The messages of the folders `list` names, newest first (`Date`, else the internal date).
    /// A draft waiting for the server is listed in its account's Drafts folder, under its
    /// Message-ID (it has no UID yet), in place of the older versions the folder holds.
    pub fn list(&self, list: &List) -> Result<Vec<Listed>> {
        if self.version < ATTACHMENTS_VERSION {
            return Err(CopyError::NoAttachmentMarker);
        }
        let older = if self.version >= DRAFTS_VERSION {
            "AND (home.role IS NOT 'drafts' OR message.message_id IS NULL
                  OR NOT EXISTS (SELECT 1 FROM draft WHERE draft.account = home.account
                                    AND draft.message_id = message.message_id))"
        } else {
            ""
        };
        let accounts = serde_json::to_string(&list.accounts).expect("strings serialize");
        let mut statement = self.connection.prepare_cached(&format!(
            "SELECT {HIT_COLUMNS}, message.size, message.has_attachments
               FROM {}
              WHERE account.id IN (SELECT value FROM json_each(?1))
                AND (?2 IS NULL AND ?3 IS NULL OR folder.role = ?2 OR folder.name = ?3)
                AND (NOT ?4 OR (' ' || message.flags || ' ' NOT LIKE '% \\Seen %'
                            AND ' ' || message.flags || ' ' NOT LIKE '% \\Deleted %'))
                {older}
              ORDER BY coalesce(message.date, message.internal_date) DESC, message.id DESC
              LIMIT ?5",
            self.from()
        ))?;
        let text = |text: &Option<String>| text.clone().map_or(Value::Null, Value::Text);
        let rows = statement.query_map(
            params![
                accounts,
                text(&list.role),
                text(&list.name),
                list.unread,
                list.limit as i64
            ],
            |r| {
                let (hit, flags) = hit(&self.connection, r)?;
                Ok(Listed {
                    hit,
                    flagged: has_flag(&flags, "\\Flagged"),
                    answered: has_flag(&flags, "\\Answered"),
                    size: r.get::<_, i64>(12)? as u64,
                    has_attachments: r.get(13)?,
                })
            },
        )?;
        let mut listed: Vec<Listed> = rows.collect::<rusqlite::Result<_>>()?;
        // Drafts are seen: an unread list has none.
        if self.version < DRAFTS_VERSION || list.unread {
            return Ok(listed);
        }
        let mut statement = self.connection.prepare_cached(
            "SELECT account.id, folder.id, folder.name, folder.role, draft.message_id,
                    coalesce(draft.date, draft.saved_at), draft.subject, draft.sender,
                    draft.recipients, length(draft.raw), draft.has_attachments
               FROM draft
               JOIN account ON account.key = draft.account
               JOIN folder ON folder.id = (SELECT id FROM folder
                                            WHERE folder.account = draft.account
                                              AND folder.role = 'drafts' ORDER BY id LIMIT 1)
              WHERE draft.raw IS NOT NULL AND account.id IN (SELECT value FROM json_each(?1))
                AND (?2 IS NULL AND ?3 IS NULL OR folder.role = ?2 OR folder.name = ?3)",
        )?;
        let rows =
            statement.query_map(params![accounts, text(&list.role), text(&list.name)], |r| {
                let people = |column: usize| -> rusqlite::Result<Vec<Person>> {
                    let json: String = r.get(column)?;
                    Ok(serde_json::from_str(&json).unwrap_or_default())
                };
                Ok(Listed {
                    hit: Hit {
                        id: r.get(4)?,
                        account: r.get(0)?,
                        folder: FolderRef {
                            id: r.get(1)?,
                            name: r.get(2)?,
                            role: r.get(3)?,
                        },
                        uidvalidity: None,
                        uid: 0,
                        date: r.get(5)?,
                        subject: r.get(6)?,
                        from: people(7)?,
                        to: people(8)?,
                        unread: false,
                    },
                    flagged: false,
                    answered: false,
                    size: r.get::<_, i64>(9)? as u64,
                    has_attachments: r.get(10)?,
                })
            })?;
        for row in rows {
            listed.push(row?);
        }
        // Stable: the order of the copy's rows stays among equal dates.
        listed.sort_by_key(|listed| std::cmp::Reverse(listed.hit.date));
        listed.truncate(list.limit);
        Ok(listed)
    }

    /// The latest version of a draft of `account`, to pick it up again: `id` is its Message-ID
    /// (with or without angle brackets), as `missive compose` prints it and as a draft waiting
    /// for the server is listed, or a message identifier, as `list` prints it. The version
    /// waiting in the copy comes first, else the newest of the Drafts folder. `None` when there
    /// is no such draft, or when it was sent.
    pub fn draft(&self, account: &str, id: &str) -> Result<Option<Vec<u8>>> {
        if MessageId::parse(id).is_some() {
            return Ok(self
                .message(id)?
                .ok()
                .filter(|stored| stored.hit.account == account)
                .map(|stored| stored.raw));
        }
        let message_id = id.trim_start_matches('<').trim_end_matches('>');
        if self.version >= DRAFTS_VERSION {
            let waiting: Option<Option<Vec<u8>>> = self
                .connection
                .query_row(
                    "SELECT draft.raw FROM draft JOIN account ON account.key = draft.account
                      WHERE account.id = ?1 AND draft.message_id = ?2",
                    params![account, message_id],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(waiting) = waiting {
                // Sent: no longer a draft.
                return Ok(waiting);
            }
        }
        Ok(self
            .connection
            .query_row(
                "SELECT message_raw.raw FROM message
                   JOIN message_raw ON message_raw.message = message.id
                   JOIN folder ON folder.id = message.folder
                   JOIN account ON account.key = folder.account
                  WHERE account.id = ?1 AND folder.role = 'drafts' AND message.message_id = ?2
                  ORDER BY message.uid DESC LIMIT 1",
                params![account, message_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// The message an identifier names, with its raw message. Whether the identifier is one, and
    /// whether it still names a message, is the answer's to say ([`NotFound`]) and not an error:
    /// the identifier of a message that has gone since it was printed is what a caller will meet.
    pub fn message(&self, id: &str) -> Result<std::result::Result<Stored, NotFound>> {
        let Some(id) = MessageId::parse(id) else {
            return Ok(Err(NotFound::Invalid));
        };
        let folder: Option<(String, Option<u32>)> = self
            .connection
            .prepare_cached(
                "SELECT folder.name, folder.uidvalidity FROM folder
                   JOIN account ON account.key = folder.account
                  WHERE account.id = ?1 AND folder.id = ?2",
            )?
            .query_row(params![id.account, id.folder], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                e => Err(e),
            })?;
        let Some((name, uidvalidity)) = folder else {
            return Ok(Err(NotFound::Unknown));
        };
        if uidvalidity.unwrap_or(0) != id.uidvalidity {
            return Ok(Err(NotFound::Renumbered(name)));
        }
        let stored = self
            .connection
            .prepare_cached(&format!(
                "SELECT {HIT_COLUMNS}, message.size, message.cc, message.message_id,
                        message_raw.raw
                   FROM {}
                   JOIN message_raw ON message_raw.message = message.id
                  WHERE account.id = ?1 AND home.id = ?2 AND message.uid = ?3",
                self.from()
            ))?
            .query_row(params![id.account, id.folder, id.uid], |r| {
                let (hit, flags) = hit(&self.connection, r)?;
                let cc: String = r.get(13)?;
                let mut cc: Vec<Person> = serde_json::from_str(&cc).unwrap_or_default();
                let raw: Vec<u8> = r.get(15)?;
                if damaged(names(&[&cc])) {
                    cc = envelope(&raw).cc;
                }
                Ok(Stored {
                    hit,
                    flagged: has_flag(&flags, "\\Flagged"),
                    answered: has_flag(&flags, "\\Answered"),
                    size: r.get::<_, i64>(12)? as u64,
                    cc,
                    message_id: r.get(14)?,
                    raw,
                })
            });
        match stored {
            Ok(stored) => Ok(Ok(stored)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(Err(NotFound::Unknown)),
            Err(e) => Err(e.into()),
        }
    }
}

/// A time in seconds since the epoch in the user's time zone (`TZ`, else the system's), as
/// `2026-10-17 09:30 +02:00`; `2026-10-17 07:30 UTC` where the zone is UTC itself.
pub fn local_time(seconds: i64) -> String {
    match chrono::DateTime::from_timestamp(seconds, 0) {
        Some(time) => written(time.with_timezone(&chrono::Local).fixed_offset()),
        None => seconds.to_string(),
    }
}

fn written(time: chrono::DateTime<chrono::FixedOffset>) -> String {
    let offset = time.offset().local_minus_utc();
    let zone = if offset == 0 {
        "UTC".to_owned()
    } else {
        time.format("%:z").to_string()
    };
    format!("{} {zone}", time.format("%Y-%m-%d %H:%M"))
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
    fn a_time_is_written_in_its_zone_with_the_offset() {
        use chrono::{DateTime, FixedOffset, Utc};
        let at = |seconds: i64, offset: i32| {
            written(
                DateTime::<Utc>::from_timestamp(seconds, 0)
                    .unwrap()
                    .with_timezone(&FixedOffset::east_opt(offset).unwrap()),
            )
        };
        assert_eq!(at(0, 0), "1970-01-01 00:00 UTC");
        assert_eq!(at(1_792_220_640, 0), "2026-10-17 07:04 UTC");
        assert_eq!(at(1_792_220_640, 7_200), "2026-10-17 09:04 +02:00");
        assert_eq!(at(1_792_220_640, -12_600), "2026-10-17 03:34 -03:30");
        // The end of a leap day and the start of the next day, and a day that changes with the zone.
        assert_eq!(at(1_709_251_199, 0), "2024-02-29 23:59 UTC");
        assert_eq!(at(1_709_251_200, 0), "2024-03-01 00:00 UTC");
        assert_eq!(at(1_709_251_199, 3_600), "2024-03-01 00:59 +01:00");
        assert_eq!(at(-1, 0), "1969-12-31 23:59 UTC");
    }

    #[test]
    fn an_identifier_is_read_back_exactly_as_it_is_written() {
        let id = message_id("alice@example.org", 12, 1745112, 4);
        assert_eq!(
            MessageId::parse(&id),
            Some(MessageId {
                account: "alice@example.org".to_owned(),
                folder: 12,
                uidvalidity: 1745112,
                uid: 4
            })
        );
        // A folder not yet examined is written with a UIDVALIDITY of 0.
        assert!(MessageId::parse("alice/1/0/1").is_some());
        for wrong in [
            "",
            "alice",
            "alice/1/2",
            "alice/1/2/3/4",
            "/1/2/3",
            "alice/+1/2/3",
            "alice/1/02/3",
            "alice/1/2/ 3",
            "alice/1/2/3 ",
            "alice/x/2/3",
            "alice/1/2/-3",
            "alice/1/2/4294967296",
            "alice/1/2/",
        ] {
            assert_eq!(MessageId::parse(wrong), None, "{wrong:?}");
        }
    }

    #[test]
    fn a_draft_is_found_waiting_by_its_message_id_else_in_the_drafts_folder() {
        let (_dir, mut store, copy) = open();
        let alice = store.account("alice").unwrap();
        let drafts = store.folder(alice, "Drafts", Some("drafts")).unwrap().id;
        store.rebuild(drafts, 9).unwrap();
        let on_server = b"Message-ID: <d@example.org>\r\nSubject: Old\r\n\r\nOld\r\n".to_vec();
        store
            .insert(
                drafts,
                &[NewMessage {
                    uid: 3,
                    flags: "\\Draft".to_owned(),
                    internal_date: None,
                    size: on_server.len() as i64,
                    raw: on_server.clone(),
                }],
            )
            .unwrap();

        assert_eq!(
            copy.draft("alice", "d@example.org").unwrap(),
            Some(on_server.clone())
        );
        assert_eq!(
            copy.draft("alice", "<d@example.org>").unwrap(),
            Some(on_server.clone())
        );
        let by_uid = message_id("alice", drafts, 9, 3);
        assert_eq!(copy.draft("alice", &by_uid).unwrap(), Some(on_server));

        let waiting = b"Message-ID: <d@example.org>\r\nSubject: New\r\n\r\nNew\r\n".to_vec();
        store.save_draft(alice, "d@example.org", &waiting).unwrap();
        assert_eq!(copy.draft("alice", "d@example.org").unwrap(), Some(waiting));
        assert_eq!(copy.draft("alice", "x@example.org").unwrap(), None);
        assert_eq!(copy.draft("bob", "d@example.org").unwrap(), None);
    }

    #[test]
    fn a_message_is_found_by_its_identifier_with_its_raw_message() {
        let (_dir, mut store, copy) = open();
        let alice = store.account("alice").unwrap();
        let folder = store.folder(alice, "INBOX", Some("inbox")).unwrap().id;
        store.rebuild(folder, 77).unwrap();
        let sent = message(
            5,
            "\\Seen \\Flagged",
            "Sat, 17 Oct 2026 09:30:00 +0000",
            "A",
            "b",
        );
        let raw = sent.raw.clone();
        store.insert(folder, &[sent]).unwrap();

        let found = copy
            .message(&message_id("alice", folder, 77, 5))
            .unwrap()
            .unwrap();

        assert_eq!(found.raw, raw);
        assert_eq!(found.hit.subject.as_deref(), Some("A"));
        assert!(!found.hit.unread);
        assert!(found.flagged);
        assert!(!found.answered);
        assert_eq!(found.size, raw.len() as u64);
    }

    /// A message whose subject and names are raw Latin-1 bytes, not encoded words.
    fn latin_1(uid: u32) -> NewMessage {
        let raw =
            b"From: Zo\xe9 M\xfcller <zoe@example.org>\r\nTo: Andr\xe9 <andre@example.net>\r\n\
            Cc: Fran\xe7ois <francois@example.net>\r\nSubject: Caf\xe9 Cr\xe8me\r\n\
            Date: Sat, 17 Oct 2026 09:30:00 +0000\r\n\r\nBonjour\r\n"
                .to_vec();
        NewMessage {
            uid,
            flags: String::new(),
            internal_date: Some(1_790_000_000),
            size: raw.len() as i64,
            raw,
        }
    }

    fn names(people: &[Person]) -> Vec<Option<&str>> {
        people.iter().map(|p| p.name.as_deref()).collect()
    }

    #[test]
    fn a_raw_latin_1_subject_and_names_are_stored_decoded_and_the_raw_message_is_kept() {
        let (_dir, mut store, copy) = open();
        let alice = store.account("alice").unwrap();
        let folder = store.folder(alice, "INBOX", Some("inbox")).unwrap().id;
        store.rebuild(folder, 77).unwrap();
        let sent = latin_1(5);
        let raw = sent.raw.clone();
        store.insert(folder, &[sent]).unwrap();

        let found = copy
            .message(&message_id("alice", folder, 77, 5))
            .unwrap()
            .unwrap();

        assert_eq!(found.hit.subject.as_deref(), Some("Café Crème"));
        assert_eq!(names(&found.hit.from), [Some("Zoé Müller")]);
        assert_eq!(names(&found.hit.to), [Some("André")]);
        assert_eq!(names(&found.cc), [Some("François")]);
        // The server's bytes are kept as they came.
        assert_eq!(found.raw, raw);
        // The index holds the decoded words too.
        assert_eq!(search(&copy, &["crème"]), [5]);
    }

    #[test]
    fn a_copy_written_before_holds_replacement_characters_that_reading_repairs_from_the_raw_message()
     {
        let (dir, mut store, copy) = open();
        let alice = store.account("alice").unwrap();
        let folder = store.folder(alice, "INBOX", Some("inbox")).unwrap().id;
        store.rebuild(folder, 77).unwrap();
        store.insert(folder, &[latin_1(5)]).unwrap();
        // What an earlier Missive wrote for this message.
        Connection::open(dir.path().join("copy.sqlite"))
            .unwrap()
            .execute(
                "UPDATE message SET subject = 'Caf\u{fffd} Cr\u{fffd}me',
                    sender = '[{\"name\":\"Zo\u{fffd} M\u{fffd}ller\",\"address\":\"zoe@example.org\"}]',
                    recipients = '[{\"name\":\"Andr\u{fffd}\",\"address\":\"andre@example.net\"}]',
                    cc = '[{\"name\":\"Fran\u{fffd}ois\",\"address\":\"francois@example.net\"}]'",
                [],
            )
            .unwrap();

        let listed = copy
            .list(&List {
                accounts: vec!["alice".to_owned()],
                role: None,
                name: None,
                unread: false,
                limit: 10,
            })
            .unwrap();
        let found = copy
            .message(&message_id("alice", folder, 77, 5))
            .unwrap()
            .unwrap();

        assert_eq!(listed[0].hit.subject.as_deref(), Some("Café Crème"));
        assert_eq!(names(&listed[0].hit.from), [Some("Zoé Müller")]);
        assert_eq!(found.hit.subject.as_deref(), Some("Café Crème"));
        assert_eq!(names(&found.hit.to), [Some("André")]);
        assert_eq!(names(&found.cc), [Some("François")]);
    }

    #[test]
    fn a_replacement_character_the_sender_really_wrote_is_left_as_it_is() {
        let (_dir, mut store, copy) = open();
        let alice = store.account("alice").unwrap();
        let folder = store.folder(alice, "INBOX", Some("inbox")).unwrap().id;
        store.rebuild(folder, 77).unwrap();
        store
            .insert(
                folder,
                &[message(
                    5,
                    "",
                    "Sat, 17 Oct 2026 09:30:00 +0000",
                    "Odd \u{fffd} sign",
                    "b",
                )],
            )
            .unwrap();

        let found = copy
            .message(&message_id("alice", folder, 77, 5))
            .unwrap()
            .unwrap();

        assert_eq!(found.hit.subject.as_deref(), Some("Odd \u{fffd} sign"));
    }

    #[test]
    fn an_identifier_that_names_no_message_says_why() {
        let (_dir, mut store, copy) = open();
        let alice = store.account("alice").unwrap();
        let folder = store.folder(alice, "INBOX", Some("inbox")).unwrap().id;
        store.rebuild(folder, 77).unwrap();
        store
            .insert(
                folder,
                &[message(5, "", "Sat, 17 Oct 2026 09:30:00 +0000", "A", "b")],
            )
            .unwrap();
        let missing = |id: String| copy.message(&id).unwrap().unwrap_err();

        assert_eq!(missing("alice/1".to_owned()), NotFound::Invalid);
        assert_eq!(
            missing(message_id("alice", folder, 77, 6)),
            NotFound::Unknown
        );
        assert_eq!(
            missing(message_id("alice", folder + 1, 77, 5)),
            NotFound::Unknown
        );
        assert_eq!(missing(message_id("bob", folder, 77, 5)), NotFound::Unknown);
        // The UID is there, but under another UIDVALIDITY: not that message, and said so, whether
        // or not the UID exists.
        assert_eq!(
            missing(message_id("alice", folder, 78, 5)),
            NotFound::Renumbered("INBOX".to_owned())
        );
        assert_eq!(
            missing(message_id("alice", folder, 78, 6)),
            NotFound::Renumbered("INBOX".to_owned())
        );
        // A folder never examined is written with 0, and only that finds its messages.
        let fresh = store.folder(alice, "Fresh", None).unwrap().id;
        assert_eq!(missing(message_id("alice", fresh, 0, 1)), NotFound::Unknown);
        assert_eq!(
            missing(message_id("alice", fresh, 1, 1)),
            NotFound::Renumbered("Fresh".to_owned())
        );
    }

    #[test]
    fn a_list_is_newest_first_and_follows_role_name_unread_and_limit() {
        let (_dir, mut store, copy) = open();
        let alice = store.account("alice").unwrap();
        let inbox = store.folder(alice, "INBOX", Some("inbox")).unwrap().id;
        let other = store.folder(alice, "Other", None).unwrap().id;
        store
            .insert(
                inbox,
                &[
                    message(1, "\\Seen", "Sat, 17 Oct 2026 09:30:00 +0000", "Old", "b"),
                    message(2, "", "Sat, 17 Oct 2026 09:32:00 +0000", "New", "b"),
                    message(
                        3,
                        "\\Deleted",
                        "Sat, 17 Oct 2026 09:31:00 +0000",
                        "Mid",
                        "b",
                    ),
                ],
            )
            .unwrap();
        store
            .insert(
                other,
                &[message(
                    1,
                    "",
                    "Sat, 17 Oct 2026 09:33:00 +0000",
                    "Other",
                    "b",
                )],
            )
            .unwrap();
        let list = |role: Option<&str>, name: Option<&str>, unread: bool, limit: usize| {
            copy.list(&List {
                accounts: vec!["alice".to_owned()],
                role: role.map(str::to_owned),
                name: name.map(str::to_owned),
                unread,
                limit,
            })
            .unwrap()
            .iter()
            .map(|m| m.hit.subject.clone().unwrap())
            .collect::<Vec<_>>()
        };

        assert_eq!(list(Some("inbox"), None, false, 10), ["New", "Mid", "Old"]);
        assert_eq!(list(Some("inbox"), None, false, 2), ["New", "Mid"]);
        // What `unread` counts: not seen and not marked deleted.
        assert_eq!(list(Some("inbox"), None, true, 10), ["New"]);
        assert_eq!(list(None, Some("Other"), false, 10), ["Other"]);
        assert_eq!(list(Some("INBOX"), None, false, 10).len(), 0);
        assert_eq!(list(None, None, false, 10), ["Other", "New", "Mid", "Old"]);
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
                messages: 0,
                skipped: Vec::new(),
            }
        );
    }

    #[test]
    fn accounts_say_the_folders_the_last_pass_skipped_and_a_pass_without_one_clears_them() {
        let (_dir, store, copy) = open();
        let alice = store.account("alice").unwrap();
        let skipped = vec![
            SkippedFolder {
                folder: "sent".to_owned(),
                kind: "refused".to_owned(),
            },
            SkippedFolder {
                folder: "Receipts".to_owned(),
                kind: "unusable".to_owned(),
            },
        ];

        store.set_skipped(alice, &skipped).unwrap();
        assert_eq!(copy.accounts().unwrap()[0].skipped, skipped);

        store.set_skipped(alice, &[]).unwrap();
        assert_eq!(copy.accounts().unwrap()[0].skipped, []);
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
