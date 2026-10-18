//! `missive daemon`: keeps the local copy of every account up to date, and answers on its socket
//! (see `docs/protocol.md`).
//!
//! One worker per account, each with its own IMAP connection and its own connection to the copy:
//! an account that fails never holds the others back. A worker syncs every folder, then idles in
//! the inbox (`IDLE`, re-issued at least every 25 minutes, under the 29 the RFC allows) until new
//! mail, a request, or the periodic check of the other folders.
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
use missive_core::keyring::{Keyring, KeyringError};
use missive_core::protocol::{
    AccountState, Answer, Command, PROTOCOL, pid_path, read_request, runtime_dir, socket_path,
};
use missive_core::store::Store;
use missive_core::sync::{self, Changes, Folder, SyncError};
use rustls::pki_types::CertificateDer;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinSet;

/// IDLE is re-issued after this long at most: RFC 2177 lets a server drop a client idle for 30
/// minutes, and asks clients to re-issue it every 29.
const IDLE_RENEWAL: Duration = Duration::from_secs(25 * 60);
/// How long a worker in the middle of a sync is given to finish it when the daemon stops.
const STOP_GRACE: Duration = Duration::from_secs(3);
/// The longest wait between two connections to an unreachable server.
const BACKOFF_MAX: Duration = Duration::from_secs(5 * 60);

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

struct Daemon {
    options: Options,
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
            };
            locked(&self.tasks).spawn(worker.run());
        }
        workers.len()
    }

    /// Answers the requests of one connection, in order, until it closes.
    async fn serve(self: Arc<Self>, stream: UnixStream) {
        let (read, mut write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let answer = match read_request(&line) {
                Ok(request) => self.answer(request.command).await,
                Err(answer) => answer,
            };
            let mut line = serde_json::to_string(&answer).expect("an answer serializes");
            line.push('\n');
            if write.write_all(line.as_bytes()).await.is_err() {
                return;
            }
        }
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

    fn state(&self, state: &str) {
        if let Err(e) = self.store.set_state(self.key, state, false) {
            log!("account {}: cannot record its state ({e})", self.key);
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
                let wake = match (capabilities.idle, inbox) {
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
                        let (wake, idled) = self.idle(session, left.min(IDLE_RENEWAL)).await;
                        session = match idled {
                            Ok(session) => session,
                            Err(error) => return (Ended::Lost(error), None),
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
        let changes =
            sync::folder(session, &mut self.store, folder, self.options.batch_size).await?;
        let role = folder.role.unwrap_or("no role");
        match changes {
            Some(changes) => {
                if changes.rebuilt > 0 {
                    log!(
                        "account {} folder {} ({role}): its UIDVALIDITY changed, so it was rebuilt",
                        self.key,
                        folder.row.id
                    );
                }
                Ok(changes)
            }
            None => {
                log!(
                    "account {} folder {} ({role}): the server refused to examine it",
                    self.key,
                    folder.row.id
                );
                Ok(Changes::default())
            }
        }
    }

    /// Idles in the selected folder for at most `timeout`, and says what ended it.
    async fn idle(
        &mut self,
        session: ImapSession,
        timeout: Duration,
    ) -> (Wake, Result<ImapSession, SyncError>) {
        let mut handle = session.idle();
        if let Err(error) = handle.init().await {
            return (Wake::Stop, Err(error.into()));
        }
        let (result, asked) = {
            let (wait, interrupt) = handle.wait_with_timeout(timeout);
            tokio::pin!(wait);
            let mut interrupt = Some(interrupt);
            let mut asked = None;
            // A request or a stop interrupts IDLE; the wait then ends, and the session is whole.
            let result = loop {
                tokio::select! {
                    result = &mut wait => break result,
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
            Err(error) => return (Wake::Stop, Err(error.into())),
        };
        match handle.done().await {
            Ok(session) => (wake, Ok(session)),
            Err(error) => (wake, Err(error.into())),
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
