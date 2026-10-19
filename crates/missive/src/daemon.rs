//! `missive daemon`: keeps the local copy of every account up to date, and answers on its socket
//! (see `docs/protocol.md`).
//!
//! One worker per account, each with its own IMAP connection and its own connection to the copy:
//! an account that fails never holds the others back. A worker syncs every folder, then idles in
//! the inbox (`IDLE`, re-issued at least every 25 minutes, under the 29 the RFC allows) until new
//! mail, a request, or the periodic check of the other folders.
//!
//! An action the user asks for (`mark`, `move`, `delete`) is recorded in the copy's queue by the
//! connection that asked, with its change to the copy, and answered at once, connected or not.
//! The account's worker is then woken: it applies the queue when it is connected, before each
//! pass over the folders and whenever it is woken, between syncs (see
//! [`missive_core::actions`]).
//!
//! A message to send (`send`) is built and written to the copy's outbox by the connection that
//! asked, and answered at once. Each account has a second worker for it, apart from the IMAP one
//! so that mail goes out while the IMAP server is down: it submits the outbox in order, tries
//! again what the server did not take after a wait of that message's own, and wakes the IMAP
//! worker, which appends the copy to Sent (see [`missive_core::outbox`]).
//!
//! A draft (`draft`) is built and saved in the copy the same way, and answered at once; the IMAP
//! worker, woken, writes it to the Drafts folder when it is connected, with the queue (see
//! [`missive_core::drafts`]).
//!
//! A connection that sends `subscribe` is a watcher: the daemon then writes it the events of the
//! workers (`docs/events.md`). They travel on a bounded broadcast channel, so that a watcher that
//! reads too slowly misses events, is told how many, and holds nothing else back.
//!
//! The log (standard error) says what happened in counts, durations, folder roles and error
//! kinds. It names an account by its number in the copy and a folder by its id there, never by
//! a name: an account's identifier is often its address, and a folder's name is the user's own.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use missive_core::account::{Account, AccountStore, Listing, Password};
use missive_core::actions::{self, Kind, Outcome, Wanted};
use missive_core::client;
use missive_core::compose::{self, Attachment, Draft, Followup, Mailbox};
use missive_core::connect::{ConnectError, ImapSession, client_config, open_imap};
use missive_core::copy::{Copy, MessageId};
use missive_core::drafts;
use missive_core::events::{
    ActionEvent, DraftEvent, Event, Reader, SendFailed, SendUncertain, Sent, SentCopyFailed, Unread,
};
use missive_core::keyring::{Keyring, KeyringError};
use missive_core::outbox::{self, Copied, Tried};
use missive_core::protocol::{
    self, AccountState, Answer, Command, PROTOCOL, pid_path, read_request, runtime_dir, socket_path,
};
use missive_core::store::{Followed, Retaken, SkippedFolder, Store};
use missive_core::sync::{self, Capabilities, Changes, Folder, Skip, SyncError};
use rustls::pki_types::CertificateDer;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::{Notify, broadcast, mpsc, oneshot, watch};
use tokio::task::JoinSet;

/// IDLE is re-issued after this long at most: RFC 2177 lets a server drop a client idle for 30
/// minutes, and asks clients to re-issue it every 29.
const IDLE_RENEWAL: Duration = Duration::from_secs(25 * 60);
/// How long a server is given to start IDLE, and to end it: a half-dead connection answers
/// nothing, and would hold the worker for ever.
const IDLE_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a worker in the middle of a sync is given to finish it when the daemon stops.
const STOP_GRACE: Duration = Duration::from_secs(3);
/// How long the daemon waits for its workers once it stops: the longest of the graces a worker
/// is given (a sync's, the outbox record's), and a second for it to end. Derived from them, so
/// that the record always fits inside the wait.
const JOIN_TIMEOUT: Duration = Duration::from_secs(
    (if STOP_GRACE.as_secs() > outbox::RECORD_GRACE.as_secs() {
        STOP_GRACE.as_secs()
    } else {
        outbox::RECORD_GRACE.as_secs()
    }) + 1,
);
/// The longest wait between two connections to an unreachable server.
const BACKOFF_MAX: Duration = Duration::from_secs(5 * 60);
/// How many events a watcher may be behind by before it misses some: a few minutes of a busy
/// mailbox, and little memory (an event is a few hundred bytes).
const EVENT_BUFFER: usize = 256;
/// How many lines wait for a connection's socket: a watcher whose socket is full holds these and
/// nothing else of the daemon's.
const LINE_QUEUE: usize = 16;
/// How many `new-message` events a folder gets in one sync: after a long pause thousands of
/// messages may be new at once, and a notification each would be noise. The `unread` event
/// carries the counts.
const NEW_MESSAGE_CAP: usize = 20;

#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// How often the folders other than the inbox are checked.
    pub check_interval: Duration,
    /// How many new messages are fetched, and written to the copy, at a time.
    pub batch_size: usize,
    /// How long the server is given to answer each command of an action.
    pub action_timeout: Duration,
    /// The least time between two counted tries of a copy for Sent.
    pub copy_spacing: Duration,
}

macro_rules! log {
    ($($arg:tt)*) => {
        eprintln!("missive daemon: {}", format_args!($($arg)*))
    };
}

/// Where a request waits for the end of the sync it asked for: it receives the account's state,
/// or why the local copy could not be written.
type Waiter = oneshot::Sender<Result<String, String>>;

/// What a worker is asked.
enum Request {
    /// A sync, answered when it ended.
    Sync(Waiter),
    /// Actions were recorded in the queue.
    Actions,
}

/// Runs the daemon until SIGTERM or SIGINT.
pub async fn run(options: Options) -> Result<(), String> {
    let dir = runtime_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("cannot make {} private: {e}", dir.display()))?;
    let pid_file = pid_path().map_err(|e| e.to_string())?;
    let mut lock = lock(&pid_file)?;
    // Installed before anything can take time, so that a signal never kills the daemon halfway.
    let mut terminate = signal(SignalKind::terminate()).map_err(|e| e.to_string())?;
    let mut interrupt = signal(SignalKind::interrupt()).map_err(|e| e.to_string())?;

    // Holding the lock, this daemon is the only one: a socket left there is a killed daemon's.
    let socket = socket_path().map_err(|e| e.to_string())?;
    match std::fs::remove_file(&socket) {
        Ok(()) => log!("removed the socket a stopped daemon left behind"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("cannot remove {}: {e}", socket.display())),
    }
    let listener = UnixListener::bind(&socket)
        .map_err(|e| format!("cannot listen on {}: {e}", socket.display()))?;

    let store = Store::default_path().map_err(|e| e.to_string())?;
    if let Some(parent) = store.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    // Opened once here so that a copy this build cannot use stops the daemon at once.
    let copy = Store::open(&store).map_err(|e| format!("cannot open the local copy: {e}"))?;
    match copy.release_claimed() {
        Ok(0) => {}
        Ok(released) => {
            log!("{released} messages of the outbox were claimed by the last daemon: queued again")
        }
        Err(e) => log!("cannot read the outbox ({e})"),
    }
    drop(copy);

    let (stop, stopping) = watch::channel(false);
    let daemon = Arc::new(Daemon {
        options,
        events: Arc::new(Events::new(&store)),
        store,
        stopping,
        workers: Arc::new(Mutex::new(BTreeMap::new())),
        senders: Mutex::new(BTreeMap::new()),
        rechecks: Mutex::new(BTreeMap::new()),
        tasks: Mutex::new(JoinSet::new()),
    });
    let accounts = daemon.reload();
    log!(
        "started (pid {}, protocol {PROTOCOL}): {accounts} accounts",
        std::process::id()
    );

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                if let Ok((stream, _)) = accepted {
                    tokio::spawn(daemon.clone().serve(stream));
                }
            }
            _ = terminate.recv() => break,
            _ = interrupt.recv() => break,
        }
    }

    log!("stopping");
    let _ = stop.send(true);
    let mut tasks = std::mem::take(&mut *locked(&daemon.tasks));
    let all = async { while tasks.join_next().await.is_some() {} };
    if tokio::time::timeout(JOIN_TIMEOUT, all).await.is_err() {
        log!("a worker did not stop in time and was cut off");
    }
    drop(listener);
    let _ = std::fs::remove_file(&socket);
    // Emptied rather than removed: the file stays locked until the process ends, and a pid left in
    // it would name another process some day.
    let _ = lock.set_len(0);
    let _ = lock.flush();
    log!("stopped");
    Ok(())
}

/// How long a starting daemon waits for the lock before it takes it for a running daemon's, and
/// how often it tries meanwhile. Nothing else runs yet, so sleeping the thread is harmless.
const LOCK_WAIT: Duration = Duration::from_millis(200);
const LOCK_RETRY: Duration = Duration::from_millis(5);

/// Takes the daemon's lock, or says which daemon holds it, and writes this daemon's pid.
fn lock(path: &Path) -> Result<File, String> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    // `status` takes this lock shared for a few microseconds to learn whether a daemon runs. A
    // daemon starting at that moment tries again briefly rather than quit; a daemon that really
    // runs holds the lock for good and is still refused once the wait is over.
    let giving_up = Instant::now() + LOCK_WAIT;
    loop {
        match file.try_lock() {
            Ok(()) => break,
            Err(TryLockError::WouldBlock) if Instant::now() < giving_up => {
                std::thread::sleep(LOCK_RETRY);
            }
            Err(TryLockError::WouldBlock) => {
                let mut pid = String::new();
                let _ = file.read_to_string(&mut pid);
                return Err(format!("a daemon is already running (pid {})", pid.trim()));
            }
            Err(TryLockError::Error(e)) => {
                return Err(format!("cannot lock {}: {e}", path.display()));
            }
        }
    }
    file.set_len(0)
        .and_then(|()| file.write_all(std::process::id().to_string().as_bytes()))
        .and_then(|()| file.flush())
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(file)
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The local copy opened read only, the way `missive unread` reads it: the events count the
/// unread messages with the same query. `None` (logged) when it cannot be opened.
fn open_copy(store: &Path) -> Option<Copy> {
    match Copy::open(store) {
        Ok(copy) => copy,
        Err(e) => {
            log!("cannot open the local copy to count the unread messages ({e})");
            None
        }
    }
}

