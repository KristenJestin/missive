//! `missive watch` and the daemon's event stream (`docs/events.md`) against the fake IMAP server
//! and a fake Secret Service on a private bus. The binary runs with a cleared environment: its
//! configuration, data, state and runtime folders are temporary, and the only session bus it can
//! see is the test's own. Every daemon and watcher a test starts is killed when the test ends,
//! even when it panics.

use std::path::PathBuf;
use std::process::{ExitStatus, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use missive_core::account::{Account, AccountStore, Password, Security, Server};
use missive_core::fake::{FakeKeyring, FakeServer, Folder, Message, SpecialUse};
use missive_core::keyring::Keyring;
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::{Child, ChildStdout, Command};

const USER: &str = "alice@example.org";
const PASSWORD: &str = "Recognisable-Pa55word-4c1d";
const OTHER_USER: &str = "bob@example.com";

const SUBJECT: &str = "Quarterly-Plan-Subject-5e1f";
const SENDER: &str = "carol.sender@example.com";
const RECIPIENT: &str = "dave.recipient@example.net";
const BODY: &str = "Body-Text-That-Stays-Private";
const PRIVATE_FOLDER: &str = "Private-Folder-Name-77c2";

/// What no daemon log may ever show.
const PRIVATE: &[&str] = &[
    SUBJECT,
    SENDER,
    RECIPIENT,
    BODY,
    PRIVATE_FOLDER,
    USER,
    OTHER_USER,
    PASSWORD,
    "Carol Sender",
];

/// A message whose subject is `subject`.
fn message_with(n: u32, subject: &str) -> Message {
    Message::new(format!(
        "From: Carol Sender <{SENDER}>\r\n\
         To: Dave Recipient <{RECIPIENT}>\r\n\
         Subject: {subject}\r\n\
         Message-ID: <{n}@example.com>\r\n\
         Date: Sat, 17 Oct 2026 09:{:02}:00 +0200\r\n\
         \r\n\
         {BODY} {n}.\r\n",
        n % 60
    ))
}

fn message(n: u32) -> Message {
    message_with(n, &format!("{SUBJECT} {n}"))
}

/// A message with a subject of about 5 KB, so that a few hundred of them cannot sit in the
/// buffers of a socket.
fn big_message(n: u32) -> Message {
    let mut subject = format!("{SUBJECT} {n}");
    while subject.len() < 5000 {
        subject.push_str("\r\n padding words that fill the line up");
    }
    message_with(n, &subject)
}

/// INBOX with three messages (one seen, one flagged), a sent folder and a folder of the user's
/// own: for `alice`, 2 unread in the inbox, 0 in `sent` and 2 in a folder without a role.
fn mailbox() -> missive_core::fake::Builder {
    FakeServer::builder()
        .user(USER, PASSWORD)
        .user(OTHER_USER, "bobs-real-password")
        .folder(
            Folder::new("INBOX")
                .message(message(1).flag("\\Seen"))
                .message(message(2).flag("\\Flagged"))
                .message(message(3)),
        )
        .folder(
            Folder::new("Sent")
                .special_use(SpecialUse::Sent)
                .message(message(4).flag("\\Seen")),
        )
        .folder(
            Folder::new(PRIVATE_FOLDER)
                .message(message(5).uid(5))
                .message(message(6).uid(9)),
        )
}

struct Env {
    home: TempDir,
    keyring: FakeKeyring,
    count: AtomicUsize,
}

impl Env {
    async fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        for folder in ["config", "data", "state", "runtime"] {
            std::fs::create_dir(home.path().join(folder)).unwrap();
        }
        Env {
            home,
            keyring: FakeKeyring::start().await,
            count: AtomicUsize::new(0),
        }
    }

    fn path(&self, folder: &str) -> PathBuf {
        self.home.path().join(folder)
    }

    async fn add_account(&self, id: &str, server: &FakeServer, user: &str, password: Option<&str>) {
        let account = Account {
            name: "Alice Example".to_owned(),
            address: user.to_owned(),
            user: user.to_owned(),
            imap: Server {
                host: "127.0.0.1".to_owned(),
                port: server.imap_tls_addr().port(),
                security: Security::Tls,
            },
            smtp: Server {
                host: "127.0.0.1".to_owned(),
                port: server.smtp_tls_addr().port(),
                security: Security::Tls,
            },
            server_saves_sent: false,
        };
        AccountStore::new(self.path("config").join("missive"))
            .save(id, &account, Some(&server.certificate_der()))
            .unwrap();
        if let Some(password) = password {
            let connection = zbus::connection::Builder::address(self.keyring.address())
                .unwrap()
                .build()
                .await
                .unwrap();
            Keyring::open_on(connection)
                .await
                .unwrap()
                .store(id, &Password::new(password.to_owned()))
                .await
                .unwrap();
        }
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
            .stdin(Stdio::null())
            .kill_on_drop(true);
        command
    }

    /// Starts `missive daemon` in the foreground and waits until it answers on its socket.
    async fn daemon(&self, args: &[&str]) -> Daemon {
        let n = self.count.fetch_add(1, Ordering::Relaxed);
        let log = std::fs::File::create(self.home.path().join(format!("daemon-{n}.log"))).unwrap();
        let mut arguments = vec!["daemon"];
        arguments.extend_from_slice(args);
        let child = self
            .command(&arguments)
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .unwrap();
        let daemon = Daemon { child };
        let started = Instant::now();
        while UnixStream::connect(self.socket()).await.is_err() {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "the daemon did not answer; its log:\n{}",
                self.logs()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        daemon
    }

    /// `missive watch`, its standard output piped to the test.
    fn watch(&self) -> Watcher {
        let n = self.count.fetch_add(1, Ordering::Relaxed);
        let log = std::fs::File::create(self.home.path().join(format!("watch-{n}.err"))).unwrap();
        let mut child = self
            .command(&["watch"])
            .stdout(Stdio::piped())
            .stderr(log)
            .spawn()
            .unwrap();
        let lines = BufReader::new(child.stdout.take().unwrap()).lines();
        Watcher { child, lines }
    }

    async fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().await.unwrap()
    }

    /// `missive sync`, which must answer within 20 seconds.
    async fn sync(&self) -> Output {
        tokio::time::timeout(Duration::from_secs(20), self.run(&["sync"]))
            .await
            .unwrap_or_else(|_| panic!("sync did not answer; the log:\n{}", self.logs()))
    }

    fn socket(&self) -> PathBuf {
        self.path("runtime").join("missive/daemon.sock")
    }

    /// Every daemon's log.
    fn logs(&self) -> String {
        let mut files: Vec<PathBuf> = std::fs::read_dir(self.home.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|e| e == "log"))
            .collect();
        files.sort();
        files
            .into_iter()
            .map(|file| std::fs::read_to_string(file).unwrap_or_default())
            .collect()
    }

    /// The identifier an event gives a message of the inbox: `<account>/<folder>/<uidvalidity>/<uid>`.
    fn inbox_message_id(&self, account: &str, uid: u32) -> String {
        let copy = Connection::open_with_flags(
            self.path("data").join("missive/copy.sqlite"),
            OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let (folder, validity): (i64, i64) = copy
            .query_row(
                "SELECT folder.id, folder.uidvalidity FROM folder \
                 JOIN account ON account.key = folder.account \
                 WHERE account.id = ?1 AND folder.role = 'inbox'",
                [account],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        format!("{account}/{folder}/{validity}/{uid}")
    }

    /// A raw connection to the daemon's socket.
    async fn connect(&self) -> Raw {
        let stream = UnixStream::connect(self.socket()).await.unwrap();
        let (read, write) = stream.into_split();
        Raw {
            lines: BufReader::new(read).lines(),
            write,
        }
    }
}

struct Daemon {
    child: Child,
}

impl Daemon {
    async fn stop(&mut self, signal: &str) -> ExitStatus {
        let pid = self.child.id().expect("the daemon is running").to_string();
        assert!(
            std::process::Command::new("kill")
                .args([signal, &pid])
                .status()
                .unwrap()
                .success()
        );
        tokio::time::timeout(Duration::from_secs(10), self.child.wait())
            .await
            .expect("the daemon exits within 10 seconds")
            .unwrap()
    }
}

struct Watcher {
    child: Child,
    lines: tokio::io::Lines<BufReader<ChildStdout>>,
}

/// One JSON line, within `seconds`, or `None` when the stream ended.
async fn read_line<R: tokio::io::AsyncBufRead + Unpin>(
    lines: &mut tokio::io::Lines<R>,
    seconds: u64,
) -> Option<Value> {
    let line = tokio::time::timeout(Duration::from_secs(seconds), lines.next_line())
        .await
        .expect("a line within the time given")
        .unwrap()?;
    Some(serde_json::from_str(&line).unwrap_or_else(|_| panic!("not JSON: {line}")))
}

impl Watcher {
    async fn next(&mut self) -> Value {
        read_line(&mut self.lines, 20)
            .await
            .expect("the stream does not end")
    }

    /// The next event, which must be `name`.
    async fn expect(&mut self, name: &str) -> Value {
        let event = self.next().await;
        assert_eq!(event["event"], name, "{event}");
        assert_eq!(event["protocol"], 1, "{event}");
        event
    }

    /// Events up to and including the next one named `name`: those before, and it.
    async fn until(&mut self, name: &str) -> (Vec<Value>, Value) {
        let mut before = Vec::new();
        loop {
            let event = self.next().await;
            if event["event"] == name {
                return (before, event);
            }
            before.push(event);
        }
    }

    /// Nothing comes for `millis`.
    async fn quiet(&mut self, millis: u64) {
        if let Ok(line) =
            tokio::time::timeout(Duration::from_millis(millis), self.lines.next_line()).await
        {
            panic!("the stream was meant to be quiet, and gave {line:?}");
        }
    }

    /// The snapshot of the present a watcher starts with: one `state` per account, then `unread`.
    async fn snapshot(&mut self, accounts: usize) -> (Vec<Value>, Value) {
        let mut states = Vec::new();
        for _ in 0..accounts {
            states.push(self.expect("state").await);
        }
        (states, self.expect("unread").await)
    }

    async fn stopped(&mut self, seconds: u64) -> ExitStatus {
        tokio::time::timeout(Duration::from_secs(seconds), self.child.wait())
            .await
            .expect("the watcher exits in time")
            .unwrap()
    }

    fn signal(&self, signal: &str) {
        let pid = self.child.id().expect("the watcher runs").to_string();
        assert!(
            std::process::Command::new("kill")
                .args([signal, &pid])
                .status()
                .unwrap()
                .success()
        );
    }
}

struct Raw {
    lines: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    write: tokio::net::unix::OwnedWriteHalf,
}

impl Raw {
    async fn send(&mut self, request: &str) {
        self.write
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
    }

    async fn next(&mut self) -> Value {
        read_line(&mut self.lines, 20)
            .await
            .expect("the stream does not end")
    }
}

fn json(output: &Output) -> Value {
    assert!(output.status.success(), "{output:?}");
    serde_json::from_slice(&output.stdout).unwrap()
}

const SUBSCRIBE: &str = r#"{"protocol":1,"request":"subscribe"}"#;

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

fn idles(server: &FakeServer) -> usize {
    server.imap_log().iter().filter(|c| *c == "IDLE").count()
}

/// A daemon that synced `alice` once and idles in her inbox, with the periodic check an hour
/// away: only what the test does makes it work.
async fn idling(server: &FakeServer, env: &Env) -> Daemon {
    env.add_account("alice", server, USER, Some(PASSWORD)).await;
    let daemon = env.daemon(&["--check-interval", "3600"]).await;
    let sync = env.sync().await;
    assert!(sync.status.success());
    eventually(10, "IDLE", || idles(server) > 0).await;
    daemon
}

fn alice_unread(inbox: u64, total: u64) -> Value {
    json!({
        "protocol": 1,
        "event": "unread",
        "total": total,
        "accounts": [{
            "id": "alice",
            "total": total,
            "roles": { "inbox": inbox, "sent": 0, "other": 2 },
        }],
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_delivered_to_the_inbox_gives_one_new_message_event_and_one_unread_change() {
    let server = mailbox().start().await;
    let env = Env::new().await;
    let _daemon = idling(&server, &env).await;
    let mut watcher = env.watch();
    let (states, unread) = watcher.snapshot(1).await;
    assert_eq!(
        states[0],
        json!({ "protocol": 1, "event": "state", "account": "alice", "state": "ready" })
    );
    assert_eq!(unread, alice_unread(2, 4));

    let uid = server.deliver("INBOX", message(7));

    assert_eq!(
        watcher.expect("new-message").await,
        json!({
            "protocol": 1,
            "event": "new-message",
            "account": "alice",
            "folder": "inbox",
            "message": env.inbox_message_id("alice", uid),
            "sender": { "name": "Carol Sender", "address": SENDER },
            "subject": format!("{SUBJECT} 7"),
        })
    );
    assert_eq!(watcher.expect("unread").await, alice_unread(3, 5));
    // One of each: the delivery did not make the daemon say anything else.
    watcher.quiet(1500).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_removed_account_leaves_the_unread_event_when_the_daemon_reloads() {
    let server = mailbox().start().await;
    let env = Env::new().await;
    env.add_account("alice", &server, USER, Some(PASSWORD))
        .await;
    env.add_account("bob", &server, OTHER_USER, Some("bobs-real-password"))
        .await;
    let _daemon = env.daemon(&["--check-interval", "3600"]).await;
    assert!(env.sync().await.status.success());
    let mut watcher = env.watch();
    let (_, unread) = watcher.snapshot(2).await;
    let ids: Vec<&str> = unread["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["alice", "bob"]);

    AccountStore::new(env.path("config").join("missive"))
        .remove("alice")
        .unwrap();
    assert!(env.sync().await.status.success());

    let (_, unread) = watcher.until("unread").await;
    let ids: Vec<&str> = unread["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["bob"]);
    assert_eq!(unread["total"], unread["accounts"][0]["total"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sync_that_ends_in_an_error_after_a_batch_still_announces_what_the_batch_brought() {
    let server = mailbox().start().await;
    let env = Env::new().await;
    let _daemon = idling(&server, &env).await;
    let mut watcher = env.watch();
    watcher.snapshot(1).await;
    // The server drops the connection after the first whole message of the batch of the two that
    // follow (a line of the answer, the seven of the message, its closing parenthesis).
    server.cut("UID FETCH 4:5 (UID FLAGS", 9);

    server.deliver("INBOX", message(7));
    server.deliver("INBOX", message(8));

    // The message that came is in the copy, so it is told now: the next pass will not find it
    // above the highest UID any more.
    let (_, first) = watcher.until("new-message").await;
    assert_eq!(first["message"], env.inbox_message_id("alice", 4));
    assert_eq!(first["subject"], format!("{SUBJECT} 7"));
    let (_, unread) = watcher.until("unread").await;
    assert_eq!(unread, alice_unread(3, 5));
    // The pass that follows the reconnection brings the other one.
    let (_, second) = watcher.until("new-message").await;
    assert_eq!(second["message"], env.inbox_message_id("alice", 5));
    let (_, unread) = watcher.until("unread").await;
    assert_eq!(unread, alice_unread(4, 6));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pass_announces_at_most_twenty_new_messages_per_folder_and_the_counts_say_the_rest() {
    let server = mailbox().start().await;
    let env = Env::new().await;
    let _daemon = idling(&server, &env).await;
    let mut watcher = env.watch();
    watcher.snapshot(1).await;

    // All in the same pass: the daemon wakes up on the first and the sync finds the 25.
    for n in 10..35 {
        server.deliver("INBOX", message(n));
    }

    let (events, unread) = watcher.until("unread").await;
    let announced: Vec<&Value> = events
        .iter()
        .filter(|event| event["event"] == "new-message")
        .collect();
    assert_eq!(announced.len(), 20);
    // The first twenty, in the order they came; the rest is in the unread counts.
    assert_eq!(announced[0]["subject"], format!("{SUBJECT} 10"));
    assert_eq!(announced[19]["subject"], format!("{SUBJECT} 29"));
    assert_eq!(unread, alice_unread(27, 29));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_total_leaves_out_junk_and_trash_in_the_event_and_in_missive_unread() {
    let server = mailbox()
        .folder(
            Folder::new("Spam")
                .special_use(SpecialUse::Junk)
                .message(message(8))
                .message(message(9)),
        )
        .folder(
            Folder::new("Bin")
                .special_use(SpecialUse::Trash)
                .message(message(10)),
        )
        .start()
        .await;
    let env = Env::new().await;
    let _daemon = idling(&server, &env).await;
    let mut watcher = env.watch();

    let (_, event) = watcher.snapshot(1).await;
    let command = json(&env.run(&["unread", "--json"]).await);

    // 2 in the inbox and 2 in a folder of the user's own; the 3 of junk and trash are listed
    // per folder and left out of every total.
    assert_eq!(event["total"], 4);
    assert_eq!(event["accounts"][0]["total"], 4);
    assert_eq!(event["accounts"][0]["roles"]["junk"], 2);
    assert_eq!(event["accounts"][0]["roles"]["trash"], 1);
    assert_eq!(command["total"], 4);
    assert_eq!(command["accounts"][0]["unread"], 4);
    let listed = |role: &str| -> u64 {
        command["accounts"][0]["folders"]
            .as_array()
            .unwrap()
            .iter()
            .find(|folder| folder["role"] == role)
            .unwrap()["unread"]
            .as_u64()
            .unwrap()
    };
    assert_eq!((listed("junk"), listed("trash")), (2, 1));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_first_sync_gives_the_unread_counts_but_no_new_message_per_message() {
    let server = mailbox().start().await;
    let env = Env::new().await;
    // No account yet: the watcher starts from an empty copy.
    let _daemon = env.daemon(&["--check-interval", "3600"]).await;
    let mut watcher = env.watch();
    let (_, unread) = watcher.snapshot(0).await;
    assert_eq!(
        unread,
        json!({ "protocol": 1, "event": "unread", "total": 0, "accounts": [] })
    );

    env.add_account("alice", &server, USER, Some(PASSWORD))
        .await;
    assert!(env.sync().await.status.success());
    let (before, ended) = watcher.until("sync-ended").await;

    // Six messages came; none of them is announced one by one.
    let names: Vec<&str> = before
        .iter()
        .map(|e| e["event"].as_str().unwrap())
        .collect();
    assert!(!names.contains(&"new-message"), "{before:?}");
    let without_unread: Vec<&Value> = before.iter().filter(|e| e["event"] != "unread").collect();
    assert_eq!(
        without_unread,
        [
            &json!({ "protocol": 1, "event": "state", "account": "alice", "state": "syncing" }),
            &json!({ "protocol": 1, "event": "sync-started", "account": "alice" }),
            &json!({ "protocol": 1, "event": "state", "account": "alice", "state": "ready" }),
        ]
    );
    assert_eq!(
        ended,
        json!({
            "protocol": 1, "event": "sync-ended", "account": "alice",
            "folders": 3, "new": 6, "removed": 0, "flags": 0, "skipped": [],
        })
    );
    // The unread counts do come, as they change: the last one is the whole copy's.
    let unreads: Vec<&Value> = before.iter().filter(|e| e["event"] == "unread").collect();
    assert!(!unreads.is_empty());
    assert_eq!(*unreads.last().unwrap(), &alice_unread(2, 4));

    // The account has now completed a sync: what comes after is announced.
    eventually(10, "IDLE", || idles(&server) > 0).await;
    let uid = server.deliver("INBOX", message(7));
    let event = watcher.expect("new-message").await;
    assert_eq!(event["message"], env.inbox_message_id("alice", uid));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_folder_the_server_refuses_is_named_in_sync_ended_and_the_next_pass_clears_it() {
    let server = mailbox().start().await;
    let env = Env::new().await;
    let _daemon = idling(&server, &env).await;
    let mut watcher = env.watch();
    watcher.snapshot(1).await;

    server.refuse("EXAMINE \"Sent\"");
    assert!(env.sync().await.status.success());
    let (before, ended) = watcher.until("sync-ended").await;

    // `ready` all the same, and the pass says what it left out; the folders count is the synced.
    assert!(
        before.contains(
            &json!({ "protocol": 1, "event": "state", "account": "alice", "state": "ready" })
        ),
        "{before:?}"
    );
    assert_eq!(ended["folders"], 2);
    assert_eq!(
        ended["skipped"],
        json!([{ "folder": "sent", "kind": "refused" }])
    );

    server.heal();
    assert!(env.sync().await.status.success());
    let (_, ended) = watcher.until("sync-ended").await;
    assert_eq!(ended["folders"], 3);
    assert_eq!(ended["skipped"], json!([]));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_folder_rebuilt_by_a_new_uidvalidity_gives_no_new_message() {
    let server = mailbox().start().await;
    let env = Env::new().await;
    let _daemon = idling(&server, &env).await;
    let mut watcher = env.watch();
    watcher.snapshot(1).await;

    server.bump_uidvalidity(PRIVATE_FOLDER);
    assert!(env.sync().await.status.success());
    let (before, ended) = watcher.until("sync-ended").await;

    // The two messages are back under new UIDs, which is not mail arriving.
    assert!(
        before.iter().all(|e| e["event"] != "new-message"),
        "{before:?}"
    );
    assert_eq!(ended["new"], 2);
    assert!(before.iter().all(|e| e["event"] != "unread"), "{before:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_flag_change_on_the_server_gives_an_unread_change_and_no_new_message() {
    let server = mailbox().start().await;
    let env = Env::new().await;
    let _daemon = idling(&server, &env).await;
    let mut watcher = env.watch();
    watcher.snapshot(1).await;

    server.set_flags("INBOX", 3, &["\\Seen"]);
    assert!(env.sync().await.status.success());
    let (before, ended) = watcher.until("sync-ended").await;

    assert!(
        before.iter().all(|e| e["event"] != "new-message"),
        "{before:?}"
    );
    assert!(before.contains(&alice_unread(1, 3)), "{before:?}");
    assert_eq!(ended["flags"], 1);
    assert_eq!(ended["new"], 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_expunged_on_the_server_gives_an_unread_change() {
    let server = mailbox().start().await;
    let env = Env::new().await;
    let _daemon = idling(&server, &env).await;
    let mut watcher = env.watch();
    watcher.snapshot(1).await;

    server.expunge("INBOX", 3);
    assert!(env.sync().await.status.success());
    let (before, ended) = watcher.until("sync-ended").await;

    assert!(before.contains(&alice_unread(1, 3)), "{before:?}");
    assert_eq!(ended["removed"], 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_change_of_connection_state_is_an_event_with_the_state_names_of_the_protocol() {
    let server = mailbox().start().await;
    let env = Env::new().await;
    let _daemon = env.daemon(&["--check-interval", "3600"]).await;
    let mut watcher = env.watch();
    watcher.snapshot(0).await;

    env.add_account("alice", &server, USER, Some(PASSWORD))
        .await;
    env.add_account("bob", &server, OTHER_USER, Some("a-wrong-password"))
        .await;
    env.add_account("carol", &server, "carol@example.com", None)
        .await;
    let _ = env.sync().await;

    let mut states = std::collections::BTreeMap::new();
    while states.len() < 3 || states.values().any(|s: &String| s == "syncing") {
        let event = watcher.next().await;
        if event["event"] == "state" {
            states.insert(
                event["account"].as_str().unwrap().to_owned(),
                event["state"].as_str().unwrap().to_owned(),
            );
        }
    }
    assert_eq!(states["alice"], "ready");
    assert_eq!(states["bob"], "auth-refused");
    assert_eq!(states["carol"], "no-login");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unreachable_server_is_a_state_event() {
    // The server drops the session at the first folder it examines: no pass can end.
    let server = mailbox().cut("EXAMINE", 0).start().await;
    let env = Env::new().await;
    let _daemon = env.daemon(&["--check-interval", "3600"]).await;
    let mut watcher = env.watch();
    watcher.snapshot(0).await;

    env.add_account("alice", &server, USER, Some(PASSWORD))
        .await;
    assert!(!env.sync().await.status.success());

    let mut states = Vec::new();
    while states.last().is_none_or(|s: &String| s != "unreachable") {
        let event = watcher.next().await;
        if event["event"] == "state" {
            assert_eq!(event["account"], "alice");
            states.push(event["state"].as_str().unwrap().to_owned());
        }
    }
    assert_eq!(states, ["syncing", "unreachable"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stopped_daemon_gives_gone_and_a_restarted_one_resumes_the_stream() {
    let server = mailbox().start().await;
    let env = Env::new().await;
    let mut daemon = idling(&server, &env).await;
    let mut watcher = env.watch();
    watcher.snapshot(1).await;

    assert!(daemon.stop("-TERM").await.success());
    assert_eq!(
        watcher.expect("gone").await,
        json!({ "protocol": 1, "event": "gone" })
    );
    // Said once, not at every attempt to reach the daemon again.
    watcher.quiet(2500).await;

    let _daemon = env.daemon(&["--check-interval", "3600"]).await;
    // The stream resumes with the present, then goes on.
    watcher.expect("state").await;
    eventually(10, "IDLE", || idles(&server) > 1).await;
    let uid = server.deliver("INBOX", message(7));
    let (_, event) = watcher.until("new-message").await;
    assert_eq!(event["message"], env.inbox_message_id("alice", uid));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_watcher_without_a_daemon_says_gone_once_waits_and_never_starts_one() {
    let server = mailbox().start().await;
    let env = Env::new().await;
    env.add_account("alice", &server, USER, Some(PASSWORD))
        .await;
    let mut watcher = env.watch();

    watcher.expect("gone").await;
    watcher.quiet(2500).await;
    assert!(!env.socket().exists(), "the watcher started a daemon");
    assert!(!env.path("runtime").join("missive/daemon.pid").exists());
    assert!(!env.path("state").join("missive").exists());

    // A daemon appears: the stream starts.
    let _daemon = env.daemon(&["--check-interval", "3600"]).await;
    watcher.until("unread").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn two_watchers_receive_the_same_events() {
    let server = mailbox().start().await;
    let env = Env::new().await;
    let _daemon = idling(&server, &env).await;
    let mut first = env.watch();
    let mut second = env.watch();
    assert_eq!(first.snapshot(1).await, second.snapshot(1).await);

    let uid = server.deliver("INBOX", message(7));

    let mut seen = Vec::new();
    for watcher in [&mut first, &mut second] {
        let mut events = Vec::new();
        events.push(watcher.expect("new-message").await);
        events.push(watcher.expect("unread").await);
        seen.push(events);
    }
    assert_eq!(seen[0], seen[1]);
    assert_eq!(seen[0][0]["message"], env.inbox_message_id("alice", uid));
    // A whole pass too, event for event.
    server.set_flags("INBOX", 3, &["\\Seen"]);
    assert!(env.sync().await.status.success());
    let mut passes = Vec::new();
    for watcher in [&mut first, &mut second] {
        passes.push(watcher.until("sync-ended").await);
    }
    assert_eq!(passes[0], passes[1]);
    assert!(passes[0].0.len() >= 3, "{:?}", passes[0]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_watcher_that_does_not_read_is_told_it_lagged_and_slows_neither_the_daemon_nor_the_others()
 {
    let server = mailbox().start().await;
    let env = Env::new().await;
    let _daemon = idling(&server, &env).await;
    let mut fast = env.watch();
    fast.snapshot(1).await;
    // Subscribed, and then it reads nothing at all.
    let mut slow = env.connect().await;
    slow.send(SUBSCRIBE).await;
    eventually(10, "the slow watcher to be subscribed", || {
        env.logs().contains("2 subscribed now")
    })
    .await;

    // 500 messages of about 5 KB, in groups of 20 (what a sync announces at most): far more than
    // the buffers can hold for a watcher that reads nothing, whatever the speed of the machine.
    // The other watcher takes each group before the next is delivered, so that nothing depends
    // on how fast it reads either.
    let started = Instant::now();
    for group in 0..25 {
        for n in 0..20 {
            server.deliver("INBOX", big_message(100 + group * 20 + n));
        }
        let mut subjects = 0;
        while subjects < 20 {
            let event = fast.next().await;
            assert_ne!(
                event["event"], "lagged",
                "a watcher that reads was told it lagged"
            );
            if event["event"] == "new-message" {
                subjects += 1;
            }
        }
    }
    // The daemon itself still answers, and its copy holds them all.
    assert!(env.sync().await.status.success());
    assert!(
        started.elapsed() < Duration::from_secs(60),
        "the slow watcher slowed the daemon down"
    );

    // What the slow watcher reads now: its answer, then the events the buffers kept, then the
    // number it missed.
    assert_eq!(slow.next().await["answer"], "subscribe");
    let mut missed = 0;
    let mut messages = 0;
    loop {
        let event = slow.next().await;
        match event["event"].as_str() {
            Some("lagged") => {
                assert_eq!(event["protocol"], 1);
                missed += event["missed"].as_u64().unwrap();
                break;
            }
            Some("new-message") => messages += 1,
            _ => {}
        }
    }
    assert!(missed > 0);
    assert!(messages > 0);
    // And it is not dropped: the connection goes on, and answers a request.
    slow.send(r#"{"protocol":1,"request":"sync"}"#).await;
    loop {
        let line = slow.next().await;
        if line["answer"] == "sync" {
            assert_eq!(line["ok"], true);
            break;
        }
    }
    assert!(env.logs().contains("missed"), "{}", env.logs());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_subscribe_request_is_answered_then_events_follow_on_the_same_connection() {
    let server = mailbox().start().await;
    let env = Env::new().await;
    let _daemon = idling(&server, &env).await;
    let mut raw = env.connect().await;

    raw.send(SUBSCRIBE).await;
    assert_eq!(
        raw.next().await,
        json!({ "protocol": 1, "answer": "subscribe", "ok": true })
    );
    assert_eq!(raw.next().await["event"], "state");
    assert_eq!(raw.next().await["event"], "unread");

    // A request on a subscribed connection is still answered, among the events.
    raw.send(r#"{"protocol":1,"request":"sync","account":"alice"}"#)
        .await;
    let mut events = Vec::new();
    let answer = loop {
        let line = raw.next().await;
        if line.get("answer").is_some() {
            break line;
        }
        events.push(line);
    };
    assert_eq!(
        answer,
        json!({
            "protocol": 1, "answer": "sync", "ok": true,
            "accounts": [{ "id": "alice", "state": "ready" }],
        })
    );
    // Subscribing twice is refused, clearly, and the first subscription goes on.
    raw.send(SUBSCRIBE).await;
    loop {
        let line = raw.next().await;
        if line.get("answer").is_some() {
            assert_eq!(line["answer"], "subscribe");
            assert_eq!(line["ok"], false);
            assert_eq!(line["error"], "failed");
            break;
        }
    }
    let uid = server.deliver("INBOX", message(7));
    loop {
        let line = raw.next().await;
        if line["event"] == "new-message" {
            assert_eq!(line["message"], env.inbox_message_id("alice", uid));
            break;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_watcher_that_closes_only_its_writing_side_keeps_receiving_events() {
    let server = mailbox().start().await;
    let env = Env::new().await;
    let _daemon = idling(&server, &env).await;
    let mut raw = env.connect().await;

    // `echo '{"protocol":1,"request":"subscribe"}' | socat - UNIX-CONNECT:…` does this: the
    // request, then the end of what it has to say, while it goes on listening.
    raw.send(SUBSCRIBE).await;
    raw.write.shutdown().await.unwrap();

    assert_eq!(raw.next().await["answer"], "subscribe");
    assert_eq!(raw.next().await["event"], "state");
    assert_eq!(raw.next().await["event"], "unread");
    let uid = server.deliver("INBOX", message(7));
    loop {
        let line = raw.next().await;
        if line["event"] == "new-message" {
            assert_eq!(line["message"], env.inbox_message_id("alice", uid));
            break;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_subscription_does_not_hold_other_connections_back() {
    let server = mailbox().start().await;
    let env = Env::new().await;
    let _daemon = idling(&server, &env).await;
    let mut raw = env.connect().await;
    raw.send(SUBSCRIBE).await;
    raw.next().await;

    // `sync` is another connection, answered while the first one is subscribed and silent.
    assert!(env.sync().await.status.success());
}

async fn stops_on(signal: &str, with_a_daemon: bool) {
    let server = mailbox().start().await;
    let env = Env::new().await;
    let _daemon = if with_a_daemon {
        Some(idling(&server, &env).await)
    } else {
        None
    };
    let mut watcher = env.watch();
    watcher.next().await;

    watcher.signal(signal);

    let status = watcher.stopped(5).await;
    assert!(status.success(), "{status:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_watcher_stops_cleanly_on_sigterm_whether_a_daemon_runs_or_not() {
    stops_on("-TERM", true).await;
    stops_on("-TERM", false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_watcher_stops_cleanly_on_sigint_whether_a_daemon_runs_or_not() {
    stops_on("-INT", true).await;
    stops_on("-INT", false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_watcher_whose_standard_output_is_closed_exits_without_a_panic() {
    let server = mailbox().start().await;
    let env = Env::new().await;
    let _daemon = idling(&server, &env).await;
    let log = std::fs::File::create(env.home.path().join("closed.err")).unwrap();
    let mut child = env
        .command(&["watch"])
        .stdout(Stdio::piped())
        .stderr(log)
        .spawn()
        .unwrap();
    // The reader of the pipe is gone before the first line is written.
    drop(child.stdout.take());

    let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .expect("the watcher exits when its standard output is closed")
        .unwrap();
    assert!(status.success(), "{status:?}");
    let err = std::fs::read_to_string(env.home.path().join("closed.err")).unwrap();
    assert!(!err.contains("panicked"), "{err}");
    server.deliver("INBOX", message(7));
}

/// `missive watch | head -1`: the reader takes what it wants, leaves, and nothing is written
/// afterwards. The watcher has to notice by itself.
async fn leaves_when_its_reader_does(watcher: Watcher) {
    let Watcher { mut child, lines } = watcher;
    drop(lines);
    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("the watcher exits soon after its reader left")
        .unwrap();
    assert!(status.success(), "{status:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_watcher_with_a_daemon_exits_when_its_reader_leaves_without_waiting_for_an_event() {
    let server = mailbox().start().await;
    let env = Env::new().await;
    let _daemon = idling(&server, &env).await;
    let mut watcher = env.watch();
    watcher.snapshot(1).await;

    leaves_when_its_reader_does(watcher).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_watcher_without_a_daemon_exits_when_its_reader_leaves_without_polling_for_ever() {
    let env = Env::new().await;
    let mut watcher = env.watch();
    watcher.expect("gone").await;

    leaves_when_its_reader_does(watcher).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_watcher_writing_to_a_regular_file_keeps_going() {
    let server = mailbox().start().await;
    let env = Env::new().await;
    let _daemon = idling(&server, &env).await;
    let file = env.home.path().join("watch.out");
    let mut child = env
        .command(&["watch"])
        .stdout(std::fs::File::create(&file).unwrap())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    // A file has no reader to lose: the stream goes on into it.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(child.try_wait().unwrap().is_none(), "the watcher left");
    server.deliver("INBOX", message(7));
    eventually(10, "the new message in the file", || {
        std::fs::read_to_string(&file).is_ok_and(|out| out.contains("new-message"))
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn no_subject_or_address_reaches_the_daemon_log_through_the_event_code() {
    let server = mailbox().start().await;
    let env = Env::new().await;
    let _daemon = idling(&server, &env).await;
    let mut watcher = env.watch();
    watcher.snapshot(1).await;
    // A slow one too, so that the log of a lagging watcher is looked at.
    let mut slow = env.connect().await;
    slow.send(SUBSCRIBE).await;

    let uid = server.deliver("INBOX", message(7));
    let (_, event) = watcher.until("new-message").await;
    // The reader of the stream does get them: that is what the stream is for.
    assert_eq!(event["subject"], format!("{SUBJECT} 7"));
    assert_eq!(event["sender"]["address"], SENDER);
    assert_eq!(event["message"], env.inbox_message_id("alice", uid));
    server.set_flags("INBOX", 3, &["\\Seen"]);
    server.expunge("INBOX", 1);
    server.bump_uidvalidity(PRIVATE_FOLDER);
    for n in 100..400 {
        server.deliver("INBOX", big_message(n));
    }
    assert!(env.sync().await.status.success());
    watcher.until("sync-ended").await;

    let logs = env.logs();
    assert!(
        logs.contains("watcher"),
        "the code was not reached:\n{logs}"
    );
    for private in PRIVATE {
        assert!(!logs.contains(private), "the log shows {private}:\n{logs}");
    }
    drop(slow);
}
