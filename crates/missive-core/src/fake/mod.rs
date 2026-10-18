//! A fake IMAP and SMTP server on localhost, for tests.
//!
//! Tests never talk to a real mail server: they describe the mailbox they need, start this server
//! on free localhost ports, point a real client at it, change the mailbox while the client is
//! connected, and read back what the client sent.
//!
//! ```no_run
//! # async fn example() {
//! use missive_core::fake::{FakeServer, Folder, Message, SpecialUse};
//!
//! let server = FakeServer::builder()
//!     .user("alice@example.org", "secret")
//!     .folder(Folder::new("INBOX").message(Message::new("Subject: Hi\r\n\r\nHello.\r\n")))
//!     .folder(Folder::new("Sent").special_use(SpecialUse::Sent))
//!     .start()
//!     .await;
//! // Connect a client to `server.imap_tls_addr()`, trusting `server.certificate_pem()`…
//! let uid = server.deliver("INBOX", Message::new("Subject: More\r\n\r\nAgain.\r\n"));
//! assert!(server.imap_log().iter().all(|command| !command.starts_with("STORE")));
//! # }
//! ```
//!
//! What the IMAP side serves: `CAPABILITY`, `NOOP`, `LOGOUT`, `STARTTLS`, `LOGIN`, `ENABLE`,
//! `LIST` (with special-use attributes), `STATUS`, `SELECT` and `EXAMINE`, `FETCH` and
//! `UID FETCH`, `SEARCH` and `UID SEARCH`, `IDLE`, and CONDSTORE (`HIGHESTMODSEQ`, `MODSEQ`,
//! `CHANGEDSINCE`) unless [`Builder::without_condstore`] is used. Anything else is answered
//! `BAD` (and logged), so a client using more than this fails loudly rather than silently.
//!
//! Changes made through [`FakeServer`] reach a connected client the way a real server tells them:
//! during `IDLE` at once, otherwise at its next `NOOP`. A client whose selected folder gets a new
//! UIDVALIDITY is disconnected with `BYE`. [`Builder::fetch_delay`] slows every `FETCH` answer,
//! to catch a client in the middle of a sync, and [`Builder::idle_keepalive`] sends keepalives
//! during `IDLE`, as many servers do.
//!
//! A server can also misbehave on purpose, from the start ([`Builder::refuse`], [`Builder::cut`])
//! or from a point of the test on ([`FakeServer::refuse`], [`FakeServer::cut`], until
//! [`FakeServer::heal`]): it answers `NO` to a chosen command, drops the client in the middle
//! of an answer, ends an answer short but with its `OK` ([`FakeServer::truncate`]), or refuses
//! one message ([`FakeServer::refuse_uid`]).
//!
//! The SMTP side serves `EHLO`/`HELO`, `STARTTLS`, `AUTH PLAIN` and `AUTH LOGIN` (offered only
//! over TLS; [`Builder::smtp_login_only`] drops `PLAIN`), `MAIL`, `RCPT`, `DATA`, `RSET`, `NOOP`
//! and `QUIT`, and records each message it accepted ([`FakeServer::smtp_messages`]).
//!
//! Both sides listen twice: once in clear with STARTTLS (no login before TLS), once with implicit
//! TLS. The certificate is self-signed for `localhost` and `127.0.0.1` and marked as a CA
//! (`basicConstraints CA:TRUE`), like Proton Mail Bridge's: a standard webpki client refuses it,
//! a client pinning [`FakeServer::certificate_pem`] accepts it.
//!
//! [`FakeKeyring`] is the keyring's counterpart: a fake Secret Service on a private session bus.

mod imap;
mod keyring;
mod smtp;

pub use keyring::{FakeItem, FakeKeyring, PromptAnswer};

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose,
};
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::{JoinHandle, JoinSet};
use tokio_rustls::TlsAcceptor;

/// The special-use role of a folder (RFC 6154), sent as an attribute in `LIST` answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpecialUse {
    All,
    Archive,
    Drafts,
    Flagged,
    Junk,
    Sent,
    Trash,
}