/// What the daemon tells its watchers.
struct Events {
    sender: broadcast::Sender<Event>,
    /// The unread counts last told, so that only a change is told.
    unread: Mutex<Unread>,
    /// The accounts whose state this daemon told: the copy keeps the state the last daemon left
    /// for the others, which is not theirs now.
    told: Mutex<BTreeSet<String>>,
}

impl Events {
    fn new(store: &Path) -> Events {
        let unread = open_copy(store)
            .and_then(|copy| Unread::of(&copy).ok())
            .unwrap_or_default();
        Events {
            sender: broadcast::channel(EVENT_BUFFER).0,
            unread: Mutex::new(unread),
            told: Mutex::new(BTreeSet::new()),
        }
    }

    /// Without a watcher there is nobody to tell, which is not an error.
    fn send(&self, event: Event) {
        if let Event::State { account, .. } = &event {
            locked(&self.told).insert(account.clone());
        }
        let _ = self.sender.send(event);
    }

    /// Tells the unread counts when they are not the ones last told. The lock is held while
    /// sending, so that two workers' events come in the order of their counts.
    fn unread(&self, copy: &Copy) {
        let unread = match Unread::of(copy) {
            Ok(unread) => unread,
            Err(e) => return log!("cannot count the unread messages ({e})"),
        };
        let mut told = locked(&self.unread);
        if *told != unread {
            told.clone_from(&unread);
            self.send(Event::Unread(unread));
        }
    }
}

struct Daemon {
    options: Options,
    events: Arc<Events>,
    store: PathBuf,
    stopping: watch::Receiver<bool>,
    /// Where to send each account's requests, by identifier.
    workers: Arc<Mutex<BTreeMap<String, mpsc::UnboundedSender<Request>>>>,
    /// Where to wake each account's SMTP worker, by identifier.
    senders: Mutex<BTreeMap<String, mpsc::UnboundedSender<()>>>,
    /// Where to tell each account's SMTP worker that the user asked for a sync, which is how a
    /// password fixed is told (the IMAP worker reads the keyring again for it too).
    rechecks: Mutex<BTreeMap<String, Arc<Notify>>>,
    tasks: Mutex<JoinSet<()>>,
}

impl Daemon {
    /// Drops the accounts of the copy that the configuration no longer holds, with their mail:
    /// the copy only, nothing is sent to a server. An account whose settings cannot be read is
    /// still configured, so it stays; so do all of them when none is configured. Told to the
    /// watchers as a change of the unread counts.
    fn forget_removed(&self, listing: &Listing) {
        let keep: Vec<String> = listing
            .accounts
            .iter()
            .map(|(id, _)| id.clone())
            .chain(listing.skipped.iter().map(|(id, _)| id.clone()))
            .collect();
        // No account at all is more likely a wrong or unmounted configuration folder than the
        // removal of the last account: its rows stay until another account is configured.
        if keep.is_empty() {
            return;
        }
        let removed = Store::open(&self.store).and_then(|mut store| store.drop_unlisted(&keep));
        match removed {
            Ok(0) => {}
            Ok(removed) => {
                log!(
                    "removed {removed} accounts that are no longer configured from the local copy"
                );
                if let Some(copy) = open_copy(&self.store) {
                    self.events.unread(&copy);
                }
            }
            Err(e) => log!("cannot remove the accounts no longer configured from the copy ({e})"),
        }
    }

    /// Starts a worker for each account of the configuration that has none, and lets the workers
    /// of removed accounts end. Returns the number of accounts.
    fn reload(&self) -> usize {
        let listing = match AccountStore::from_env().and_then(|store| Ok((store.list()?, store))) {
            Ok(listing) => listing,
            Err(e) => {
                log!("cannot read the accounts ({})", e.kind());
                return locked(&self.workers).len();
            }
        };
        let (listing, accounts) = listing;
        for (_, kind) in &listing.skipped {
            log!("skipped an account whose settings cannot be read ({kind})");
        }
        self.forget_removed(&listing);
        let mut workers = locked(&self.workers);
        let mut senders = locked(&self.senders);
        let mut rechecks = locked(&self.rechecks);
        // An account whose settings cannot be read now keeps its worker: it was not removed.
        let kept = |id: &String| {
            listing.accounts.iter().any(|(a, _)| a == id)
                || listing.skipped.iter().any(|(s, _)| s == id)
        };
        workers.retain(|id, _| kept(id));
        senders.retain(|id, _| kept(id));
        rechecks.retain(|id, _| kept(id));
        for (id, account) in listing.accounts {
            if workers.contains_key(&id) {
                continue;
            }
            let store = match Store::open(&self.store) {
                Ok(store) => store,
                Err(e) => {
                    log!("cannot open the local copy for an account ({e})");
                    continue;
                }
            };
            let key = match store.account(&id) {
                Ok(key) => key,
                Err(e) => {
                    log!("cannot add an account to the local copy ({e})");
                    continue;
                }
            };
            let certificate = accounts.certificate(&id).unwrap_or_else(|e| {
                log!(
                    "account {key}: its imported certificate cannot be read ({})",
                    e.kind()
                );
                None
            });
            let reader = match Reader::open(&self.store) {
                Ok(reader) => reader,
                Err(e) => {
                    log!("cannot open the local copy for an account ({e})");
                    continue;
                }
            };
            let Some(copy) = open_copy(&self.store) else {
                continue;
            };
            let sending = match Store::open(&self.store) {
                Ok(store) => store,
                Err(e) => {
                    log!("cannot open the local copy for an account ({e})");
                    continue;
                }
            };
            let (requests, receiver) = mpsc::unbounded_channel();
            let (wake, woken) = mpsc::unbounded_channel();
            let recheck = Arc::new(Notify::new());
            let smtp = SmtpWorker {
                id: id.clone(),
                key,
                account: account.clone(),
                certificate: certificate.clone(),
                store: sending,
                wake: woken,
                recheck: recheck.clone(),
                imap: requests.downgrade(),
                workers: self.workers.clone(),
                stop: self.stopping.clone(),
                events: self.events.clone(),
            };
            locked(&self.tasks).spawn(smtp.run());
            workers.insert(id.clone(), requests);
            senders.insert(id.clone(), wake);
            rechecks.insert(id.clone(), recheck);
            let worker = Worker {
                id,
                key,
                account,
                certificate,
                store,
                options: self.options,
                requests: receiver,
                stop: self.stopping.clone(),
                waiting: Vec::new(),
                idle_refused: false,
                idle_backoff: Backoff::default(),
                queue_waits: false,
                queue_failed: false,
                completed: reader.completed(key).unwrap_or(false),
                reader,
                copy,
                events: self.events.clone(),
                told: None,
            };
            locked(&self.tasks).spawn(worker.run());
        }
        workers.len()
    }

    /// Answers the requests of one connection, in order, until it closes. What is written to the
    /// connection goes through a queue and its own task: a watcher's events and the answers to its
    /// requests share a socket, and a socket that is full stops that task, not this one's reading.
    async fn serve(self: Arc<Self>, stream: UnixStream) {
        let (read, write) = stream.into_split();
        let (lines, queue) = mpsc::channel(LINE_QUEUE);
        tokio::spawn(write_lines(write, queue));
        let mut lines_in = BufReader::new(read).lines();
        let mut forwarder = None;
        while let Ok(Some(line)) = lines_in.next_line().await {
            let answer = match read_request(&line) {
                Ok(request) if request.command == Command::Subscribe => {
                    if forwarder.is_some() {
                        Answer::error(
                            Some("subscribe"),
                            "failed",
                            "this connection is already subscribed",
                        )
                    } else {
                        // Subscribed before the present is read: what changes meanwhile is
                        // told twice at worst, never missed.
                        let events = self.events.sender.subscribe();
                        let present = self.present();
                        log!(
                            "a watcher subscribed ({} subscribed now)",
                            self.events.sender.receiver_count()
                        );
                        let answer = Answer {
                            protocol: PROTOCOL,
                            answer: Some("subscribe".to_owned()),
                            ok: true,
                            ..Answer::default()
                        };
                        if !send(&lines, &answer).await {
                            break;
                        }
                        forwarder = Some(tokio::spawn(forward(present, events, lines.clone())));
                        continue;
                    }
                }
                Ok(request) => self.answer(request.command).await,
                Err(answer) => answer,
            };
            if !send(&lines, &answer).await {
                break;
            }
        }
        if let Some(forwarder) = forwarder {
            // The end of what the peer has to say is not the end of what it wants to hear: a
            // client may close its writing side after `subscribe` and go on reading. The stream
            // ends when a write fails, which is the peer closing the connection, or when the
            // daemon stops.
            let _ = forwarder.await;
            log!(
                "a watcher left ({} subscribed now)",
                self.events.sender.receiver_count()
            );
        }
    }

    /// What a watcher starts with: each account's state, then the unread counts.
    fn present(&self) -> Vec<Event> {
        let reader = match Reader::open(&self.store) {
            Ok(reader) => reader,
            Err(e) => {
                log!("cannot open the local copy for a watcher ({e})");
                return Vec::new();
            }
        };
        let mut present = Vec::new();
        match reader.states() {
            Ok(states) => {
                let told = locked(&self.events.told);
                present.extend(
                    states
                        .into_iter()
                        .filter(|(account, _)| told.contains(account))
                        .map(|(account, state)| Event::State { account, state }),
                )
            }
            Err(e) => log!("cannot read the accounts' states for a watcher ({e})"),
        }
        match open_copy(&self.store).map(|copy| Unread::of(&copy)) {
            Some(Ok(unread)) => present.push(Event::Unread(unread)),
            Some(Err(e)) => log!("cannot count the unread messages for a watcher ({e})"),
            None => {}
        }
        present
    }

