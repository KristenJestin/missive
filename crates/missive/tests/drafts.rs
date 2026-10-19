//! Drafts: `missive compose` without `--send`, `compose --draft`, and the daemon writing them to
//! the account's Drafts folder, against the fake IMAP and SMTP server and a fake Secret Service on
//! a private bus. The binary runs with a cleared
//! environment: its configuration, data, state and runtime folders are temporary, and the only
//! session bus it can see is the test's own. Every daemon a test starts is killed when the test
//! ends, even when it panics.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use mail_parser::{MessageParser, MimeHeaders};
use missive_core::account::{Account, AccountStore, Password, Security, Server};
use missive_core::fake::{FakeKeyring, FakeServer, Folder, Message, SpecialUse};
use missive_core::keyring::Keyring;
use rusqlite::{Connection, OpenFlags};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::unix::OwnedReadHalf;
use tokio::net::{TcpListener, TcpStream, UnixStream};
use tokio::process::{Child, Command};

const USER: &str = "alice@example.org";
const PASSWORD: &str = "Recognisable-Pa55word-4c1d";

const SUBJECT: &str = "Réunion-Subject-Privé-5e1f ☕";
const RECIPIENT: &str = "zoe.recipient@example.com";
const COPIED: &str = "carl.copied@example.net";
const HIDDEN: &str = "hidden.bcc@example.net";
const BODY: &str = "Body-Text-That-Stays-Private";
const NAME: &str = "Zoé Ça-Recipient";

/// What no log and no output may ever show: the message's subject, addresses, words and names,
/// the login and the password, and the password as the SMTP login sends it: base64 alone (the
/// `AUTH LOGIN` lines) and inside the `AUTH PLAIN` token.
fn private() -> Vec<String> {
    let mut private: Vec<String> = [
        "Réunion-Subject",
        RECIPIENT,
        COPIED,
        HIDDEN,
        BODY,
        NAME,
        USER,
        PASSWORD,
        "Alice Example",
    ]
    .map(str::to_owned)
    .to_vec();
    private.push(STANDARD.encode(USER));
    private.push(STANDARD.encode(PASSWORD));
    private.push(STANDARD.encode(format!("\0{USER}\0{PASSWORD}")));
    private
}

/// INBOX with one message, a Sent folder and a Drafts folder.
fn mailbox() -> missive_core::fake::Builder {
    with_drafts(Folder::new("Drafts").special_use(SpecialUse::Drafts))
}

/// [`mailbox`], with this Drafts folder.
fn with_drafts(drafts: Folder) -> missive_core::fake::Builder {
    FakeServer::builder()
        .user(USER, PASSWORD)
        .folder(Folder::new("INBOX").message(Message::new(
            "From: someone@example.com\r\nSubject: Hi\r\n\r\nHello.\r\n",
        )))
        .folder(Folder::new("Sent").special_use(SpecialUse::Sent))
        .folder(drafts)
}

/// How the test account reaches its servers.
#[derive(Clone)]
struct Setup {
    imap: u16,
    smtp: (u16, Security),
    /// The certificate the account pins; the server's own by default.
    certificate: Option<rustls::pki_types::CertificateDer<'static>>,
    server_saves_sent: bool,
}

impl Setup {
    fn of(server: &FakeServer) -> Setup {
        Setup {
            imap: server.imap_tls_addr().port(),
            smtp: (server.smtp_tls_addr().port(), Security::Tls),
            certificate: Some(server.certificate_der()),
            server_saves_sent: false,
        }
    }
}

struct Env {
    home: TempDir,
    keyring: FakeKeyring,
    /// The logs of every daemon started, one file each.
    logs: AtomicUsize,
}

impl Env {
    async fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        for folder in ["config", "data", "state", "runtime", "files"] {
            std::fs::create_dir(home.path().join(folder)).unwrap();
        }
        Env {
            home,
            keyring: FakeKeyring::start().await,
            logs: AtomicUsize::new(0),
        }
    }

    fn path(&self, folder: &str) -> PathBuf {
        self.home.path().join(folder)
    }

    /// Adds the account `alice`, with its password in the keyring.
    async fn add_account(&self, setup: &Setup) {
        let account = Account {
            name: "Alice Example".to_owned(),
            address: USER.to_owned(),
            user: USER.to_owned(),
            imap: Server {
                host: "127.0.0.1".to_owned(),
                port: setup.imap,
                security: Security::Tls,
            },
            smtp: Server {
                host: "127.0.0.1".to_owned(),
                port: setup.smtp.0,
                security: setup.smtp.1,
            },
            server_saves_sent: setup.server_saves_sent,
        };
        AccountStore::new(self.path("config").join("missive"))
            .save("alice", &account, setup.certificate.as_ref())
            .unwrap();
        let connection = zbus::connection::Builder::address(self.keyring.address())
            .unwrap()
            .build()
            .await
            .unwrap();
        Keyring::open_on(connection)
            .await
            .unwrap()
            .store("alice", &Password::new(PASSWORD.to_owned()))
            .await
            .unwrap();
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_missive"));
        command
            .args(args)
            .env_clear()
            .env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.path("config"))
            .env("XDG_DATA_HOME", self.path("data"))
            .env("XDG_STATE_HOME", self.path("state"))
            .env("XDG_RUNTIME_DIR", self.path("runtime"))
            .env("DBUS_SESSION_BUS_ADDRESS", self.keyring.address())
            .stdin(Stdio::null());
        command
    }

    /// Starts `missive daemon` in the foreground and waits until it answers on its socket.
    async fn daemon(&self) -> Daemon {
        let n = self.logs.fetch_add(1, Ordering::Relaxed);
        let log = std::fs::File::create(self.home.path().join(format!("daemon-{n}.log"))).unwrap();
        let child = self
            .command(&["daemon", "--check-interval", "3600"])
            .stdout(Stdio::null())
            .stderr(log)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let started = Instant::now();
        while UnixStream::connect(self.socket()).await.is_err() {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "the daemon did not answer; its log:\n{}",
                self.logs()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Daemon { child }
    }

    async fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().await.unwrap()
    }

    /// Writes a file to attach, and returns its path.
    fn file(&self, name: &str, content: &[u8]) -> String {
        let path = self.path("files").join(name);
        std::fs::write(&path, content).unwrap();
        path.to_str().unwrap().to_owned()
    }

    /// `missive compose --json` with `args`, without `--send`: saves a draft, and returns its
    /// identifier. It must answer within 5 seconds, connected or not.
    async fn save(&self, args: &[&str]) -> String {
        let mut arguments = vec!["compose", "--json"];
        arguments.extend_from_slice(args);
        let started = Instant::now();
        let output = self.run(&arguments).await;
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "compose waited for the server"
        );
        succeeded(&output);
        let saved: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(saved.as_object().unwrap().len(), 1, "{saved}");
        saved["id"].as_str().unwrap().to_owned()
    }

    /// `missive list --folder drafts --json`.
    async fn drafts(&self) -> Vec<serde_json::Value> {
        let output = self.run(&["list", "--folder", "drafts", "--json"]).await;
        succeeded(&output);
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn socket(&self) -> PathBuf {
        self.path("runtime").join("missive/daemon.sock")
    }

    fn pid_file(&self) -> PathBuf {
        self.path("runtime").join("missive/daemon.pid")
    }

    /// Every daemon's log.
    fn logs(&self) -> String {
        let mut logs = String::new();
        let mut files: Vec<PathBuf> = std::fs::read_dir(self.home.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|e| e == "log"))
            .collect();
        files.push(self.path("state").join("missive/daemon.log"));
        files.sort();
        for file in files {
            logs.push_str(&std::fs::read_to_string(file).unwrap_or_default());
        }
        logs
    }

    fn copy_path(&self) -> PathBuf {
        self.path("data").join("missive/copy.sqlite")
    }

    /// `missive status --json`, read.
    async fn status(&self) -> serde_json::Value {
        let output = self.run(&["status", "--json"]).await;
        succeeded(&output);
        serde_json::from_slice(&output.stdout).unwrap()
    }

    /// Subscribes to the daemon's events.
    async fn watch(&self) -> Watcher {
        let stream = UnixStream::connect(self.socket()).await.unwrap();
        let (read, mut write) = stream.into_split();
        write
            .write_all(b"{\"protocol\":1,\"request\":\"subscribe\"}\n")
            .await
            .unwrap();
        // Kept open: closing it is not leaving.
        std::mem::forget(write);
        Watcher {
            lines: BufReader::new(read).lines(),
        }
    }

    /// Waits until the account's first sync ended.
    async fn ready(&self) {
        eventually(20, "the account ready", || {
            Connection::open_with_flags(self.copy_path(), OpenFlags::SQLITE_OPEN_READ_ONLY)
                .ok()
                .and_then(|copy| {
                    copy.query_row("SELECT state FROM account WHERE id = 'alice'", [], |r| {
                        r.get::<_, String>(0)
                    })
                    .ok()
                })
                .as_deref()
                == Some("ready")
        })
        .await;
    }
}

