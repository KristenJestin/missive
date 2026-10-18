//! `missive status`, `unread`, `search`, `list` and `show`: answers from the local copy alone. They open it read
//! only and never create it; they touch neither the network, nor the keyring, nor the daemon's
//! socket, so they answer with the daemon stopped and the network down.

use std::fs::{File, TryLockError};
use std::io::Write;

use clap::{Args, ValueEnum};
use missive_core::account::{AccountStore, Listing};
use missive_core::body::{self, Attachment};
use missive_core::copy::{Copy, FolderRef, Hit, List, NotFound, Person, Search, Stored, utc};
use missive_core::protocol::pid_path;
use missive_core::store::{SkippedFolder, Store};
use serde::Serialize;

/// The folder roles a search can be narrowed to.
#[derive(Clone, Copy, ValueEnum)]
enum Role {
    Inbox,
    Sent,
    Drafts,
    Trash,
    Junk,
    Archive,
}

impl Role {
    fn as_str(self) -> &'static str {
        match self {
            Role::Inbox => "inbox",
            Role::Sent => "sent",
            Role::Drafts => "drafts",
            Role::Trash => "trash",
            Role::Junk => "junk",
            Role::Archive => "archive",
        }
    }
}

#[derive(Args)]
pub struct SearchArgs {
    /// The words to find: a message has to hold every one of them, in its sender, recipients,
    /// subject or text. Case and accents do not matter.
    #[arg(required = true, value_name = "TEXT")]
    text: Vec<String>,
    /// One JSON array on standard output (see docs/cli.md).
    #[arg(long)]
    json: bool,
    /// Only this account's messages, by identifier, as `account list` shows it.
    #[arg(long)]
    account: Option<String>,
    /// Only the folders with this role.
    #[arg(long, value_enum)]
    folder: Option<Role>,
    /// The most messages to list, the newest first.
    #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u64).range(1..))]
    limit: u64,
}

#[derive(Args)]
pub struct ListArgs {
    /// One JSON array on standard output (see docs/cli.md).
    #[arg(long)]
    json: bool,
    /// Only this account's messages, by identifier, as `account list` shows it.
    #[arg(long)]
    account: Option<String>,
    /// A folder, by role (`inbox`, `sent`…) or by the name the server gives it
    /// [default: the inbox of each account].
    #[arg(long, value_name = "ROLE OR NAME")]
    folder: Option<String>,
    /// Only the unread messages.
    #[arg(long)]
    unread: bool,
    /// The most messages to list, the newest first.
    #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u64).range(1..))]
    limit: u64,
}

#[derive(Args)]
pub struct ShowArgs {
    /// The message's identifier, as `search` and `list` print it.
    #[arg(value_name = "MESSAGE ID")]
    id: String,
    /// One JSON object on standard output (see docs/cli.md).
    #[arg(long, conflicts_with = "raw")]
    json: bool,
    /// The message exactly as the server sent it, on standard output.
    #[arg(long)]
    raw: bool,
}

/// Whether a character must not reach a terminal: a control character, or one of the Unicode
/// format characters that reorder or hide text (bidirectional overrides and isolates, zero-width
/// spaces, the byte order mark, tags). The zero-width joiner and non-joiner stay: they are part
/// of how emoji sequences and some scripts are written.
fn hides_text(c: char) -> bool {
    c.is_control()
        || matches!(c,
            '\u{ad}' | '\u{61c}' | '\u{180e}'
            | '\u{200b}' | '\u{200e}'..='\u{200f}'
            | '\u{2028}'..='\u{202e}'
            | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{206f}'
            | '\u{feff}' | '\u{fff9}'..='\u{fffb}'
            | '\u{e0000}'..='\u{e007f}')
}

/// The exit status of `show` and `open` when the identifier names no message.
pub const NOT_FOUND: u8 = 3;

/// Why `show` or `open` failed: the identifier names no message, or something else went wrong.
pub enum ShowError {
    NotFound(String),
    Failed(String),
}

impl<E: std::fmt::Display> From<E> for ShowError {
    fn from(error: E) -> Self {
        ShowError::Failed(error.to_string())
    }
}

type Result<T> = std::result::Result<T, String>;