    async fn answer(&self, command: Command) -> Answer {
        let name = command.name();
        match command {
            Command::Sync { account } => {
                self.reload();
                let targets: Vec<(String, mpsc::UnboundedSender<Request>)> = locked(&self.workers)
                    .iter()
                    .filter(|(id, _)| account.as_ref().is_none_or(|a| a == *id))
                    .map(|(id, sender)| (id.clone(), sender.clone()))
                    .collect();
                if let Some(id) = &account
                    && targets.is_empty()
                {
                    return Answer::error(
                        Some(name),
                        "no-such-account",
                        format!("no account {id} (missive account list shows them)"),
                    );
                }
                let mut pending = Vec::new();
                for (id, sender) in targets {
                    if let Some(recheck) = locked(&self.rechecks).get(&id) {
                        recheck.notify_one();
                    }
                    let (waiter, answer) = oneshot::channel();
                    let _ = sender.send(Request::Sync(waiter));
                    pending.push((id, answer));
                }
                let mut accounts = Vec::new();
                for (id, answer) in pending {
                    match answer.await {
                        Ok(Ok(state)) => accounts.push(AccountState { id, state }),
                        Ok(Err(why)) => {
                            return Answer::error(Some(name), "failed", format!("{id}: {why}"));
                        }
                        Err(_) => {
                            return Answer::error(Some(name), "failed", "the daemon is stopping");
                        }
                    }
                }
                Answer {
                    protocol: PROTOCOL,
                    answer: Some(name.to_owned()),
                    ok: true,
                    accounts: Some(accounts),
                    ..Answer::default()
                }
            }
            Command::Subscribe => unreachable!("`serve` answers a subscription itself"),
            Command::Mark { messages, mark_as } => match Kind::parse(&mark_as) {
                Some(kind)
                    if kind.flag().is_some()
                        && !matches!(kind, Kind::Answered | Kind::Forwarded) =>
                {
                    self.record(name, &messages, Wanted::Mark(kind))
                }
                _ => Answer::error(
                    Some(name),
                    "bad-request",
                    "`as` is read, unread, flagged or unflagged",
                ),
            },
            Command::Move { messages, to } => self.record(name, &messages, Wanted::Move(to)),
            Command::Delete {
                messages,
                permanently,
            } => self.record(name, &messages, Wanted::Delete { permanently }),
            Command::Send(send) => self.enqueue(send),
            Command::Draft(draft) => self.draft(draft),
            Command::Resend { outbox, yes } => self.retake(name, outbox, yes),
            Command::Discard { outbox } => self.retake(name, outbox, false),
        }
    }

    /// Puts a `failed` or `uncertain` message of the outbox back in the queue and wakes its
    /// account's SMTP worker (`resend`), or removes it (`discard`). Only the user asks for either;
    /// an `uncertain` message is resent only with `yes`, since it may have arrived already.
    fn retake(&self, name: &str, outbox: i64, yes: bool) -> Answer {
        let resend = name == "resend";
        self.reload();
        let retaken = Store::open(&self.store).and_then(|store| {
            if resend {
                store.resend(outbox, yes)
            } else {
                store.discard(outbox)
            }
        });
        let done = if resend { "resent" } else { "discarded" };
        let account = match retaken {
            Ok(Retaken::Done(account)) => account,
            Ok(Retaken::Unknown) => {
                return Answer::error(
                    Some(name),
                    "no-such-message",
                    format!("no message {outbox} in the outbox (missive outbox lists them)"),
                );
            }
            Ok(Retaken::Uncertain) => {
                return Answer::error(
                    Some(name),
                    "uncertain",
                    format!(
                        "message {outbox} is uncertain: the server may have taken it already, so \
                         it is resent only when asked with yes"
                    ),
                );
            }
            Ok(Retaken::Not(state)) => {
                return Answer::error(
                    Some(name),
                    "not-failed",
                    format!(
                        "message {outbox} is {state}: only a failed or uncertain message can be \
                         {done}"
                    ),
                );
            }
            Err(e) => {
                log!("cannot write the outbox ({e})");
                return Answer::error(
                    Some(name),
                    "failed",
                    format!("the local copy could not be written ({e})"),
                );
            }
        };
        log!("message {outbox} of the outbox {done}, as asked");
        if resend && let Some(sender) = locked(&self.senders).get(&account) {
            let _ = sender.send(());
        }
        Answer {
            protocol: PROTOCOL,
            answer: Some(name.to_owned()),
            ok: true,
            outbox: Some(outbox),
            ..Answer::default()
        }
    }

    /// Saves a draft in the copy, and wakes the account's IMAP worker, which writes it to the
    /// Drafts folder; with `send`, puts it in the outbox too, and wakes the SMTP worker. The
    /// fields given replace those of the draft picked up. Answered once in the copy: whether
    /// the server is reachable does not matter here.
    fn draft(&self, request: protocol::SaveDraft) -> Answer {
        let name = "draft";
        self.reload();
        let no_account = || {
            Answer::error(
                Some(name),
                "no-such-account",
                format!(
                    "no account {} (missive account list shows them)",
                    request.account
                ),
            )
        };
        let (Some(sender), Some(worker)) = (
            locked(&self.senders).get(&request.account).cloned(),
            locked(&self.workers).get(&request.account).cloned(),
        ) else {
            return no_account();
        };
        let account = match AccountStore::from_env().and_then(|store| store.get(&request.account)) {
            Ok(Some(account)) => account,
            Ok(None) => return no_account(),
            Err(e) => {
                return Answer::error(
                    Some(name),
                    "failed",
                    format!("the account's settings cannot be read ({})", e.kind()),
                );
            }
        };
        let failed = |e: String| {
            log!("cannot write a draft to the local copy ({e})");
            Answer::error(
                Some(name),
                "failed",
                format!("the local copy could not be written ({e})"),
            )
        };
        let mut store = match Store::open(&self.store) {
            Ok(store) => store,
            Err(e) => return failed(e.to_string()),
        };
        let key = match store.account(&request.account) {
            Ok(key) => key,
            Err(e) => return failed(e.to_string()),
        };
        let asked = match self.followup(&store, name, &request.answers, &request.forwards) {
            Ok(asked) => asked,
            Err(refused) => return *refused,
        };
        // Only a draft Missive wrote is replaced: another client's stays on the server as it is,
        // and a new draft, Missive's own, is made from it (`kept`).
        let (mut draft, message_id, kept, source, stored_followup) = match &request.draft {
            None => (Draft::default(), None, None, None, Followup::default()),
            Some(id) => match store.find_draft(key, id) {
                Ok(Some(stored)) => match Draft::read(&stored.raw) {
                    Ok(draft) if compose::is_own_draft(&stored.raw) => (
                        draft,
                        stored.message_id,
                        None,
                        None,
                        Followup::read(&stored.raw),
                    ),
                    // Named by its own Message-ID, whichever way the request named it.
                    Ok(draft) => (
                        draft,
                        None,
                        Some(id.clone()),
                        Some(stored.message_id.unwrap_or_else(|| id.clone())),
                        Followup::default(),
                    ),
                    Err(why) => return Answer::error(Some(name), "bad-request", why),
                },
                Ok(None) => {
                    return Answer::error(
                        Some(name),
                        "no-such-message",
                        format!(
                            "{id}: no such draft in the local copy (missive list --folder drafts \
                             shows them)"
                        ),
                    );
                }
                Err(e) => return failed(e.to_string()),
            },
        };
        let sending = match (&message_id, &source) {
            (Some(id), _) => store.draft_sending(key, id),
            (None, Some(source)) => store.source_sending(key, source),
            (None, None) => Ok(false),
        };
        match sending {
            Ok(false) => {}
            Ok(true) => {
                return Answer::error(
                    Some(name),
                    "busy",
                    "this draft is being sent; missive watch tells when it is",
                );
            }
            Err(e) => return failed(e.to_string()),
        }
        // Without UIDPLUS its last version stays in the Drafts folder, listed: sending it again
        // would send the same message twice. The refusal comes once the request's own changes
        // are saved: they are not lost with it.
        let mut sent_already = false;
        if request.send
            && let Some(id) = &message_id
        {
            match store.already_sent(key, id) {
                Ok(false) => {}
                Ok(true) => sent_already = true,
                Err(e) => return failed(e.to_string()),
            }
        }
        // What the request names replaces what the draft kept; a request that names nothing
        // leaves the draft's link as it is, so that a draft picked up later still marks the
        // message it answers.
        let followup = if asked == Followup::default() {
            stored_followup.clone()
        } else {
            asked
        };
        let changed = followup != stored_followup
            || request.to.is_some()
            || request.cc.is_some()
            || request.bcc.is_some()
            || request.subject.is_some()
            || request.body.is_some()
            || request.attachments.is_some()
            || request.in_reply_to.is_some();
        let mailboxes = |list: &[String]| -> Result<Vec<Mailbox>, String> {
            list.iter().map(|text| Mailbox::parse(text)).collect()
        };
        let mut html_dropped = false;
        let fields = (|| {
            if let Some(to) = &request.to {
                draft.to = mailboxes(to)?;
            }
            if let Some(cc) = &request.cc {
                draft.cc = mailboxes(cc)?;
            }
            if let Some(bcc) = &request.bcc {
                draft.bcc = mailboxes(bcc)?;
            }
            if let Some(subject) = request.subject {
                draft.subject = Some(subject);
            }
            if let Some(body) = request.body {
                html_dropped = draft.set_body(body);
            }
            if let Some(attachments) = request.attachments {
                let attachments = attachments
                    .into_iter()
                    .map(|a| Attachment {
                        name: a.name,
                        data: a.data,
                    })
                    .collect();
                html_dropped |= draft.set_attachments(attachments);
            }
            if let Some(id) = request.in_reply_to {
                draft.in_reply_to = vec![id];
            }
            if !request.references.is_empty() {
                draft.references = request.references;
            }
            if request.send && !sent_already {
                draft.check()?;
            }
            // Under the sent message's own Message-ID the edit would be one more version of a
            // message that is gone: removed with it when a resend goes through, and never
            // sendable. A new draft keeps it.
            let version_of = message_id.as_deref().filter(|_| !sent_already);
            compose::write(&account, &draft, now(), version_of)
        })();
        let built = match fields {
            Ok(built) => built,
            Err(why) => return Answer::error(Some(name), "bad-request", why),
        };
        // A draft sent as it is needs no new version.
        let save = !request.send || (request.draft.is_some() && changed);
        if save {
            let saved = store.save_draft(key, &built.message_id, &built.as_draft(&followup));
            if let Err(e) = saved {
                return failed(e.to_string());
            }
            log!(
                "account {key}: a draft is saved in the local copy ({} bytes)",
                built.raw.len()
            );
            let _ = worker.send(Request::Actions);
        }
        if sent_already {
            let mut answer = if save {
                Answer::error(
                    Some(name),
                    "already-sent",
                    format!(
                        "already sent; your changes are saved as a new draft {}",
                        built.message_id
                    ),
                )
            } else {
                Answer::error(
                    Some(name),
                    "already-sent",
                    "this draft was already sent (its message is in the Sent folder or was sent \
                     from the outbox); write a new message instead",
                )
            };
            answer.message_id = save.then_some(built.message_id);
            return answer;
        }
        if !request.send {
            return Answer {
                protocol: PROTOCOL,
                answer: Some(name.to_owned()),
                ok: true,
                message_id: Some(built.message_id),
                kept,
                html_dropped,
                ..Answer::default()
            };
        }
        // The draft removed once the message is sent: Missive's own only.
        let draft = (save || (request.draft.is_some() && kept.is_none()))
            .then_some(built.message_id.as_str());
        let sent_copy = !account.server_saves_sent;
        let followed = followed(followup.answers.as_deref(), followup.forwards.as_deref());
        let queued = match (&source, followed) {
            (Some(source), followed) => {
                store.enqueue_kept(key, &built, sent_copy, draft, followed, source)
            }
            (None, Some(followed)) => {
                store.enqueue_followup(key, &built, sent_copy, draft, followed)
            }
            (None, None) => store.enqueue(key, &built, sent_copy, draft),
        };
        let outbox = match queued {
            Ok(outbox) => outbox,
            Err(e) => return failed(e.to_string()),
        };
        log!(
            "account {key}: message {outbox} is in the outbox ({} recipients, {} bytes)",
            built.recipients.len(),
            built.raw.len()
        );
        let _ = sender.send(());
        Answer {
            protocol: PROTOCOL,
            answer: Some(name.to_owned()),
            ok: true,
            outbox: Some(outbox),
            message_id: Some(built.message_id),
            kept,
            html_dropped,
            ..Answer::default()
        }
    }