impl SpecialUse {
    fn attribute(self) -> &'static str {
        match self {
            SpecialUse::All => "\\All",
            SpecialUse::Archive => "\\Archive",
            SpecialUse::Drafts => "\\Drafts",
            SpecialUse::Flagged => "\\Flagged",
            SpecialUse::Junk => "\\Junk",
            SpecialUse::Sent => "\\Sent",
            SpecialUse::Trash => "\\Trash",
        }
    }
}

/// A message as a test describes it: the raw RFC 822 bytes, its flags, and optionally its UID.
#[derive(Clone, Debug)]
pub struct Message {
    raw: Vec<u8>,
    flags: Vec<String>,
    uid: Option<u32>,
    internal_date: String,
}

impl Message {
    /// A message with no flags, the next free UID of its folder, and a fixed internal date.
    pub fn new(raw: impl Into<Vec<u8>>) -> Self {
        Message {
            raw: raw.into(),
            flags: Vec::new(),
            uid: None,
            internal_date: "01-Jan-2026 00:00:00 +0000".to_owned(),
        }
    }

    /// Adds a flag, as IMAP writes it: `\Seen`, `\Flagged`, `$Junk`…
    pub fn flag(mut self, flag: &str) -> Self {
        self.flags.push(flag.to_owned());
        self
    }

    /// Gives the message this UID instead of the next free one, to leave gaps. It must be at
    /// least the folder's next UID.
    pub fn uid(mut self, uid: u32) -> Self {
        self.uid = Some(uid);
        self
    }

    /// Sets the IMAP internal date, written as IMAP does: `17-Oct-2026 09:30:00 +0200`.
    pub fn internal_date(mut self, date: &str) -> Self {
        self.internal_date = date.to_owned();
        self
    }
}

/// A folder as a test describes it. Names use `/` as the hierarchy delimiter (`Archive/2026`).
#[derive(Clone, Debug)]
pub struct Folder {
    name: String,
    special_use: Option<SpecialUse>,
    uidvalidity: Option<u32>,
    messages: Vec<Message>,
}

impl Folder {
    pub fn new(name: &str) -> Self {
        Folder {
            name: name.to_owned(),
            special_use: None,
            uidvalidity: None,
            messages: Vec::new(),
        }
    }

    pub fn special_use(mut self, role: SpecialUse) -> Self {
        self.special_use = Some(role);
        self
    }

    /// Sets the folder's UIDVALIDITY; by default each folder gets a distinct one.
    pub fn uidvalidity(mut self, uidvalidity: u32) -> Self {
        self.uidvalidity = Some(uidvalidity);
        self
    }

    /// Adds a message, after the ones already added.
    pub fn message(mut self, message: Message) -> Self {
        self.messages.push(message);
        self
    }
}

/// A message the SMTP side accepted: its envelope and its data, dot-unstuffed, without the final
/// `.` line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SmtpMessage {
    pub from: String,
    pub to: Vec<String>,
    pub data: Vec<u8>,
}

/// Describes the server before it starts: see [`FakeServer::builder`].
#[derive(Debug, Default)]
pub struct Builder {
    users: Vec<(String, String)>,
    folders: Vec<Folder>,
    without_condstore: bool,
    imap_login_unavailable: bool,
    smtp_login_only: bool,
    fetch_delay: Duration,
    idle_keepalive: Duration,
    rules: Vec<Rule>,
    refused_uids: Vec<u32>,
}

impl Builder {
    /// Accepts this user name and password, on IMAP and SMTP alike. Any other password is refused
    /// (`NO [AUTHENTICATIONFAILED]`, `535`). Every user sees the same folders.
    pub fn user(mut self, name: &str, password: &str) -> Self {
        self.users.push((name.to_owned(), password.to_owned()));
        self
    }

    /// Adds a folder. `INBOX` exists even when no test describes it, as on every server.
    pub fn folder(mut self, folder: Folder) -> Self {
        self.folders.push(folder);
        self
    }

    /// Serves without CONDSTORE, like a server that lacks it.
    pub fn without_condstore(mut self) -> Self {
        self.without_condstore = true;
        self
    }