impl Drop for Env {
    /// Kills a daemon `compose` started, which no [`Daemon`] owns.
    fn drop(&mut self) {
        if let Some(pid) = running_daemon(&self.pid_file()) {
            let _ = std::process::Command::new("kill")
                .args(["-KILL", &pid.to_string()])
                .status();
        }
    }
}

/// The pid in a daemon's pid file, if that process is still a `missive`.
fn running_daemon(pid_file: &Path) -> Option<u32> {
    let pid: u32 = std::fs::read_to_string(pid_file)
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let command = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let binary = env!("CARGO_BIN_EXE_missive").as_bytes();
    command.starts_with(binary).then_some(pid)
}

/// A daemon started by the test, killed when dropped.
struct Daemon {
    child: Child,
}

impl Daemon {
    async fn kill(&mut self) -> ExitStatus {
        self.child.kill().await.unwrap();
        self.child.wait().await.unwrap()
    }
}

/// The daemon's events, as a watcher reads them.
struct Watcher {
    lines: Lines<BufReader<OwnedReadHalf>>,
}

impl Watcher {
    /// The next event named `name`, within 30 seconds.
    async fn next(&mut self, name: &str) -> serde_json::Value {
        loop {
            let line = tokio::time::timeout(Duration::from_secs(30), self.lines.next_line())
                .await
                .unwrap_or_else(|_| panic!("no {name} event in time"))
                .unwrap()
                .expect("the daemon still runs");
            let event: serde_json::Value = serde_json::from_str(&line).unwrap();
            if event["event"] == name {
                return event;
            }
        }
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

fn succeeded(output: &Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(output),
        stderr(output)
    );
}

fn failed(output: &Output) {
    assert!(
        !output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(output),
        stderr(output)
    );
}

/// Waits until `condition` holds, for at most `seconds`.
async fn eventually(seconds: u64, what: &str, mut condition: impl FnMut() -> bool) {
    let started = Instant::now();
    while !condition() {
        assert!(
            started.elapsed() < Duration::from_secs(seconds),
            "{what} did not happen within {seconds} seconds"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A TCP relay to a server that can be switched off: while it is off, every connection is closed
/// at once, as an unreachable server's would be.
struct Relay {
    port: u16,
    up: Arc<AtomicBool>,
}

impl Relay {
    async fn start(to: SocketAddr) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let up = Arc::new(AtomicBool::new(false));
        let relay_up = up.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut client, _)) = listener.accept().await else {
                    continue;
                };
                if !relay_up.load(Ordering::SeqCst) {
                    continue;
                }
                tokio::spawn(async move {
                    let mut server = TcpStream::connect(to).await.unwrap();
                    let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
                });
            }
        });
        Relay { port, up }
    }
}

/// The account on a server as `setup` says, a daemon, and its first sync.
async fn started(setup: &Setup) -> (Env, Daemon) {
    let env = Env::new().await;
    env.add_account(setup).await;
    let daemon = env.daemon().await;
    env.ready().await;
    (env, daemon)
}

/// Waits until the fake SMTP server took `count` messages, then a little longer, and checks it
/// took no more.
async fn took(server: &FakeServer, count: usize) {
    eventually(30, "the messages sent", || {
        server.smtp_messages().len() >= count
    })
    .await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(server.smtp_messages().len(), count);
}

/// The drafts on the server now, parsed: `(uid, subject, Message-ID)`.
fn server_drafts(server: &FakeServer) -> Vec<(u32, Option<String>, Option<String>)> {
    server
        .uids("Drafts")
        .into_iter()
        .map(|uid| {
            let raw = server.raw("Drafts", uid).unwrap();
            let parsed = MessageParser::default().parse(&raw).unwrap();
            (
                uid,
                parsed.subject().map(str::to_owned),
                parsed.message_id().map(str::to_owned),
            )
        })
        .collect()
}