    /// The message a request says it answers or forwards, checked: it names one message of the
    /// copy, whichever account's (a reply may be written from another account), and no more
    /// than an identifier. The answer to give when it does not.
    fn followup(
        &self,
        store: &Store,
        name: &str,
        answers: &Option<String>,
        forwards: &Option<String>,
    ) -> Result<Followup, Box<Answer>> {
        let asked = Followup {
            answers: answers.clone(),
            forwards: forwards.clone(),
        };
        for id in asked.answers.iter().chain(&asked.forwards) {
            match store.check_message(id) {
                Ok(Ok(())) => {}
                Ok(Err(refused)) => {
                    return Err(Box::new(Answer::error(
                        Some(name),
                        refused.error,
                        refused.message,
                    )));
                }
                Err(e) => {
                    log!("cannot read the local copy ({e})");
                    return Err(Box::new(Answer::error(
                        Some(name),
                        "failed",
                        format!("the local copy could not be read ({e})"),
                    )));
                }
            }
        }
        asked
            .check()
            .map_err(|why| Box::new(Answer::error(Some(name), "bad-request", why)))?;
        Ok(asked)
    }

    /// Builds a message and writes it to its account's outbox, then wakes the account's SMTP
    /// worker. Answered once it is in the outbox: whether the server is reachable does not
    /// matter here.
    fn enqueue(&self, send: protocol::Send) -> Answer {
        let name = "send";
        self.reload();
        let Some(sender) = locked(&self.senders).get(&send.account).cloned() else {
            return Answer::error(
                Some(name),
                "no-such-account",
                format!(
                    "no account {} (missive account list shows them)",
                    send.account
                ),
            );
        };
        let account = match AccountStore::from_env().and_then(|store| store.get(&send.account)) {
            Ok(Some(account)) => account,
            Ok(None) => {
                return Answer::error(
                    Some(name),
                    "no-such-account",
                    format!(
                        "no account {} (missive account list shows them)",
                        send.account
                    ),
                );
            }
            Err(e) => {
                return Answer::error(
                    Some(name),
                    "failed",
                    format!("the account's settings cannot be read ({})", e.kind()),
                );
            }
        };
        let mailboxes = |list: &[String]| -> Result<Vec<Mailbox>, String> {
            list.iter().map(|text| Mailbox::parse(text)).collect()
        };
        let draft = (|| {
            Ok::<_, String>(Draft {
                to: mailboxes(&send.to)?,
                cc: mailboxes(&send.cc)?,
                bcc: mailboxes(&send.bcc)?,
                subject: send.subject,
                body: send.body,
                html: None,
                attachments: send
                    .attachments
                    .into_iter()
                    .map(|a| Attachment {
                        name: a.name,
                        data: a.data,
                    })
                    .collect(),
                in_reply_to: send.in_reply_to.into_iter().collect(),
                references: send.references,
            })
        })();
        let built = match draft.and_then(|draft| compose::build(&account, &draft, now())) {
            Ok(built) => built,
            Err(why) => return Answer::error(Some(name), "bad-request", why),
        };
        let checked = match Store::open(&self.store) {
            Ok(store) => self.followup(&store, name, &send.answers, &send.forwards),
            Err(e) => {
                log!("cannot read the local copy ({e})");
                return Answer::error(
                    Some(name),
                    "failed",
                    format!("the local copy could not be read ({e})"),
                );
            }
        };
        if let Err(refused) = checked {
            return *refused;
        }
        let queued = Store::open(&self.store).and_then(|store| {
            let key = store.account(&send.account)?;
            let sent_copy = !account.server_saves_sent;
            Ok((
                key,
                match followed(send.answers.as_deref(), send.forwards.as_deref()) {
                    Some(followed) => {
                        store.enqueue_followup(key, &built, sent_copy, None, followed)?
                    }
                    None => store.enqueue(key, &built, sent_copy, None)?,
                },
            ))
        });
        let (key, outbox) = match queued {
            Ok(queued) => queued,
            Err(e) => {
                log!("cannot write a message to the outbox ({e})");
                return Answer::error(
                    Some(name),
                    "failed",
                    format!("the local copy could not be written ({e})"),
                );
            }
        };
        log!(
            "account {key}: message {outbox} is in the outbox ({} recipients, {} bytes)",
            built.recipients.len(),
            built.raw.len()
        );
        let _ = sender.send(());
        Answer {
            protocol: PROTOCOL,
            answer: Some(name.to_owned()),
            ok: true,
            outbox: Some(outbox),
            message_id: Some(built.message_id),
            ..Answer::default()
        }
    }

    /// Records an action in the queue, with its change to the copy, and wakes the workers of its
    /// accounts. Answered once recorded: whether the server is reachable does not matter here.
    fn record(&self, name: &str, messages: &[String], wanted: Wanted) -> Answer {
        if messages.is_empty() {
            return Answer::error(Some(name), "bad-request", "no message is named");
        }
        self.reload();
        let workers = locked(&self.workers).clone();
        // A message of an account that is no longer configured has no worker to apply it.
        for id in messages {
            if let Some(parsed) = MessageId::parse(id)
                && !workers.contains_key(&parsed.account)
            {
                return Answer::error(
                    Some(name),
                    "no-such-account",
                    format!(
                        "no account {} (missive account list shows them)",
                        parsed.account
                    ),
                );
            }
        }
        let queued = Store::open(&self.store).and_then(|mut store| store.queue(messages, &wanted));
        let queued = match queued {
            Ok(Ok(queued)) => queued,
            Ok(Err(refused)) => return Answer::error(Some(name), refused.error, refused.message),
            Err(e) => {
                log!("cannot record an action in the local copy ({e})");
                return Answer::error(
                    Some(name),
                    "failed",
                    format!("the local copy could not be written ({e})"),
                );
            }
        };
        log!(
            "recorded an action ({name}) on {} messages",
            queued.messages.len()
        );
        for account in &queued.accounts {
            if let Some(worker) = workers.get(account) {
                let _ = worker.send(Request::Actions);
            }
        }
        if let Some(copy) = open_copy(&self.store) {
            self.events.unread(&copy);
        }
        Answer {
            protocol: PROTOCOL,
            answer: Some(name.to_owned()),
            ok: true,
            messages: Some(queued.messages),
            actions: Some(queued.actions),
            ..Answer::default()
        }
    }
}

/// Now, in seconds since the epoch.
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Queues an answer for the connection's writer. `false` when the connection is gone.
async fn send(lines: &mpsc::Sender<String>, answer: &Answer) -> bool {
    let mut line = serde_json::to_string(answer).expect("an answer serializes");
    line.push('\n');
    lines.send(line).await.is_ok()
}