    /// Answers every IMAP `LOGIN` `NO [UNAVAILABLE]` (RFC 5530), as a bridge does while it is
    /// starting, whatever the credentials.
    pub fn imap_login_unavailable(mut self) -> Self {
        self.imap_login_unavailable = true;
        self
    }

    /// Offers only `AUTH LOGIN` on SMTP, not `PLAIN`. As some servers do, it then refuses an
    /// unknown user name (`535`) at the user name step, before asking for a password.
    pub fn smtp_login_only(mut self) -> Self {
        self.smtp_login_only = true;
        self
    }

    /// Waits this long before answering each `FETCH` or `UID FETCH`, like a slow server or a
    /// big mailbox: a test can then catch a client in the middle of a sync.
    pub fn fetch_delay(mut self, delay: Duration) -> Self {
        self.fetch_delay = delay;
        self
    }

    /// Sends `* OK Still here` every `interval` to a client in `IDLE`, as Dovecot does every two
    /// minutes.
    pub fn idle_keepalive(mut self, interval: Duration) -> Self {
        self.idle_keepalive = interval;
        self
    }

    /// Starts as [`FakeServer::refuse`] would leave it.
    pub fn refuse(mut self, command: &str) -> Self {
        self.rules.push(Rule::new(command, Action::Refuse));
        self
    }

    /// Starts as [`FakeServer::cut`] would leave it.
    pub fn cut(mut self, command: &str, lines: usize) -> Self {
        self.rules.push(Rule::new(command, Action::Cut(lines)));
        self
    }

    /// Starts as [`FakeServer::truncate`] would leave it.
    pub fn truncate(mut self, command: &str, lines: usize) -> Self {
        self.rules.push(Rule::new(command, Action::Truncate(lines)));
        self
    }

    /// Starts as [`FakeServer::refuse_uid`] would leave it.
    pub fn refuse_uid(mut self, uid: u32) -> Self {
        self.refused_uids.push(uid);
        self
    }

    /// Generates the certificate and starts listening on four free localhost ports.
    pub async fn start(self) -> FakeServer {
        let mut folders = Vec::new();
        if !self.folders.iter().any(|f| f.name == "INBOX") {
            folders.push(Folder::new("INBOX"));
        }
        folders.extend(self.folders);
        let folders = folders
            .into_iter()
            .enumerate()
            .map(|(index, folder)| {
                let uidvalidity = folder.uidvalidity.unwrap_or(100 + index as u32);
                let mut state = FolderState {
                    name: folder.name,
                    special_use: folder.special_use,
                    uidvalidity,
                    uidnext: 1,
                    highest_modseq: 1,
                    messages: Vec::new(),
                };
                for message in folder.messages {
                    state.add(message);
                }
                state
            })
            .collect();

        let (certificate, pem, key) = certificate();
        let config =
            ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .expect("ring supports the default protocol versions")
                .with_no_client_auth()
                .with_single_cert(vec![certificate.clone()], key.into())
                .expect("the generated key matches its certificate");
        let acceptor = TlsAcceptor::from(Arc::new(config));

        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                users: self.users,
                folders,
                imap_log: Vec::new(),
                smtp_messages: Vec::new(),
                rules: self.rules,
                refused_uids: self.refused_uids,
            }),
            changes: watch::channel(()).0,
            condstore: !self.without_condstore,
            imap_login_unavailable: self.imap_login_unavailable,
            smtp_login_only: self.smtp_login_only,
            fetch_delay: self.fetch_delay,
            idle_keepalive: self.idle_keepalive,
        });

        let mut listeners = Vec::new();
        let mut addresses = Vec::new();
        for protocol in [Protocol::Imap, Protocol::Smtp] {
            for implicit_tls in [false, true] {
                let listener = TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("a free localhost port");
                addresses.push(listener.local_addr().unwrap());
                listeners.push(tokio::spawn(accept(
                    listener,
                    shared.clone(),
                    acceptor.clone(),
                    protocol,
                    implicit_tls,
                )));
            }
        }

        FakeServer {
            shared,
            listeners,
            imap_starttls: addresses[0],
            imap_tls: addresses[1],
            smtp_starttls: addresses[2],
            smtp_tls: addresses[3],
            certificate,
            pem,
        }
    }
}

