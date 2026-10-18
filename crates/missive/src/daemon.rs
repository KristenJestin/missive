//! `missive daemon`: keeps the local copy of every account up to date, and answers on its socket
//! (see `docs/protocol.md`).
//!
//! One worker per account, each with its own IMAP connection and its own connection to the copy:
//! an account that fails never holds the others back. A worker syncs every folder, then idles in
//! the inbox (`IDLE`, re-issued at least every 25 minutes, under the 29 the RFC allows) until new
//! mail, a request, or the periodic check of the other folders.
//!
//! A connection that sends `subscribe` is a watcher: the daemon then writes it the events of the
//! workers (`docs/events.md`). They travel on a bounded broadcast channel, so that a watcher that
//! reads too slowly misses events, is told how many, and holds nothing else back.
//!
//! The log (standard error) says what happened in counts, durations, folder roles and error
//! kinds. It names an account by its number in the copy and a folder by its id there, never by
//! a name: an account's identifier is often its address, and a folder's name is the user's own.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use async_imap::extensions::idle::IdleResponse;
use missive_core::account::{Account, AccountStore, Password};
use missive_core::connect::{ConnectError, ImapSession, client_config, open_imap};
use missive_core::copy::Copy;
use missive_core::events::{Event, Reader, Unread};
use missive_core::keyring::{Keyring, KeyringError};
use missive_core::protocol::{
    AccountState, Answer, Command, PROTOCOL, pid_path, read_request, runtime_dir, socket_path,
};
use missive_core::store::Store;
use missive_core::sync::{self, Changes, Folder, SyncError};
use rustls::pki_types::CertificateDer;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::JoinSet;

/// IDLE is re-issued after this long at most: RFC 2177 lets a server drop a client idle for 30
/// minutes, and asks clients to re-issue it every 29.
const IDLE_RENEWAL: Duration = Duration::from_secs(25 * 60);
/// How long a server is given to start IDLE, and to end it. A server that advertises IDLE but
/// refuses it (`NO`) is not heard from again by `async-imap`, which goes on reading.
const IDLE_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a worker in the middle of a sync is given to finish it when the daemon stops.
const STOP_GRACE: Duration = Duration::from_secs(3);
/// The longest wait between two connections to an unreachable server.
const BACKOFF_MAX: Duration = Duration::from_secs(5 * 60);
/// How many events a watcher may be behind by before it misses some: a few minutes of a busy
/// mailbox, and little memory (an event is a few hundred bytes).
const EVENT_BUFFER: usize = 256;
/// How many lines wait for a connection's socket: a watcher whose socket is full holds these and
/// nothing else of the daemon's.
const LINE_QUEUE: usize = 16;

#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// How often the folders other than the inbox are checked.
    pub check_interval: Duration,
    /// How many new messages are fetched, and written to the copy, at a time.
    pub batch_size: usize,
}

macro_rules! log {
    ($($arg:tt)*) => {
        eprintln!("missive daemon: {}", format_args!($($arg)*))
    };
}