/// Writes the lines of a connection until it fails or no one has more to say.
async fn write_lines(mut write: OwnedWriteHalf, mut queue: mpsc::Receiver<String>) {
    while let Some(line) = queue.recv().await {
        if write.write_all(line.as_bytes()).await.is_err() {
            return;
        }
    }
}

/// Queues `present`, then each event as it comes, for one watcher. The queue is full when the
/// watcher's socket is: this then waits for room, and the events pile up in the channel, which
/// drops the oldest ones past [`EVENT_BUFFER`]: the watcher is told how many when it reads again.
async fn forward(
    present: Vec<Event>,
    mut events: broadcast::Receiver<Event>,
    lines: mpsc::Sender<String>,
) {
    for event in present {
        if lines.send(event.line() + "\n").await.is_err() {
            return;
        }
    }
    loop {
        let event = match events.recv().await {
            Ok(event) => event,
            Err(RecvError::Lagged(missed)) => {
                log!("a watcher missed {missed} events and was told so");
                Event::Lagged { missed }
            }
            Err(RecvError::Closed) => return,
        };
        if lines.send(event.line() + "\n").await.is_err() {
            return;
        }
    }
}

/// Resolves once the daemon is stopping.
async fn stopped(stop: &mut watch::Receiver<bool>) {
    loop {
        if *stop.borrow_and_update() {
            return;
        }
        if stop.changed().await.is_err() {
            return std::future::pending().await;
        }
    }
}

/// What ended a session with the server.
enum Ended {
    /// The daemon is stopping, or the account was removed.
    Stopped,
    /// The connection failed, or the copy could not be written: connect again later.
    Lost(SyncError),
    /// The server did not answer IDLE in time although it advertises it: what the connection is
    /// in is not known, so it is dropped, and the next one tries IDLE again.
    IdleStalled,
}

/// What ended a wait between two syncs.
enum Wake {
    /// New data in the inbox.
    Inbox,
    /// A sync was asked for, or the periodic check is due.
    Sync,
    /// Actions were recorded.
    Actions,
    /// IDLE ran its time, or the server refused it; the periodic check may not be due yet.
    Renew,
    Stop,
}

/// What ended [`Worker::wait`].
#[derive(Debug, PartialEq, Eq)]
enum Waited {
    Sync,
    Actions,
    Timeout,
    Stop,
}

/// Connection attempts to an unreachable server: 1 second apart, then twice as long each time, up
/// to [`BACKOFF_MAX`].
#[derive(Default)]
struct Backoff(Duration);

impl Backoff {
    fn next(&mut self) -> Duration {
        self.0 = if self.0.is_zero() {
            Duration::from_secs(1)
        } else {
            (self.0 * 2).min(BACKOFF_MAX)
        };
        self.0
    }
}

struct Worker {
    id: String,
    /// The account's number in the copy, which the log names it by.
    key: i64,
    account: Account,
    certificate: Option<CertificateDer<'static>>,
    store: Store,
    options: Options,
    requests: mpsc::UnboundedReceiver<Request>,
    stop: watch::Receiver<bool>,
    /// The requests the next sync answers.
    waiting: Vec<Waiter>,
    /// Whether the server refused IDLE on this connection: the inbox is then checked with the
    /// other folders until the next connection, which tries IDLE again.
    idle_refused: bool,
    /// Connection attempts after IDLE stalled, so that a server that never answers it is not
    /// connected to again and again: reset when IDLE starts.
    idle_backoff: Backoff,
    /// Whether the first action of the queue failed (a `NO`, a lost connection…): the queue then
    /// waits for the next pass, rather than being tried again at each wake.
    queue_waits: bool,
    /// Whether the queue stopped on an error: the next pass syncs the folders before it tries
    /// the queue again, so that an action that always fails never keeps the account from its mail.
    queue_failed: bool,
    /// Whether a pass over every folder of the account ever ended: until then what a sync brings
    /// is the account's mail so far, not mail that arrives.
    completed: bool,
    reader: Reader,
    /// The same copy, for the unread counts: see [`open_copy`].
    copy: Copy,
    events: Arc<Events>,
    /// The state last told to the watchers.
    told: Option<String>,
}

impl Worker {
    async fn run(mut self) {
        let tls = client_config(self.certificate.clone());
        let mut password: Option<Password> = None;
        let mut backoff = Backoff::default();
        loop {
            self.take_requests();
            if password.is_none() {
                // A keyring that asks for a passphrase holds the read until someone types it:
                // a stop does not wait for it.
                let mut stop = self.stop.clone();
                let read = tokio::select! {
                    read = self.password() => read,
                    _ = stopped(&mut stop) => return,
                };
                match read {
                    Some(found) => password = Some(found),
                    None => {
                        self.settle("no-login");
                        if self.wait(None, false).await == Waited::Stop {
                            return;
                        }
                        continue;
                    }
                }
            }
            let login = password.as_ref().expect("read above");
            self.state("syncing");
            let opened = tokio::select! {
                opened = open_imap(&self.account.imap, &self.account.user, login, tls.clone()) => opened,
                _ = stopped(&mut self.stop) => return,
            };
            let retry = match opened {
                Ok(session) => match self.connected(session, &mut backoff).await {
                    Ended::Stopped => return,
                    // Not unreachable: the server answered everything else.
                    Ended::IdleStalled => {
                        let delay = self.idle_backoff.next();
                        log!(
                            "account {}: the server did not start IDLE within {} s; connecting \
                             again in {} s",
                            self.key,
                            IDLE_COMMAND_TIMEOUT.as_secs(),
                            delay.as_secs()
                        );
                        Some(delay)
                    }
                    // Answered and recorded now: a session lost at the same point at every
                    // pass would otherwise leave the requests waiting, and the state `syncing`,
                    // for ever.
                    Ended::Lost(error) => {
                        log!("account {}: the session ended ({})", self.key, kind(&error));
                        match error {
                            SyncError::Connection(_) => self.settle("unreachable"),
                            SyncError::Store(error) => {
                                let why = format!("the local copy could not be written ({error})");
                                for waiter in self.waiting.drain(..) {
                                    let _ = waiter.send(Err(why.clone()));
                                }
                            }
                        }
                        Some(backoff.next())
                    }
                },
                // Asking the server again with the same password would only be refused again:
                // only a request (after the user fixed it) tries again.
                Err(ConnectError::AuthRefused) => {
                    password = None;
                    self.settle("auth-refused");
                    None
                }
                Err(ConnectError::CertificateRefused) => {
                    self.settle("certificate-refused");
                    None
                }
                Err(ConnectError::Unreachable(reason)) => {
                    self.settle("unreachable");
                    let delay = backoff.next();
                    log!(
                        "account {}: {reason}; trying again in {} s",
                        self.key,
                        delay.as_secs()
                    );
                    Some(delay)
                }
            };
            if self.wait(retry, false).await == Waited::Stop {
                return;
            }
        }
    }

    /// The account's password from the keyring. `None` when it has none or cannot be read.
    async fn password(&mut self) -> Option<Password> {
        let found = match Keyring::open().await {
            Ok(keyring) => keyring.password(&self.id).await,
            Err(e) => Err(e),
        };
        match found {
            Ok(found) => found,
            Err(e) => {
                let why = match e {
                    KeyringError::Unavailable(_) => "it is unavailable",
                    KeyringError::NoDefaultCollection => "it has no default collection",
                    KeyringError::Dismissed => "it stayed locked",
                    KeyringError::Service(_) => "it answered an error",
                };
                log!("account {}: the keyring cannot be read: {why}", self.key);
                None
            }
        }
    }

    /// Moves the requests sent so far to those the next sync answers. Actions need no answer:
    /// the next pass applies the queue.
    fn take_requests(&mut self) {
        while let Ok(request) = self.requests.try_recv() {
            if let Request::Sync(waiter) = request {
                self.waiting.push(waiter);
            }
        }
    }

    fn state(&mut self, state: &str) {
        if let Err(e) = self.store.set_state(self.key, state, false) {
            log!("account {}: cannot record its state ({e})", self.key);
        }
        self.tell(state);
    }

    /// Tells the watchers the state when it is not the one they know.
    fn tell(&mut self, state: &str) {
        if self.told.as_deref() != Some(state) {
            self.told = Some(state.to_owned());
            self.events.send(Event::State {
                account: self.id.clone(),
                state: state.to_owned(),
            });
        }
    }

    /// Records a state the account stays in until something changes, and answers the requests.
    fn settle(&mut self, state: &str) {
        self.state(state);
        log!("account {}: {state}", self.key);
        self.answer(state);
    }

    fn answer(&mut self, state: &str) {
        for waiter in self.waiting.drain(..) {
            let _ = waiter.send(Ok(state.to_owned()));
        }
    }

    /// Waits for a request, for `timeout` when there is one, or for the daemon to stop. Actions
    /// end the wait only when `actions`: without a connection they wait for the next one, and do
    /// not make the worker connect sooner than its backoff says.
    async fn wait(&mut self, timeout: Option<Duration>, actions: bool) -> Waited {
        let sleep = async {
            match timeout {
                Some(timeout) => tokio::time::sleep(timeout).await,
                None => std::future::pending().await,
            }
        };
        tokio::pin!(sleep);
        loop {
            return tokio::select! {
                request = self.requests.recv() => match request {
                    Some(Request::Sync(waiter)) => {
                        self.waiting.push(waiter);
                        Waited::Sync
                    }
                    Some(Request::Actions) if actions => Waited::Actions,
                    Some(Request::Actions) => continue,
                    None => Waited::Stop,
                },
                _ = &mut sleep => Waited::Timeout,
                _ = stopped(&mut self.stop) => Waited::Stop,
            };
        }
    }

