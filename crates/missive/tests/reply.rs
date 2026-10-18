//! `missive compose --reply`, `--reply-all` and `--forward`, against the fake IMAP and SMTP server
//! and a fake Secret Service on a private bus. The binary runs with a cleared environment: its
//! configuration, data, state and runtime folders are temporary, and the only session bus it can
//! see is the test's own. Every daemon a test starts is killed when the test ends, even when it
//! panics.

use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use mail_parser::{MessageParser, MimeHeaders};
use missive_core::account::{Account, AccountStore, Password, Security, Server};
use missive_core::fake::{FakeKeyring, FakeServer, Folder, Message, SpecialUse};
use missive_core::keyring::Keyring;
use rusqlite::{Connection, OpenFlags};
use tempfile::TempDir;
use tokio::net::UnixStream;
use tokio::process::{Child, Command};

const USER: &str = "alice@example.org";
const PASSWORD: &str = "Recognisable-Pa55word-4c1d";

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
        Daemon { _child: child }
    }

    async fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().await.unwrap()
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
    _child: Child,
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

/// INBOX with one message (UID 1), a Sent folder and a Drafts folder.
fn mailbox() -> missive_core::fake::Builder {
    FakeServer::builder()
        .user(USER, PASSWORD)
        .folder(Folder::new("INBOX").message(Message::new(ORIGINAL)))
        .folder(Folder::new("Sent").special_use(SpecialUse::Sent))
        .folder(Folder::new("Drafts").special_use(SpecialUse::Drafts))
}

/// Sent to Alice and Carl, copied to Dana and to Alice again in capitals, part of a thread.
const ORIGINAL: &str = "From: Zoé Ça <zoe@example.com>\r\n\
    To: Alice Example <alice@example.org>, carl@example.net\r\n\
    Cc: Dana <dana@example.net>, ALICE@EXAMPLE.ORG\r\n\
    Subject: Lunch?\r\n\
    Message-ID: <lunch@example.com>\r\n\
    References: <root@example.com>\r\n\
    Date: Sat, 17 Oct 2026 07:30:00 +0200\r\n\
    Content-Type: text/plain; charset=utf-8\r\n\
    Content-Transfer-Encoding: 8bit\r\n\
    \r\n\
    Are you free?\r\n\
    \r\n\
    Zoé\r\n";

/// The identifier `missive list` gives the message of the inbox.
async fn original(env: &Env) -> String {
    let output = env.run(&["list", "--json"]).await;
    succeeded(&output);
    let listed: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(listed.len(), 1);
    listed[0]["id"].as_str().unwrap().to_owned()
}

/// The addresses of a header, in order.
fn addresses(address: Option<&mail_parser::Address>) -> Vec<String> {
    address
        .into_iter()
        .flat_map(|address| address.iter())
        .map(|mailbox| mailbox.address().unwrap().to_owned())
        .collect()
}

/// The identifiers of an `In-Reply-To` or `References` header.
fn ids(value: &mail_parser::HeaderValue) -> Vec<String> {
    match value {
        mail_parser::HeaderValue::Text(id) => vec![id.to_string()],
        mail_parser::HeaderValue::TextList(ids) => ids.iter().map(|id| id.to_string()).collect(),
        _ => Vec::new(),
    }
}