/// Where a request waits for the end of the sync it asked for: it receives the account's state,
/// or why the local copy could not be written.
type Waiter = oneshot::Sender<Result<String, String>>;

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
    Store::open(&store).map_err(|e| format!("cannot open the local copy: {e}"))?;

    let (stop, stopping) = watch::channel(false);
    let daemon = Arc::new(Daemon {
        options,
        events: Arc::new(Events::new(&store)),
        store,
        stopping,
        workers: Mutex::new(BTreeMap::new()),
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
    if tokio::time::timeout(STOP_GRACE + Duration::from_secs(3), all)
        .await
        .is_err()
    {
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

/// Takes the daemon's lock, or says which daemon holds it, and writes this daemon's pid.
fn lock(path: &Path) -> Result<File, String> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            let mut pid = String::new();
            let _ = file.read_to_string(&mut pid);
            return Err(format!("a daemon is already running (pid {})", pid.trim()));
        }
        Err(TryLockError::Error(e)) => {
            return Err(format!("cannot lock {}: {e}", path.display()));
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
}

impl Events {
    fn new(store: &Path) -> Events {
        let unread = open_copy(store)
            .and_then(|copy| Unread::of(&copy).ok())
            .unwrap_or_default();
        Events {
            sender: broadcast::channel(EVENT_BUFFER).0,
            unread: Mutex::new(unread),
        }
    }

    /// Without a watcher there is nobody to tell, which is not an error.
    fn send(&self, event: Event) {
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
    /// Where to send each account's sync requests, by identifier.
    workers: Mutex<BTreeMap<String, mpsc::UnboundedSender<Waiter>>>,
    tasks: Mutex<JoinSet<()>>,
}

impl Daemon {
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
        let mut workers = locked(&self.workers);
        // An account whose settings cannot be read now keeps its worker: it was not removed.
        workers.retain(|id, _| {
            listing.accounts.iter().any(|(a, _)| a == id)
                || listing.skipped.iter().any(|(s, _)| s == id)
        });
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
            let (requests, receiver) = mpsc::unbounded_channel();
            workers.insert(id.clone(), requests);
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
            forwarder.abort();
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
            Ok(states) => present.extend(
                states
                    .into_iter()
                    .map(|(account, state)| Event::State { account, state }),
            ),
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
                let targets: Vec<(String, mpsc::UnboundedSender<Waiter>)> = locked(&self.workers)
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
                    let (waiter, answer) = oneshot::channel();
                    let _ = sender.send(waiter);
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
        }
    }
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
    /// The server did not start IDLE although it advertises it: the connection is dropped, and
    /// the next one does without IDLE.
    IdleRefused,
}

/// What ended a wait between two syncs.
enum Wake {
    /// New data in the inbox.
    Inbox,
    /// A sync was asked for, or the periodic check is due.
    Sync,
    /// IDLE ran its time; the periodic check may not be due yet.
    Renew,
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
    requests: mpsc::UnboundedReceiver<Waiter>,
    stop: watch::Receiver<bool>,
    /// The requests the next sync answers.
    waiting: Vec<Waiter>,
    /// Whether the server refused IDLE: the inbox is then checked with the other folders.
    idle_refused: bool,
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
                match self.password().await {
                    Some(found) => password = Some(found),
                    None => {
                        self.settle("no-login");
                        if !self.wait(None).await {
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
                    Ended::IdleRefused => {
                        log!(
                            "account {}: the server refuses IDLE, so the inbox is checked every \
                             {} s with the other folders",
                            self.key,
                            self.options.check_interval.as_secs()
                        );
                        self.idle_refused = true;
                        // Not unreachable: connect again at once.
                        Some(Duration::ZERO)
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
            if !self.wait(retry).await {
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

    /// Moves the requests sent so far to those the next sync answers.
    fn take_requests(&mut self) {
        while let Ok(waiter) = self.requests.try_recv() {
            self.waiting.push(waiter);
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

    /// Waits for a request, for `timeout` when there is one, or for the daemon to stop. `false`
    /// when the worker must end.
    async fn wait(&mut self, timeout: Option<Duration>) -> bool {
        let sleep = async {
            match timeout {
                Some(timeout) => tokio::time::sleep(timeout).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            request = self.requests.recv() => match request {
                Some(waiter) => {
                    self.waiting.push(waiter);
                    true
                }
                None => false,
            },
            _ = sleep => true,
            _ = stopped(&mut self.stop) => false,
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
            // Every folder.
            self.take_requests();
            self.events.send(Event::SyncStarted {
                account: self.id.clone(),
            });
            self.state("syncing");
            let started = Instant::now();
            let mut folders = check!(sync::folders(&mut session, &mut self.store, self.key).await);
            let mut changes = Changes::default();
            for folder in &mut folders {
                if *self.stop.borrow() {
                    return (Ended::Stopped, Some(session));
                }
                changes += check!(self.folder(&mut session, folder).await);
            }
            if let Err(e) = self.store.set_state(self.key, "ready", true) {
                log!("account {}: cannot record its state ({e})", self.key);
            }
            self.completed = true;
            self.tell("ready");
            self.events.send(Event::SyncEnded {
                account: self.id.clone(),
                folders: folders.len(),
                new: changes.new,
                removed: changes.removed,
                flags: changes.flags,
            });
            log!(
                "account {}: ready, {} folders synced in {:.2} s ({} new, {} removed, {} flag \
                 changes, {} rebuilt)",
                self.key,
                folders.len(),
                started.elapsed().as_secs_f64(),
                changes.new,
                changes.removed,
                changes.flags,
                changes.rebuilt
            );
            self.answer("ready");
            *backoff = Backoff::default();

            // Then the inbox only, until the next check of every folder.
            let inbox = folders.iter().position(|f| f.role == Some("inbox"));
            let next_check = Instant::now() + self.options.check_interval;
            loop {
                let left = next_check.saturating_duration_since(Instant::now());
                let wake = match (capabilities.idle && !self.idle_refused, inbox) {
                    (true, Some(inbox)) => {
                        let changes = check!(self.folder(&mut session, &mut folders[inbox]).await);
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
                    _ => {
                        if self.wait(Some(left)).await {
                            Wake::Sync
                        } else {
                            Wake::Stop
                        }
                    }
                };
                match wake {
                    Wake::Stop => return (Ended::Stopped, Some(session)),
                    Wake::Sync => break,
                    Wake::Renew if Instant::now() >= next_check => break,
                    Wake::Renew | Wake::Inbox => {}
                }
            }
        }
    }

    /// Syncs one folder and logs what is worth knowing.
    async fn folder(
        &mut self,
        session: &mut ImapSession,
        folder: &mut Folder,
    ) -> Result<Changes, SyncError> {
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
            Some(changes) => {
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
                Ok(changes)
            }
            None => {
                log!(
                    "account {} folder {} ({role}): left as it is: the server refused to examine or \
                     list it, or answered something no sync can use",
                    self.key,
                    folder.row.id
                );
                Ok(Changes::default())
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
        match self
            .reader
            .arrived(&self.id, folder.role, folder.row.id, last_uid)
        {
            Ok(messages) => {
                for message in messages {
                    self.events.send(Event::NewMessage(message));
                    // The daemon runs on one thread: without this, a burst of mail would
                    // fill the channel before any watcher, however quick, could take its
                    // events out of it.
                    tokio::task::yield_now().await;
                }
            }
            Err(e) => log!("account {}: cannot read the new messages ({e})", self.key),
        }
    }

    /// Idles in the selected folder for at most `timeout` in all, and says what ended it.
    async fn idle(
        &mut self,
        session: ImapSession,
        timeout: Duration,
    ) -> Result<(Wake, ImapSession), Ended> {
        let mut handle = session.idle();
        match tokio::time::timeout(IDLE_COMMAND_TIMEOUT, handle.init()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(Ended::Lost(error.into())),
            Err(_) => return Err(Ended::IdleRefused),
        }
        let (result, asked) = {
            // The wait's own timeout only counts silence, and a server's keepalives
            // (`* OK Still here`) break it: the sleep bounds the whole of it.
            let (wait, interrupt) = handle.wait_with_timeout(timeout);
            tokio::pin!(wait);
            let renewal = tokio::time::sleep(timeout);
            tokio::pin!(renewal);
            let mut interrupt = Some(interrupt);
            let mut asked = None;
            // A request, a stop or the end of the time interrupts IDLE; the wait then ends, and
            // the session is whole.
            let result = loop {
                tokio::select! {
                    result = &mut wait => break result,
                    _ = &mut renewal, if interrupt.is_some() => {
                        asked = Some(Wake::Renew);
                        interrupt = None;
                    }
                    request = self.requests.recv(), if interrupt.is_some() => {
                        asked = Some(match request {
                            Some(waiter) => {
                                self.waiting.push(waiter);
                                Wake::Sync
                            }
                            None => Wake::Stop,
                        });
                        interrupt = None;
                    }
                    _ = stopped(&mut self.stop), if interrupt.is_some() => {
                        asked = Some(Wake::Stop);
                        interrupt = None;
                    }
                }
            };
            (result, asked)
        };
        let wake = match result {
            Ok(IdleResponse::NewData(_)) => asked.unwrap_or(Wake::Inbox),
            Ok(IdleResponse::Timeout) => asked.unwrap_or(Wake::Renew),
            Ok(IdleResponse::ManualInterrupt) => asked.unwrap_or(Wake::Sync),
            Err(error) => return Err(Ended::Lost(error.into())),
        };
        match tokio::time::timeout(IDLE_COMMAND_TIMEOUT, handle.done()).await {
            Ok(Ok(session)) => Ok((wake, session)),
            Ok(Err(error)) => Err(Ended::Lost(error.into())),
            Err(_) => Err(Ended::Lost(SyncError::Connection(
                ConnectError::Unreachable("the IMAP server did not end IDLE".to_owned()),
            ))),
        }
    }
}

/// The kind of a sync error, for the log.
fn kind(error: &SyncError) -> String {
    match error {
        SyncError::Connection(error) => error.state().to_owned(),
        SyncError::Store(error) => format!("the local copy could not be written: {error}"),
    }
}

/// `$XDG_STATE_HOME/missive/daemon.log`, or `~/.local/state/missive/daemon.log`: where a daemon
/// started on demand writes its log.
pub fn log_path() -> Result<PathBuf, String> {
    let state = match std::env::var_os("XDG_STATE_HOME").map(PathBuf::from) {
        Some(path) if path.is_absolute() => path,
        _ => match std::env::var_os("HOME") {
            Some(home) if !home.is_empty() => PathBuf::from(home).join(".local/state"),
            _ => return Err("neither XDG_STATE_HOME nor HOME is set".to_owned()),
        },
    };
    Ok(state.join("missive").join("daemon.log"))
}

/// Connects to the daemon, starting one when none answers.
pub async fn connect() -> Result<UnixStream, String> {
    let socket = socket_path().map_err(|e| e.to_string())?;
    match UnixStream::connect(&socket).await {
        Ok(stream) => return Ok(stream),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) => {}
        Err(e) => {
            return Err(format!(
                "cannot reach the daemon at {}: {e}",
                socket.display()
            ));
        }
    }
    let log = log_path()?;
    let mut child = start(&log)?;
    let started = Instant::now();
    loop {
        if let Ok(stream) = UnixStream::connect(&socket).await {
            return Ok(stream);
        }
        // Another `sync` may have started one first: this one then stops, and that one answers.
        let exited = child.try_wait().ok().flatten().is_some();
        if (exited && started.elapsed() > Duration::from_secs(2))
            || started.elapsed() > Duration::from_secs(10)
        {
            return Err(format!(
                "the daemon did not start (its log is {})",
                log.display()
            ));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Starts `missive daemon` in its own process group, so that the terminal's Ctrl-C does not reach
/// it, logging to `log`.
fn start(log: &Path) -> Result<std::process::Child, String> {
    use std::os::unix::process::CommandExt;
    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .map_err(|e| format!("cannot open {}: {e}", log.display()))?;
    let program = std::env::current_exe().map_err(|e| e.to_string())?;
    std::process::Command::new(program)
        .arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(file)
        .process_group(0)
        .spawn()
        .map_err(|e| format!("cannot start the daemon: {e}"))
}