    /// Syncs over a session until it ends, then logs out when it can.
    async fn connected(&mut self, session: ImapSession, backoff: &mut Backoff) -> Ended {
        let mut stop = self.stop.clone();
        let (ended, session) = tokio::select! {
            ended = self.session(session, backoff) => ended,
            // A sync that does not reach a point where it can stop is cut off: what it wrote is
            // whole, since no transaction waits on the network.
            _ = async { stopped(&mut stop).await; tokio::time::sleep(STOP_GRACE).await } => {
                (Ended::Stopped, None)
            }
        };
        if let (Ended::Stopped, Some(mut session)) = (&ended, session)
            && tokio::time::timeout(Duration::from_secs(2), session.logout())
                .await
                .is_ok_and(|logout| logout.is_ok())
        {
            log!("account {}: logged out", self.key);
        }
        ended
    }

    async fn session(
        &mut self,
        mut session: ImapSession,
        backoff: &mut Backoff,
    ) -> (Ended, Option<ImapSession>) {
        macro_rules! check {
            ($result:expr) => {
                match $result {
                    Ok(value) => value,
                    Err(error) => return (Ended::Lost(error), Some(session)),
                }
            };
        }
        let capabilities = check!(sync::prepare(&mut session).await);
        if let Err(e) = self.store.set_capabilities(self.key, capabilities) {
            log!("account {}: cannot record its capabilities ({e})", self.key);
        }
        self.idle_refused = false;
        log!(
            "account {}: connected ({}, {})",
            self.key,
            if capabilities.condstore {
                "CONDSTORE"
            } else {
                "no CONDSTORE"
            },
            if capabilities.idle { "IDLE" } else { "no IDLE" }
        );
        loop {
            // What waits in the queue first, then every folder; after a queue that failed, the
            // folders first.
            self.take_requests();
            let queue_first = !self.queue_failed;
            if queue_first {
                check!(self.apply(&mut session, capabilities, &mut [], true).await);
            }
            self.events.send(Event::SyncStarted {
                account: self.id.clone(),
            });
            self.state("syncing");
            let started = Instant::now();
            let mut folders = check!(sync::folders(&mut session, &mut self.store, self.key).await);
            let mut changes = Changes::default();
            let mut skipped = Vec::new();
            for folder in &mut folders {
                if *self.stop.borrow() {
                    return (Ended::Stopped, Some(session));
                }
                match check!(self.folder(&mut session, folder).await) {
                    Ok(synced) => changes += synced,
                    // The account is `ready` all the same: the pass says what it left out.
                    Err(skip) => skipped.push(SkippedFolder {
                        folder: folder
                            .role
                            .map_or_else(|| folder.name.clone(), str::to_owned),
                        kind: skip.name().to_owned(),
                    }),
                }
            }
            if let Err(e) = self.store.set_skipped(self.key, &skipped) {
                log!(
                    "account {}: cannot record the skipped folders ({e})",
                    self.key
                );
            }
            if let Err(e) = self.store.set_state(self.key, "ready", true) {
                log!("account {}: cannot record its state ({e})", self.key);
            }
            self.completed = true;
            self.tell("ready");
            self.events.send(Event::SyncEnded {
                account: self.id.clone(),
                folders: folders.len() - skipped.len(),
                new: changes.new,
                removed: changes.removed,
                flags: changes.flags,
                skipped: skipped.clone(),
            });
            log!(
                "account {}: ready, {} folders synced in {:.2} s ({} new, {} removed, {} flag \
                 changes, {} rebuilt, {} skipped)",
                self.key,
                folders.len() - skipped.len(),
                started.elapsed().as_secs_f64(),
                changes.new,
                changes.removed,
                changes.flags,
                changes.rebuilt,
                skipped.len()
            );
            self.answer("ready");
            *backoff = Backoff::default();
            if !queue_first {
                check!(
                    self.apply(&mut session, capabilities, &mut folders, true)
                        .await
                );
            }

            // Then the inbox only, until the next check of every folder.
            let inbox = folders.iter().position(|f| f.role == Some("inbox"));
            let next_check = Instant::now() + self.options.check_interval;
            loop {
                check!(
                    self.apply(&mut session, capabilities, &mut folders, false)
                        .await
                );
                let left = next_check.saturating_duration_since(Instant::now());
                // A copy for Sent due again wakes the worker, which tries it at the top of this
                // loop: it would otherwise wait for a request or the next check.
                let left = left.min(self.copy_due().unwrap_or(left));
                let wake = match (capabilities.idle && !self.idle_refused, inbox) {
                    (true, Some(inbox)) => {
                        let changes = check!(self.folder(&mut session, &mut folders[inbox]).await)
                            .unwrap_or_default();
                        if changes != Changes::default() {
                            log!(
                                "account {} folder {} (inbox): {} new, {} removed, {} flag changes",
                                self.key,
                                folders[inbox].row.id,
                                changes.new,
                                changes.removed,
                                changes.flags
                            );
                        }
                        let wake;
                        (wake, session) = match self.idle(session, left.min(IDLE_RENEWAL)).await {
                            Ok(idled) => idled,
                            Err(ended) => return (ended, None),
                        };
                        wake
                    }
                    _ => match self.wait(Some(left), true).await {
                        Waited::Stop => Wake::Stop,
                        Waited::Actions => Wake::Actions,
                        Waited::Sync => Wake::Sync,
                        // The check of every folder is made only when it is due.
                        Waited::Timeout => Wake::Renew,
                    },
                };
                match wake {
                    Wake::Stop => return (Ended::Stopped, Some(session)),
                    Wake::Sync => break,
                    Wake::Renew if Instant::now() >= next_check => break,
                    // The actions are applied at the top of the loop.
                    Wake::Renew | Wake::Inbox | Wake::Actions => {}
                }
            }
        }
    }

    /// How long until the earliest copy for Sent may be tried again. `None` when none waits, or
    /// when its time is past: it was not tried at this pass (no Sent folder yet, say), so a timer
    /// would only fire again at once, and the next pass or wake takes it.
    fn copy_due(&self) -> Option<Duration> {
        let next_at = match self.store.next_copy_at(self.key) {
            Ok(next_at) => next_at?,
            Err(e) => {
                log!("account {}: cannot read the outbox ({e})", self.key);
                return None;
            }
        };
        let late = u64::try_from(next_at.checked_sub(outbox::now())?).ok()?;
        (late > 0).then(|| Duration::from_millis(late))
    }

    /// Applies the account's queue on the server, tells what became of each action, and syncs
    /// at once those of `folders` whose copy has to catch up with the server. An action that
    /// failed is tried again at the next `pass` only. A connection error of the queue is an error
    /// only when a command got no answer in time; otherwise the session goes on, and its next
    /// command tells whether the connection is still there.
    async fn apply(
        &mut self,
        session: &mut ImapSession,
        capabilities: Capabilities,
        folders: &mut [Folder],
        pass: bool,
    ) -> Result<(), SyncError> {
        let (copies, copied) = outbox::copy_sent(
            session,
            &mut self.store,
            self.key,
            self.options.action_timeout,
            self.options.copy_spacing,
        )
        .await;
        for copy in &copies.copied {
            match copy {
                Copied::Appended { id } => {
                    log!("account {}: message {id} is in Sent", self.key);
                }
                Copied::Retried { id, attempts } => log!(
                    "account {}: the copy of message {id} could not be appended to Sent \
                     ({attempts} times so far)",
                    self.key
                ),
                Copied::GivenUp {
                    id,
                    message_id,
                    why,
                } => {
                    log!(
                        "account {}: the copy of message {id} is not in Sent: {why}",
                        self.key
                    );
                    self.events.send(Event::SentCopyFailed(SentCopyFailed {
                        account: self.id.clone(),
                        outbox: *id,
                        message_id: message_id.clone(),
                        message: format!(
                            "the message was sent, but its copy is not in Sent: {why}"
                        ),
                    }));
                }
            }
        }
        match copied {
            Ok(()) => {}
            Err(error) if copies.timed_out => return Err(error),
            Err(error @ SyncError::Connection(_)) => {
                log!(
                    "account {}: the copies for Sent stopped ({})",
                    self.key,
                    kind(&error)
                );
            }
            Err(error) => return Err(error),
        }
        if let Some(id) = copies.resync
            && let Some(folder) = folders.iter_mut().find(|f| f.row.id == id)
        {
            // A folder skipped here is told by the next pass.
            let _ = self.folder(session, folder).await?;
        }
        let (written, result) = drafts::write(
            session,
            &mut self.store,
            self.key,
            &self.id,
            capabilities,
            self.options.action_timeout,
            pass,
        )
        .await;
        for outcome in &written.outcomes {
            self.tell_draft(outcome);
        }
        match result {
            Ok(()) => {}
            Err(error) if written.timed_out => return Err(error),
            Err(error @ SyncError::Connection(_)) => {
                log!(
                    "account {}: the drafts stopped ({})",
                    self.key,
                    kind(&error)
                );
            }
            Err(error) => return Err(error),
        }
        if let Some(id) = written.resync
            && let Some(folder) = folders.iter_mut().find(|f| f.row.id == id)
        {
            // A folder skipped here is told by the next pass.
            let _ = self.folder(session, folder).await?;
        }
        if self.queue_waits && !pass {
            return Ok(());
        }
        let (applied, result) = actions::apply(
            session,
            &mut self.store,
            self.key,
            &self.id,
            capabilities,
            self.options.action_timeout,
        )
        .await;
        self.queue_waits = applied
            .outcomes
            .iter()
            .any(|outcome| matches!(outcome.result, actions::Result::Retried { .. }));
        self.queue_failed = result.is_err();
        if applied.taken_back > 0 {
            log!(
                "account {}: took back the \\Deleted flag a refused action left on {} messages",
                self.key,
                applied.taken_back
            );
        }
        for outcome in &applied.outcomes {
            self.tell_outcome(outcome);
        }
        if !applied.outcomes.is_empty() {
            self.events.unread(&self.copy);
        }
        match result {
            Ok(()) => {}
            // What the connection says next cannot be trusted.
            Err(error) if applied.timed_out => return Err(error),
            Err(error @ SyncError::Connection(_)) => {
                log!("account {}: the queue stopped ({})", self.key, kind(&error));
            }
            Err(error) => return Err(error),
        }
        for id in applied.resync {
            if let Some(folder) = folders.iter_mut().find(|f| f.row.id == id) {
                // The queue may have changed what the copy knows of it (its HIGHESTMODSEQ).
                folder.row = self.store.folder(self.key, &folder.name, folder.role)?;
                // A folder skipped here is told by the next pass.
                let _ = self.folder(session, folder).await?;
            }
        }
        Ok(())
    }