/// The identifiers of the configured accounts: a removed account is no longer shown, though the
/// copy may still hold it.
fn configured() -> Result<Vec<String>> {
    let Listing { accounts, skipped } = AccountStore::from_env()
        .and_then(|store| store.list())
        .map_err(|e| e.to_string())?;
    // On stderr, so that the JSON on stdout stays valid.
    for (id, kind) in &skipped {
        eprintln!(
            "missive: skipped the account folder {id}: its account.toml is unusable ({kind})"
        );
    }
    Ok(accounts.into_iter().map(|(id, _)| id).collect())
}

/// The local copy, or `None` (said on standard error) when the daemon has not made it yet.
fn open() -> Result<Option<Copy>> {
    let path = Store::default_path().map_err(|e| e.to_string())?;
    let copy = Copy::open(&path).map_err(|e| e.to_string())?;
    if copy.is_none() {
        eprintln!("missive: no local copy yet (missive sync makes it)");
    }
    Ok(copy)
}

/// Whether a daemon runs: it holds `daemon.pid` locked for as long as it lives, and the lock
/// goes with the process, killed or not. The file is only looked at (never created), and the
/// lock is taken shared and let go at once, so as not to be in the way of a daemon starting.
fn daemon_running() -> bool {
    let Ok(path) = pid_path() else {
        return false;
    };
    let Ok(file) = File::open(path) else {
        return false;
    };
    match file.try_lock_shared() {
        Ok(()) => {
            let _ = file.unlock();
            false
        }
        Err(TryLockError::WouldBlock) => true,
        Err(TryLockError::Error(_)) => false,
    }
}

#[derive(Serialize)]
struct Status {
    daemon: &'static str,
    accounts: Vec<AccountStatus>,
}

#[derive(Serialize)]
struct AccountStatus {
    id: String,
    /// What the copy last recorded; `None` when it knows none to report: the account has not
    /// reached the copy yet, or the daemon that was syncing it is gone.
    state: Option<String>,
    syncing: bool,
    last_sync: Option<i64>,
    messages: u64,
    /// The folders the last complete pass left as they were: told apart from the state, which
    /// stays `ready`.
    skipped: Vec<SkippedFolder>,
    #[serde(skip)]
    in_copy: bool,
}

pub fn status(json: bool) -> Result<()> {
    let configured = configured()?;
    let copy = open()?;
    let running = daemon_running();
    let rows = match &copy {
        Some(copy) => copy.accounts().map_err(|e| e.to_string())?,
        None => Vec::new(),
    };
    let accounts: Vec<AccountStatus> = configured
        .into_iter()
        .map(|id| {
            let Some(row) = rows.iter().find(|row| row.id == id) else {
                return AccountStatus {
                    id,
                    state: None,
                    syncing: false,
                    last_sync: None,
                    messages: 0,
                    skipped: Vec::new(),
                    in_copy: false,
                };
            };
            // A daemon killed mid-sync leaves `syncing` behind: it only means something while a
            // daemon lives.
            let syncing = row.state == "syncing";
            AccountStatus {
                id,
                state: (!syncing || running).then(|| row.state.clone()),
                syncing: syncing && running,
                last_sync: row.last_sync,
                messages: row.messages,
                skipped: row.skipped.clone(),
                in_copy: true,
            }
        })
        .collect();
    let daemon = if running { "running" } else { "stopped" };
    if json {
        let status = Status { daemon, accounts };
        println!(
            "{}",
            serde_json::to_string(&status).map_err(|e| e.to_string())?
        );
        return Ok(());
    }
    println!("daemon: {daemon}");
    for account in &accounts {
        let messages = match account.messages {
            1 => "1 message".to_owned(),
            n => format!("{n} messages"),
        };
        let synced = match account.last_sync {
            Some(seconds) => format!("last sync {}", utc(seconds)),
            None => "never synced".to_owned(),
        };
        let skipped = if account.skipped.is_empty() {
            String::new()
        } else {
            let folders: Vec<String> = account
                .skipped
                .iter()
                .map(|s| format!("{} ({})", s.folder, s.kind))
                .collect();
            format!("\tskipped: {}", folders.join(", "))
        };
        match (&account.state, account.in_copy) {
            (None, false) => println!("{}\tnever synced\t{messages}", account.id),
            (None, true) => println!(
                "{}\tunknown (the daemon is not running)\t{synced}\t{messages}{skipped}",
                account.id
            ),
            (Some(state), _) => {
                println!("{}\t{state}\t{synced}\t{messages}{skipped}", account.id)
            }
        }
    }
    Ok(())
}

#[derive(Serialize)]
struct Unread {
    total: u64,
    accounts: Vec<AccountUnread>,
}