/// A running fake server. It stops, connections included, when dropped.
///
/// The methods that change the mailbox panic when the folder or the UID does not exist: that is a
/// mistake in the test, not a case to handle.
pub struct FakeServer {
    shared: Arc<Shared>,
    listeners: Vec<JoinHandle<()>>,
    imap_starttls: SocketAddr,
    imap_tls: SocketAddr,
    smtp_starttls: SocketAddr,
    smtp_tls: SocketAddr,
    certificate: CertificateDer<'static>,
    pem: String,
}

impl FakeServer {
    pub fn builder() -> Builder {
        Builder::default()
    }

    /// IMAP in clear, upgraded with `STARTTLS` (like port 143).
    pub fn imap_starttls_addr(&self) -> SocketAddr {
        self.imap_starttls
    }

    /// IMAP over implicit TLS (like port 993).
    pub fn imap_tls_addr(&self) -> SocketAddr {
        self.imap_tls
    }

    /// SMTP in clear, upgraded with `STARTTLS` (like port 587).
    pub fn smtp_starttls_addr(&self) -> SocketAddr {
        self.smtp_starttls
    }

    /// SMTP over implicit TLS (like port 465).
    pub fn smtp_tls_addr(&self) -> SocketAddr {
        self.smtp_tls
    }

    /// The server's certificate in PEM, as a user would import it.
    pub fn certificate_pem(&self) -> String {
        self.pem.clone()
    }

    /// The server's certificate in DER.
    pub fn certificate_der(&self) -> CertificateDer<'static> {
        self.certificate.clone()
    }

    /// Delivers a message at the end of a folder, as new mail arrives, and returns its UID.
    pub fn deliver(&self, folder: &str, message: Message) -> u32 {
        self.shared
            .change(|state| state.folder(folder).add(message))
    }

    /// Replaces the flags of a message, as another client would.
    pub fn set_flags(&self, folder: &str, uid: u32, flags: &[&str]) {
        self.shared.change(|state| {
            let folder = state.folder(folder);
            folder.highest_modseq += 1;
            let modseq = folder.highest_modseq;
            let message = folder.message(uid);
            message.flags = flags.iter().map(|f| f.to_string()).collect();
            message.modseq = modseq;
        })
    }

    /// Removes a message for good, as another client's expunge would.
    pub fn expunge(&self, folder: &str, uid: u32) {
        self.shared.change(|state| {
            let folder = state.folder(folder);
            folder.message(uid);
            folder.messages.retain(|m| m.uid != uid);
        })
    }

    /// Gives a folder a new UIDVALIDITY and renumbers its messages from 1, as a server does when
    /// it rebuilds a folder. Returns the new UIDVALIDITY.
    pub fn bump_uidvalidity(&self, folder: &str) -> u32 {
        self.shared.change(|state| {
            let folder = state.folder(folder);
            folder.uidvalidity += 1;
            for (index, message) in folder.messages.iter_mut().enumerate() {
                message.uid = index as u32 + 1;
            }
            folder.uidnext = folder.messages.len() as u32 + 1;
            folder.uidvalidity
        })
    }

    /// The flags of a message now, or `None` when the folder holds no such UID.
    pub fn flags(&self, folder: &str, uid: u32) -> Option<Vec<String>> {
        let mut state = self.shared.state();
        let folder = state.folder(folder);
        let message = folder.messages.iter().find(|m| m.uid == uid)?;
        Some(message.flags.clone())
    }

    /// Every IMAP command received so far, from every connection, in order, without its tag:
    /// `EXAMINE "INBOX"`, `UID FETCH 1:* (FLAGS)`, `IDLE`, `DONE`… The password of `LOGIN` is
    /// replaced by `***`.
    pub fn imap_log(&self) -> Vec<String> {
        self.shared.state().imap_log.clone()
    }

    /// Answers `{tag} NO [UNAVAILABLE] …`, and nothing else, to every IMAP command that starts
    /// with `command` (written without its tag, in any case): `server.refuse("UID SEARCH")`. The
    /// command is logged but not run.
    pub fn refuse(&self, command: &str) {
        self.shared
            .state()
            .rules
            .push(Rule::new(command, Action::Refuse));
    }

    /// Answers every IMAP command that starts with `command` with the first `lines` lines of its
    /// answer only, then `* BYE` and a clean close (TLS `close_notify`), as a server that drops a
    /// client in the middle of an answer: `server.cut("UID FETCH 1:*", 2)`. Lines are counted by
    /// their `\r\n`, so a cut inside a literal leaves it unfinished.
    pub fn cut(&self, command: &str, lines: usize) {
        self.shared
            .state()
            .rules
            .push(Rule::new(command, Action::Cut(lines)));
    }

    /// Answers every IMAP command that starts with `command` with the first `lines` lines of its
    /// answer only, but still ends it with its tagged `OK`: a server that forgot part of what it
    /// should say and said it was done, as `server.truncate("EXAMINE", 3)` leaves out `EXISTS`.
    /// Lines are counted as for [`cut`](FakeServer::cut).
    pub fn truncate(&self, command: &str, lines: usize) {
        self.shared
            .state()
            .rules
            .push(Rule::new(command, Action::Truncate(lines)));
    }

    /// Refuses one message: every `UID FETCH` whose set holds `uid` is answered with the other
    /// messages of the set, the one refused left out, and then `NO`, as Dovecot does with
    /// `imap_fetch_failure = no-after` for a message it cannot read. Whatever the items asked,
    /// sizes included; a `FETCH` by sequence number is not affected.
    pub fn refuse_uid(&self, uid: u32) {
        self.shared.state().refused_uids.push(uid);
    }

    /// Forgets every [`refuse`](FakeServer::refuse), [`cut`](FakeServer::cut),
    /// [`truncate`](FakeServer::truncate) and [`refuse_uid`](FakeServer::refuse_uid): the server
    /// behaves again.
    pub fn heal(&self) {
        let mut state = self.shared.state();
        state.rules.clear();
        state.refused_uids.clear();
    }

    /// Every message the SMTP side accepted, in order.
    pub fn smtp_messages(&self) -> Vec<SmtpMessage> {
        self.shared.state().smtp_messages.clone()
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        for listener in &self.listeners {
            listener.abort();
        }
    }
}