    /// Logs what became of an action, and tells the watchers.
    fn tell_outcome(&mut self, outcome: &Outcome) {
        let (name, reason, attempts, message, copied, maybe_copied) = match &outcome.result {
            actions::Result::Applied => ("applied", None, None, None, None, None),
            actions::Result::Retried { attempts, reason } => {
                ("retried", Some(*reason), Some(*attempts), None, None, None)
            }
            actions::Result::Refused {
                reason,
                message,
                copied,
                maybe_copied,
            } => (
                "refused",
                Some(*reason),
                None,
                Some(message.clone()),
                copied.then_some(true),
                maybe_copied.then_some(true),
            ),
        };
        log!(
            "account {}: action {} ({}, {} messages) {name}{}",
            self.key,
            outcome.action,
            outcome.kind.name(),
            outcome.messages.len(),
            match (reason, &message) {
                (_, Some(message)) => format!(": {message}"),
                (Some(reason), None) => format!(" ({reason})"),
                (None, None) => String::new(),
            }
        );
        self.events.send(Event::Action(ActionEvent {
            account: self.id.clone(),
            action: outcome.kind.name().to_owned(),
            outcome: name.to_owned(),
            messages: outcome.messages.clone(),
            id: outcome.action,
            reason: reason.map(str::to_owned),
            attempts,
            message,
            copied,
            maybe_copied,
        }));
    }

    /// Logs what became of a draft, and tells the watchers.
    fn tell_draft(&mut self, outcome: &drafts::Outcome) {
        let mut event = DraftEvent {
            account: self.id.clone(),
            draft: outcome.draft.clone(),
            outcome: String::new(),
            id: None,
            left: None,
            reason: None,
            attempts: None,
            message: None,
        };
        let left = match &outcome.result {
            drafts::Result::Saved { id, left } => {
                event.outcome = "saved".to_owned();
                event.id.clone_from(id);
                Some(left)
            }
            drafts::Result::Removed { left } => {
                event.outcome = "removed".to_owned();
                Some(left)
            }
            drafts::Result::Retried { attempts, reason } => {
                event.outcome = "retried".to_owned();
                event.reason = Some((*reason).to_owned());
                event.attempts = Some(*attempts);
                None
            }
        };
        if let Some(left) = left {
            event.left = Some(left.count);
            event.message = left.why.map(str::to_owned);
        }
        log!(
            "account {}: draft {} {}{}",
            self.key,
            outcome.row,
            event.outcome,
            match (&event.reason, &event.message) {
                (_, Some(why)) => format!(": {why}"),
                (Some(reason), None) =>
                    format!(" ({reason}, {} times so far)", event.attempts.unwrap_or(0)),
                (None, None) => String::new(),
            }
        );
        self.events.send(Event::Draft(event));
    }

    /// Syncs one folder and logs what is worth knowing.
    async fn folder(
        &mut self,
        session: &mut ImapSession,
        folder: &mut Folder,
    ) -> Result<Result<Changes, Skip>, SyncError> {
        // What arrives is announced one message at a time only in a folder that was synced before:
        // the first sync of an account, of a folder that is new, or of one the server renumbered,
        // brings mail the user already has.
        let last_uid = (self.completed && folder.row.uidvalidity.is_some())
            .then(|| self.reader.last_uid(folder.row.id).ok())
            .flatten();
        let validity = folder.row.uidvalidity;
        let changes =
            match sync::folder(session, &mut self.store, folder, self.options.batch_size).await {
                Ok(changes) => changes,
                Err(e) => {
                    // The batches that came before the error are in the copy, and the next pass
                    // starts from their highest UID: they are told now or never.
                    if folder.row.uidvalidity == validity {
                        self.announce(folder, last_uid).await;
                    }
                    self.events.unread(&self.copy);
                    return Err(e);
                }
            };
        let role = folder.role.unwrap_or("no role");
        match changes {
            Ok(changes) => {
                self.publish(folder, &changes, last_uid).await;
                if changes.rebuilt > 0 {
                    log!(
                        "account {} folder {} ({role}): its UIDVALIDITY changed, so it was rebuilt",
                        self.key,
                        folder.row.id
                    );
                }
                if changes.refused > 0 {
                    log!(
                        "account {} folder {} ({role}): {} refused by the server, asked again at \
                         the next pass",
                        self.key,
                        folder.row.id,
                        changes.refused
                    );
                }
                Ok(Ok(changes))
            }
            Err(skip) => {
                log!(
                    "account {} folder {} ({role}): left as it is: the server refused to examine or \
                     list it, or answered something no sync can use",
                    self.key,
                    folder.row.id
                );
                Ok(Err(skip))
            }
        }
    }

    /// Tells the watchers what a folder's sync changed.
    async fn publish(&mut self, folder: &Folder, changes: &Changes, last_uid: Option<u32>) {
        if changes.new == 0 && changes.removed == 0 && changes.flags == 0 && changes.rebuilt == 0 {
            return;
        }
        if changes.new > 0 && changes.rebuilt == 0 {
            self.announce(folder, last_uid).await;
        }
        self.events.unread(&self.copy);
    }

    /// Tells the watchers of the messages of a folder above `last_uid`, the highest UID it held
    /// before this sync. `None` when its mail is not news: see [`Worker::folder`].
    async fn announce(&mut self, folder: &Folder, last_uid: Option<u32>) {
        let Some(last_uid) = last_uid else {
            return;
        };
        match self.reader.arrived(
            &self.id,
            folder.role,
            folder.row.id,
            last_uid,
            NEW_MESSAGE_CAP,
        ) {
            Ok(messages) => {
                for message in messages {
                    self.events.send(Event::NewMessage(message));
                    // The daemon runs on one thread: without this the whole burst is put in the
                    // channel before any watcher task runs, and the channel's room is all a
                    // watcher has. Giving the watchers a turn after each event lets those that
                    // are keeping up take theirs; one that is not still lags, and is told.
                    tokio::task::yield_now().await;
                }
            }
            Err(e) => log!("account {}: cannot read the new messages ({e})", self.key),
        }
    }

    /// Idles in the selected folder for at most `timeout` in all, and says what ended it.
    async fn idle(
        &mut self,
        mut session: ImapSession,
        timeout: Duration,
    ) -> Result<(Wake, ImapSession), Ended> {
        let mut idle =
            match tokio::time::timeout(IDLE_COMMAND_TIMEOUT, sync::idle(&mut session)).await {
                Ok(Ok(Some(idle))) => idle,
                // The server answered IDLE without starting it: the session is whole, and goes on
                // without IDLE.
                Ok(Ok(None)) => {
                    log!(
                        "account {}: the server refuses IDLE, so the inbox is checked every {} s \
                         with the other folders",
                        self.key,
                        self.options.check_interval.as_secs()
                    );
                    self.idle_refused = true;
                    return Ok((Wake::Renew, session));
                }
                Ok(Err(error)) => return Err(Ended::Lost(error)),
                Err(_) => return Err(Ended::IdleStalled),
            };
        self.idle_backoff = Backoff::default();
        // A request, a stop or the end of the time interrupts the wait, which loses nothing when
        // it is dropped; the server's keepalives (`* OK Still here`) do not.
        let wake = tokio::select! {
            news = idle.news(&mut session) => match news {
                Ok(()) => Wake::Inbox,
                Err(error) => return Err(Ended::Lost(error)),
            },
            _ = tokio::time::sleep(timeout) => Wake::Renew,
            request = self.requests.recv() => match request {
                Some(Request::Sync(waiter)) => {
                    self.waiting.push(waiter);
                    Wake::Sync
                }
                Some(Request::Actions) => Wake::Actions,
                None => Wake::Stop,
            },
            _ = stopped(&mut self.stop) => Wake::Stop,
        };
        match tokio::time::timeout(IDLE_COMMAND_TIMEOUT, idle.done(&mut session)).await {
            Ok(Ok(())) => Ok((wake, session)),
            Ok(Err(error)) => Err(Ended::Lost(error)),
            Err(_) => Err(Ended::Lost(SyncError::Connection(
                ConnectError::Unreachable("the IMAP server did not end IDLE".to_owned()),
            ))),
        }
    }
}

/// The message a request to send answers or forwards, to mark once it is sent. A request naming
/// both is a reply: a forward is never also a reply.
fn followed<'a>(answers: Option<&'a str>, forwards: Option<&'a str>) -> Option<Followed<'a>> {
    answers
        .map(Followed::Answered)
        .or_else(|| forwards.map(Followed::Forwarded))
}

/// Submits an account's outbox: see [`missive_core::outbox`]. It never talks to the IMAP
/// server; it wakes the IMAP worker when a copy is to be appended to Sent.
struct SmtpWorker {
    id: String,
    /// The account's number in the copy, which the log names it by.
    key: i64,
    account: Account,
    certificate: Option<CertificateDer<'static>>,
    store: Store,
    /// A message was written to the outbox.
    wake: mpsc::UnboundedReceiver<()>,
    /// The user asked for a sync: a password may have been fixed.
    recheck: Arc<Notify>,
    /// The account's IMAP worker; weak, so that it still ends when its account is removed.
    imap: mpsc::WeakUnboundedSender<Request>,
    /// Every account's IMAP worker: a message answered or forwarded may be another account's.
    workers: Arc<Mutex<BTreeMap<String, mpsc::UnboundedSender<Request>>>>,
    stop: watch::Receiver<bool>,
    events: Arc<Events>,
}