#[derive(Serialize)]
struct AccountUnread {
    id: String,
    unread: u64,
    folders: Vec<FolderCount>,
}

#[derive(Serialize)]
struct FolderCount {
    #[serde(flatten)]
    folder: FolderRef,
    unread: u64,
}

pub fn unread(json: bool) -> Result<()> {
    let configured = configured()?;
    let folders = match open()? {
        Some(copy) => copy.unread().map_err(|e| e.to_string())?,
        None => Vec::new(),
    };
    let accounts: Vec<AccountUnread> = configured
        .into_iter()
        .map(|id| {
            let of_account = || folders.iter().filter(|f| f.account == id);
            AccountUnread {
                // As in the `unread` event: the junk and trash folders are listed, not added up.
                unread: of_account().filter(|f| f.counted()).map(|f| f.unread).sum(),
                folders: of_account()
                    .map(|f| FolderCount {
                        folder: f.folder.clone(),
                        unread: f.unread,
                    })
                    .collect(),
                id,
            }
        })
        .collect();
    let total = accounts.iter().map(|a| a.unread).sum();
    if json {
        let unread = Unread { total, accounts };
        println!(
            "{}",
            serde_json::to_string(&unread).map_err(|e| e.to_string())?
        );
        return Ok(());
    }
    for account in &accounts {
        println!("{}: {} unread", account.id, account.unread);
        for folder in &account.folders {
            let inbox = folder.folder.role.as_deref() == Some("inbox");
            if folder.unread > 0 || inbox {
                let label = folder.folder.role.as_deref().unwrap_or(&folder.folder.name);
                println!("  {label}: {}", folder.unread);
            }
        }
    }
    println!("total: {total}");
    Ok(())
}

/// The accounts to read: the one asked for, else every configured one.
fn selected(account: Option<&String>) -> Result<Vec<String>> {
    let accounts = configured()?;
    let Some(account) = account else {
        return Ok(accounts);
    };
    if !accounts.contains(account) {
        return Err(format!(
            "no account {account} (missive account list shows them)"
        ));
    }
    Ok(vec![account.clone()])
}

pub fn search(args: SearchArgs) -> Result<()> {
    let accounts = selected(args.account.as_ref())?;
    let hits = match open()? {
        Some(copy) => copy
            .search(&Search {
                terms: args.text,
                accounts,
                role: args.folder.map(|role| role.as_str().to_owned()),
                limit: args.limit as usize,
            })
            .map_err(|e| e.to_string())?,
        None => Vec::new(),
    };
    if args.json {
        println!(
            "{}",
            serde_json::to_string(&hits).map_err(|e| e.to_string())?
        );
        return Ok(());
    }
    for hit in &hits {
        println!("{}", line(hit));
    }
    Ok(())
}

/// A hit for people: its identifier, date, sender and subject, separated by tabs.
fn line(hit: &Hit) -> String {
    let (date, from, subject) = columns(hit);
    format!("{}\t{date}\t{from}\t{subject}", hit.id)
}

/// The date, the first sender and the subject of a hit, ready to be a column each.
fn columns(hit: &Hit) -> (String, String, String) {
    let from = hit
        .from
        .first()
        .and_then(|p| {
            p.name
                .as_deref()
                .filter(|n| !n.is_empty())
                .or(p.address.as_deref())
        })
        .unwrap_or("");
    let date = hit.date.map(utc).unwrap_or_default();
    let subject = hit.subject.as_deref().unwrap_or("(no subject)");
    // A tab or a line break in a header would break the columns.
    let clean = |text: &str| text.replace(hides_text, " ");
    (date, clean(from), clean(subject))
}

pub fn list(args: ListArgs) -> Result<()> {
    let accounts = selected(args.account.as_ref())?;
    let (role, name) = match &args.folder {
        Some(folder) => (Some(folder.to_ascii_lowercase()), Some(folder.clone())),
        None => (Some("inbox".to_owned()), None),
    };
    let listed = match open()? {
        Some(copy) => {
            let matches = |folder: &FolderRef| {
                folder.role.is_some() && folder.role == role || Some(&folder.name) == name.as_ref()
            };
            if let Some(folder) = &args.folder
                && !copy
                    .unread()
                    .map_err(|e| e.to_string())?
                    .iter()
                    .any(|f| accounts.contains(&f.account) && matches(&f.folder))
            {
                return Err(format!("no folder {folder} (missive unread shows them)"));
            }
            copy.list(&List {
                accounts,
                role,
                name,
                unread: args.unread,
                limit: args.limit as usize,
            })
            .map_err(|e| e.to_string())?
        }
        None => Vec::new(),
    };
    if args.json {
        println!(
            "{}",
            serde_json::to_string(&listed).map_err(|e| e.to_string())?
        );
        return Ok(());
    }
    for message in &listed {
        let (date, from, subject) = columns(&message.hit);
        let flag = |on: bool, letter: char| if on { letter } else { '.' };
        println!(
            "{}\t{}{}{}{}\t{date}\t{from}\t{subject}",
            message.hit.id,
            flag(message.hit.unread, 'u'),
            flag(message.flagged, 'f'),
            flag(message.answered, 'a'),
            flag(message.has_attachments, '@'),
        );
    }
    Ok(())
}