/// What every connection shares with the [`FakeServer`].
struct Shared {
    state: Mutex<State>,
    /// Signalled after each change made by a test or a client, so that idling sessions look.
    changes: watch::Sender<()>,
    condstore: bool,
    imap_login_unavailable: bool,
    smtp_login_only: bool,
    fetch_delay: Duration,
    idle_keepalive: Duration,
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        // A test that panicked while holding the lock already failed; the state is still usable.
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn change<T>(&self, change: impl FnOnce(&mut State) -> T) -> T {
        let result = change(&mut self.state());
        self.changes.send_modify(|_| {});
        result
    }

    /// What a test asked the server to do with this command line, tag included.
    fn rule(&self, line: &str) -> Option<Action> {
        let command = line.split_once(' ').map_or("", |(_, command)| command);
        let command = command.to_ascii_uppercase();
        self.state()
            .rules
            .iter()
            .find(|rule| command.starts_with(&rule.command))
            .map(|rule| rule.action)
    }

    fn knows(&self, user: &str) -> bool {
        self.state().users.iter().any(|(u, _)| u == user)
    }

    fn authenticate(&self, user: &str, password: &str) -> bool {
        self.state()
            .users
            .iter()
            .any(|(u, p)| u == user && p == password)
    }
}

struct State {
    users: Vec<(String, String)>,
    folders: Vec<FolderState>,
    imap_log: Vec<String>,
    smtp_messages: Vec<SmtpMessage>,
    rules: Vec<Rule>,
    refused_uids: Vec<u32>,
}

/// A misbehaviour a test asked for: what to do with the commands that start with `command`.
#[derive(Debug)]
struct Rule {
    /// In upper case.
    command: String,
    action: Action,
}