impl SmtpWorker {
    async fn run(mut self) {
        match self.store.sending_left(self.key) {
            Ok(left) => {
                for left in left {
                    log!(
                        "account {}: message {} was being sent when the daemon stopped: it is \
                         uncertain",
                        self.key,
                        left.id
                    );
                    self.events.send(Event::SendUncertain(SendUncertain {
                        account: self.id.clone(),
                        outbox: left.id,
                        message_id: left.message_id,
                        message: UNCERTAIN_LEFT.to_owned(),
                    }));
                }
            }
            Err(e) => log!("account {}: cannot read the outbox ({e})", self.key),
        }
        let tls = client_config(self.certificate.clone());
        let mut password: Option<Password> = None;
        let mut backoff = Backoff::default();
        loop {
            // A worker whose account was removed takes no other message: its successor, if the
            // account is added again, may already be sending them.
            loop {
                match self.wake.try_recv() {
                    Ok(()) => {}
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return,
                }
            }
            // Each message waits for its own time: one the server refuses for now holds back no
            // other, and a new message goes at once.
            match self.store.next_due(self.key) {
                Ok(Some(due)) => {
                    let early = due - outbox::now();
                    if early > 0 {
                        if !self.wait(Some(Duration::from_millis(early as u64))).await {
                            return;
                        }
                        continue;
                    }
                }
                Ok(None) => {
                    if !self.wait(None).await {
                        return;
                    }
                    continue;
                }
                Err(e) => {
                    log!("account {}: cannot read the outbox ({e})", self.key);
                    if !self.wait(Some(backoff.next())).await {
                        return;
                    }
                    continue;
                }
            }
            if password.is_none() {
                password = self.password().await;
            }
            let Some(login) = &password else {
                log!(
                    "account {}: no password in the keyring, so the outbox waits",
                    self.key
                );
                if !self.wait(Some(backoff.next())).await {
                    return;
                }
                continue;
            };
            // What is written is whole: the outbox says where the message stopped.
            let submitted = tokio::select! {
                submitted = outbox::submit_next(
                    &mut self.store,
                    self.key,
                    &self.account,
                    login,
                    tls.clone(),
                ) => submitted,
                _ = stopped(&mut self.stop) => return,
            };
            let tried = match submitted {
                Ok(Some(submission)) => {
                    // The server's answer is known only here: a stop does not drop it, or the
                    // message it took would stay `sending`, then `uncertain`. It is a few local
                    // writes, given the time of all its tries, not all the time a broken disk
                    // takes.
                    let record = outbox::record(&mut self.store, self.key, submission);
                    tokio::pin!(record);
                    tokio::select! {
                        tried = &mut record => tried.map(Some),
                        _ = async {
                            stopped(&mut self.stop).await;
                            tokio::time::sleep(outbox::RECORD_GRACE).await;
                        } => {
                            log!(
                                "account {}: the daemon stopped before the outbox could be \
                                 written: the message is left as it was",
                                self.key
                            );
                            return;
                        }
                    }
                }
                Ok(None) => Ok(None),
                Err(e) => Err(e),
            };
            let tried = match tried {
                Ok(Some(tried)) => tried,
                Ok(None) => continue,
                Err(e) => {
                    log!("account {}: cannot write the outbox ({e})", self.key);
                    if !self.wait(Some(backoff.next())).await {
                        return;
                    }
                    continue;
                }
            };
            // A login that failed fails for every message: the worker waits too, so as not to log
            // in once per message, and a message written meanwhile does not cut the wait short:
            // a wrong password would be tried again at each one.
            let mut refused = None;
            let login_failed = match &tried {
                Tried::Login { error, .. } => {
                    if *error == ConnectError::AuthRefused {
                        refused = password.take();
                    }
                    true
                }
                Tried::Again { .. } => false,
                Tried::Sent { .. } | Tried::Failed { .. } | Tried::Uncertain { .. } => {
                    backoff = Backoff::default();
                    false
                }
            };
            self.tell(tried);
            if login_failed && !self.pause(backoff.next(), refused.as_ref()).await {
                return;
            }
        }
    }

    /// The account's password from the keyring, read as the IMAP worker reads it. `&mut`
    /// although only read: the copy is not `Sync`, and the future must be `Send`.
    async fn password(&mut self) -> Option<Password> {
        read_password(&self.id, self.key).await
    }

    /// Waits for `timeout`, or for a message written to (or put back in) the outbox. `false`
    /// when the daemon stops or the account was removed.
    async fn wait(&mut self, timeout: Option<Duration>) -> bool {
        let sleep = async {
            match timeout {
                Some(timeout) => tokio::time::sleep(timeout).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            woken = self.wake.recv() => woken.is_some(),
            // A sync: the password the worker waits for may be there.
            _ = self.recheck.notified() => true,
            _ = sleep => true,
            _ = stopped(&mut self.stop) => false,
        }
    }

    /// Waits the whole of `timeout`: a message written meanwhile is for later, but a password
    /// that is not `refused` any more (the user fixed it, and asked for a sync) ends it. `false`
    /// when the daemon stops or the account was removed.
    async fn pause(&mut self, timeout: Duration, refused: Option<&Password>) -> bool {
        let until = tokio::time::Instant::now() + timeout;
        loop {
            tokio::select! {
                woken = self.wake.recv() => {
                    if woken.is_none() {
                        return false;
                    }
                }
                _ = self.recheck.notified(), if refused.is_some() => {
                    let now = tokio::select! {
                        now = read_password(&self.id, self.key) => now,
                        _ = stopped(&mut self.stop) => return false,
                    };
                    if let (Some(now), Some(refused)) = (now, refused)
                        && now.expose() != refused.expose()
                    {
                        // The messages wait their own time too, from the same refusals.
                        if let Err(e) = self.store.due_now(self.key) {
                            log!("account {}: cannot write the outbox ({e})", self.key);
                        }
                        return true;
                    }
                }
                _ = tokio::time::sleep_until(until) => return true,
                _ = stopped(&mut self.stop) => return false,
            }
        }
    }

    /// Logs what came of a try, and tells the watchers.
    fn tell(&self, tried: Tried) {
        let account = self.id.clone();
        match tried {
            Tried::Sent {
                id,
                message_id,
                sent_copy,
                marked,
            } => {
                log!(
                    "account {}: message {id} sent (copy in Sent: {sent_copy}, original marked \
                     in {} accounts)",
                    self.key,
                    marked.len()
                );
                if sent_copy == "append"
                    && let Some(imap) = self.imap.upgrade()
                {
                    let _ = imap.send(Request::Actions);
                }
                // The original's account applies its flag now, not at its next pass.
                let workers = locked(&self.workers);
                for worker in marked.iter().filter_map(|account| workers.get(account)) {
                    let _ = worker.send(Request::Actions);
                }
                self.events.send(Event::Sent(Sent {
                    account,
                    outbox: id,
                    message_id,
                    sent_copy: sent_copy.to_owned(),
                }));
            }
            Tried::Again { id, reason } => {
                log!("account {}: message {id} not sent yet: {reason}", self.key);
            }
            Tried::Login { id, error } => {
                log!(
                    "account {}: message {id} not sent yet: the SMTP login failed ({})",
                    self.key,
                    error.state()
                );
            }
            Tried::Failed {
                id,
                message_id,
                code,
                step,
            } => {
                log!(
                    "account {}: message {id} refused for good ({code} to {step})",
                    self.key
                );
                self.events.send(Event::SendFailed(SendFailed {
                    account,
                    outbox: id,
                    message_id,
                    code,
                    step: step.to_owned(),
                    message: format!(
                        "the server refused the message for good (it answered {code} to \
                         {step}): it is not sent again"
                    ),
                }));
            }
            Tried::Uncertain {
                id,
                message_id,
                reason,
            } => {
                log!("account {}: message {id} uncertain: {reason}", self.key);
                self.events.send(Event::SendUncertain(SendUncertain {
                    account,
                    outbox: id,
                    message_id,
                    message: format!(
                        "{reason}: the message may have been sent, so it is not sent again"
                    ),
                }));
            }
        }
    }
}

/// The SMTP worker's read of the keyring, apart from the worker so that a stop can be raced
/// against it: a keyring that asks for a passphrase holds the read until someone types it.
async fn read_password(id: &str, key: i64) -> Option<Password> {
    let found = match Keyring::open().await {
        Ok(keyring) => keyring.password(id).await,
        Err(e) => Err(e),
    };
    found.unwrap_or_else(|_| {
        log!("account {key}: the keyring cannot be read");
        None
    })
}

const UNCERTAIN_LEFT: &str = "the daemon stopped while the message was being sent: it may have \
                              been sent, so it is not sent again";

/// The kind of a sync error, for the log.
fn kind(error: &SyncError) -> String {
    match error {
        SyncError::Connection(error) => error.state().to_owned(),
        SyncError::Store(error) => format!("the local copy could not be written: {error}"),
    }
}

/// Connects to the daemon, starting one when none answers (see [`client::connect`]).
pub async fn connect() -> Result<UnixStream, String> {
    let socket = socket_path().map_err(|e| e.to_string())?;
    let program = std::env::current_exe().map_err(|e| e.to_string())?;
    client::connect(&socket, &program).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wait_for_the_workers_leaves_the_record_all_its_grace_and_a_second() {
        assert!(JOIN_TIMEOUT >= outbox::RECORD_GRACE + Duration::from_secs(1));
        assert!(JOIN_TIMEOUT >= STOP_GRACE + Duration::from_secs(1));
    }
}