#[derive(Serialize)]
struct Shown {
    #[serde(flatten)]
    hit: Hit,
    flagged: bool,
    answered: bool,
    size: u64,
    cc: Vec<Person>,
    reply_to: Vec<Person>,
    message_id: Option<String>,
    text: Option<String>,
    html: Option<String>,
    attachments: Vec<Attachment>,
}

/// The message an identifier names, for `show` and `open`.
pub fn message(id: &str) -> std::result::Result<Stored, ShowError> {
    let configured = configured()?;
    let Some(copy) = open()? else {
        return Err(ShowError::NotFound(
            "no such message in the local copy".to_owned(),
        ));
    };
    match copy.message(id)? {
        // A removed account's mail may stay in the copy: it is not shown, as `list` does not.
        Ok(stored) if configured.contains(&stored.hit.account) => Ok(stored),
        Ok(_) => Err(ShowError::NotFound(NotFound::Unknown.to_string())),
        Err(not_found) => Err(ShowError::NotFound(not_found.to_string())),
    }
}

pub fn show(args: ShowArgs) -> std::result::Result<(), ShowError> {
    let stored = message(&args.id)?;
    if args.raw {
        std::io::stdout().write_all(&stored.raw)?;
        return Ok(());
    }
    let body = body::read(&stored.raw);
    if args.json {
        let shown = Shown {
            hit: stored.hit,
            flagged: stored.flagged,
            answered: stored.answered,
            size: stored.size,
            cc: stored.cc,
            reply_to: body.reply_to,
            message_id: stored.message_id,
            text: body.text,
            html: body.html,
            attachments: body.attachments,
        };
        println!("{}", serde_json::to_string(&shown)?);
        return Ok(());
    }
    // A header or a body can hold anything: no escape sequence reaches the terminal, only the
    // line breaks and tabs of the text.
    let clean = |text: &str| text.replace(hides_text, " ");
    let people = |label: &str, people: &[Person]| {
        if !people.is_empty() {
            let people: Vec<String> = people
                .iter()
                .map(|p| match (p.name.as_deref(), p.address.as_deref()) {
                    (Some(name), Some(address)) if !name.is_empty() => {
                        format!("{name} <{address}>")
                    }
                    (_, Some(address)) => address.to_owned(),
                    (name, None) => name.unwrap_or_default().to_owned(),
                })
                .collect();
            println!("{label}: {}", clean(&people.join(", ")));
        }
    };
    people("From", &stored.hit.from);
    people("To", &stored.hit.to);
    people("Cc", &stored.cc);
    people("Reply-To", &body.reply_to);
    if let Some(date) = stored.hit.date {
        println!("Date: {}", utc(date));
    }
    println!(
        "Subject: {}",
        clean(stored.hit.subject.as_deref().unwrap_or("(no subject)"))
    );
    if let Some(id) = &stored.message_id {
        println!("Message-ID: {}", clean(id));
    }
    if !body.attachments.is_empty() {
        let attachments: Vec<String> = body
            .attachments
            .iter()
            .map(|a| {
                let size = match a.size {
                    1 => "1 byte".to_owned(),
                    n => format!("{n} bytes"),
                };
                format!(
                    "{} ({}, {size})",
                    a.name.as_deref().unwrap_or("(unnamed)"),
                    a.media_type
                )
            })
            .collect();
        println!("Attachments: {}", clean(&attachments.join(", ")));
    }
    let readable = body.readable.replace("\r\n", "\n");
    let readable: String = readable
        .chars()
        .filter(|c| !hides_text(*c) || matches!(c, '\n' | '\t'))
        .collect();
    println!("\n{readable}");
    Ok(())
}
