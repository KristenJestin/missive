//! `missive status`, `unread` and `search`: answers from the local copy alone. They open it read
//! only and never create it; they touch neither the network, nor the keyring, nor the daemon's
//! socket, so they answer with the daemon stopped and the network down.

use std::fs::{File, TryLockError};

use clap::{Args, ValueEnum};
use missive_core::account::{AccountStore, Listing};
use missive_core::copy::{Copy, FolderRef, Hit, Search};
use missive_core::protocol::pid_path;
use missive_core::store::Store;
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
        match (&account.state, account.in_copy) {
            (None, false) => println!("{}\tnever synced\t{messages}", account.id),
            (None, true) => println!(
                "{}\tunknown (the daemon is not running)\t{synced}\t{messages}",
                account.id
            ),
            (Some(state), _) => println!("{}\t{state}\t{synced}\t{messages}", account.id),
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

pub fn search(args: SearchArgs) -> Result<()> {
    let mut accounts = configured()?;
    if let Some(account) = &args.account {
        if !accounts.contains(account) {
            return Err(format!(
                "no account {account} (missive account list shows them)"
            ));
        }
        accounts = vec![account.clone()];
    }
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
    let clean = |text: &str| text.replace(|c: char| c.is_control(), " ");
    format!("{}\t{date}\t{}\t{}", hit.id, clean(from), clean(subject))
}

/// A time in seconds since the epoch as `2026-10-17 07:30 UTC`.
fn utc(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let rest = seconds.rem_euclid(86_400);
    // The civil date of a day count (Howard Hinnant's algorithm), days from 1970-01-01.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        rest / 3_600,
        rest % 3_600 / 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_time_is_written_in_utc() {
        assert_eq!(utc(0), "1970-01-01 00:00 UTC");
        assert_eq!(utc(1_792_220_640), "2026-10-17 07:04 UTC");
        // The end of a leap day and the start of the next day.
        assert_eq!(utc(1_709_251_199), "2024-02-29 23:59 UTC");
        assert_eq!(utc(1_709_251_200), "2024-03-01 00:00 UTC");
        assert_eq!(utc(-1), "1969-12-31 23:59 UTC");
    }
}
