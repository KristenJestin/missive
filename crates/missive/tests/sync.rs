//! `missive daemon` and `missive sync` against the fake IMAP server and a fake Secret Service on a
//! private bus. The binary runs with a cleared environment: its configuration, data, state and
//! runtime folders are temporary, and the only session bus it can see is the test's own. Every
//! daemon a test starts is killed when the test ends, even when it panics.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use missive_core::account::{Account, AccountStore, Password, Security, Server};
use missive_core::fake::{FakeKeyring, FakeServer, Folder, Message, SpecialUse};
use missive_core::keyring::Keyring;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream, UnixStream};
use tokio::process::{Child, Command};

const USER: &str = "alice@example.org";
const PASSWORD: &str = "Recognisable-Pa55word-4c1d";
const OTHER_USER: &str = "bob@example.com";
const OTHER_PASSWORD: &str = "Bobs-Recognisable-Pa55word";

const SUBJECT: &str = "Quarterly-Plan-Subject-5e1f";
const SENDER: &str = "carol.sender@example.com";
const RECIPIENT: &str = "dave.recipient@example.net";
const BODY: &str = "Body-Text-That-Stays-Private";
const PRIVATE_FOLDER: &str = "Private-Folder-Name-77c2";

/// What no log and no output may ever show: the messages' subjects, addresses and words, a
/// folder's name, the login and the passwords.
const PRIVATE: &[&str] = &[
    SUBJECT,
    SENDER,
    RECIPIENT,
    BODY,
    PRIVATE_FOLDER,
    USER,
    OTHER_USER,
    PASSWORD,
    OTHER_PASSWORD,
    "Carol Sender",
];

/// The `n`th test message: its subject, addresses and body are all in [`PRIVATE`].
fn message(n: u32) -> Message {
    Message::new(format!(
        "From: Carol Sender <{SENDER}>\r\n\
         To: Dave Recipient <{RECIPIENT}>\r\n\
         Subject: {SUBJECT} {n}\r\n\
         Message-ID: <{n}@example.com>\r\n\
         Date: Sat, 17 Oct 2026 09:{:02}:00 +0200\r\n\
         \r\n\
         {BODY} {n}.\r\n",
        n % 60
    ))
}