/// The flags the server holds for UID 1 of the inbox.
fn flags(server: &FakeServer) -> Vec<String> {
    server.flags("INBOX", 1).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reply_sent_arrives_with_the_right_headers_and_quote_and_marks_the_original_answered() {
    let server = mailbox().start().await;
    let (env, _daemon) = started(&Setup::of(&server)).await;
    let id = original(&env).await;
    assert!(!flags(&server).contains(&"\\Answered".to_owned()));

    let output = env
        .run(&[
            "compose",
            "--reply",
            &id,
            "--body",
            "Noon works.",
            "--send",
            "--json",
        ])
        .await;
    succeeded(&output);
    took(&server, 1).await;

    let sent = &server.smtp_messages()[0];
    assert_eq!(sent.from, USER);
    assert_eq!(sent.to, ["zoe@example.com"]);
    let parsed = MessageParser::default().parse(&sent.data).unwrap();
    assert_eq!(addresses(parsed.to()), ["zoe@example.com"]);
    assert_eq!(addresses(parsed.cc()), Vec::<String>::new());
    assert_eq!(parsed.subject(), Some("Re: Lunch?"));
    assert_eq!(ids(parsed.in_reply_to()), ["lunch@example.com"]);
    assert_eq!(
        ids(parsed.references()),
        ["root@example.com", "lunch@example.com"]
    );
    assert_eq!(
        parsed.body_text(0).unwrap(),
        "Noon works.\r\n\r\nOn 2026-10-17 07:30 +0200, Zoé Ça wrote:\r\n\
         > Are you free?\r\n> \r\n> Zoé\r\n"
    );

    // The copy shows it at once; the server is told as soon as the account is connected.
    let shown = env.run(&["show", "--json", &id]).await;
    succeeded(&shown);
    let shown: serde_json::Value = serde_json::from_slice(&shown.stdout).unwrap();
    assert_eq!(shown["answered"], true);
    eventually(30, "the original marked answered", || {
        flags(&server).contains(&"\\Answered".to_owned())
    })
    .await;
    assert!(
        server
            .imap_log()
            .iter()
            .any(|line| line == "UID STORE 1 +FLAGS (\\Answered)"),
        "{:?}",
        server.imap_log()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reply_saved_as_a_draft_does_not_mark_the_original() {
    let server = mailbox().start().await;
    let (env, _daemon) = started(&Setup::of(&server)).await;
    let id = original(&env).await;

    let output = env
        .run(&["compose", "--reply", &id, "--body", "Later", "--json"])
        .await;
    succeeded(&output);
    eventually(30, "the draft on the server", || {
        !server.uids("Drafts").is_empty()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let draft = server.raw("Drafts", 1).unwrap();
    let parsed = MessageParser::default().parse(&draft).unwrap();
    assert_eq!(parsed.subject(), Some("Re: Lunch?"));
    assert_eq!(ids(parsed.in_reply_to()), ["lunch@example.com"]);
    assert!(
        parsed
            .body_text(0)
            .unwrap()
            .starts_with("Later\r\n\r\nOn 2026-10-17")
    );
    assert!(!flags(&server).contains(&"\\Answered".to_owned()));
    assert!(
        !server
            .imap_log()
            .iter()
            .any(|line| line.contains("Answered")),
        "{:?}",
        server.imap_log()
    );
    assert!(server.smtp_messages().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reply_the_server_refuses_never_marks_the_original() {
    let server = mailbox().smtp_answer("MAIL", "550 no thanks").start().await;
    let (env, _daemon) = started(&Setup::of(&server)).await;
    let id = original(&env).await;

    succeeded(&env.run(&["compose", "--reply", &id, "--send"]).await);
    let started = Instant::now();
    while env.status().await["accounts"][0]["outbox"][0]["state"] != "failed" {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the reply was not refused"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_millis(1500)).await;

    assert!(!flags(&server).contains(&"\\Answered".to_owned()));
    let shown = env.run(&["show", "--json", &id]).await;
    let shown: serde_json::Value = serde_json::from_slice(&shown.stdout).unwrap();
    assert_eq!(shown["answered"], false);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reply_all_goes_to_everyone_but_the_account_once() {
    let server = mailbox().start().await;
    let (env, _daemon) = started(&Setup::of(&server)).await;
    let id = original(&env).await;

    succeeded(&env.run(&["compose", "--reply-all", &id, "--send"]).await);
    took(&server, 1).await;

    let sent = &server.smtp_messages()[0];
    assert_eq!(
        sent.to,
        ["zoe@example.com", "carl@example.net", "dana@example.net"]
    );
    let parsed = MessageParser::default().parse(&sent.data).unwrap();
    assert_eq!(
        addresses(parsed.to()),
        ["zoe@example.com", "carl@example.net"]
    );
    assert_eq!(addresses(parsed.cc()), ["dana@example.net"]);
    assert_eq!(parsed.subject(), Some("Re: Lunch?"));
}

#[tokio::test(flavor = "multi_thread")]
async fn what_the_user_gives_replaces_the_recipients_and_the_subject_of_a_reply() {
    let server = mailbox().start().await;
    let (env, _daemon) = started(&Setup::of(&server)).await;
    let id = original(&env).await;

    succeeded(
        &env.run(&[
            "compose",
            "--reply",
            &id,
            "--to",
            "other@example.com",
            "--subject",
            "Another subject",
            "--send",
        ])
        .await,
    );
    took(&server, 1).await;

    let sent = &server.smtp_messages()[0];
    assert_eq!(sent.to, ["other@example.com"]);
    let parsed = MessageParser::default().parse(&sent.data).unwrap();
    assert_eq!(parsed.subject(), Some("Another subject"));
    assert_eq!(ids(parsed.in_reply_to()), ["lunch@example.com"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_forward_carries_the_original_byte_identical_as_a_message_rfc822_part() {
    let server = mailbox().start().await;
    let (env, _daemon) = started(&Setup::of(&server)).await;
    let id = original(&env).await;

    succeeded(
        &env.run(&[
            "compose",
            "--forward",
            &id,
            "--to",
            "ina@example.net",
            "--body",
            "For you.",
            "--send",
        ])
        .await,
    );
    took(&server, 1).await;

    let sent = &server.smtp_messages()[0];
    assert_eq!(sent.to, ["ina@example.net"]);
    let parsed = MessageParser::default().parse(&sent.data).unwrap();
    assert_eq!(parsed.subject(), Some("Fwd: Lunch?"));
    assert!(parsed.in_reply_to().as_text().is_none());
    assert!(parsed.references().as_text().is_none());
    assert_eq!(parsed.body_text(0).unwrap(), "For you.");
    let attachments: Vec<_> = parsed.attachments().collect();
    assert_eq!(attachments.len(), 1);
    let content_type = attachments[0].content_type().unwrap();
    assert_eq!(
        (content_type.ctype(), content_type.subtype()),
        ("message", Some("rfc822"))
    );
    assert_eq!(attachments[0].attachment_name(), Some("Lunch?.eml"));
    // The server held the message exactly as the fake served it.
    assert_eq!(attachments[0].contents(), ORIGINAL.as_bytes());

    // A forward is not an answer.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(!flags(&server).contains(&"\\Answered".to_owned()));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_that_does_not_exist_cannot_be_answered() {
    let server = mailbox().start().await;
    let (env, _daemon) = started(&Setup::of(&server)).await;
    for flag in ["--reply", "--reply-all", "--forward"] {
        let output = env.run(&["compose", flag, "alice/999/1/1", "--send"]).await;
        assert_eq!(output.status.code(), Some(3), "{flag}: {}", stderr(&output));
    }
    assert!(server.smtp_messages().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn one_way_of_answering_at_a_time_and_none_with_a_draft() {
    let env = Env::new().await;
    for args in [
        ["compose", "--reply", "a/1/1/1", "--forward", "a/1/1/1"],
        ["compose", "--reply", "a/1/1/1", "--reply-all", "a/1/1/1"],
        [
            "compose",
            "--reply-all",
            "a/1/1/1",
            "--draft",
            "x@example.org",
        ],
    ] {
        let output = env.run(&args).await;
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?}: {}",
            stderr(&output)
        );
    }
}