impl Rule {
    fn new(command: &str, action: Action) -> Self {
        Rule {
            command: command.to_ascii_uppercase(),
            action,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Refuse,
    Cut(usize),
    Truncate(usize),
}

impl State {
    fn find(&mut self, name: &str) -> Option<&mut FolderState> {
        self.folders.iter_mut().find(|f| f.name == name)
    }

    fn folder(&mut self, name: &str) -> &mut FolderState {
        self.find(name)
            .unwrap_or_else(|| panic!("the fake server has no folder {name:?}"))
    }
}

struct FolderState {
    name: String,
    special_use: Option<SpecialUse>,
    uidvalidity: u32,
    uidnext: u32,
    highest_modseq: u64,
    /// In UID order.
    messages: Vec<StoredMessage>,
}

impl FolderState {
    fn add(&mut self, message: Message) -> u32 {
        let uid = message.uid.unwrap_or(self.uidnext);
        assert!(
            uid >= self.uidnext,
            "UID {uid} is below the next UID {} of {:?}",
            self.uidnext,
            self.name
        );
        self.uidnext = uid + 1;
        self.highest_modseq += 1;
        self.messages.push(StoredMessage {
            uid,
            raw: message.raw,
            flags: message.flags,
            internal_date: message.internal_date,
            modseq: self.highest_modseq,
        });
        uid
    }

    fn message(&mut self, uid: u32) -> &mut StoredMessage {
        let name = &self.name;
        self.messages
            .iter_mut()
            .find(|m| m.uid == uid)
            .unwrap_or_else(|| panic!("the fake server has no UID {uid} in {name:?}"))
    }
}

struct StoredMessage {
    uid: u32,
    raw: Vec<u8>,
    flags: Vec<String>,
    internal_date: String,
    modseq: u64,
}

/// A connection, in clear or in TLS.
trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

#[derive(Clone, Copy)]
enum Protocol {
    Imap,
    Smtp,
}

async fn accept(
    listener: TcpListener,
    shared: Arc<Shared>,
    acceptor: TlsAcceptor,
    protocol: Protocol,
    implicit_tls: bool,
) {
    // Owned here, so that aborting this task drops every connection with it.
    let mut connections = JoinSet::new();
    loop {
        let Ok((tcp, _)) = listener.accept().await else {
            continue;
        };
        while connections.try_join_next().is_some() {}
        let shared = shared.clone();
        let acceptor = acceptor.clone();
        let connection: Pin<Box<dyn Future<Output = ()> + Send>> = Box::pin(async move {
            let Some(stream) = open(tcp, &acceptor, implicit_tls).await else {
                return;
            };
            match protocol {
                Protocol::Imap => imap::serve(shared, acceptor, stream, implicit_tls).await,
                Protocol::Smtp => smtp::serve(shared, acceptor, stream, implicit_tls).await,
            }
        });
        connections.spawn(connection);
    }
}

async fn open(tcp: TcpStream, acceptor: &TlsAcceptor, implicit_tls: bool) -> Option<Box<dyn Io>> {
    if !implicit_tls {
        return Some(Box::new(tcp));
    }
    // A client that refuses the certificate ends here, which is what such a test expects.
    let tls = acceptor.accept(tcp).await.ok()?;
    Some(Box::new(tls))
}

/// Upgrades a connection after `STARTTLS`.
async fn start_tls(stream: Box<dyn Io>, acceptor: &TlsAcceptor) -> Option<Box<dyn Io>> {
    let tls = acceptor.accept(stream).await.ok()?;
    Some(Box::new(tls))
}

/// A self-signed certificate marked as a CA, for `localhost` and `127.0.0.1`, like the one
/// Proton Mail Bridge generates.
fn certificate() -> (CertificateDer<'static>, String, PrivatePkcs8KeyDer<'static>) {
    let key = KeyPair::generate().expect("a key pair");
    let mut params = CertificateParams::new(vec!["localhost".to_owned(), "127.0.0.1".to_owned()])
        .expect("valid names");
    params
        .distinguished_name
        .push(DnType::CommonName, "127.0.0.1");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
        KeyUsagePurpose::KeyCertSign,
    ];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let certificate = params.self_signed(&key).expect("a self-signed certificate");
    (
        certificate.der().clone(),
        certificate.pem(),
        PrivatePkcs8KeyDer::from(key.serialize_der()),
    )
}
