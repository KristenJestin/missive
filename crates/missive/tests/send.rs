//! `missive compose --send`, the outbox and the daemon sending it, against the fake IMAP and SMTP
//! server and a fake Secret Service on a private bus. The binary runs with a cleared
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
    FakeServer::builder()
        .user(USER, PASSWORD)
        .folder(Folder::new("INBOX").message(Message::new(
            "From: someone@example.com\r\nSubject: Hi\r\n\r\nHello.\r\n",
        )))
        .folder(Folder::new("Sent").special_use(SpecialUse::Sent))
        .folder(Folder::new("Drafts").special_use(SpecialUse::Drafts))
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
        self.set_password(PASSWORD).await;
    }

    /// Takes the account's password out of the keyring.
    async fn remove_password(&self) {
        let connection = zbus::connection::Builder::address(self.keyring.address())
            .unwrap()
            .build()
            .await
            .unwrap();
        Keyring::open_on(connection)
            .await
            .unwrap()
            .delete("alice")
            .await
            .unwrap();
    }

    /// Puts `password` in the keyring as the account's.
    async fn set_password(&self, password: &str) {
        let connection = zbus::connection::Builder::address(self.keyring.address())
            .unwrap()
            .build()
            .await
            .unwrap();
        Keyring::open_on(connection)
            .await
            .unwrap()
            .store("alice", &Password::new(password.to_owned()))
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

    /// `missive compose --send` of a message to [`RECIPIENT`], `args` added; it must answer
    /// within 5 seconds, connected or not.
    async fn compose(&self, args: &[&str]) -> Output {
        let mut arguments = vec!["compose", "--send", "--to", RECIPIENT, "--subject", SUBJECT];
        arguments.extend_from_slice(args);
        let started = Instant::now();
        let output = self.run(&arguments).await;
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "compose waited for the server"
        );
        output
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

    /// `(id, state, attempts)` of each message of the outbox.
    fn outbox(&self) -> Vec<(i64, String, u32)> {
        let Ok(copy) =
            Connection::open_with_flags(self.copy_path(), OpenFlags::SQLITE_OPEN_READ_ONLY)
        else {
            return Vec::new();
        };
        let Ok(mut statement) = copy.prepare("SELECT id, state, attempts FROM outbox ORDER BY id")
        else {
            return Vec::new();
        };
        statement
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    /// The raw message the outbox holds, by number.
    fn outbox_raw(&self, id: i64) -> Vec<u8> {
        Connection::open_with_flags(self.copy_path(), OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap()
            .query_row("SELECT raw FROM outbox WHERE id = ?1", [id], |r| r.get(0))
            .unwrap()
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
    /// SIGTERM, and waits for the daemon to end by itself, at most `seconds`.
    async fn stop(&mut self, seconds: u64) -> ExitStatus {
        let pid = self.child.id().unwrap();
        let _ = std::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status();
        tokio::time::timeout(Duration::from_secs(seconds), self.child.wait())
            .await
            .expect("the daemon did not stop in time")
            .unwrap()
    }

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

/// The Sent copy appended by the client, as the fake server keeps it.
fn sent_copies(server: &FakeServer) -> Vec<Vec<u8>> {
    server
        .uids("Sent")
        .into_iter()
        .map(|uid| server.raw("Sent", uid).unwrap())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_with_utf8_and_two_attachments_arrives_as_built_with_bcc_only_in_the_envelope() {
    let server = mailbox().start().await;
    let (env, _daemon) = started(&Setup::of(&server)).await;
    let body = env.file("body.txt", format!("{BODY}\nÀ bientôt.\n.\n").as_bytes());
    let pdf = env.file("compte rendu été.pdf", b"%PDF-1.4 fake");
    let png = env.file("photo.png", &[0x89, b'P', b'N', b'G', 0, 1, 2, 255]);
    let mut watcher = env.watch().await;
    let to = format!("{NAME} <{RECIPIENT}>");

    let output = env
        .run(&[
            "compose",
            "--send",
            "--json",
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
            "--attach",
            &pdf,
            "--attach",
            &png,
        ])
        .await;
    succeeded(&output);
    let queued: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let message_id = queued["message_id"].as_str().unwrap().to_owned();
    assert!(message_id.ends_with("@example.org"), "{queued}");

    let sent = watcher.next("sent").await;
    assert_eq!(
        sent,
        serde_json::json!({
            "protocol": 1, "event": "sent", "account": "alice", "outbox": queued["outbox"],
            "message_id": message_id, "sent_copy": "append",
        })
    );
    took(&server, 1).await;
    let message = &server.smtp_messages()[0];
    assert_eq!(message.from, USER);
    assert_eq!(message.to, [RECIPIENT, COPIED, HIDDEN]);
    let text = String::from_utf8_lossy(&message.data);
    assert!(!text.contains(HIDDEN), "{text}");
    assert!(!text.to_ascii_lowercase().contains("\nbcc:"), "{text}");
    let parsed = MessageParser::default().parse(&message.data).unwrap();
    assert_eq!(parsed.subject(), Some(SUBJECT));
    assert_eq!(parsed.message_id(), Some(message_id.as_str()));
    let recipient = parsed.to().unwrap().first().unwrap();
    assert_eq!(recipient.name(), Some(NAME));
    assert_eq!(
        parsed.from().unwrap().first().unwrap().name(),
        Some("Alice Example")
    );
    assert_eq!(
        parsed.body_text(0).unwrap(),
        format!("{BODY}\r\nÀ bientôt.\r\n.\r\n")
    );
    let attachments: Vec<_> = parsed.attachments().collect();
    assert_eq!(attachments.len(), 2);
    assert_eq!(
        attachments[0].attachment_name(),
        Some("compte rendu été.pdf")
    );
    assert_eq!(
        attachments[0].content_type().unwrap().subtype(),
        Some("pdf")
    );
    assert_eq!(attachments[0].contents(), b"%PDF-1.4 fake");
    assert_eq!(attachments[1].attachment_name(), Some("photo.png"));
    assert_eq!(
        attachments[1].content_type().unwrap().subtype(),
        Some("png")
    );

    // The copy in Sent is what was built, byte for byte, with the Bcc header before it.
    eventually(20, "the copy in Sent", || server.uids("Sent").len() == 1).await;
    let copy = &sent_copies(&server)[0];
    assert!(
        copy.starts_with(b"Bcc: "),
        "{}",
        String::from_utf8_lossy(copy)
    );
    assert!(copy.ends_with(&message.data));
    let header = &copy[..copy.len() - message.data.len()];
    assert!(String::from_utf8_lossy(header).contains(HIDDEN));
    assert!(
        server
            .imap_log()
            .iter()
            .any(|command| command == &format!("APPEND \"Sent\" (\\Seen) {{{}}}", copy.len()))
    );
    assert_eq!(server.flags("Sent", 1), Some(vec!["\\Seen".to_owned()]));
    eventually(20, "the outbox empty", || env.outbox().is_empty()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn starttls_with_a_pinned_ca_certificate_sends() {
    let server = mailbox().start().await;
    let mut setup = Setup::of(&server);
    setup.smtp = (server.smtp_starttls_addr().port(), Security::Starttls);
    let (env, _daemon) = started(&setup).await;
    let mut watcher = env.watch().await;

    succeeded(&env.compose(&["--body", BODY]).await);

    watcher.next("sent").await;
    took(&server, 1).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_certificate_is_refused_with_certificate_refused_and_the_message_waits() {
    let server = mailbox().start().await;
    let other = FakeServer::builder().start().await;
    let mut setup = Setup::of(&server);
    setup.certificate = Some(other.certificate_der());
    let env = Env::new().await;
    env.add_account(&setup).await;
    let _daemon = env.daemon().await;

    succeeded(&env.compose(&["--body", BODY]).await);

    eventually(20, "a refused try", || {
        env.outbox()
            .first()
            .is_some_and(|(_, _, attempts)| *attempts >= 1)
    })
    .await;
    let status = env.status().await;
    let outbox = &status["accounts"][0]["outbox"][0];
    assert_eq!(outbox["state"], "queued", "{status}");
    assert!(
        outbox["last_error"]
            .as_str()
            .unwrap()
            .starts_with("certificate-refused"),
        "{status}"
    );
    assert!(server.smtp_messages().is_empty());
}

/// A server whose SMTP side is reached through a relay that is off.
async fn smtp_down() -> (FakeServer, Relay, Env) {
    let server = mailbox().start().await;
    let relay = Relay::start(server.smtp_tls_addr()).await;
    let mut setup = Setup::of(&server);
    setup.smtp.0 = relay.port;
    let env = Env::new().await;
    env.add_account(&setup).await;
    (server, relay, env)
}

#[tokio::test(flavor = "multi_thread")]
async fn offline_the_message_is_sent_once_the_server_is_back() {
    let (server, relay, env) = smtp_down().await;
    let _daemon = env.daemon().await;
    env.ready().await;

    succeeded(&env.compose(&["--body", BODY]).await);
    eventually(20, "a failed try", || {
        env.outbox()
            .first()
            .is_some_and(|(_, _, attempts)| *attempts >= 1)
    })
    .await;
    assert_eq!(
        env.status().await["accounts"][0]["outbox"][0]["state"],
        "queued"
    );
    relay.up.store(true, Ordering::SeqCst);

    took(&server, 1).await;
    eventually(20, "the copy in Sent", || server.uids("Sent").len() == 1).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn offline_the_message_is_sent_once_the_server_is_back_after_a_daemon_restart() {
    let (server, relay, env) = smtp_down().await;
    let mut daemon = env.daemon().await;
    env.ready().await;
    let output = env.compose(&["--json", "--body", BODY]).await;
    succeeded(&output);
    let queued: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let built = env.outbox_raw(queued["outbox"].as_i64().unwrap());
    daemon.kill().await;

    relay.up.store(true, Ordering::SeqCst);
    let _restarted = env.daemon().await;

    took(&server, 1).await;
    // What arrives is what was built before the restart.
    assert_eq!(server.smtp_messages()[0].data, built);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_4xx_before_data_is_retried() {
    for command in ["MAIL", "RCPT", "DATA"] {
        let server = mailbox()
            .smtp_answer(command, "451 4.3.0 try again later")
            .start()
            .await;
        let (env, _daemon) = started(&Setup::of(&server)).await;

        succeeded(&env.compose(&["--body", BODY]).await);
        eventually(20, "a refused try", || {
            env.outbox()
                .first()
                .is_some_and(|(_, _, attempts)| *attempts >= 1)
        })
        .await;
        assert!(server.smtp_messages().is_empty(), "{command}");
        server.heal();

        took(&server, 1).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_5xx_fails_is_reported_and_not_retried() {
    let server = mailbox()
        .smtp_answer("RCPT", "550 5.1.1 no such user")
        .start()
        .await;
    let (env, _daemon) = started(&Setup::of(&server)).await;
    let mut watcher = env.watch().await;

    let output = env.compose(&["--json", "--body", BODY]).await;
    succeeded(&output);
    let queued: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    let failed = watcher.next("send-failed").await;
    assert_eq!(failed["outbox"], queued["outbox"]);
    assert_eq!(failed["message_id"], queued["message_id"]);
    assert_eq!(failed["code"], 550);
    assert_eq!(failed["step"], "RCPT TO");
    let status = env.status().await;
    let outbox = &status["accounts"][0]["outbox"];
    assert_eq!(outbox[0]["state"], "failed", "{status}");
    assert_eq!(outbox[0]["attempts"], 0, "{status}");
    let text = env.run(&["status"]).await;
    assert!(stdout(&text).contains("1 failed"), "{}", stdout(&text));

    // The next message goes; the failed one is not tried again.
    server.heal();
    succeeded(&env.compose(&["--body", BODY]).await);
    watcher.next("sent").await;
    took(&server, 1).await;
    assert_eq!(env.outbox()[0].1, "failed");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_connection_cut_after_the_final_dot_is_uncertain_and_not_resent() {
    let server = mailbox().start().await;
    server.smtp_cut_after_data();
    let (env, mut daemon) = started(&Setup::of(&server)).await;
    let mut watcher = env.watch().await;

    succeeded(&env.compose(&["--body", BODY]).await);

    let uncertain = watcher.next("send-uncertain").await;
    assert!(
        uncertain["message"]
            .as_str()
            .unwrap()
            .contains("not sent again"),
        "{uncertain}"
    );
    server.heal();
    assert_eq!(
        env.status().await["accounts"][0]["outbox"][0]["state"],
        "uncertain"
    );
    // Neither a new message nor a new daemon sends it again.
    succeeded(&env.compose(&["--body", BODY]).await);
    watcher.next("sent").await;
    daemon.kill().await;
    let _restarted = env.daemon().await;
    took(&server, 2).await;
    assert_eq!(env.outbox()[0].1, "uncertain");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_a_killed_daemon_left_sending_is_uncertain_and_not_resent() {
    let (server, relay, env) = smtp_down().await;
    let mut daemon = env.daemon().await;
    env.ready().await;
    succeeded(&env.compose(&["--body", BODY]).await);
    daemon.kill().await;
    // As a daemon killed between the mark and the server's answer leaves it.
    Connection::open(env.copy_path())
        .unwrap()
        .execute("UPDATE outbox SET state = 'sending'", [])
        .unwrap();

    relay.up.store(true, Ordering::SeqCst);
    let _restarted = env.daemon().await;

    eventually(20, "uncertain", || {
        env.outbox()
            .first()
            .is_some_and(|(_, state, _)| state == "uncertain")
    })
    .await;
    took(&server, 0).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_a_killed_daemon_left_claimed_is_queued_again_and_sent() {
    let (server, relay, env) = smtp_down().await;
    let mut daemon = env.daemon().await;
    env.ready().await;
    succeeded(&env.compose(&["--body", BODY]).await);
    daemon.kill().await;
    // As a daemon killed before the message's end of data leaves it.
    Connection::open(env.copy_path())
        .unwrap()
        .execute("UPDATE outbox SET state = 'claimed'", [])
        .unwrap();

    relay.up.store(true, Ordering::SeqCst);
    let _restarted = env.daemon().await;

    took(&server, 1).await;
}

/// How long the server takes to answer `MAIL FROM` in the tests where a worker is held there.
const SLOW_MAIL: Duration = Duration::from_secs(3);

/// A daemon whose worker is held at the `MAIL FROM` of the first message, then the account
/// removed from the configuration, and the daemon made to read it again: that worker's account
/// is gone.
async fn removed_while_sending() -> (FakeServer, Env, Daemon) {
    let server = mailbox().start().await;
    let (env, daemon) = started(&Setup::of(&server)).await;
    server.smtp_delay("MAIL", SLOW_MAIL);
    succeeded(&env.compose(&["--body", "first"]).await);
    eventually(10, "the first message at MAIL FROM", || {
        server.smtp_delayed() == 1
    })
    .await;
    assert!(
        AccountStore::new(env.path("config").join("missive"))
            .remove("alice")
            .unwrap()
    );
    // Any request reads the configuration again.
    env.run(&["sync"]).await;
    (server, env, daemon)
}

/// Waits until the server took `count` messages, then as long as a worker held at `MAIL FROM`
/// may take to send another, and returns the bodies it took.
async fn bodies_taken(server: &FakeServer, count: usize) -> Vec<String> {
    eventually(30, "the messages sent", || {
        server.smtp_messages().len() >= count
    })
    .await;
    tokio::time::sleep(SLOW_MAIL + Duration::from_secs(2)).await;
    let mut bodies: Vec<String> = server
        .smtp_messages()
        .iter()
        .map(|m| {
            let data = String::from_utf8_lossy(&m.data).into_owned();
            data.split("\r\n\r\n")
                .nth(1)
                .unwrap_or_default()
                .trim()
                .to_owned()
        })
        .collect();
    bodies.sort();
    bodies
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_being_sent_when_its_account_is_removed_and_added_again_is_delivered_once() {
    let (server, env, _daemon) = removed_while_sending().await;

    // Added again: a new worker, on the same account number, while the old one still sends.
    env.add_account(&Setup::of(&server)).await;
    succeeded(&env.compose(&["--body", "second"]).await);

    assert_eq!(bodies_taken(&server, 2).await, ["first", "second"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_worker_of_a_removed_account_takes_no_other_message() {
    let (server, env, _daemon) = removed_while_sending().await;
    // A message of the account written meanwhile, as the old worker would find it.
    Connection::open(env.copy_path())
        .unwrap()
        .execute(
            "INSERT INTO outbox (account, sender, recipients, message_id, raw, sent_copy,
                                 queued_at)
             SELECT account, sender, recipients, 'second@example.org',
                    CAST(replace(CAST(raw AS TEXT), 'first', 'second') AS BLOB), sent_copy,
                    queued_at FROM outbox",
            [],
        )
        .unwrap();

    assert_eq!(bodies_taken(&server, 1).await, ["first"]);
    assert_eq!(env.outbox().last().unwrap().1, "queued");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_sent_copy_is_not_appended_when_the_server_saves_sent_mail() {
    let server = mailbox().start().await;
    let mut setup = Setup::of(&server);
    setup.server_saves_sent = true;
    let (env, _daemon) = started(&setup).await;
    let mut watcher = env.watch().await;

    succeeded(&env.compose(&["--body", BODY]).await);

    assert_eq!(watcher.next("sent").await["sent_copy"], "server");
    took(&server, 1).await;
    assert!(env.outbox().is_empty());
    assert!(server.uids("Sent").is_empty());
    assert!(
        !server.imap_log().iter().any(|c| c.starts_with("APPEND")),
        "{:?}",
        server.imap_log()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_sent_folder_the_event_says_so_and_no_folder_is_created() {
    let server = FakeServer::builder().user(USER, PASSWORD).start().await;
    let (env, _daemon) = started(&Setup::of(&server)).await;
    let mut watcher = env.watch().await;

    succeeded(&env.compose(&["--body", BODY]).await);

    assert_eq!(watcher.next("sent").await["sent_copy"], "none");
    took(&server, 1).await;
    assert!(env.outbox().is_empty());
    assert!(
        !server
            .imap_log()
            .iter()
            .any(|c| c.starts_with("APPEND") || c.starts_with("CREATE")),
        "{:?}",
        server.imap_log()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn what_cannot_be_sent_is_refused_before_the_daemon() {
    let server = mailbox().start().await;
    let env = Env::new().await;
    env.add_account(&Setup::of(&server)).await;

    let invalid = env
        .run(&["compose", "--send", "--to", "not an address"])
        .await;
    failed(&invalid);
    let nobody = env.run(&["compose", "--send", "--subject", "x"]).await;
    failed(&nobody);
    assert!(stderr(&nobody).contains("recipient"), "{}", stderr(&nobody));
    let missing = env
        .run(&[
            "compose",
            "--send",
            "--to",
            RECIPIENT,
            "--attach",
            "/nonexistent/file",
        ])
        .await;
    failed(&missing);

    // Nothing reached a daemon: none was started.
    assert!(running_daemon(&env.pid_file()).is_none());
    assert!(env.outbox().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn no_password_body_subject_or_address_appears_in_the_logs_of_sending() {
    let server = mailbox()
        .smtp_answer("AUTH", "535 5.7.8 authentication credentials invalid")
        .start()
        .await;
    let (env, _daemon) = started(&Setup::of(&server)).await;
    let mut watcher = env.watch().await;
    let to = format!("{NAME} <{RECIPIENT}>");
    let body = env.file("body.txt", BODY.as_bytes());
    let attachment = env.file("a.txt", BODY.as_bytes());
    let args = [
        "compose",
        "--send",
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
        "--attach",
        &attachment,
    ];

    let mut outputs = vec![env.run(&args).await];
    eventually(20, "a refused login", || {
        env.outbox()
            .first()
            .is_some_and(|(_, _, attempts)| *attempts >= 1)
    })
    .await;
    server.heal();
    server.smtp_answer("RCPT", "451 4.3.0 try again later");
    eventually(20, "a refused try", || {
        env.outbox()
            .first()
            .is_some_and(|(_, _, attempts)| *attempts >= 2)
    })
    .await;
    server.heal();
    watcher.next("sent").await;
    server.smtp_answer("RCPT", "550 5.1.1 no such user");
    outputs.push(env.run(&args).await);
    watcher.next("send-failed").await;
    server.heal();
    server.smtp_cut_after_data();
    outputs.push(env.run(&args).await);
    watcher.next("send-uncertain").await;
    outputs.push(env.run(&["status"]).await);
    outputs.push(env.run(&["status", "--json"]).await);

    let logs = env.logs();
    assert!(logs.contains("outbox"), "{logs}");
    assert!(logs.contains("SMTP login failed (auth-refused)"), "{logs}");
    for private in private() {
        assert!(!logs.contains(&private), "the log shows {private}:\n{logs}");
        for output in &outputs {
            assert!(!stdout(output).contains(&private), "{}", stdout(output));
            assert!(!stderr(output).contains(&private), "{}", stderr(output));
        }
    }
}

/// `missive outbox --json`, read.
async fn listed(env: &Env) -> serde_json::Value {
    let output = env.run(&["outbox", "--json"]).await;
    succeeded(&output);
    serde_json::from_slice(&output.stdout).unwrap()
}

/// The bodies the server took, in order.
fn bodies(server: &FakeServer) -> Vec<String> {
    server
        .smtp_messages()
        .iter()
        .map(|m| {
            let data = String::from_utf8_lossy(&m.data).into_owned();
            data.split("\r\n\r\n")
                .nth(1)
                .unwrap_or_default()
                .trim()
                .to_owned()
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_outbox_lists_a_failed_and_an_uncertain_message() {
    let server = mailbox()
        .smtp_answer("RCPT", "550 5.1.1 no such user")
        .start()
        .await;
    let (env, _daemon) = started(&Setup::of(&server)).await;
    let mut watcher = env.watch().await;
    succeeded(&env.compose(&["--body", BODY]).await);
    watcher.next("send-failed").await;
    server.heal();
    server.smtp_cut_after_data();
    succeeded(&env.compose(&["--body", BODY]).await);
    watcher.next("send-uncertain").await;

    let listed = listed(&env).await;
    let messages = listed.as_array().unwrap();
    assert_eq!(messages.len(), 2, "{listed}");
    assert_eq!(messages[0]["account"], "alice");
    assert_eq!(messages[0]["state"], "failed");
    assert!(messages[0]["outbox"].is_i64(), "{listed}");
    assert!(messages[0]["message_id"].is_string(), "{listed}");
    assert!(
        messages[0]["last_error"].as_str().unwrap().contains("550"),
        "{listed}"
    );
    assert_eq!(messages[1]["state"], "uncertain");
    let text = env.run(&["outbox"]).await;
    succeeded(&text);
    let text = stdout(&text);
    assert!(
        text.contains("failed") && text.contains("uncertain"),
        "{text}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_message_is_resent_and_arrives_once() {
    let server = mailbox()
        .smtp_answer("RCPT", "550 5.1.1 no such user")
        .start()
        .await;
    let (env, _daemon) = started(&Setup::of(&server)).await;
    let mut watcher = env.watch().await;
    let output = env.compose(&["--json", "--body", BODY]).await;
    let queued: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    watcher.next("send-failed").await;
    server.heal();

    let id = queued["outbox"].to_string();
    succeeded(&env.run(&["outbox", "resend", &id]).await);

    let sent = watcher.next("sent").await;
    assert_eq!(sent["outbox"], queued["outbox"]);
    took(&server, 1).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_uncertain_message_is_resent_only_with_yes() {
    let server = mailbox().start().await;
    server.smtp_cut_after_data();
    let (env, _daemon) = started(&Setup::of(&server)).await;
    let mut watcher = env.watch().await;
    let output = env.compose(&["--json", "--body", BODY]).await;
    let queued: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    watcher.next("send-uncertain").await;
    server.heal();
    let id = queued["outbox"].to_string();

    let refused = env.run(&["outbox", "resend", &id]).await;
    failed(&refused);
    assert!(stderr(&refused).contains("--yes"), "{}", stderr(&refused));
    took(&server, 1).await;
    assert_eq!(env.outbox()[0].1, "uncertain");

    succeeded(&env.run(&["outbox", "resend", &id, "--yes"]).await);
    watcher.next("sent").await;
    took(&server, 2).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_message_is_discarded_and_never_sent() {
    let server = mailbox()
        .smtp_answer("RCPT", "550 5.1.1 no such user")
        .start()
        .await;
    let (env, _daemon) = started(&Setup::of(&server)).await;
    let mut watcher = env.watch().await;
    let output = env.compose(&["--json", "--body", BODY]).await;
    let queued: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    watcher.next("send-failed").await;
    server.heal();

    let id = queued["outbox"].to_string();
    succeeded(&env.run(&["outbox", "discard", &id]).await);

    assert!(env.outbox().is_empty(), "{:?}", env.outbox());
    assert_eq!(listed(&env).await, serde_json::json!([]));
    // A new message goes; the discarded one is gone for good.
    succeeded(&env.compose(&["--body", "after"]).await);
    watcher.next("sent").await;
    took(&server, 1).await;
    assert_eq!(bodies(&server), ["after"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn only_a_failed_or_uncertain_message_can_be_resent_or_discarded() {
    let (_server, _relay, env) = smtp_down().await;
    let _daemon = env.daemon().await;
    env.ready().await;
    let output = env.compose(&["--json", "--body", BODY]).await;
    let queued: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let id = queued["outbox"].to_string();

    for action in [&["resend", &id, "--yes"][..], &["discard", &id]] {
        let refused = env.run(&[&["outbox"][..], action].concat()).await;
        failed(&refused);
        // Queued, or claimed by a try that cannot reach the server.
        assert!(
            stderr(&refused).contains("only a failed or uncertain message"),
            "{}",
            stderr(&refused)
        );
        let unknown = env.run(&["outbox", action[0], "999"]).await;
        failed(&unknown);
        assert!(stderr(&unknown).contains("999"), "{}", stderr(&unknown));
    }
    assert!(
        matches!(env.outbox()[0].1.as_str(), "queued" | "claimed"),
        "{:?}",
        env.outbox()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_waiting_for_a_retry_does_not_hold_back_the_others() {
    let server = mailbox()
        .smtp_answer("RCPT TO:<slow@example.com>", "451 4.3.0 try again later")
        .start()
        .await;
    let (env, _daemon) = started(&Setup::of(&server)).await;
    let mut watcher = env.watch().await;
    succeeded(
        &env.compose(&["--cc", "slow@example.com", "--body", "first"])
            .await,
    );
    eventually(20, "a refused try", || {
        env.outbox()
            .first()
            .is_some_and(|(_, _, attempts)| *attempts >= 1)
    })
    .await;

    succeeded(&env.compose(&["--body", "second"]).await);

    watcher.next("sent").await;
    assert_eq!(bodies(&server), ["second"]);
    assert_eq!(env.outbox()[0].1, "queued");
    server.heal();
    eventually(30, "the first message sent", || {
        server.smtp_messages().len() == 2
    })
    .await;
    assert_eq!(bodies(&server), ["second", "first"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_killed_during_the_sent_append_does_not_append_it_twice() {
    let server = mailbox().start().await;
    let (env, mut daemon) = started(&Setup::of(&server)).await;
    server.stall_after_append();
    succeeded(&env.compose(&["--body", BODY]).await);
    eventually(20, "the copy taken by the server", || {
        server.uids("Sent").len() == 1
    })
    .await;
    daemon.kill().await;
    server.heal();

    let _restarted = env.daemon().await;

    eventually(20, "the outbox empty", || env.outbox().is_empty()).await;
    assert_eq!(server.uids("Sent").len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sent_copy_refused_five_times_is_given_up_with_an_event() {
    let server = mailbox().start().await;
    let (env, _daemon) = started(&Setup::of(&server)).await;
    let mut watcher = env.watch().await;
    server.refuse("APPEND");
    let output = env.compose(&["--json", "--body", BODY]).await;
    let queued: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    watcher.next("sent").await;

    // Each pass tries the copy again.
    for _ in 0..10 {
        if env.outbox().is_empty() {
            break;
        }
        env.run(&["sync"]).await;
    }

    let given_up = watcher.next("sent-copy-failed").await;
    assert_eq!(given_up["account"], "alice");
    assert_eq!(given_up["outbox"], queued["outbox"]);
    assert_eq!(given_up["message_id"], queued["message_id"]);
    assert!(
        given_up["message"].as_str().unwrap().contains("too many"),
        "{given_up}"
    );
    assert!(env.outbox().is_empty());
    assert!(server.uids("Sent").is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_message_does_not_cut_short_the_wait_after_a_refused_login() {
    let server = mailbox()
        .smtp_answer("AUTH", "535 5.7.8 authentication failed")
        .start()
        .await;
    let (env, _daemon) = started(&Setup::of(&server)).await;
    succeeded(&env.compose(&["--body", "first"]).await);
    // The second refusal starts a wait of 2 seconds.
    eventually(20, "two refused logins", || {
        env.outbox()
            .first()
            .is_some_and(|(_, _, attempts)| *attempts >= 2)
    })
    .await;

    succeeded(&env.compose(&["--body", "second"]).await);

    tokio::time::sleep(Duration::from_millis(300)).await;
    let outbox = env.outbox();
    assert_eq!(outbox.len(), 2, "{outbox:?}");
    assert_eq!(
        outbox[1].2, 0,
        "the new message logged in again: {outbox:?}"
    );
    // Waiting is not losing: both go once the server takes the login.
    server.heal();
    took(&server, 2).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_password_fixed_during_the_wait_after_a_refused_login_ends_the_wait() {
    let server = mailbox().start().await;
    let env = Env::new().await;
    env.add_account(&Setup::of(&server)).await;
    env.set_password("a-wrong-password").await;
    let _daemon = env.daemon().await;
    succeeded(&env.compose(&["--body", BODY]).await);
    // The third refusal starts a wait of 4 seconds.
    eventually(20, "three refused logins", || {
        env.outbox()
            .first()
            .is_some_and(|(_, _, attempts)| *attempts >= 3)
    })
    .await;

    // The user fixes the password, and tells the daemon what it already listens to.
    let fixed = Instant::now();
    env.set_password(PASSWORD).await;
    succeeded(&env.run(&["sync"]).await);

    eventually(3, "the message sent before the wait ended", || {
        server.smtp_messages().len() == 1
    })
    .await;
    assert!(fixed.elapsed() < Duration::from_secs(3));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_password_that_comes_during_the_wait_for_one_ends_the_wait() {
    let server = mailbox().start().await;
    let env = Env::new().await;
    env.add_account(&Setup::of(&server)).await;
    env.remove_password().await;
    let _daemon = env.daemon().await;
    succeeded(&env.compose(&["--body", BODY]).await);
    // The third wait is 4 seconds.
    eventually(20, "three waits for a password", || {
        env.logs().matches("no password in the keyring").count() >= 3
    })
    .await;

    let fixed = Instant::now();
    env.set_password(PASSWORD).await;
    env.run(&["sync"]).await;

    eventually(3, "the message sent before the wait ended", || {
        server.smtp_messages().len() == 1
    })
    .await;
    assert!(fixed.elapsed() < Duration::from_secs(3));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sync_with_the_same_wrong_password_does_not_end_the_wait_after_a_refused_login() {
    let server = mailbox().start().await;
    let env = Env::new().await;
    env.add_account(&Setup::of(&server)).await;
    env.set_password("a-wrong-password").await;
    let _daemon = env.daemon().await;
    succeeded(&env.compose(&["--body", BODY]).await);
    eventually(20, "two refused logins", || {
        env.outbox()
            .first()
            .is_some_and(|(_, _, attempts)| *attempts >= 2)
    })
    .await;
    let tries = env.outbox()[0].2;

    // The account stays `auth-refused`: the sync fails.
    env.run(&["sync"]).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Asking again with the password that was refused would only be refused again.
    assert_eq!(env.outbox()[0].2, tries, "{:?}", env.outbox());
}

/// Holds the answer to the end of data for 2 seconds, then, once the server has it in hand, makes
/// every change of the outbox fail, as a busy or a broken disk would. Returns the connection that
/// lifts the failure (`lift`), and the time the server answers at.
async fn blocked_at_the_answer(server: &FakeServer, env: &Env) -> (Connection, Instant) {
    server.smtp_delay(".", Duration::from_secs(2));
    succeeded(&env.compose(&["--body", BODY]).await);
    eventually(20, "the end of data held", || server.smtp_delayed() == 1).await;
    let answers_at = Instant::now() + Duration::from_secs(2);
    let copy = Connection::open(env.copy_path()).unwrap();
    copy.execute_batch(
        "CREATE TRIGGER no_update BEFORE UPDATE ON outbox
           BEGIN SELECT RAISE(ABORT, 'the copy cannot be written'); END;
         CREATE TRIGGER no_delete BEFORE DELETE ON outbox
           BEGIN SELECT RAISE(ABORT, 'the copy cannot be written'); END;",
    )
    .unwrap();
    (copy, answers_at)
}

fn lift(copy: &Connection) {
    copy.execute_batch("DROP TRIGGER no_update; DROP TRIGGER no_delete;")
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_stopped_while_a_sent_message_is_recorded_finishes_recording_it() {
    let server = mailbox().start().await;
    let (env, mut daemon) = started(&Setup::of(&server)).await;
    let (copy, answers_at) = blocked_at_the_answer(&server, &env).await;
    // The server answered, and the first try of the record failed: it waits for the next.
    tokio::time::sleep_until((answers_at + Duration::from_millis(400)).into()).await;
    let stopping = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1000)).await;
        lift(&copy);
    });

    daemon.stop(15).await;
    stopping.await.unwrap();

    assert_eq!(server.smtp_messages().len(), 1);
    let outbox = env.outbox();
    assert_eq!(
        outbox.iter().map(|m| m.1.as_str()).collect::<Vec<_>>(),
        ["sent"],
        "{outbox:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_stopped_during_the_record_gives_it_all_its_tries() {
    let server = mailbox().start().await;
    let (env, mut daemon) = started(&Setup::of(&server)).await;
    let (copy, answers_at) = blocked_at_the_answer(&server, &env).await;
    // The record tries at 0, 1, 2, 3 and 4 seconds after the answer; the daemon stops just after
    // the first, and the copy is writable again between the fourth and the fifth.
    tokio::time::sleep_until((answers_at + Duration::from_millis(300)).into()).await;
    let stopping = tokio::spawn(async move {
        tokio::time::sleep_until((answers_at + Duration::from_millis(3500)).into()).await;
        lift(&copy);
    });

    daemon.stop(15).await;
    stopping.await.unwrap();

    assert_eq!(server.smtp_messages().len(), 1);
    let outbox = env.outbox();
    assert_eq!(
        outbox.iter().map(|m| m.1.as_str()).collect::<Vec<_>>(),
        ["sent"],
        "{outbox:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_stopped_while_the_record_cannot_be_written_still_stops() {
    let server = mailbox().start().await;
    let (env, mut daemon) = started(&Setup::of(&server)).await;
    let (_copy, answers_at) = blocked_at_the_answer(&server, &env).await;
    tokio::time::sleep_until((answers_at + Duration::from_millis(400)).into()).await;

    let started = Instant::now();
    daemon.stop(15).await;

    assert!(
        started.elapsed() < Duration::from_secs(9),
        "{:?}",
        started.elapsed()
    );
    // Not recorded: the next daemon will say the message is uncertain.
    assert_eq!(env.outbox()[0].1, "sending");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_search_of_sent_is_not_one_of_the_tries_of_the_copy() {
    let server = mailbox().start().await;
    let (env, _daemon) = started(&Setup::of(&server)).await;
    let mut watcher = env.watch().await;
    // The first try appends the copy, and the connection drops before the answer.
    server.cut_after_append();
    succeeded(&env.compose(&["--body", BODY]).await);
    watcher.next("sent").await;
    eventually(20, "the copy taken by the server", || {
        server.uids("Sent").len() == 1
    })
    .await;
    let tries = || -> Option<i64> {
        Connection::open_with_flags(env.copy_path(), OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap()
            .query_row("SELECT copy_attempts FROM outbox", [], |r| r.get(0))
            .ok()
    };
    eventually(20, "the first try counted", || tries() == Some(1)).await;
    // From now on the search of Sent fails, at more passes than the copy has tries but fewer than
    // it has failed searches.
    server.heal();
    server.cut("UID SEARCH", 0);
    for _ in 0..4 {
        env.run(&["sync"]).await;
    }

    assert_eq!(
        tries(),
        Some(1),
        "the search was counted, or the copy given up"
    );
    // The search works again: the copy is found in Sent, not appended a second time.
    server.heal();
    for _ in 0..5 {
        if env.outbox().is_empty() {
            break;
        }
        env.run(&["sync"]).await;
    }
    assert!(env.outbox().is_empty());
    assert_eq!(server.uids("Sent").len(), 1);
}

/// A message sent whose copy was appended once, the answer lost: the next tries search Sent first.
async fn appended_without_an_answer(server: &FakeServer, env: &Env) -> Watcher {
    let mut watcher = env.watch().await;
    server.cut_after_append();
    succeeded(&env.compose(&["--body", BODY]).await);
    watcher.next("sent").await;
    eventually(20, "the copy taken by the server", || {
        server.uids("Sent").len() == 1
    })
    .await;
    eventually(20, "the first try counted", || {
        Connection::open_with_flags(env.copy_path(), OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap()
            .query_row("SELECT copy_attempts FROM outbox", [], |r| {
                r.get::<_, i64>(0)
            })
            .is_ok_and(|tries| tries == 1)
    })
    .await;
    server.heal();
    watcher
}

#[tokio::test(flavor = "multi_thread")]
async fn a_copy_whose_search_of_sent_keeps_failing_is_given_up_and_status_counts_the_failures() {
    let server = mailbox().start().await;
    let (env, _daemon) = started(&Setup::of(&server)).await;
    let mut watcher = appended_without_an_answer(&server, &env).await;
    server.cut("UID SEARCH", 0);

    env.run(&["sync"]).await;
    let status = env.status().await;
    let waiting = &status["accounts"][0]["outbox"][0];
    assert_eq!(waiting["state"], "sent", "{status}");
    assert!(
        waiting["copy_search_failures"].as_u64().unwrap() >= 1,
        "{status}"
    );
    let output = env.run(&["status"]).await;
    assert!(
        stdout(&output).contains("message 1 sent: its copy for Sent is not written yet"),
        "{}",
        stdout(&output)
    );

    // The bound is the one of the appends: five.
    for _ in 0..10 {
        if env.outbox().is_empty() {
            break;
        }
        env.run(&["sync"]).await;
    }
    assert!(env.outbox().is_empty(), "{:?}", env.outbox());
    let given_up = watcher.next("sent-copy-failed").await;
    assert!(
        given_up["message"]
            .as_str()
            .unwrap()
            .contains("looking for it in Sent failed too many times"),
        "{given_up}"
    );
    // Given up, not appended again: the copy that is there stays the only one.
    assert_eq!(server.uids("Sent").len(), 1);
}

/// The server answers `NO` to `command` while the copy is looked for in Sent: that is not an
/// answer, so the copy that went in once is not appended again.
async fn a_refused_look_in_sent_does_not_append_the_copy_again(command: &str) {
    let server = mailbox().start().await;
    let (env, _daemon) = started(&Setup::of(&server)).await;
    appended_without_an_answer(&server, &env).await;
    server.refuse(command);

    env.run(&["sync"]).await;
    env.run(&["sync"]).await;

    assert_eq!(server.uids("Sent").len(), 1, "appended a second time");
    assert_eq!(env.outbox().len(), 1, "the copy was dropped");
    // The server answers again: the copy is found in Sent, and nothing is appended.
    server.heal();
    for _ in 0..5 {
        if env.outbox().is_empty() {
            break;
        }
        env.run(&["sync"]).await;
    }
    assert!(env.outbox().is_empty());
    assert_eq!(server.uids("Sent").len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_search_of_sent_the_server_refuses_does_not_append_the_copy_again() {
    a_refused_look_in_sent_does_not_append_the_copy_again("UID SEARCH").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_examine_of_sent_the_server_refuses_does_not_append_the_copy_again() {
    a_refused_look_in_sent_does_not_append_the_copy_again("EXAMINE").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn status_says_that_a_message_stays_claimed_for_good() {
    let (_server, _relay, env) = smtp_down().await;
    let mut daemon = env.daemon().await;
    env.ready().await;
    succeeded(&env.compose(&["--body", "stuck"]).await);
    succeeded(&env.compose(&["--body", "taken"]).await);
    daemon.kill().await;
    // What a worker that could not put its message back in the queue leaves: claimed, since long.
    // The other one was claimed just now: a try, as far as `status` can tell.
    let long_ago = missive_core::outbox::now() - 2 * 60 * 60 * 1000;
    let copy = Connection::open(env.copy_path()).unwrap();
    copy.execute(
        "UPDATE outbox SET state = 'claimed', next_attempt_at = ?1 WHERE id = 1",
        [long_ago],
    )
    .unwrap();
    copy.execute(
        "UPDATE outbox SET state = 'claimed', next_attempt_at = ?1 WHERE id = 2",
        [missive_core::outbox::now()],
    )
    .unwrap();

    let status = env.status().await;
    let outbox = &status["accounts"][0]["outbox"];
    assert_eq!(outbox[0]["state"], "claimed", "{status}");
    assert_eq!(outbox[0]["stuck"], true, "{status}");
    assert_eq!(outbox[1]["stuck"], false, "{status}");
    let output = env.run(&["status"]).await;
    succeeded(&output);
    let text = stdout(&output);
    assert!(text.contains("message 1 claimed: stuck"), "{text}");
    assert!(!text.contains("message 2 claimed"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_claim_without_a_claim_time_is_not_called_stuck() {
    let (_server, _relay, env) = smtp_down().await;
    let mut daemon = env.daemon().await;
    env.ready().await;
    succeeded(&env.compose(&["--body", "old claim"]).await);
    daemon.kill().await;
    // What a daemon older than the claim time leaves: claimed, with the time its message was
    // queued for (0: at once), which says nothing of when the claim was made.
    let copy = Connection::open(env.copy_path()).unwrap();
    copy.execute(
        "UPDATE outbox SET state = 'claimed', next_attempt_at = 0 WHERE id = 1",
        [],
    )
    .unwrap();

    let status = env.status().await;
    let outbox = &status["accounts"][0]["outbox"];
    assert_eq!(outbox[0]["state"], "claimed", "{status}");
    assert_eq!(outbox[0]["stuck"], false, "{status}");
    let output = env.run(&["status"]).await;
    succeeded(&output);
    assert!(!stdout(&output).contains("stuck"), "{}", stdout(&output));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stop_does_not_wait_for_a_keyring_read_during_the_wait_after_a_refused_login() {
    let server = mailbox()
        .smtp_answer("AUTH", "535 5.7.8 authentication failed")
        .start()
        .await;
    let env = Env::new().await;
    env.add_account(&Setup::of(&server)).await;
    let mut daemon = env.daemon().await;
    env.ready().await;
    succeeded(&env.compose(&["--body", BODY]).await);
    eventually(20, "two refused logins", || {
        env.outbox()
            .first()
            .is_some_and(|(_, _, attempts)| *attempts >= 2)
    })
    .await;
    // The keyring waits for a passphrase nobody types; a sync makes the worker look at it.
    env.keyring.stall_secret_reads();
    succeeded(&env.run(&["sync"]).await);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let stopping = Instant::now();
    daemon.stop(15).await;

    assert!(
        stopping.elapsed() < Duration::from_secs(3),
        "the stop waited {:?}",
        stopping.elapsed()
    );
    assert!(
        !env.logs().contains("did not stop in time"),
        "a worker was cut off"
    );
}