/// Where `command` is in the server's log, the first time.
fn logged(server: &FakeServer, command: &str) -> usize {
    let log = server.imap_log();
    log.iter()
        .position(|line| line == command)
        .unwrap_or_else(|| panic!("{command} is not in the log: {log:?}"))
}

/// The commands of the server's log that remove messages, or mark them for it.
fn removals(server: &FakeServer) -> Vec<String> {
    server
        .imap_log()
        .into_iter()
        .filter(|line| line.contains("EXPUNGE") || line.contains("\\Deleted"))
        .collect()
}

/// Waits until no draft of the account waits in the copy: each one is done with the server.
async fn settled(env: &Env) {
    let started = Instant::now();
    loop {
        let status = env.status().await;
        if status["accounts"][0]["drafts"]
            .as_array()
            .is_none_or(|drafts| drafts.is_empty())
        {
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "the drafts did not settle within 60 seconds: {status}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Waits until the server's Drafts folder holds these subjects, in this order, then a little
/// longer, and checks it holds no more.
async fn drafts_on_server(server: &FakeServer, subjects: &[&str]) {
    let now = || {
        server_drafts(server)
            .into_iter()
            .map(|(_, subject, _)| subject.unwrap_or_default())
            .collect::<Vec<_>>()
    };
    eventually(30, "the drafts on the server", || now() == subjects).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(now(), subjects);
}

#[tokio::test(flavor = "multi_thread")]
async fn compose_without_send_saves_a_draft_and_prints_its_identifier() {
    let server = mailbox().start().await;
    let (env, _daemon) = started(&Setup::of(&server)).await;

    let text = env
        .run(&[
            "compose",
            "--to",
            RECIPIENT,
            "--subject",
            "One",
            "--body",
            BODY,
        ])
        .await;
    succeeded(&text);
    let id = env.save(&["--to", RECIPIENT, "--subject", "Two"]).await;
    assert!(id.ends_with("@example.org"), "{id}");
    // A draft needs no recipient.
    env.save(&["--subject", "Three"]).await;

    drafts_on_server(&server, &["One", "Two", "Three"]).await;
    for uid in server.uids("Drafts") {
        let mut flags = server.flags("Drafts", uid).unwrap();
        flags.sort();
        assert_eq!(flags, ["\\Draft", "\\Seen"]);
    }
    assert!(stdout(&text).contains("@example.org"), "{}", stdout(&text));
    assert_eq!(server_drafts(&server)[1].2.as_deref(), Some(id.as_str()));

    // What cannot be a message saves nothing, and an unknown draft says so.
    failed(&env.run(&["compose", "--to", "not an address"]).await);
    failed(
        &env.run(&[
            "compose",
            "--to",
            RECIPIENT,
            "--attach",
            "/nonexistent/file",
        ])
        .await,
    );
    let unknown = env
        .run(&[
            "compose",
            "--draft",
            "nothing@example.org",
            "--subject",
            "x",
        ])
        .await;
    assert_eq!(unknown.status.code(), Some(3), "{}", stderr(&unknown));
    drafts_on_server(&server, &["One", "Two", "Three"]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_draft_is_saved_picked_up_changed_and_sent_with_append_then_the_removal_of_the_old_uid_only()
 {
    let server = mailbox().start().await;
    let (env, _daemon) = started(&Setup::of(&server)).await;
    let mut watcher = env.watch().await;
    let to = format!("{NAME} <{RECIPIENT}>");
    let attachment = env.file("notes.txt", b"attached notes");

    let id = env
        .save(&[
            "--to",
            &to,
            "--cc",
            COPIED,
            "--bcc",
            HIDDEN,
            "--subject",
            "First",
            "--body",
            BODY,
            "--attach",
            &attachment,
        ])
        .await;
    let saved = watcher.next("draft").await;
    assert_eq!(saved["outcome"], "saved", "{saved}");
    assert_eq!(saved["draft"], id.as_str(), "{saved}");
    drafts_on_server(&server, &["First"]).await;
    let first = server.raw("Drafts", 1).unwrap();
    eventually(20, "the draft in the copy", || {
        block(env.drafts()).first().is_some_and(|d| d["uid"] == 1)
    })
    .await;
    let listed = env.drafts().await;
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0]["subject"], "First");

    // Picked up again: the subject changes, the rest is kept.
    let output = env
        .run(&["compose", "--json", "--draft", &id, "--subject", SUBJECT])
        .await;
    succeeded(&output);
    let again: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(again["id"], id.as_str());
    drafts_on_server(&server, &[SUBJECT]).await;
    let second = server.raw("Drafts", 2).unwrap();
    let parsed = MessageParser::default().parse(&second).unwrap();
    assert_eq!(parsed.message_id(), Some(id.as_str()));
    assert!(parsed.date().is_some());
    let first_date = MessageParser::default()
        .parse(&first)
        .unwrap()
        .date()
        .map(|d| d.to_timestamp());
    assert!(parsed.date().map(|d| d.to_timestamp()) >= first_date);
    let recipient = parsed.to().unwrap().first().unwrap();
    assert_eq!(
        (recipient.name(), recipient.address()),
        (Some(NAME), Some(RECIPIENT))
    );
    assert_eq!(
        parsed.cc().unwrap().first().unwrap().address(),
        Some(COPIED)
    );
    assert_eq!(
        parsed.bcc().unwrap().first().unwrap().address(),
        Some(HIDDEN)
    );
    assert!(parsed.body_text(0).unwrap().contains(BODY));
    let attachments: Vec<_> = parsed.attachments().collect();
    assert_eq!(attachments.len(), 1);
    assert_eq!(attachments[0].attachment_name(), Some("notes.txt"));
    assert_eq!(attachments[0].contents(), b"attached notes");

    // The new version first, then the old one's removal, by its UID only.
    let append = format!("APPEND \"Drafts\" (\\Draft \\Seen) {{{}}}", second.len());
    let store = logged(&server, "UID STORE 1 +FLAGS.SILENT (\\Deleted)");
    assert!(logged(&server, &append) < store);
    assert!(store < logged(&server, "UID EXPUNGE 1"));
    assert_eq!(
        removals(&server),
        ["UID STORE 1 +FLAGS.SILENT (\\Deleted)", "UID EXPUNGE 1"]
    );
    eventually(20, "the new version in the copy", || {
        let listed = block(env.drafts());
        listed.len() == 1 && listed[0]["uid"] == 2
    })
    .await;

    // Sent, as it is now: the draft goes once the server took the message.
    let output = env
        .run(&["compose", "--json", "--draft", &id, "--send"])
        .await;
    succeeded(&output);
    let queued: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(queued["message_id"], id.as_str());
    took(&server, 1).await;
    let message = &server.smtp_messages()[0];
    assert_eq!(message.to, [RECIPIENT, COPIED, HIDDEN]);
    let sent = MessageParser::default().parse(&message.data).unwrap();
    assert_eq!(sent.subject(), Some(SUBJECT));
    assert!(sent.bcc().is_none());
    drafts_on_server(&server, &[]).await;
    assert_eq!(
        removals(&server)[2..],
        ["UID STORE 2 +FLAGS.SILENT (\\Deleted)", "UID EXPUNGE 2"]
    );
    eventually(20, "no draft in the copy", || {
        block(env.drafts()).is_empty()
    })
    .await;
    assert!(!server.imap_log().iter().any(|line| line == "EXPUNGE"));
}

/// Runs a future to its end from a synchronous condition of [`eventually`].
fn block<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(future))
}

/// An HTML reply another client saved as a draft: multipart/alternative, threaded, with a
/// Reply-To, as the server holds it.
fn foreign_html_reply() -> String {
    format!(
        "From: Alice Example <{USER}>\r\nTo: {NAME} <{RECIPIENT}>\r\nReply-To: {USER}\r\n\
         Subject: Re: Other client\r\nMessage-ID: <other@example.com>\r\n\
         In-Reply-To: <asked@example.com>\r\n\
         References: <first@example.com> <asked@example.com>\r\nMIME-Version: 1.0\r\n\
         Content-Type: multipart/alternative; boundary=\"b1\"\r\n\r\n\
         --b1\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{BODY}\r\n\
         --b1\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p><b>{BODY}</b></p>\r\n\
         --b1--\r\n"
    )
}

/// The Drafts folder with [`foreign_html_reply`], a daemon synced, and the draft's identifier.
async fn with_a_foreign_draft() -> (FakeServer, Env, Daemon, String) {
    let server = with_drafts(
        Folder::new("Drafts")
            .special_use(SpecialUse::Drafts)
            .message(Message::new(foreign_html_reply()).flag("\\Draft")),
    )
    .start()
    .await;
    let (env, daemon) = started(&Setup::of(&server)).await;
    let listed = env.drafts().await;
    let id = listed[0]["id"].as_str().unwrap().to_owned();
    (server, env, daemon, id)
}

/// The thread a message answers: its In-Reply-To and References.
fn thread(raw: &[u8]) -> (Vec<String>, Vec<String>) {
    let parsed = MessageParser::default().parse(raw).unwrap();
    let ids = |value: &mail_parser::HeaderValue| match value {
        mail_parser::HeaderValue::Text(id) => vec![id.to_string()],
        mail_parser::HeaderValue::TextList(ids) => ids.iter().map(|id| id.to_string()).collect(),
        _ => Vec::new(),
    };
    (ids(parsed.in_reply_to()), ids(parsed.references()))
}

fn answered() -> (Vec<String>, Vec<String>) {
    (
        vec!["asked@example.com".to_owned()],
        vec![
            "first@example.com".to_owned(),
            "asked@example.com".to_owned(),
        ],
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn a_foreign_html_reply_draft_picked_up_with_a_change_stays_byte_identical_beside_a_new_draft()
 {
    let (server, env, _daemon, id) = with_a_foreign_draft().await;

    let output = env
        .run(&["compose", "--json", "--draft", &id, "--subject", "Changed"])
        .await;
    succeeded(&output);
    let saved: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let new = saved["id"].as_str().unwrap();
    assert_ne!(new, "other@example.com");
    assert!(stderr(&output).contains("is kept"), "{}", stderr(&output));

    drafts_on_server(&server, &["Re: Other client", "Changed"]).await;
    assert_eq!(
        server.raw("Drafts", 1).unwrap(),
        foreign_html_reply().into_bytes()
    );
    assert!(removals(&server).is_empty(), "{:?}", removals(&server));
    let raw = server.raw("Drafts", 2).unwrap();
    let parsed = MessageParser::default().parse(&raw).unwrap();
    assert_eq!(parsed.message_id(), Some(new));
    assert_eq!(
        parsed.to().unwrap().first().unwrap().address(),
        Some(RECIPIENT)
    );
    assert!(parsed.body_text(0).unwrap().contains(BODY));
    assert_eq!(thread(&raw), answered());

    // The new draft is Missive's own: picked up again, it is replaced.
    env.save(&["--draft", new, "--subject", "Again"]).await;
    drafts_on_server(&server, &["Re: Other client", "Again"]).await;
    assert_eq!(
        server.raw("Drafts", 1).unwrap(),
        foreign_html_reply().into_bytes()
    );
    assert_eq!(
        removals(&server),
        ["UID STORE 2 +FLAGS.SILENT (\\Deleted)", "UID EXPUNGE 2"]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_foreign_html_reply_draft_picked_up_and_sent_stays_byte_identical_on_the_server() {
    let (server, env, _daemon, id) = with_a_foreign_draft().await;

    let output = env.run(&["compose", "--draft", &id, "--send"]).await;
    succeeded(&output);
    assert!(stderr(&output).contains("is kept"), "{}", stderr(&output));
    took(&server, 1).await;

    let sent = &server.smtp_messages()[0];
    assert_eq!(thread(&sent.data), answered());
    let parsed = MessageParser::default().parse(&sent.data).unwrap();
    assert_ne!(parsed.message_id(), Some("other@example.com"));
    assert!(parsed.header("X-Missive-Draft").is_none());
    drafts_on_server(&server, &["Re: Other client"]).await;
    assert_eq!(
        server.raw("Drafts", 1).unwrap(),
        foreign_html_reply().into_bytes()
    );
    assert!(removals(&server).is_empty(), "{:?}", removals(&server));
}

/// Asks the daemon on its socket, as the window does, and reads its answer.
async fn ask(env: &Env, request: serde_json::Value) -> serde_json::Value {
    let stream = UnixStream::connect(env.socket()).await.unwrap();
    let (read, mut write) = stream.into_split();
    let mut line = request.to_string();
    line.push('\n');
    write.write_all(line.as_bytes()).await.unwrap();
    let mut answer = String::new();
    BufReader::new(read).read_line(&mut answer).await.unwrap();
    serde_json::from_str(&answer).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_foreign_draft_sent_twice_while_queued_queues_one_message() {
    let (server, env, _daemon, id) = with_a_foreign_draft().await;
    // The first message waits at the server: it is still in the outbox when the second send comes.
    server.smtp_delay("MAIL", Duration::from_secs(4));
    let send = |draft: &str| {
        serde_json::json!({
            "protocol": 1, "request": "draft", "account": "alice", "draft": draft, "send": true,
        })
    };

    let first = ask(&env, send(&id)).await;
    assert_eq!(first["ok"], true, "{first}");
    eventually(10, "the first message at MAIL FROM", || {
        server.smtp_delayed() == 1
    })
    .await;
    // A send asked again after a lost answer, by the identifier the window used or by the
    // draft's Message-ID: the same draft, so the same answer.
    for again in [id.as_str(), "other@example.com"] {
        let second = ask(&env, send(again)).await;
        assert_eq!(second["ok"], false, "{second}");
        assert_eq!(second["error"], "busy", "{second}");
    }

    took(&server, 1).await;
    drafts_on_server(&server, &["Re: Other client"]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_near_miss_message_id_beside_the_draft_survives_a_replacement() {
    let own = |id: &str, subject: &str| {
        Message::new(format!(
            "From: {USER}\r\nSubject: {subject}\r\nMessage-ID: <{id}>\r\nX-Missive-Draft: 1\r\n\r\nx\r\n"
        ))
        .flag("\\Draft")
    };
    let server = with_drafts(
        Folder::new("Drafts")
            .special_use(SpecialUse::Drafts)
            .message(own("x1@example.org", "Near miss"))
            .message(own("1@example.org", "First")),
    )
    .start()
    .await;
    let (env, _daemon) = started(&Setup::of(&server)).await;

    let id = env
        .save(&["--draft", "1@example.org", "--subject", "Second"])
        .await;
    assert_eq!(id, "1@example.org");
    drafts_on_server(&server, &["Near miss", "Second"]).await;
    assert_eq!(server_drafts(&server)[0].0, 1);
    assert_eq!(
        removals(&server),
        ["UID STORE 2 +FLAGS.SILENT (\\Deleted)", "UID EXPUNGE 2"]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_marked_deleted_by_another_client_in_drafts_survives_a_draft_replacement() {
    let server = with_drafts(
        Folder::new("Drafts")
            .special_use(SpecialUse::Drafts)
            .message(
                Message::new(
                    "Subject: Marked elsewhere\r\nMessage-ID: <x@example.com>\r\n\r\nx\r\n",
                )
                .flag("\\Deleted"),
            ),
    )
    .start()
    .await;
    let (env, _daemon) = started(&Setup::of(&server)).await;

    let id = env.save(&["--to", RECIPIENT, "--subject", "First"]).await;
    drafts_on_server(&server, &["Marked elsewhere", "First"]).await;
    env.save(&["--draft", &id, "--subject", "Second"]).await;
    drafts_on_server(&server, &["Marked elsewhere", "Second"]).await;
    succeeded(&env.run(&["compose", "--draft", &id, "--send"]).await);
    took(&server, 1).await;
    drafts_on_server(&server, &["Marked elsewhere"]).await;

    assert_eq!(server.uids("Drafts"), [1]);
    assert_eq!(
        server.flags("Drafts", 1),
        Some(vec!["\\Deleted".to_owned()])
    );
    assert_eq!(
        removals(&server),
        [
            "UID STORE 2 +FLAGS.SILENT (\\Deleted)",
            "UID EXPUNGE 2",
            "UID STORE 3 +FLAGS.SILENT (\\Deleted)",
            "UID EXPUNGE 3"
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_expunge_takes_back_only_the_deleted_flag_missive_set() {
    let server = mailbox().start().await;
    let (env, _daemon) = started(&Setup::of(&server)).await;
    let id = env.save(&["--to", RECIPIENT, "--subject", "First"]).await;
    drafts_on_server(&server, &["First"]).await;
    // Another client marked that version for removal, and has not expunged it yet.
    server.set_flags("Drafts", 1, &["\\Draft", "\\Seen", "\\Deleted"]);
    server.refuse("UID EXPUNGE");

    env.save(&["--draft", &id, "--subject", "Second"]).await;

    drafts_on_server(&server, &["First", "Second"]).await;
    settled(&env).await;
    assert!(
        server
            .flags("Drafts", 1)
            .unwrap()
            .contains(&"\\Deleted".to_owned()),
        "{:?}",
        server.flags("Drafts", 1)
    );
    // Ours is taken back from the new version's removal only if it had been set: it had not.
    assert!(
        !removals(&server).iter().any(|line| line.contains("-FLAGS")),
        "{:?}",
        removals(&server)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_expunge_takes_back_the_deleted_flag_it_set_itself() {
    let server = mailbox().start().await;
    let (env, _daemon) = started(&Setup::of(&server)).await;
    let id = env.save(&["--to", RECIPIENT, "--subject", "First"]).await;
    drafts_on_server(&server, &["First"]).await;
    server.refuse("UID EXPUNGE");

    env.save(&["--draft", &id, "--subject", "Second"]).await;

    drafts_on_server(&server, &["First", "Second"]).await;
    settled(&env).await;
    assert!(
        !server
            .flags("Drafts", 1)
            .unwrap()
            .contains(&"\\Deleted".to_owned())
    );
}

/// A server whose IMAP side is reached through a relay, an account on it synced once, and the
/// relay switched off: the daemon returned is offline.
async fn imap_down() -> (FakeServer, Relay, Env, Daemon) {
    let server = mailbox().start().await;
    let relay = Relay::start(server.imap_tls_addr()).await;
    relay.up.store(true, Ordering::SeqCst);
    let mut setup = Setup::of(&server);
    setup.imap = relay.port;
    let (env, mut daemon) = started(&setup).await;
    daemon.kill().await;
    relay.up.store(false, Ordering::SeqCst);
    let daemon = env.daemon().await;
    (server, relay, env, daemon)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_draft_saved_offline_is_listed_at_once_and_reaches_the_server_once_as_one_draft() {
    let (server, relay, env, _daemon) = imap_down().await;

    let id = env.save(&["--to", RECIPIENT, "--subject", "First"]).await;
    let listed = env.drafts().await;
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0]["id"], id.as_str());
    assert_eq!(listed[0]["subject"], "First");
    env.save(&["--draft", &id, "--subject", "Second"]).await;
    let listed = env.drafts().await;
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0]["subject"], "Second");
    let status = env.status().await;
    assert_eq!(
        status["accounts"][0]["drafts"][0]["id"],
        id.as_str(),
        "{status}"
    );
    assert_eq!(
        status["accounts"][0]["drafts"][0]["state"], "waiting",
        "{status}"
    );

    relay.up.store(true, Ordering::SeqCst);

    drafts_on_server(&server, &["Second"]).await;
    let appends = server
        .imap_log()
        .iter()
        .filter(|line| line.starts_with("APPEND"))
        .count();
    assert_eq!(appends, 1);
    eventually(
        20,
        "the draft in the copy under its server identifier",
        || {
            let listed = block(env.drafts());
            listed.len() == 1 && listed[0]["uid"] == 1
        },
    )
    .await;
    assert!(
        env.status().await["accounts"][0]["drafts"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_append_whose_answer_never_came_leaves_one_draft_on_the_server_not_two() {
    let server = mailbox().start().await;
    let (env, _daemon) = started(&Setup::of(&server)).await;
    server.cut_after_append();

    let id = env.save(&["--to", RECIPIENT, "--subject", "First"]).await;
    eventually(20, "the cut append", || !server.uids("Drafts").is_empty()).await;
    // A retry may still be on its way: only a draft that left the copy is done with the server.
    settled(&env).await;
    server.heal();

    drafts_on_server(&server, &["First"]).await;
    assert_eq!(
        server
            .imap_log()
            .iter()
            .filter(|line| line.starts_with("APPEND"))
            .count(),
        1
    );
    assert_eq!(server_drafts(&server)[0].2.as_deref(), Some(id.as_str()));
    let search = format!("UID SEARCH HEADER Message-ID \"<{id}>\" HEADER X-Missive-Draft \"1\"");
    assert!(
        server.imap_log().contains(&search),
        "{:?}",
        server.imap_log()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn two_appends_cut_in_a_row_converge_to_one_draft_on_the_server() {
    let server = mailbox().start().await;
    let (env, _daemon) = started(&Setup::of(&server)).await;
    server.cut_after_append();

    let id = env.save(&["--to", RECIPIENT, "--subject", "First"]).await;
    eventually(20, "the first cut append", || {
        !server.uids("Drafts").is_empty()
    })
    .await;
    env.save(&["--draft", &id, "--subject", "Second"]).await;
    eventually(20, "the second cut append", || {
        server_drafts(&server)
            .iter()
            .any(|(_, subject, _)| subject.as_deref() == Some("Second"))
    })
    .await;

    // Still cut: a version the server already holds is not appended again, and the others go.
    settled(&env).await;
    drafts_on_server(&server, &["Second"]).await;
    assert_eq!(server_drafts(&server)[0].2.as_deref(), Some(id.as_str()));
    server.heal();
    succeeded(&env.run(&["sync"]).await);
    drafts_on_server(&server, &["Second"]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn without_uidplus_an_append_whose_answer_never_came_is_not_appended_again() {
    let server = mailbox().without_uidplus().start().await;
    let (env, _daemon) = started(&Setup::of(&server)).await;
    server.cut_after_append();

    env.save(&["--to", RECIPIENT, "--subject", "First"]).await;
    eventually(20, "the cut append", || !server.uids("Drafts").is_empty()).await;

    settled(&env).await;
    drafts_on_server(&server, &["First"]).await;
    server.heal();
    succeeded(&env.run(&["sync"]).await);
    drafts_on_server(&server, &["First"]).await;
    assert_eq!(
        server
            .imap_log()
            .iter()
            .filter(|line| line.starts_with("APPEND"))
            .count(),
        1
    );
    assert!(removals(&server).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_no_on_append_keeps_the_local_draft_and_the_old_server_version() {
    let server = mailbox().start().await;
    let (env, _daemon) = started(&Setup::of(&server)).await;
    let mut watcher = env.watch().await;
    let id = env.save(&["--to", RECIPIENT, "--subject", "First"]).await;
    drafts_on_server(&server, &["First"]).await;
    server.refuse("APPEND");

    env.save(&["--draft", &id, "--subject", "Second"]).await;

    loop {
        let event = watcher.next("draft").await;
        if event["outcome"] == "retried" {
            assert_eq!(event["reason"], "refused", "{event}");
            break;
        }
    }
    drafts_on_server(&server, &["First"]).await;
    assert!(removals(&server).is_empty());
    let listed = env.drafts().await;
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0]["subject"], "Second");
    let status = env.status().await;
    assert!(
        status["accounts"][0]["drafts"][0]["attempts"].as_u64() >= Some(1),
        "{status}"
    );

    server.heal();
    succeeded(&env.run(&["sync"]).await);
    drafts_on_server(&server, &["Second"]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn without_uidplus_the_old_version_is_left_in_place_and_the_user_is_told() {
    let server = mailbox().without_uidplus().start().await;
    let (env, _daemon) = started(&Setup::of(&server)).await;
    let mut watcher = env.watch().await;
    let id = env.save(&["--to", RECIPIENT, "--subject", "First"]).await;
    drafts_on_server(&server, &["First"]).await;
    watcher.next("draft").await;

    env.save(&["--draft", &id, "--subject", "Second"]).await;

    let saved = watcher.next("draft").await;
    assert_eq!(saved["outcome"], "saved", "{saved}");
    assert_eq!(saved["left"], 1, "{saved}");
    assert!(
        saved["message"].as_str().unwrap().contains("UIDPLUS"),
        "{saved}"
    );
    drafts_on_server(&server, &["First", "Second"]).await;
    assert!(removals(&server).is_empty());
    let logs = env.logs();
    assert!(logs.contains("UIDPLUS"), "{logs}");
}

#[tokio::test(flavor = "multi_thread")]
async fn without_uidplus_a_sent_draft_left_in_drafts_is_not_sent_a_second_time() {
    let server = mailbox().without_uidplus().start().await;
    let (env, _daemon) = started(&Setup::of(&server)).await;
    let id = env.save(&["--to", RECIPIENT, "--subject", "First"]).await;
    drafts_on_server(&server, &["First"]).await;
    eventually(20, "the draft in the copy", || {
        block(env.drafts()).first().is_some_and(|d| d["uid"] == 1)
    })
    .await;
    succeeded(&env.run(&["compose", "--draft", &id, "--send"]).await);
    took(&server, 1).await;
    eventually(20, "the copy in Sent", || !server.uids("Sent").is_empty()).await;
    settled(&env).await;
    succeeded(&env.run(&["sync"]).await);
    // Without UIDPLUS the draft's version cannot be removed: it is still there, and listed.
    drafts_on_server(&server, &["First"]).await;
    assert_eq!(env.drafts().await.len(), 1);

    let output = env.run(&["compose", "--draft", &id, "--send"]).await;

    failed(&output);
    assert!(
        stderr(&output).contains("already sent"),
        "{}",
        stderr(&output)
    );
    took(&server, 1).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_non_ascii_message_id_is_not_put_in_a_quoted_search_and_the_draft_settles() {
    // A server that answers BAD to a quoted string holding more than ASCII.
    let server = with_drafts(
        Folder::new("Drafts")
            .special_use(SpecialUse::Drafts)
            .message(
                Message::new(
                    "From: Alice Example <alice@example.org>\r\nTo: zoe.recipient@example.com\r\n\
                     Subject: Elsewhere\r\nMessage-ID: <café-1@example.com>\r\n\
                     X-Missive-Draft: 1\r\n\r\nA body.\r\n",
                )
                .flag("\\Draft"),
            ),
    )
    .bad("UID SEARCH HEADER Message-ID \"<caf\u{e9}")
    .start()
    .await;
    let (env, _daemon) = started(&Setup::of(&server)).await;
    let id = env.drafts().await[0]["id"].as_str().unwrap().to_owned();

    env.save(&["--draft", &id, "--subject", "Changed"]).await;

    settled(&env).await;
    drafts_on_server(&server, &["Changed"]).await;
    let appends = server
        .imap_log()
        .iter()
        .filter(|line| line.starts_with("APPEND"))
        .count();
    assert_eq!(appends, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_draft_appended_to_a_folder_the_copy_numbers_otherwise_is_listed_after_a_resync() {
    let server = mailbox().start().await;
    let (env, _daemon) = started(&Setup::of(&server)).await;
    // The server renumbered Drafts; the copy has not heard of it: the UID the APPEND answers
    // means nothing there.
    let validity = server.bump_uidvalidity("Drafts");

    env.save(&["--to", RECIPIENT, "--subject", "First"]).await;

    drafts_on_server(&server, &["First"]).await;
    eventually(
        20,
        "the draft in the copy, numbered as the server does",
        || {
            let listed = block(env.drafts());
            listed.len() == 1
                && listed[0]["uid"] == 1
                && listed[0]["id"]
                    .as_str()
                    .is_some_and(|id| id.contains(&validity.to_string()))
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_drafts_folder_the_draft_stays_local_and_status_says_so() {
    let server = FakeServer::builder()
        .user(USER, PASSWORD)
        .folder(Folder::new("INBOX"))
        .folder(Folder::new("Sent").special_use(SpecialUse::Sent))
        .start()
        .await;
    let (env, _daemon) = started(&Setup::of(&server)).await;

    let id = env.save(&["--to", RECIPIENT, "--subject", "First"]).await;
    succeeded(&env.run(&["sync"]).await);

    let status = env.status().await;
    let draft = &status["accounts"][0]["drafts"][0];
    assert_eq!(draft["id"], id.as_str(), "{status}");
    assert_eq!(draft["state"], "no-drafts-folder", "{status}");
    let text = env.run(&["status"]).await;
    assert!(
        stdout(&text).contains("no Drafts folder"),
        "{}",
        stdout(&text)
    );
    assert!(
        !server
            .imap_log()
            .iter()
            .any(|line| line.starts_with("APPEND") || line.starts_with("CREATE")),
        "{:?}",
        server.imap_log()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_draft_whose_message_is_refused_for_good_stays_a_draft() {
    let server = mailbox()
        .smtp_answer("RCPT", "550 5.1.1 no such user")
        .start()
        .await;
    let (env, _daemon) = started(&Setup::of(&server)).await;
    let mut watcher = env.watch().await;
    let id = env.save(&["--to", RECIPIENT, "--subject", "First"]).await;
    drafts_on_server(&server, &["First"]).await;

    succeeded(&env.run(&["compose", "--draft", &id, "--send"]).await);

    watcher.next("send-failed").await;
    drafts_on_server(&server, &["First"]).await;
    assert!(removals(&server).is_empty());
    assert_eq!(env.drafts().await.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn no_body_subject_or_address_appears_in_the_logs_of_drafts() {
    let server = mailbox().start().await;
    let (env, _daemon) = started(&Setup::of(&server)).await;
    let mut watcher = env.watch().await;
    let to = format!("{NAME} <{RECIPIENT}>");
    let body = env.file("body.txt", BODY.as_bytes());
    let args = [
        "compose",
        "--to",
        &to,
        "--cc",
        COPIED,
        "--bcc",
        HIDDEN,
        "--subject",
        SUBJECT,
        "--body-file",
        &body,
    ];

    let mut outputs = vec![env.run(&args).await];
    watcher.next("draft").await;
    let id = server_drafts(&server)[0].2.clone().unwrap();
    server.refuse("APPEND");
    outputs.push(env.run(&["compose", "--draft", &id, "--body", BODY]).await);
    loop {
        if watcher.next("draft").await["outcome"] == "retried" {
            break;
        }
    }
    outputs.push(env.run(&["status"]).await);
    outputs.push(env.run(&["status", "--json"]).await);
    server.heal();
    succeeded(&env.run(&["sync"]).await);
    outputs.push(env.run(&["compose", "--draft", &id, "--send"]).await);
    watcher.next("sent").await;
    drafts_on_server(&server, &[]).await;

    let logs = env.logs();
    assert!(logs.contains("draft"), "{logs}");
    for private in private() {
        assert!(!logs.contains(&private), "the log shows {private}:\n{logs}");
        for output in &outputs {
            assert!(!stdout(output).contains(&private), "{}", stdout(output));
            assert!(!stderr(output).contains(&private), "{}", stderr(output));
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn mailto_saves_a_draft_from_the_link_and_says_where_to_find_it() {
    let server = mailbox().start().await;
    let (env, _daemon) = started(&Setup::of(&server)).await;

    let link = format!(
        "mailto:{RECIPIENT},second@example.com?cc={COPIED}&bcc={HIDDEN}&subject=Caf%C3%A9%20link\
         &body=Line%20one%0D%0ALine%20two&in-reply-to=%3Casked@example.com%3E\
         &from=evil@example.net&attach=/etc/passwd&reply-to=evil@example.net"
    );
    let text = env.run(&["mailto", &link]).await;
    succeeded(&text);
    let said = stdout(&text);
    assert_eq!(said.lines().count(), 1, "{said}");
    assert!(said.contains("@example.org"), "{said}");
    assert!(said.contains("missive list --folder drafts"), "{said}");

    let json = env
        .run(&["mailto", "--json", "mailto:?subject=Second"])
        .await;
    succeeded(&json);
    let saved: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(saved.as_object().unwrap().len(), 1, "{saved}");

    drafts_on_server(&server, &["Café link", "Second"]).await;
    let uid = server.uids("Drafts")[0];
    let raw = server.raw("Drafts", uid).unwrap();
    let parsed = MessageParser::default().parse(&raw).unwrap();
    let addresses = |address: Option<&mail_parser::Address>| -> Vec<String> {
        address
            .map(|a| {
                a.iter()
                    .filter_map(|m| m.address().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    };
    assert_eq!(addresses(parsed.to()), [RECIPIENT, "second@example.com"]);
    assert_eq!(addresses(parsed.cc()), [COPIED]);
    assert_eq!(addresses(parsed.bcc()), [HIDDEN]);
    assert_eq!(addresses(parsed.from()), [USER]);
    assert_eq!(
        parsed.body_text(0).unwrap().trim_end(),
        "Line one\r\nLine two"
    );
    assert_eq!(thread(&raw).0, ["asked@example.com"]);
    // The link chose neither the sender nor a file, and set no other header.
    assert_eq!(parsed.attachment_count(), 0);
    assert!(parsed.header("Reply-To").is_none());
    assert!(!String::from_utf8_lossy(&raw).contains("evil@example.net"));
    assert!(!String::from_utf8_lossy(&raw).contains("passwd"));
}

#[tokio::test(flavor = "multi_thread")]
async fn mailto_refuses_a_link_that_would_inject_a_header_and_saves_nothing() {
    let server = mailbox().start().await;
    let (env, _daemon) = started(&Setup::of(&server)).await;

    for link in [
        "mailto:a@example.com?subject=Hi%0D%0ABcc:evil@example.net",
        "mailto:a@example.com?subject=100%",
        "https://example.com",
    ] {
        let output = env.run(&["mailto", link]).await;
        failed(&output);
        assert!(stderr(&output).contains("mailto"), "{}", stderr(&output));
        assert!(stdout(&output).is_empty(), "{}", stdout(&output));
    }
    assert!(env.drafts().await.is_empty());
    drafts_on_server(&server, &[]).await;
}

/// Whether the fake window wrote its five arguments.
fn written(args: &Path) -> bool {
    std::fs::read_to_string(args)
        .is_ok_and(|text| text.lines().count() == 5 && text.ends_with('\n'))
}

/// A copy of `missive` alone in a folder, so that no real `missive-window` sits beside it, and a
/// fake `missive-window` in another folder, for the `PATH`, that writes its arguments to `args`,
/// one per line, with the shell's builtins alone (the `PATH` holds nothing else).
fn fake_window(env: &Env) -> (PathBuf, PathBuf, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let bin = env.path("bin");
    let path = env.path("path");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&path).unwrap();
    let missive = bin.join("missive");
    std::fs::copy(env!("CARGO_BIN_EXE_missive"), &missive).unwrap();
    let args = env.path("args");
    let window = path.join("missive-window");
    std::fs::write(
        &window,
        format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n", args.display()),
    )
    .unwrap();
    std::fs::set_permissions(&window, std::fs::Permissions::from_mode(0o755)).unwrap();
    (missive, path, args)
}

#[tokio::test(flavor = "multi_thread")]
async fn with_a_display_mailto_and_compose_open_the_window_on_the_draft_saved_first() {
    let server = mailbox().start().await;
    let (env, _daemon) = started(&Setup::of(&server)).await;
    let (missive, path, args) = fake_window(&env);

    let run = |command: &[&str]| {
        let mut real = Command::new(&missive);
        real.args(command)
            .env_clear()
            .env("HOME", env.home.path())
            .env("XDG_CONFIG_HOME", env.path("config"))
            .env("XDG_DATA_HOME", env.path("data"))
            .env("XDG_STATE_HOME", env.path("state"))
            .env("XDG_RUNTIME_DIR", env.path("runtime"))
            .env("DBUS_SESSION_BUS_ADDRESS", env.keyring.address())
            .env("PATH", &path)
            .env("WAYLAND_DISPLAY", "wayland-fake")
            .stdin(Stdio::null());
        real
    };
    let output = run(&["mailto", "--json", "mailto:zoe@example.com?subject=Linked"])
        .output()
        .await
        .unwrap();
    succeeded(&output);
    let saved: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let id = saved["id"].as_str().unwrap().to_owned();
    eventually(10, "the window started", || written(&args)).await;
    assert_eq!(
        std::fs::read_to_string(&args).unwrap(),
        format!("compose\n--account\nalice\n--draft\n{id}\n")
    );
    // The draft the window is started on is in the copy already.
    let listed = env.drafts().await;
    assert!(listed.iter().any(|d| d["id"] == id.as_str()), "{listed:?}");

    std::fs::remove_file(&args).unwrap();
    let output = run(&["compose", "--json", "--to", RECIPIENT])
        .output()
        .await
        .unwrap();
    succeeded(&output);
    let saved: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    eventually(10, "the window started", || written(&args)).await;
    assert!(
        std::fs::read_to_string(&args)
            .unwrap()
            .ends_with(&format!("--draft\n{}\n", saved["id"].as_str().unwrap()))
    );

    // Sending opens nothing; without a display, nothing opens either.
    std::fs::remove_file(&args).unwrap();
    let output = run(&["compose", "--json", "--to", RECIPIENT, "--send"])
        .output()
        .await
        .unwrap();
    succeeded(&output);
    let output = run(&["mailto", "mailto:zoe@example.com"])
        .env_remove("WAYLAND_DISPLAY")
        .output()
        .await
        .unwrap();
    succeeded(&output);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!args.exists(), "no window without a display");
}