/// INBOX with three messages (one seen, one flagged), a sent folder and a folder of the user's
/// own.
fn mailbox() -> missive_core::fake::Builder {
    FakeServer::builder()
        .user(USER, PASSWORD)
        .user(OTHER_USER, OTHER_PASSWORD)
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
    /// The logs of every daemon started, one file each.
    logs: AtomicUsize,
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
            logs: AtomicUsize::new(0),
        }
    }

    fn path(&self, folder: &str) -> PathBuf {
        self.home.path().join(folder)
    }

    /// Adds an account on the fake's implicit TLS port (or `port`), with the fake's certificate
    /// pinned, and its password in the keyring unless `password` is `None`.
    async fn add_account(&self, id: &str, server: &FakeServer, user: &str, password: Option<&str>) {
        self.add_account_on(id, server, server.imap_tls_addr().port(), user, password)
            .await;
    }

    async fn add_account_on(
        &self,
        id: &str,
        server: &FakeServer,
        port: u16,
        user: &str,
        password: Option<&str>,
    ) {
        let account = Account {
            name: "Alice Example".to_owned(),
            address: user.to_owned(),
            user: user.to_owned(),
            imap: Server {
                host: "127.0.0.1".to_owned(),
                port,
                security: Security::Tls,
            },
            smtp: Server {
                host: "127.0.0.1".to_owned(),
                port: server.smtp_tls_addr().port(),
                security: Security::Tls,
            },
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
            .stdin(Stdio::null());
        command
    }

    /// Starts `missive daemon` in the foreground and waits until it answers on its socket.
    async fn daemon(&self, args: &[&str]) -> Daemon {
        let daemon = self.spawn_daemon(args);
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

    fn spawn_daemon(&self, args: &[&str]) -> Daemon {
        let n = self.logs.fetch_add(1, Ordering::Relaxed);
        let log = std::fs::File::create(self.home.path().join(format!("daemon-{n}.log"))).unwrap();
        let mut arguments = vec!["daemon"];
        arguments.extend_from_slice(args);
        let child = self
            .command(&arguments)
            .stdout(Stdio::null())
            .stderr(log)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        Daemon { child }
    }

    async fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().await.unwrap()
    }

    async fn sync(&self) -> Output {
        self.run(&["sync"]).await
    }

    fn socket(&self) -> PathBuf {
        self.path("runtime").join("missive/daemon.sock")
    }

    fn pid_file(&self) -> PathBuf {
        self.path("runtime").join("missive/daemon.pid")
    }

    /// Every daemon's log, the one written when `sync` starts a daemon included.
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

    /// The local copy, read only, as `status` or `search` would open it.
    fn copy(&self) -> Option<Connection> {
        let connection =
            Connection::open_with_flags(self.copy_path(), OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
        // The schema may not be written yet.
        connection
            .query_row("SELECT count(*) FROM message", [], |_| Ok(()))
            .ok()?;
        Some(connection)
    }

    /// `(folder, uid, flags)` of every message in the copy.
    fn messages(&self) -> Vec<(String, u32, String)> {
        let Some(copy) = self.copy() else {
            return Vec::new();
        };
        let mut statement = copy
            .prepare(
                "SELECT folder.name, message.uid, message.flags FROM message \
                 JOIN folder ON folder.id = message.folder ORDER BY folder.name, message.uid",
            )
            .unwrap();
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    fn uids(&self, folder: &str) -> Vec<u32> {
        self.messages()
            .into_iter()
            .filter(|(f, _, _)| f == folder)
            .map(|(_, uid, _)| uid)
            .collect()
    }

    fn flags(&self, folder: &str, uid: u32) -> Option<String> {
        self.messages()
            .into_iter()
            .find(|(f, u, _)| f == folder && *u == uid)
            .map(|(_, _, flags)| flags)
    }

    /// An account's connection state in the copy.
    fn state(&self, id: &str) -> Option<String> {
        self.copy()?
            .query_row("SELECT state FROM account WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .ok()
    }

    /// Every table of the copy, row by row, without what depends on when things happened (the
    /// time of the last sync) or on the order rows were written (a message's row number).
    fn dump(&self) -> Vec<String> {
        let mut rows = self.rows(&["SELECT key, id, state FROM account ORDER BY key"]);
        rows.extend(self.mail());
        rows
    }

    /// The folders and messages of the copy, as [`Env::dump`] gives them: what a sync must never
    /// lose.
    fn mail(&self) -> Vec<String> {
        self.rows(&[
            "SELECT id, account, name, role, uidvalidity, highest_modseq FROM folder ORDER BY id",
            "SELECT folder, uid, flags, internal_date, size, date, subject, sender, recipients, \
             cc, message_id, hex(raw) FROM message \
             JOIN message_raw ON message_raw.message = message.id ORDER BY folder, uid",
        ])
    }

    fn rows(&self, queries: &[&str]) -> Vec<String> {
        let copy = self.copy().unwrap();
        let mut rows = Vec::new();
        for query in queries {
            let mut statement = copy.prepare(query).unwrap();
            let columns = statement.column_count();
            let mut result = statement.query([]).unwrap();
            while let Some(row) = result.next().unwrap() {
                let values: Vec<String> = (0..columns)
                    .map(|i| match row.get_ref(i).unwrap() {
                        ValueRef::Null => "NULL".to_owned(),
                        ValueRef::Integer(n) => n.to_string(),
                        ValueRef::Real(n) => n.to_string(),
                        ValueRef::Text(text) => String::from_utf8_lossy(text).into_owned(),
                        ValueRef::Blob(blob) => format!("{} bytes", blob.len()),
                    })
                    .collect();
                rows.push(values.join(" | "));
            }
        }
        rows
    }

    fn table_names(&self) -> BTreeSet<String> {
        let copy = self.copy().unwrap();
        let mut statement = copy
            .prepare("SELECT name FROM sqlite_schema WHERE type = 'table'")
            .unwrap();
        statement
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }
}

impl Drop for Env {
    /// Kills a daemon `sync` started, which no [`Daemon`] owns.
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
    fn signal(&self, signal: &str) {
        let pid = self.child.id().expect("the daemon is running").to_string();
        let status = std::process::Command::new("kill")
            .args([signal, &pid])
            .status()
            .unwrap();
        assert!(status.success());
    }

    /// Sends a signal and waits for the daemon to exit.
    async fn stop(&mut self, signal: &str) -> ExitStatus {
        self.signal(signal);
        tokio::time::timeout(Duration::from_secs(10), self.child.wait())
            .await
            .expect("the daemon exits within 10 seconds")
            .unwrap()
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

/// `missive sync`, which must answer within 20 seconds.
async fn sync_soon(env: &Env) -> Output {
    tokio::time::timeout(Duration::from_secs(20), env.sync())
        .await
        .unwrap_or_else(|_| panic!("sync did not answer; the log:\n{}", env.logs()))
}

/// The fake's IMAP commands that log in as `user`.
fn logins(server: &FakeServer, user: &str) -> usize {
    let login = format!("LOGIN \"{user}\"");
    server
        .imap_log()
        .iter()
        .filter(|command| command.starts_with(&login))
        .count()
}

#[tokio::test(flavor = "multi_thread")]
async fn messages_and_flags_appear_in_the_copy() {
    let server = mailbox().start().await;
    let env = Env::new().await;
    env.add_account("alice", &server, USER, Some(PASSWORD))
        .await;
    let _daemon = env.daemon(&[]).await;

    let output = env.sync().await;

    succeeded(&output);
    assert_eq!(stdout(&output), "alice: ready\n");
    assert_eq!(
        env.messages(),
        [
            ("INBOX".to_owned(), 1, "\\Seen".to_owned()),
            ("INBOX".to_owned(), 2, "\\Flagged".to_owned()),
            ("INBOX".to_owned(), 3, String::new()),
            (PRIVATE_FOLDER.to_owned(), 5, String::new()),
            (PRIVATE_FOLDER.to_owned(), 9, String::new()),
            ("Sent".to_owned(), 1, "\\Seen".to_owned()),
        ]
    );
    let copy = env.copy().unwrap();
    let roles: Vec<(String, Option<String>)> = copy
        .prepare("SELECT name, role FROM folder ORDER BY name")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        roles,
        [
            ("INBOX".to_owned(), Some("inbox".to_owned())),
            (PRIVATE_FOLDER.to_owned(), None),
            ("Sent".to_owned(), Some("sent".to_owned())),
        ]
    );
    let (subject, sender, recipients, size, raw): (String, String, String, i64, Vec<u8>) = copy
        .query_row(
            "SELECT subject, sender, recipients, size, raw FROM message \
             JOIN folder ON folder.id = message.folder \
             JOIN message_raw ON message_raw.message = message.id \
             WHERE folder.name = 'INBOX' AND uid = 3",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(subject, format!("{SUBJECT} 3"));
    let sender: serde_json::Value = serde_json::from_str(&sender).unwrap();
    assert_eq!(
        sender,
        serde_json::json!([{ "name": "Carol Sender", "address": SENDER }])
    );
    assert!(recipients.contains(RECIPIENT), "{recipients}");
    assert_eq!(raw.len() as i64, size);
    assert!(String::from_utf8(raw).unwrap().contains(BODY));
    let (state, last_sync): (String, Option<i64>) = copy
        .query_row(
            "SELECT state, last_sync FROM account WHERE id = 'alice'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(state, "ready");
    assert!(last_sync.is_some());
    // Reading did not mark anything as read on the server.
    assert_eq!(server.flags("INBOX", 3), Some(Vec::new()));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_message_is_picked_up_through_idle() {
    let server = mailbox().start().await;
    let env = Env::new().await;
    env.add_account("alice", &server, USER, Some(PASSWORD))
        .await;
    // The periodic check is an hour away: only IDLE can bring the message in.
    let _daemon = env.daemon(&["--check-interval", "3600"]).await;
    succeeded(&env.sync().await);
    eventually(10, "IDLE", || server.imap_log().iter().any(|c| c == "IDLE")).await;

    let uid = server.deliver("INBOX", message(7));

    eventually(10, "the new message in the copy", || {
        env.uids("INBOX").contains(&uid)
    })
    .await;
    assert_eq!(env.uids("INBOX"), [1, 2, 3, uid]);
}

async fn a_flag_change_is_reflected(server: &FakeServer) {
    let env = Env::new().await;
    env.add_account("alice", server, USER, Some(PASSWORD)).await;
    let _daemon = env.daemon(&["--check-interval", "3600"]).await;
    succeeded(&env.sync().await);

    server.set_flags("INBOX", 3, &["\\Seen", "\\Answered"]);
    server.set_flags("INBOX", 1, &[]);
    succeeded(&env.sync().await);

    assert_eq!(env.flags("INBOX", 3).unwrap(), "\\Answered \\Seen");
    assert_eq!(env.flags("INBOX", 1).unwrap(), "");
    assert_eq!(env.flags("INBOX", 2).unwrap(), "\\Flagged");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_flag_changed_on_the_server_is_reflected() {
    let server = mailbox().start().await;
    a_flag_change_is_reflected(&server).await;
    // The fake has CONDSTORE: only what changed was fetched.
    assert!(
        server.imap_log().iter().any(|c| c.contains("CHANGEDSINCE")),
        "{:?}",
        server.imap_log()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_flag_changed_on_a_server_without_condstore_is_reflected_by_a_full_flag_fetch() {
    let server = mailbox().without_condstore().start().await;
    a_flag_change_is_reflected(&server).await;
    assert!(
        !server.imap_log().iter().any(|c| c.contains("CHANGEDSINCE")),
        "{:?}",
        server.imap_log()
    );
}

async fn an_expunge_is_reflected(server: &FakeServer) {
    let env = Env::new().await;
    env.add_account("alice", server, USER, Some(PASSWORD)).await;
    let _daemon = env.daemon(&["--check-interval", "3600"]).await;
    succeeded(&env.sync().await);
    assert_eq!(env.uids("INBOX"), [1, 2, 3]);

    server.expunge("INBOX", 2);
    server.expunge(PRIVATE_FOLDER, 9);
    succeeded(&env.sync().await);

    assert_eq!(env.uids("INBOX"), [1, 3]);
    assert_eq!(env.uids(PRIVATE_FOLDER), [5]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_expunged_on_the_server_leaves_the_copy() {
    an_expunge_is_reflected(&mailbox().start().await).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_expunged_on_a_server_without_condstore_leaves_the_copy() {
    an_expunge_is_reflected(&mailbox().without_condstore().start().await).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_uidvalidity_change_rebuilds_the_folder() {
    let server = mailbox().start().await;
    let env = Env::new().await;
    env.add_account("alice", &server, USER, Some(PASSWORD))
        .await;
    let _daemon = env.daemon(&["--check-interval", "3600"]).await;
    succeeded(&env.sync().await);
    assert_eq!(env.uids(PRIVATE_FOLDER), [5, 9]);
    let before = env.dump();

    let uidvalidity = server.bump_uidvalidity(PRIVATE_FOLDER);
    succeeded(&env.sync().await);

    // Renumbered from 1 by the server: the old UIDs are gone, the messages are back under the
    // new ones.
    assert_eq!(env.uids(PRIVATE_FOLDER), [1, 2]);
    let copy = env.copy().unwrap();
    let (stored, subjects): (i64, String) = copy
        .query_row(
            "SELECT folder.uidvalidity, group_concat(message.subject, ',') FROM message \
             JOIN folder ON folder.id = message.folder WHERE folder.name = ?1 \
             GROUP BY folder.id ORDER BY message.uid",
            [PRIVATE_FOLDER],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(stored, i64::from(uidvalidity));
    assert_eq!(subjects, format!("{SUBJECT} 5,{SUBJECT} 6"));
    // The other folders were left alone.
    let id: i64 = copy
        .query_row(
            "SELECT id FROM folder WHERE name = ?1",
            [PRIVATE_FOLDER],
            |row| row.get(0),
        )
        .unwrap();
    let untouched = |rows: &[String]| -> Vec<String> {
        rows.iter()
            .filter(|row| !row.contains(PRIVATE_FOLDER) && !row.starts_with(&format!("{id} |")))
            .cloned()
            .collect()
    };
    assert_eq!(untouched(&env.dump()), untouched(&before));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_uidvalidity_change_of_the_inbox_while_idling_in_it_rebuilds_it_after_reconnecting() {
    let server = mailbox().start().await;
    let env = Env::new().await;
    env.add_account("alice", &server, USER, Some(PASSWORD))
        .await;
    let _daemon = env.daemon(&["--check-interval", "3600"]).await;
    succeeded(&env.sync().await);
    server.expunge("INBOX", 1);
    eventually(10, "the expunge, through IDLE", || {
        env.uids("INBOX") == [2, 3]
    })
    .await;

    // The server drops a client idling in a folder it rebuilds (`BYE`).
    server.bump_uidvalidity("INBOX");

    eventually(15, "the rebuilt inbox", || env.uids("INBOX") == [1, 2]).await;
    assert_eq!(logins(&server, USER), 2);
    assert_eq!(env.flags("INBOX", 1).unwrap(), "\\Flagged");
    succeeded(&env.sync().await);
}

/// Syncs the mailbox, lets `break_server` make the server misbehave, syncs again: the copy's
/// folders and messages must be what they were. Returns the second sync's output.
async fn a_broken_listing_changes_nothing(
    server: &FakeServer,
    break_server: impl FnOnce(&FakeServer),
) -> (Env, Daemon, Output) {
    let env = Env::new().await;
    env.add_account("alice", server, USER, Some(PASSWORD)).await;
    let daemon = env.daemon(&["--check-interval", "3600"]).await;
    succeeded(&env.sync().await);
    let before = env.mail();
    assert_eq!(env.uids("INBOX"), [1, 2, 3]);

    break_server(server);
    let output = sync_soon(&env).await;

    assert_eq!(
        env.mail(),
        before,
        "the copy changed; the log:\n{}",
        env.logs()
    );
    (env, daemon, output)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_flag_listing_cut_short_without_condstore_removes_nothing() {
    let server = mailbox().without_condstore().start().await;

    let (env, _daemon, output) = a_broken_listing_changes_nothing(&server, |server| {
        server.cut("UID FETCH 1:* (FLAGS", 1);
    })
    .await;

    failed(&output);
    assert_eq!(stdout(&output), "alice: unreachable\n");
    server.heal();
    succeeded(&sync_soon(&env).await);
    assert_eq!(env.uids("INBOX"), [1, 2, 3]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_uid_search_answered_no_with_condstore_removes_nothing() {
    let server = mailbox().start().await;

    let (env, _daemon, _) = a_broken_listing_changes_nothing(&server, |server| {
        server.refuse("UID SEARCH");
    })
    .await;

    assert!(
        env.logs()
            .contains("the server refused to examine or list it"),
        "{}",
        env.logs()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_examine_cut_after_its_uidvalidity_removes_nothing() {
    let server = mailbox().start().await;

    let (_env, _daemon, output) = a_broken_listing_changes_nothing(&server, |server| {
        // FLAGS, PERMANENTFLAGS, UIDVALIDITY, and no EXISTS.
        server.cut("EXAMINE \"INBOX\"", 3);
    })
    .await;

    failed(&output);
    assert_eq!(stdout(&output), "alice: unreachable\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_changedsince_listing_cut_short_does_not_move_the_sync_point() {
    let server = mailbox().start().await;

    let (env, _daemon, output) = a_broken_listing_changes_nothing(&server, |server| {
        // Cut first: the daemon idling in the inbox hears of the change at once.
        server.cut("UID FETCH 1:* (FLAGS) (CHANGEDSINCE", 0);
        server.set_flags("INBOX", 3, &["\\Seen"]);
    })
    .await;

    failed(&output);
    server.heal();
    succeeded(&sync_soon(&env).await);
    assert_eq!(env.flags("INBOX", 3).unwrap(), "\\Seen");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_the_server_refuses_to_send_is_skipped_and_asked_again_at_the_next_pass() {
    let server = mailbox().start().await;
    server.refuse("UID FETCH 2 (UID FLAGS");
    let env = Env::new().await;
    env.add_account("alice", &server, USER, Some(PASSWORD))
        .await;
    let _daemon = env
        .daemon(&["--check-interval", "3600", "--batch-size", "1"])
        .await;
    let asked = || {
        server
            .imap_log()
            .iter()
            .filter(|c| c.starts_with("UID FETCH 2 (UID FLAGS"))
            .count()
    };

    succeeded(&sync_soon(&env).await);

    assert_eq!(env.uids("INBOX"), [1, 3]);
    assert!(env.logs().contains("1 refused"), "{}", env.logs());
    // Asked by the pass, and by the sync of the inbox before IDLE; then no more until the next
    // sync of the inbox.
    eventually(10, "IDLE", || server.imap_log().iter().any(|c| c == "IDLE")).await;
    let asked_once = asked();
    assert!((1..=2).contains(&asked_once), "{asked_once}");
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(asked(), asked_once);
    server.heal();
    succeeded(&sync_soon(&env).await);
    assert_eq!(env.uids("INBOX"), [1, 2, 3]);
}

/// Single-UID commands of the log that fetch `uid` whole or ask its size.
fn asks_for(server: &FakeServer, uid: u32) -> (usize, usize) {
    let log = server.imap_log();
    let count = |tail: &str| {
        log.iter()
            .filter(|c| **c == format!("UID FETCH {uid} {tail}"))
            .count()
    };
    (
        count("(UID RFC822.SIZE)"),
        count("(UID FLAGS INTERNALDATE BODY.PEEK[])"),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_refused_in_the_sizes_command_and_in_the_body_fetch_does_not_hold_the_others_back()
 {
    let server = mailbox().start().await;
    server.refuse_uid(2);
    let env = Env::new().await;
    env.add_account("alice", &server, USER, Some(PASSWORD))
        .await;
    let _daemon = env.daemon(&["--check-interval", "3600"]).await;

    succeeded(&sync_soon(&env).await);

    // The sizes of the others came with the NO, and the refusal landed on the message alone.
    assert_eq!(env.uids("INBOX"), [1, 3]);
    assert_eq!(env.uids("Sent").len(), 1);
    assert_eq!(env.uids(PRIVATE_FOLDER), [5, 9]);
    assert!(env.logs().contains("1 refused"), "{}", env.logs());
    let log = server.imap_log();
    assert!(
        log.iter()
            .any(|c| c.starts_with("UID FETCH 3 (UID FLAGS INTERNALDATE")
                || c.starts_with("UID FETCH 1,3 (UID FLAGS INTERNALDATE")
                || c.starts_with("UID FETCH 1:3 (UID FLAGS INTERNALDATE")),
        "{log:?}"
    );
    // Asked by the pass, and by the sync of the inbox before IDLE: then no more.
    eventually(10, "IDLE", || server.imap_log().iter().any(|c| c == "IDLE")).await;
    let (sizes, bodies) = asks_for(&server, 2);
    assert!((1..=2).contains(&sizes) && (1..=2).contains(&bodies));
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(asks_for(&server, 2), (sizes, bodies));
    server.heal();
    succeeded(&sync_soon(&env).await);
    assert_eq!(env.uids("INBOX"), [1, 2, 3]);
}

#[tokio::test(flavor = "multi_thread")]
async fn every_message_left_without_a_size_is_fetched_alone() {
    let server = mailbox().start().await;
    // The sizes command of the inbox is refused whole, and so is one message.
    server.refuse("UID FETCH 1:3 (UID RFC822.SIZE)");
    server.refuse_uid(2);
    let env = Env::new().await;
    env.add_account("alice", &server, USER, Some(PASSWORD))
        .await;
    let _daemon = env.daemon(&["--check-interval", "3600"]).await;

    succeeded(&sync_soon(&env).await);

    assert_eq!(env.uids("INBOX"), [1, 3]);
    for uid in [1, 2, 3] {
        assert!(
            asks_for(&server, uid).1 >= 1,
            "{uid}: {:?}",
            server.imap_log()
        );
    }
    assert!(env.logs().contains("1 refused"), "{}", env.logs());
}

#[tokio::test(flavor = "multi_thread")]
async fn scattered_uids_are_asked_in_commands_of_a_bounded_length() {
    let mut inbox = Folder::new("INBOX");
    for n in 1..=400 {
        inbox = inbox.message(message(n).uid(n * 2));
    }
    let server = FakeServer::builder()
        .user(USER, PASSWORD)
        .folder(inbox)
        .start()
        .await;
    let env = Env::new().await;
    env.add_account("alice", &server, USER, Some(PASSWORD))
        .await;
    let _daemon = env.daemon(&["--batch-size", "100"]).await;

    succeeded(&sync_soon(&env).await);

    assert_eq!(env.uids("INBOX").len(), 400);
    let longest = server.imap_log().iter().map(String::len).max().unwrap();
    assert!(longest < 1100, "{longest}");
    assert!(
        server
            .imap_log()
            .iter()
            .filter(|c| c.ends_with("(UID RFC822.SIZE)"))
            .count()
            >= 2
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_folder_examined_without_its_exists_is_left_alone_and_the_next_one_still_syncs() {
    // A tagged OK, but no EXISTS: the server forgot a line it must send.
    let server = mailbox().truncate("EXAMINE \"Sent\"", 3).start().await;
    let env = Env::new().await;
    env.add_account("alice", &server, USER, Some(PASSWORD))
        .await;
    let _daemon = env.daemon(&["--check-interval", "3600"]).await;

    let output = sync_soon(&env).await;

    succeeded(&output);
    assert_eq!(stdout(&output), "alice: ready\n");
    assert_eq!(env.uids("INBOX"), [1, 2, 3]);
    assert!(env.uids("Sent").is_empty());
    assert_eq!(env.uids(PRIVATE_FOLDER), [5, 9]);
    assert_eq!(env.state("alice").as_deref(), Some("ready"));
    assert!(
        env.logs().contains("(sent): left as it is"),
        "{}",
        env.logs()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_listing_shorter_than_exists_leaves_its_folder_as_it_is_and_the_next_one_still_syncs() {
    // The inbox (3 messages) and the private folder (2) are listed short, the sent folder (1)
    // is not; each listing ends with its tagged OK.
    let server = mailbox()
        .without_condstore()
        .truncate("UID FETCH 1:* (FLAGS", 1)
        .start()
        .await;
    let env = Env::new().await;
    env.add_account("alice", &server, USER, Some(PASSWORD))
        .await;
    let _daemon = env.daemon(&["--check-interval", "3600"]).await;

    let output = sync_soon(&env).await;

    succeeded(&output);
    assert!(env.uids("INBOX").is_empty());
    assert_eq!(env.uids("Sent").len(), 1);
    assert!(env.uids(PRIVATE_FOLDER).is_empty());
    assert_eq!(env.state("alice").as_deref(), Some("ready"));
    assert!(
        env.logs().contains("(inbox): left as it is"),
        "{}",
        env.logs()
    );
    // Healed, the next sync brings what was left.
    server.heal();
    succeeded(&sync_soon(&env).await);
    assert_eq!(env.uids("INBOX"), [1, 2, 3]);
    assert_eq!(env.uids(PRIVATE_FOLDER), [5, 9]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_lost_at_every_pass_answers_the_sync_and_records_unreachable() {
    let server = mailbox().cut("EXAMINE", 0).start().await;
    let env = Env::new().await;
    env.add_account("alice", &server, USER, Some(PASSWORD))
        .await;
    let _daemon = env.daemon(&[]).await;

    let output = sync_soon(&env).await;

    failed(&output);
    assert_eq!(stdout(&output), "alice: unreachable\n");
    eventually(10, "unreachable in the copy", || {
        env.state("alice").as_deref() == Some("unreachable")
    })
    .await;
    // And again: the next request is answered too.
    assert_eq!(stdout(&sync_soon(&env).await), "alice: unreachable\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn idle_keepalives_do_not_keep_the_other_folders_from_being_checked() {
    let server = mailbox()
        .idle_keepalive(Duration::from_millis(200))
        .start()
        .await;
    let env = Env::new().await;
    env.add_account("alice", &server, USER, Some(PASSWORD))
        .await;
    let _daemon = env.daemon(&["--check-interval", "1"]).await;
    succeeded(&env.sync().await);
    eventually(10, "IDLE", || server.imap_log().iter().any(|c| c == "IDLE")).await;

    let uid = server.deliver("Sent", message(8));

    eventually(10, "the new sent message", || {
        env.uids("Sent").contains(&uid)
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_message_in_another_folder_comes_with_the_periodic_check() {
    let server = mailbox().start().await;
    let env = Env::new().await;
    env.add_account("alice", &server, USER, Some(PASSWORD))
        .await;
    let _daemon = env.daemon(&["--check-interval", "1"]).await;
    succeeded(&env.sync().await);

    let uid = server.deliver("Sent", message(8));

    eventually(10, "the new sent message", || {
        env.uids("Sent").contains(&uid)
    })
    .await;
    // IDLE is ended for each check and issued again, as it must be before the server's limit.
    eventually(10, "IDLE issued again", || {
        server.imap_log().iter().filter(|c| *c == "IDLE").count() >= 3
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn killing_the_daemon_mid_sync_and_restarting_it_ends_with_the_same_copy_as_an_uninterrupted_sync()
 {
    let mut inbox = Folder::new("INBOX");
    for n in 1..=40 {
        inbox = inbox.message(if n % 3 == 0 {
            message(n).flag("\\Seen")
        } else {
            message(n)
        });
    }
    let mut archive = Folder::new("Archive").special_use(SpecialUse::Archive);
    for n in 41..=50 {
        archive = archive.message(message(n));
    }
    let server = FakeServer::builder()
        .user(USER, PASSWORD)
        .folder(inbox)
        .folder(archive)
        .fetch_delay(Duration::from_millis(40))
        .start()
        .await;

    let uninterrupted = Env::new().await;
    uninterrupted
        .add_account("alice", &server, USER, Some(PASSWORD))
        .await;
    let _reference = uninterrupted.daemon(&["--batch-size", "2"]).await;
    succeeded(&uninterrupted.sync().await);
    assert_eq!(uninterrupted.messages().len(), 50);

    let interrupted = Env::new().await;
    interrupted
        .add_account("alice", &server, USER, Some(PASSWORD))
        .await;
    let mut daemon = interrupted.daemon(&["--batch-size", "2"]).await;
    eventually(20, "a few batches in the copy", || {
        interrupted.messages().len() >= 6
    })
    .await;
    daemon.child.kill().await.unwrap();
    let at_kill = interrupted.messages().len();
    assert!(at_kill < 50, "the sync ended before the kill");
    // The killed daemon left its socket behind; the new one recovers it.
    assert!(interrupted.socket().exists());
    let _restarted = interrupted.daemon(&["--batch-size", "2"]).await;
    succeeded(&interrupted.sync().await);

    assert_eq!(interrupted.table_names(), uninterrupted.table_names());
    assert_eq!(interrupted.dump(), uninterrupted.dump());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_first_sync_is_usable_early_inbox_first_newest_first() {
    let mut inbox = Folder::new("INBOX");
    for n in 1..=20 {
        inbox = inbox.message(message(n));
    }
    let server = FakeServer::builder()
        .user(USER, PASSWORD)
        .folder(inbox)
        .folder(Folder::new("Archive").message(message(21)))
        .fetch_delay(Duration::from_millis(100))
        .start()
        .await;
    let env = Env::new().await;
    env.add_account("alice", &server, USER, Some(PASSWORD))
        .await;
    let _daemon = env.daemon(&["--batch-size", "2"]).await;

    eventually(10, "a first batch", || !env.messages().is_empty()).await;
    let first = env.messages();

    assert_eq!(
        first,
        [
            ("INBOX".to_owned(), 19, String::new()),
            ("INBOX".to_owned(), 20, String::new()),
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_server_log_shows_only_read_only_commands() {
    let server = mailbox().start().await;
    let env = Env::new().await;
    env.add_account("alice", &server, USER, Some(PASSWORD))
        .await;
    let mut daemon = env.daemon(&["--check-interval", "1"]).await;
    succeeded(&env.sync().await);
    // Everything a sync does: new mail through IDLE, a flag change, an expunge, a rebuilt folder
    // (the inbox, which the daemon is idling in, so the server drops it), the periodic check.
    let uid = server.deliver("INBOX", message(7));
    eventually(10, "the new message", || env.uids("INBOX").contains(&uid)).await;
    server.set_flags("INBOX", 2, &["\\Flagged", "\\Answered"]);
    succeeded(&env.sync().await);
    server.expunge("INBOX", 1);
    succeeded(&env.sync().await);
    server.bump_uidvalidity("INBOX");
    eventually(15, "the rebuilt inbox", || env.uids("INBOX") == [1, 2, 3]).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    succeeded(&env.sync().await);
    assert!(daemon.stop("-TERM").await.success());

    let log = server.imap_log();
    for expected in ["EXAMINE", "UID FETCH", "IDLE", "DONE", "LOGOUT"] {
        assert!(
            log.iter().any(|c| c.starts_with(expected)),
            "no {expected}: {log:?}"
        );
    }
    for command in &log {
        let upper = command.to_ascii_uppercase();
        let name = upper
            .strip_prefix("UID ")
            .unwrap_or(&upper)
            .split(' ')
            .next()
            .unwrap()
            .to_owned();
        let allowed = match name.as_str() {
            "CAPABILITY" | "LOGIN" | "LIST" | "EXAMINE" | "SEARCH" | "IDLE" | "DONE" | "NOOP"
            | "LOGOUT" => true,
            "ENABLE" => upper == "ENABLE CONDSTORE",
            // Only BODY.PEEK reads a message without marking it as read.
            "FETCH" => {
                let items = upper.replace("RFC822.SIZE", "");
                !items.contains("BODY[") && !items.contains("RFC822")
            }
            _ => false,
        };
        assert!(allowed, "{command:?} is not read only");
    }
    // The message that was UID 3, now 2, was never marked as read.
    assert_eq!(server.flags("INBOX", 2), Some(Vec::new()));
}

#[tokio::test(flavor = "multi_thread")]
async fn no_message_content_subject_or_address_appears_in_the_logs() {
    let server = mailbox().start().await;
    let env = Env::new().await;
    env.add_account("alice", &server, USER, Some(PASSWORD))
        .await;
    env.add_account("bob", &server, OTHER_USER, Some("a-wrong-password"))
        .await;
    env.add_account("carol", &server, "carol@example.com", None)
        .await;
    // `sync` starts the daemon, which logs to its state folder.
    let mut outputs = vec![env.sync().await];
    let uid = server.deliver("INBOX", message(7));
    eventually(10, "the new message", || env.uids("INBOX").contains(&uid)).await;
    server.set_flags("INBOX", 3, &["\\Seen"]);
    server.expunge("INBOX", 1);
    server.bump_uidvalidity(PRIVATE_FOLDER);
    outputs.push(env.sync().await);
    outputs.push(env.run(&["sync", "--account", "alice"]).await);
    outputs.push(env.run(&["sync", "--account", "nobody"]).await);

    let logs = env.logs();
    assert!(
        logs.contains("account"),
        "the daemon logged nothing: {logs}"
    );
    for private in PRIVATE {
        assert!(!logs.contains(private), "the log shows {private}:\n{logs}");
        for output in &outputs {
            assert!(!stdout(output).contains(private), "{}", stdout(output));
            assert!(!stderr(output).contains(private), "{}", stderr(output));
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn one_account_refused_never_stops_the_others() {
    let server = mailbox().start().await;
    let env = Env::new().await;
    env.add_account("alice", &server, USER, Some(PASSWORD))
        .await;
    env.add_account("bob", &server, OTHER_USER, Some("a-wrong-password"))
        .await;
    let _daemon = env.daemon(&[]).await;

    let output = env.sync().await;

    failed(&output);
    assert_eq!(stdout(&output), "alice: ready\nbob: auth-refused\n");
    assert!(stderr(&output).contains("bob"), "{}", stderr(&output));
    assert_eq!(env.uids("INBOX"), [1, 2, 3]);
    assert_eq!(env.state("alice").unwrap(), "ready");
    assert_eq!(env.state("bob").unwrap(), "auth-refused");
}

#[tokio::test(flavor = "multi_thread")]
async fn auth_refused_does_not_retry_in_a_loop() {
    let server = mailbox().start().await;
    let env = Env::new().await;
    env.add_account("bob", &server, OTHER_USER, Some("a-wrong-password"))
        .await;
    let _daemon = env.daemon(&["--check-interval", "1"]).await;
    eventually(10, "auth-refused", || {
        env.state("bob").as_deref() == Some("auth-refused")
    })
    .await;

    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(logins(&server, OTHER_USER), 1);

    // Asking again is what tries again.
    let output = env.sync().await;
    failed(&output);
    assert_eq!(stdout(&output), "bob: auth-refused\n");
    assert_eq!(logins(&server, OTHER_USER), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_account_without_a_password_is_no_login() {
    let server = mailbox().start().await;
    let env = Env::new().await;
    env.add_account("alice", &server, USER, None).await;
    let _daemon = env.daemon(&[]).await;

    let output = env.sync().await;

    failed(&output);
    assert_eq!(stdout(&output), "alice: no-login\n");
    assert_eq!(env.state("alice").unwrap(), "no-login");
    assert_eq!(logins(&server, USER), 0);
}

/// A TCP relay to the fake server that can be switched off: while it is off, every connection is
/// closed at once, as an unreachable server's would be.
struct Relay {
    port: u16,
    up: Arc<AtomicBool>,
}

impl Relay {
    async fn start(to: std::net::SocketAddr) -> Self {
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

#[tokio::test(flavor = "multi_thread")]
async fn unreachable_retries_with_a_backoff_and_never_asks_to_log_in_again() {
    let server = mailbox().start().await;
    let relay = Relay::start(server.imap_tls_addr()).await;
    let env = Env::new().await;
    env.add_account_on("alice", &server, relay.port, USER, Some(PASSWORD))
        .await;
    let _daemon = env.daemon(&[]).await;

    let output = env.sync().await;
    failed(&output);
    assert_eq!(stdout(&output), "alice: unreachable\n");
    assert!(
        !stderr(&output).to_lowercase().contains("log in"),
        "{}",
        stderr(&output)
    );

    relay.up.store(true, Ordering::SeqCst);
    // Without being asked: the daemon tries again by itself.
    let mut states = BTreeSet::new();
    eventually(20, "ready", || {
        let state = env.state("alice");
        states.extend(state.clone());
        state.as_deref() == Some("ready")
    })
    .await;
    assert!(
        states
            .iter()
            .all(|s| ["unreachable", "syncing", "ready"].contains(&s.as_str())),
        "{states:?}"
    );
    assert_eq!(env.uids("INBOX"), [1, 2, 3]);
    assert_eq!(env.keyring.items().len(), 1);
    // The password was read once, not at each attempt.
    assert_eq!(env.keyring.secret_reads(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_daemon_refuses_to_start() {
    let server = mailbox().start().await;
    let env = Env::new().await;
    env.add_account("alice", &server, USER, Some(PASSWORD))
        .await;
    let _daemon = env.daemon(&[]).await;

    let second = tokio::time::timeout(Duration::from_secs(10), env.run(&["daemon"]))
        .await
        .expect("the second daemon stops at once");

    failed(&second);
    assert!(
        stderr(&second).contains("already running"),
        "{}",
        stderr(&second)
    );
    // The first one still answers.
    succeeded(&env.sync().await);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stale_socket_from_a_killed_daemon_is_recovered() {
    let server = mailbox().start().await;
    let env = Env::new().await;
    env.add_account("alice", &server, USER, Some(PASSWORD))
        .await;
    let mut killed = env.daemon(&[]).await;
    killed.child.kill().await.unwrap();
    assert!(env.socket().exists());

    let _daemon = env.daemon(&[]).await;

    succeeded(&env.sync().await);
}

#[tokio::test(flavor = "multi_thread")]
async fn sync_starts_the_daemon_when_none_answers() {
    let server = mailbox().start().await;
    let env = Env::new().await;
    env.add_account("alice", &server, USER, Some(PASSWORD))
        .await;

    let output = env.sync().await;

    succeeded(&output);
    assert_eq!(stdout(&output), "alice: ready\n");
    let pid = running_daemon(&env.pid_file()).expect("a daemon runs on");
    assert!(env.logs().contains("started"), "{}", env.logs());
    // It is stopped the way a user stops it.
    std::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .unwrap();
    eventually(10, "the daemon to stop", || !env.socket().exists()).await;
}

async fn stops_cleanly_on(signal: &str) {
    let server = mailbox().start().await;
    let env = Env::new().await;
    env.add_account("alice", &server, USER, Some(PASSWORD))
        .await;
    let mut daemon = env.daemon(&[]).await;
    succeeded(&env.sync().await);
    eventually(10, "IDLE", || server.imap_log().iter().any(|c| c == "IDLE")).await;

    let status = daemon.stop(signal).await;

    assert!(status.success(), "{status:?}\n{}", env.logs());
    assert!(!env.socket().exists());
    assert_eq!(server.imap_log().last().unwrap(), "LOGOUT");
    assert_eq!(std::fs::read_to_string(env.pid_file()).unwrap(), "");
    assert!(env.logs().contains("stopped"), "{}", env.logs());
}

#[tokio::test(flavor = "multi_thread")]
async fn sigterm_stops_the_daemon_cleanly() {
    stops_cleanly_on("-TERM").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sigint_stops_the_daemon_cleanly() {
    stops_cleanly_on("-INT").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sync_of_an_unknown_account_fails() {
    let server = mailbox().start().await;
    let env = Env::new().await;
    env.add_account("alice", &server, USER, Some(PASSWORD))
        .await;
    let _daemon = env.daemon(&[]).await;

    let output = env.run(&["sync", "--account", "nobody"]).await;

    failed(&output);
    assert!(
        stderr(&output).contains("no account nobody"),
        "{}",
        stderr(&output)
    );
    let alice = env.run(&["sync", "--account", "alice"]).await;
    succeeded(&alice);
    assert_eq!(stdout(&alice), "alice: ready\n");
}

/// Sends one request line on the daemon's socket and reads one answer line.
async fn ask(env: &Env, request: &str) -> serde_json::Value {
    let mut stream = UnixStream::connect(env.socket()).await.unwrap();
    stream
        .write_all(format!("{request}\n").as_bytes())
        .await
        .unwrap();
    let mut answer = String::new();
    BufReader::new(stream).read_line(&mut answer).await.unwrap();
    serde_json::from_str(&answer).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_socket_speaks_the_documented_protocol() {
    let server = mailbox().start().await;
    let env = Env::new().await;
    env.add_account("alice", &server, USER, Some(PASSWORD))
        .await;
    let _daemon = env.daemon(&[]).await;

    assert_eq!(
        ask(&env, r#"{"protocol":1,"request":"sync"}"#).await,
        serde_json::json!({
            "protocol": 1,
            "answer": "sync",
            "ok": true,
            "accounts": [{ "id": "alice", "state": "ready" }],
        })
    );
    assert_eq!(
        ask(
            &env,
            r#"{"protocol":1,"request":"sync","account":"nobody"}"#
        )
        .await["error"],
        "no-such-account"
    );
    assert_eq!(
        ask(&env, r#"{"protocol":2,"request":"sync"}"#).await["error"],
        "unsupported-protocol"
    );
    assert_eq!(
        ask(&env, r#"{"protocol":1,"request":"dance"}"#).await["error"],
        "unknown-request"
    );
    assert_eq!(ask(&env, "not json").await["error"], "bad-request");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_copy_is_in_wal_mode_with_a_schema_version() {
    let server = mailbox().start().await;
    let env = Env::new().await;
    env.add_account("alice", &server, USER, Some(PASSWORD))
        .await;
    let _daemon = env.daemon(&[]).await;
    succeeded(&env.sync().await);

    let copy = env.copy().unwrap();
    let mode: String = copy
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    let version: i64 = copy
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();

    assert_eq!(mode, "wal");
    assert_eq!(version, 1);
    assert_eq!(
        env.table_names(),
        ["account", "folder", "message", "message_raw"]
            .into_iter()
            .map(str::to_owned)
            .collect()
    );
}
