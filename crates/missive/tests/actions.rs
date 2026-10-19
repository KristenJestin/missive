//! `missive mark`, `move` and `delete`, and the daemon applying them, against the fake IMAP
//! server and a fake Secret Service on a private bus. The binary runs with a cleared environment: its configuration, data, state and
//! runtime folders are temporary, and the only session bus it can see is the test's own. Every
//! daemon a test starts is killed when the test ends, even when it panics.

use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use missive_core::account::{Account, AccountStore, Password, Security, Server};
use missive_core::fake::{FakeKeyring, FakeServer, Folder, Message, SpecialUse};
use missive_core::keyring::Keyring;
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

/// INBOX with four messages (one seen, one flagged, one marked `\Deleted` by another client), an
/// empty archive, a trash with two messages (the second marked `\Deleted` by another client), and
/// a folder of the user's own. Their UIDVALIDITYs are 100 to 103 in this order.
fn mailbox() -> missive_core::fake::Builder {
    FakeServer::builder()
        .user(USER, PASSWORD)
        .user(OTHER_USER, OTHER_PASSWORD)
        .folder(
            Folder::new("INBOX")
                .message(message(1).flag("\\Seen"))
                .message(message(2).flag("\\Flagged"))
                .message(message(3))
                .message(message(4).flag("\\Deleted")),
        )
        .folder(Folder::new("Archive").special_use(SpecialUse::Archive))
        .folder(
            Folder::new("Trash")
                .special_use(SpecialUse::Trash)
                .message(message(10))
                .message(message(11).flag("\\Deleted")),
        )
        .folder(Folder::new(PRIVATE_FOLDER).message(message(20)))
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

    /// The raw message of the copy, by the server's folder name and UID.
    fn raw(&self, folder: &str, uid: u32) -> Option<Vec<u8>> {
        self.copy()?
            .query_row(
                "SELECT message_raw.raw FROM message_raw
                   JOIN message ON message.id = message_raw.message
                   JOIN folder ON folder.id = message.folder
                  WHERE folder.name = ?1 AND message.uid = ?2",
                rusqlite::params![folder, uid],
                |row| row.get(0),
            )
            .ok()
    }

    /// An account's connection state in the copy.
    fn state(&self, id: &str) -> Option<String> {
        self.copy()?
            .query_row("SELECT state FROM account WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .ok()
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

impl Env {
    /// The identifier of a message of the copy, by the server's folder name and UID, as `list`
    /// prints it.
    fn id(&self, folder: &str, uid: u32) -> String {
        self.copy()
            .unwrap()
            .query_row(
                "SELECT account.id || '/' || folder.id || '/' || folder.uidvalidity || '/' || ?2
                   FROM folder JOIN account ON account.key = folder.account
                  WHERE folder.name = ?1",
                rusqlite::params![folder, uid],
                |row| row.get(0),
            )
            .unwrap()
    }

    /// `(folder, uid, flags)` of every message, by the folder it is shown in: a message whose move
    /// waits is shown in its target, under the UID the server still has it at.
    fn shown(&self) -> Vec<(String, u32, String)> {
        let copy = self.copy().unwrap();
        let mut statement = copy
            .prepare(
                "SELECT folder.name, message.uid, message.flags FROM message \
                 JOIN folder ON folder.id = coalesce(message.moving_to, message.folder) \
                 ORDER BY folder.name, message.uid",
            )
            .unwrap();
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    /// The UIDs shown in a folder.
    fn shown_uids(&self, folder: &str) -> Vec<u32> {
        self.shown()
            .into_iter()
            .filter(|(f, _, _)| f == folder)
            .map(|(_, uid, _)| uid)
            .collect()
    }

    /// How many actions wait in the queue.
    fn queue(&self) -> i64 {
        self.copy()
            .unwrap()
            .query_row("SELECT count(*) FROM action", [], |row| row.get(0))
            .unwrap()
    }

    /// A `missive` command that must answer within 20 seconds.
    async fn act(&self, args: &[&str]) -> Output {
        tokio::time::timeout(Duration::from_secs(20), self.run(args))
            .await
            .unwrap_or_else(|_| panic!("{args:?} did not answer; the log:\n{}", self.logs()))
    }
}

/// The commands of the log from `from` on, but those of the sync and of IDLE: what applying the
/// actions sent.
fn writes(server: &FakeServer, from: usize) -> Vec<String> {
    server.imap_log()[from..]
        .iter()
        .filter(|command| {
            let upper = command.to_ascii_uppercase();
            let name = upper.split(' ').next().unwrap();
            let background = matches!(
                name,
                "CAPABILITY"
                    | "LOGIN"
                    | "ENABLE"
                    | "LIST"
                    | "EXAMINE"
                    | "IDLE"
                    | "DONE"
                    | "NOOP"
                    | "LOGOUT"
            ) || upper.starts_with("UID FETCH ")
                || upper == "UID SEARCH ALL";
            !background
        })
        .cloned()
        .collect()
}

/// Waits until the commands sent to apply actions from `from` on are `expected`, and checks that
/// no more come.
async fn wrote(server: &FakeServer, from: usize, expected: &[&str]) {
    eventually(20, "the commands of the actions", || {
        writes(server, from).len() >= expected.len()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(writes(server, from), expected);
}

/// A server, an account on it, a daemon, and a first sync. The periodic check is an hour away:
/// the test says when a pass runs, with `missive sync`.
async fn started(builder: missive_core::fake::Builder) -> (FakeServer, Env, Daemon) {
    started_with(builder, &[]).await
}

/// [`started`], the daemon given `args` too.
async fn started_with(
    builder: missive_core::fake::Builder,
    args: &[&str],
) -> (FakeServer, Env, Daemon) {
    let server = builder.start().await;
    let env = Env::new().await;
    env.add_account("alice", &server, USER, Some(PASSWORD))
        .await;
    let mut arguments = vec!["--check-interval", "3600"];
    arguments.extend_from_slice(args);
    let daemon = env.daemon(&arguments).await;
    succeeded(&sync_soon(&env).await);
    (server, env, daemon)
}

fn flags(list: &[&str]) -> Option<Vec<String>> {
    Some(list.iter().map(|f| (*f).to_owned()).collect())
}

#[tokio::test(flavor = "multi_thread")]
async fn each_mark_is_one_uid_store_on_the_server_and_shows_in_the_copy_at_once() {
    let (server, env, _daemon) = started(mailbox()).await;
    let cases = [
        (
            "--read",
            3,
            &["\\Seen"][..],
            "UID STORE 3 +FLAGS (\\Seen)",
            "\\Seen",
        ),
        ("--unread", 1, &[][..], "UID STORE 1 -FLAGS (\\Seen)", ""),
        (
            "--flagged",
            3,
            &["\\Seen", "\\Flagged"][..],
            "UID STORE 3 +FLAGS (\\Flagged)",
            "\\Flagged \\Seen",
        ),
        (
            "--unflagged",
            2,
            &[][..],
            "UID STORE 2 -FLAGS (\\Flagged)",
            "",
        ),
    ];
    for (mark, uid, on_server, command, in_copy) in cases {
        let from = server.imap_log().len();

        let output = env.act(&["mark", &env.id("INBOX", uid), mark]).await;

        succeeded(&output);
        assert_eq!(
            stdout(&output),
            format!(
                "1 message marked {}; the server is told as soon as the account is connected.\n",
                &mark[2..]
            )
        );
        // In the copy as soon as it answered.
        assert_eq!(env.flags("INBOX", uid).as_deref(), Some(in_copy), "{mark}");
        wrote(&server, from, &["SELECT \"INBOX\"", command]).await;
        assert_eq!(server.flags("INBOX", uid), flags(on_server), "{mark}");
        succeeded(&sync_soon(&env).await);
        assert_eq!(env.flags("INBOX", uid).as_deref(), Some(in_copy), "{mark}");
    }
    assert_eq!(env.queue(), 0);
    // Nothing else changed on the server.
    assert_eq!(server.uids("INBOX"), [1, 2, 3, 4]);
    assert_eq!(server.flags("INBOX", 4), flags(&["\\Deleted"]));
}

#[tokio::test(flavor = "multi_thread")]
async fn several_messages_of_several_folders_are_one_store_per_folder() {
    let (server, env, _daemon) = started(mailbox()).await;
    let from = server.imap_log().len();
    let ids = [env.id("INBOX", 3), env.id("Trash", 1), env.id("INBOX", 2)];

    let output = env
        .act(&["mark", &ids[0], &ids[1], &ids[2], "--read", "--json"])
        .await;

    succeeded(&output);
    let answer: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(answer, serde_json::json!({ "messages": ids }));
    wrote(
        &server,
        from,
        &[
            "SELECT \"INBOX\"",
            "UID STORE 2:3 +FLAGS (\\Seen)",
            "SELECT \"Trash\"",
            "UID STORE 1 +FLAGS (\\Seen)",
        ],
    )
    .await;
    assert_eq!(server.flags("INBOX", 2), flags(&["\\Flagged", "\\Seen"]));
    assert_eq!(server.flags("Trash", 1), flags(&["\\Seen"]));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_move_on_a_server_with_move_is_one_uid_move_and_the_message_keeps_its_row_under_its_new_uid()
 {
    let (server, env, _daemon) = started(mailbox()).await;
    let from = server.imap_log().len();
    let raw = server.raw("INBOX", 3).unwrap();

    let output = env
        .act(&["move", &env.id("INBOX", 3), "--to", "archive"])
        .await;

    succeeded(&output);
    // Shown in the archive at once, under the UID the server still has it at.
    assert_eq!(env.shown_uids("Archive"), [3]);
    assert_eq!(env.shown_uids("INBOX"), [1, 2, 4]);
    wrote(
        &server,
        from,
        &["SELECT \"INBOX\"", "UID MOVE 3 \"Archive\""],
    )
    .await;
    assert_eq!(server.uids("INBOX"), [1, 2, 4]);
    assert_eq!(server.uids("Archive"), [1]);
    assert_eq!(server.raw("Archive", 1).unwrap(), raw);
    // COPYUID gave the new UID: the row went to it, and the message is not fetched again.
    eventually(10, "the row at its new UID", || env.uids("Archive") == [1]).await;
    succeeded(&sync_soon(&env).await);
    assert_eq!(env.uids("Archive"), [1]);
    assert_eq!(env.uids("INBOX"), [1, 2, 4]);
    let fetched = server.imap_log()[from..]
        .iter()
        .filter(|c| c.contains("BODY.PEEK[]"))
        .count();
    assert_eq!(fetched, 0);
    assert_eq!(env.queue(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_move_on_a_server_with_only_uidplus_copies_then_expunges_only_that_uid() {
    let (server, env, _daemon) = started(mailbox().without_move()).await;
    let from = server.imap_log().len();

    succeeded(
        &env.act(&["move", &env.id("INBOX", 3), "--to", "Archive"])
            .await,
    );

    wrote(
        &server,
        from,
        &[
            "SELECT \"INBOX\"",
            "STATUS \"Archive\" (UIDNEXT)",
            "UID COPY 3 \"Archive\"",
            "UID SEARCH DELETED UID 3",
            "UID STORE 3 +FLAGS.SILENT (\\Deleted)",
            "UID EXPUNGE 3",
        ],
    )
    .await;
    // The message another client marked deleted is still there.
    assert_eq!(server.uids("INBOX"), [1, 2, 4]);
    assert_eq!(server.uids("Archive"), [1]);
    eventually(10, "the row at its new UID", || env.uids("Archive") == [1]).await;
    succeeded(&sync_soon(&env).await);
    assert_eq!(env.uids("Archive"), [1]);
    assert_eq!(env.uids("INBOX"), [1, 2, 4]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_without_move_or_uidplus_refuses_the_move_and_nothing_changes() {
    let (server, env, _daemon) = started(mailbox().without_move().without_uidplus()).await;
    // As a copy that does not know the server's capabilities yet: the action is queued, and the
    // worker refuses it.
    env.forget_capabilities();
    let before = env.messages();
    let from = server.imap_log().len();

    succeeded(
        &env.act(&["move", &env.id("INBOX", 3), "--to", "archive"])
            .await,
    );

    eventually(10, "the move refused", || env.queue() == 0).await;
    assert_eq!(env.shown_uids("INBOX"), [1, 2, 3, 4]);
    assert_eq!(env.messages(), before);
    assert!(
        writes(&server, from).is_empty(),
        "{:?}",
        writes(&server, from)
    );
    assert_eq!(server.uids("INBOX"), [1, 2, 3, 4]);
    assert!(server.uids("Archive").is_empty());
    // The queue is empty once the row left; the daemon logs the refusal around that, and a log
    // read in between holds the line cut short.
    eventually(10, "the refusal in the log", || {
        env.logs()
            .contains("refused: the server has neither MOVE nor UIDPLUS")
    })
    .await;
    // A permanent delete is refused the same way.
    succeeded(
        &env.act(&["delete", "--permanently", &env.id("Trash", 1)])
            .await,
    );
    eventually(10, "the delete refused", || env.queue() == 0).await;
    assert!(writes(&server, from).is_empty());
    succeeded(&sync_soon(&env).await);
    assert_eq!(env.uids("Trash"), [1, 2]);
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_moves_to_the_trash() {
    let (server, env, _daemon) = started(mailbox()).await;
    let from = server.imap_log().len();

    let output = env.act(&["delete", &env.id("INBOX", 3)]).await;

    succeeded(&output);
    assert!(
        stdout(&output).contains("moved to the trash"),
        "{}",
        stdout(&output)
    );
    assert_eq!(env.shown_uids("Trash"), [1, 2, 3]);
    wrote(&server, from, &["SELECT \"INBOX\"", "UID MOVE 3 \"Trash\""]).await;
    assert_eq!(server.uids("Trash"), [1, 2, 3]);
    eventually(10, "the row at its new UID", || {
        env.uids("Trash") == [1, 2, 3]
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_permanently_from_the_trash_expunges_that_uid_only() {
    let (server, env, _daemon) = started(mailbox()).await;
    let from = server.imap_log().len();

    let output = env
        .act(&["delete", &env.id("Trash", 1), "--permanently"])
        .await;

    succeeded(&output);
    assert_eq!(env.uids("Trash"), [2]);
    wrote(
        &server,
        from,
        &[
            "SELECT \"Trash\"",
            "UID SEARCH DELETED UID 1",
            "UID STORE 1 +FLAGS.SILENT (\\Deleted)",
            "UID EXPUNGE 1",
        ],
    )
    .await;
    assert_eq!(server.uids("Trash"), [2]);
    succeeded(&sync_soon(&env).await);
    assert_eq!(env.uids("Trash"), [2]);
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_permanently_outside_the_trash_is_refused() {
    let (server, env, _daemon) = started(mailbox()).await;
    let from = server.imap_log().len();
    let before = env.messages();

    let output = env
        .act(&["delete", "--permanently", &env.id("INBOX", 3)])
        .await;

    failed(&output);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("not in the trash"),
        "{}",
        stderr(&output)
    );
    // And a plain delete of a message already in the trash says how to delete it for good.
    let output = env.act(&["delete", &env.id("Trash", 1)]).await;
    failed(&output);
    assert!(
        stderr(&output).contains("--permanently"),
        "{}",
        stderr(&output)
    );
    assert_eq!(env.queue(), 0);
    assert_eq!(env.messages(), before);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(writes(&server, from).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_trash_delete_says_so_and_delete_permanently_is_accepted() {
    let server = FakeServer::builder()
        .user(USER, PASSWORD)
        .folder(Folder::new("INBOX").message(message(1)).message(message(2)))
        .start()
        .await;
    let env = Env::new().await;
    env.add_account("alice", &server, USER, Some(PASSWORD))
        .await;
    let _daemon = env.daemon(&["--check-interval", "3600"]).await;
    succeeded(&sync_soon(&env).await);

    let output = env.act(&["delete", &env.id("INBOX", 1)]).await;
    failed(&output);
    assert!(
        stderr(&output).contains("no trash folder"),
        "{}",
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("--permanently"),
        "{}",
        stderr(&output)
    );

    succeeded(
        &env.act(&["delete", "--permanently", &env.id("INBOX", 1)])
            .await,
    );
    eventually(10, "the message deleted for good", || {
        server.uids("INBOX") == [2]
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_marked_deleted_by_another_client_survives_a_move_and_a_permanent_delete() {
    for builder in [mailbox(), mailbox().without_move()] {
        let (server, env, _daemon) = started(builder).await;

        succeeded(
            &env.act(&["move", &env.id("INBOX", 3), "--to", "archive"])
                .await,
        );
        succeeded(
            &env.act(&["delete", "--permanently", &env.id("Trash", 1)])
                .await,
        );

        eventually(20, "both actions applied", || {
            server.uids("INBOX") == [1, 2, 4] && server.uids("Trash") == [2]
        })
        .await;
        assert_eq!(server.flags("INBOX", 4), flags(&["\\Deleted"]));
        assert_eq!(server.flags("Trash", 2), flags(&["\\Deleted"]));
        assert!(
            !server
                .imap_log()
                .iter()
                .any(|c| c == "EXPUNGE" || c == "CLOSE"),
            "{:?}",
            server.imap_log()
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_message_is_refused_with_its_exit_status_and_nothing_is_recorded() {
    let (_server, env, _daemon) = started(mailbox()).await;
    let good = env.id("INBOX", 3);
    for bad in ["alice/1", "alice/1/100/99", "nobody/1/100/3"] {
        let output = env.act(&["mark", &good, bad, "--read"]).await;
        failed(&output);
        let expected = if bad.starts_with("nobody") { 1 } else { 3 };
        assert_eq!(
            output.status.code(),
            Some(expected),
            "{bad}: {}",
            stderr(&output)
        );
    }
    let output = env.act(&["move", &good, "--to", "Nowhere-Folder"]).await;
    failed(&output);
    assert!(stderr(&output).contains("no folder"), "{}", stderr(&output));
    assert_eq!(env.queue(), 0);
    assert_eq!(env.flags("INBOX", 3).as_deref(), Some(""));
}

/// A server reached through a relay, with an account and a first sync, then a daemon stopped
/// and the relay switched off: the next daemon finds the server down.
async fn offline() -> (FakeServer, Relay, Env) {
    offline_from(mailbox()).await
}

/// [`offline`], on a server `builder` describes.
async fn offline_from(builder: missive_core::fake::Builder) -> (FakeServer, Relay, Env) {
    let server = builder.start().await;
    let relay = Relay::start(server.imap_tls_addr()).await;
    relay.up.store(true, Ordering::SeqCst);
    let env = Env::new().await;
    env.add_account_on("alice", &server, relay.port, USER, Some(PASSWORD))
        .await;
    let mut daemon = env.daemon(&["--check-interval", "3600"]).await;
    succeeded(&sync_soon(&env).await);
    assert!(daemon.stop("-TERM").await.success());
    relay.up.store(false, Ordering::SeqCst);
    (server, relay, env)
}

/// Asks, offline, for actions whose order matters, each answered at once.
async fn ask_offline(env: &Env) {
    let three = env.id("INBOX", 3);
    let one = env.id("INBOX", 1);
    for args in [
        vec!["mark", &three, "--read"],
        vec!["mark", &one, "--unread"],
        vec!["mark", &one, "--read"],
        vec!["move", &three, "--to", "archive"],
    ] {
        let started = Instant::now();
        let output = env.act(&args).await;
        succeeded(&output);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{args:?} waited for the server"
        );
    }
    assert_eq!(env.state("alice").as_deref(), Some("unreachable"));
    assert_eq!(env.queue(), 4);
    assert_eq!(env.shown_uids("Archive"), [3]);
    assert_eq!(env.flags("INBOX", 1).as_deref(), Some("\\Seen"));
}

/// What the queue of [`ask_offline`] sends once the server is back, in order.
const OFFLINE_WRITES: &[&str] = &[
    "SELECT \"INBOX\"",
    "UID STORE 3 +FLAGS (\\Seen)",
    "SELECT \"INBOX\"",
    "UID STORE 1 -FLAGS (\\Seen)",
    "SELECT \"INBOX\"",
    "UID STORE 1 +FLAGS (\\Seen)",
    "SELECT \"INBOX\"",
    "UID MOVE 3 \"Archive\"",
];

async fn applied_in_order(server: &FakeServer, env: &Env, from: usize) {
    wrote(server, from, OFFLINE_WRITES).await;
    assert_eq!(server.uids("INBOX"), [1, 2, 4]);
    assert_eq!(server.flags("INBOX", 1), flags(&["\\Seen"]));
    assert_eq!(server.uids("Archive"), [1]);
    assert_eq!(server.flags("Archive", 1), flags(&["\\Seen"]));
    eventually(10, "the queue empty", || env.queue() == 0).await;
    succeeded(&sync_soon(env).await);
    assert_eq!(env.uids("INBOX"), [1, 2, 4]);
    assert_eq!(env.uids("Archive"), [1]);
    assert_eq!(env.flags("Archive", 1).as_deref(), Some("\\Seen"));
}

#[tokio::test(flavor = "multi_thread")]
async fn actions_asked_while_the_server_is_down_are_applied_in_order_once_it_is_back() {
    let (server, relay, env) = offline().await;
    let _daemon = env.daemon(&["--check-interval", "3600"]).await;
    eventually(10, "unreachable", || {
        env.state("alice").as_deref() == Some("unreachable")
    })
    .await;
    let from = server.imap_log().len();

    ask_offline(&env).await;
    relay.up.store(true, Ordering::SeqCst);

    applied_in_order(&server, &env, from).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn actions_asked_offline_are_applied_after_a_daemon_restart() {
    let (server, relay, env) = offline().await;
    let mut daemon = env.daemon(&["--check-interval", "3600"]).await;
    eventually(10, "unreachable", || {
        env.state("alice").as_deref() == Some("unreachable")
    })
    .await;
    let from = server.imap_log().len();
    ask_offline(&env).await;
    daemon.child.kill().await.unwrap();

    relay.up.store(true, Ordering::SeqCst);
    let _restarted = env.daemon(&["--check-interval", "3600"]).await;

    applied_in_order(&server, &env, from).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sync_while_a_mark_waits_does_not_mark_the_message_unread_again() {
    let (server, env, _daemon) = started(mailbox()).await;
    server.refuse("UID STORE");

    succeeded(&env.act(&["mark", &env.id("INBOX", 3), "--read"]).await);
    eventually(10, "a NO", || {
        server.imap_log().iter().any(|c| c.starts_with("UID STORE"))
    })
    .await;
    // Passes run while it waits: the server still says unread.
    succeeded(&sync_soon(&env).await);
    succeeded(&sync_soon(&env).await);

    assert_eq!(env.flags("INBOX", 3).as_deref(), Some("\\Seen"));
    assert_eq!(env.queue(), 1);
    assert_eq!(server.flags("INBOX", 3), Some(Vec::new()));
    server.heal();
    succeeded(&sync_soon(&env).await);
    assert_eq!(server.flags("INBOX", 3), flags(&["\\Seen"]));
    assert_eq!(env.queue(), 0);
    assert_eq!(env.flags("INBOX", 3).as_deref(), Some("\\Seen"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sync_while_a_move_waits_does_not_bring_the_message_back_in_its_old_folder() {
    let (server, env, _daemon) = started(mailbox()).await;
    server.refuse("UID MOVE");

    succeeded(
        &env.act(&["move", &env.id("INBOX", 3), "--to", "archive"])
            .await,
    );
    eventually(10, "a NO", || {
        server.imap_log().iter().any(|c| c.starts_with("UID MOVE"))
    })
    .await;
    succeeded(&sync_soon(&env).await);

    assert_eq!(env.shown_uids("INBOX"), [1, 2, 4]);
    assert_eq!(env.shown_uids("Archive"), [3]);
    assert_eq!(env.messages().len(), 7);
    server.heal();
    succeeded(&sync_soon(&env).await);
    assert_eq!(server.uids("Archive"), [1]);
    assert_eq!(server.uids("INBOX"), [1, 2, 4]);
    succeeded(&sync_soon(&env).await);
    assert_eq!(env.uids("INBOX"), [1, 2, 4]);
    assert_eq!(env.uids("Archive"), [1]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_no_on_store_and_on_move_is_tried_again_then_refused_and_the_copy_matches_the_server() {
    let (server, env, _daemon) = started(mailbox()).await;
    server.refuse("UID STORE");
    server.refuse("UID MOVE");
    let tries = |command: &str| {
        server
            .imap_log()
            .iter()
            .filter(|c| c.starts_with(command))
            .count()
    };
    succeeded(&env.act(&["mark", &env.id("INBOX", 3), "--read"]).await);
    eventually(10, "a first NO", || tries("UID STORE") == 1).await;

    // Then one try per pass, not one at each wake, until the fifth NO refuses it.
    for pass in 2..=5 {
        succeeded(&sync_soon(&env).await);
        assert_eq!(tries("UID STORE"), pass);
        assert_eq!(env.queue(), if pass < 5 { 1 } else { 0 });
        // Meanwhile the copy keeps it, whatever the server says.
        if pass < 5 {
            assert_eq!(env.flags("INBOX", 3).as_deref(), Some("\\Seen"));
        }
    }
    // Refused, it is taken back: the copy shows what the server has.
    eventually(10, "the mark taken back", || {
        env.flags("INBOX", 3).as_deref() == Some("")
    })
    .await;

    succeeded(
        &env.act(&["move", &env.id("INBOX", 3), "--to", "archive"])
            .await,
    );
    eventually(10, "a first NO", || tries("UID MOVE") == 1).await;
    for _ in 2..=5 {
        succeeded(&sync_soon(&env).await);
    }
    assert_eq!(tries("UID MOVE"), 5);
    assert_eq!(env.queue(), 0);
    assert_eq!(env.shown_uids("INBOX"), [1, 2, 3, 4]);
    assert!(env.shown_uids("Archive").is_empty());
    assert_eq!(server.uids("INBOX"), [1, 2, 3, 4]);
    assert_eq!(server.flags("INBOX", 3), Some(Vec::new()));
}

/// Cuts the connection right after the server ran `command`, before its answer, once.
async fn cut_once(server: &FakeServer, command: &str) {
    server.cut(command, 0);
    eventually(10, command, || {
        server.imap_log().iter().any(|c| c.starts_with(command))
    })
    .await;
    server.heal();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_connection_cut_after_a_move_was_sent_loses_and_duplicates_nothing() {
    let (server, env, _daemon) = started(mailbox()).await;
    succeeded(
        &env.act(&["move", &env.id("INBOX", 3), "--to", "archive"])
            .await,
    );
    cut_once(&server, "UID MOVE").await;

    // The worker connects again and applies the queue: the UID is gone from the inbox, so the
    // second MOVE moves nothing.
    eventually(20, "the queue empty", || env.queue() == 0).await;
    succeeded(&sync_soon(&env).await);

    assert_eq!(server.uids("INBOX"), [1, 2, 4]);
    assert_eq!(server.uids("Archive"), [1]);
    assert_eq!(env.uids("INBOX"), [1, 2, 4]);
    assert_eq!(env.uids("Archive"), [1]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_connection_cut_after_a_store_was_sent_loses_nothing() {
    let (server, env, _daemon) = started(mailbox()).await;
    succeeded(&env.act(&["mark", &env.id("INBOX", 3), "--read"]).await);
    cut_once(&server, "UID STORE").await;

    eventually(20, "the queue empty", || env.queue() == 0).await;
    succeeded(&sync_soon(&env).await);

    assert_eq!(server.flags("INBOX", 3), flags(&["\\Seen"]));
    assert_eq!(env.flags("INBOX", 3).as_deref(), Some("\\Seen"));
    assert_eq!(server.uids("INBOX"), [1, 2, 3, 4]);
    assert_eq!(env.uids("INBOX"), [1, 2, 3, 4]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_connection_cut_after_a_copy_was_sent_does_not_copy_twice() {
    let (server, env, _daemon) = started(mailbox().without_move()).await;
    let from = server.imap_log().len();
    succeeded(
        &env.act(&["move", &env.id("INBOX", 3), "--to", "archive"])
            .await,
    );
    cut_once(&server, "UID COPY").await;

    eventually(20, "the queue empty", || env.queue() == 0).await;
    succeeded(&sync_soon(&env).await);

    assert_eq!(server.uids("Archive"), [1]);
    assert_eq!(server.uids("INBOX"), [1, 2, 4]);
    assert_eq!(env.uids("Archive"), [1]);
    assert_eq!(env.uids("INBOX"), [1, 2, 4]);
    // The copy was found by its Message-ID, not made again.
    let log = writes(&server, from);
    assert_eq!(
        log.iter().filter(|c| c.starts_with("UID COPY")).count(),
        1,
        "{log:?}"
    );
    assert!(
        log.iter()
            .any(|c| c == "UID SEARCH UID 1:* HEADER Message-ID \"<3@example.com>\""),
        "{log:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_action_on_a_message_the_server_no_longer_has_is_dropped_without_touching_anything_else()
{
    let (server, relay, env) = offline().await;
    let _daemon = env.daemon(&["--check-interval", "3600"]).await;
    eventually(10, "unreachable", || {
        env.state("alice").as_deref() == Some("unreachable")
    })
    .await;
    succeeded(&env.act(&["mark", &env.id("INBOX", 3), "--flagged"]).await);
    succeeded(
        &env.act(&["move", &env.id("INBOX", 2), "--to", "archive"])
            .await,
    );
    // Meanwhile another client expunged both.
    server.expunge("INBOX", 3);
    server.expunge("INBOX", 2);

    relay.up.store(true, Ordering::SeqCst);
    eventually(20, "the queue empty", || env.queue() == 0).await;
    succeeded(&sync_soon(&env).await);

    assert_eq!(server.uids("INBOX"), [1, 4]);
    assert!(server.uids("Archive").is_empty());
    assert_eq!(server.flags("INBOX", 1), flags(&["\\Seen"]));
    assert_eq!(server.flags("INBOX", 4), flags(&["\\Deleted"]));
    assert_eq!(env.uids("INBOX"), [1, 4]);
    assert!(env.uids("Archive").is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn watch_tells_when_an_action_is_applied_retried_or_refused() {
    let (server, env, _daemon) = started(mailbox().without_move().without_uidplus()).await;
    let mut stream = UnixStream::connect(env.socket()).await.unwrap();
    stream
        .write_all(b"{\"protocol\":1,\"request\":\"subscribe\"}\n")
        .await
        .unwrap();
    let mut lines = BufReader::new(stream).lines();
    let mut next_action = async || loop {
        let line = tokio::time::timeout(Duration::from_secs(20), lines.next_line())
            .await
            .expect("an event in time")
            .unwrap()
            .unwrap();
        let event: serde_json::Value = serde_json::from_str(&line).unwrap();
        if event["event"] == "action" {
            return event;
        }
    };
    let three = env.id("INBOX", 3);

    succeeded(&env.act(&["mark", &three, "--read"]).await);
    assert_eq!(
        next_action().await,
        serde_json::json!({
            "protocol": 1, "event": "action", "account": "alice", "action": "read",
            "outcome": "applied", "messages": [three], "id": 1,
        })
    );
    server.refuse("UID STORE");
    succeeded(&env.act(&["mark", &three, "--flagged"]).await);
    assert_eq!(
        next_action().await,
        serde_json::json!({
            "protocol": 1, "event": "action", "account": "alice", "action": "flagged",
            "outcome": "retried", "messages": [three], "id": 2, "reason": "refused",
            "attempts": 1,
        })
    );
    server.heal();
    succeeded(&sync_soon(&env).await);
    assert_eq!(next_action().await["outcome"], "applied");
    env.forget_capabilities();
    succeeded(&env.act(&["move", &three, "--to", "archive"]).await);
    let refused = next_action().await;
    assert_eq!(refused["action"], "move");
    assert_eq!(refused["outcome"], "refused");
    assert_eq!(refused["reason"], "unsupported");
    assert!(
        refused["message"].as_str().unwrap().contains("MOVE"),
        "{refused}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn no_subject_address_or_body_appears_in_the_logs_of_actions() {
    let (server, env, _daemon) = started(mailbox()).await;
    let mut outputs = vec![
        env.act(&["mark", &env.id("INBOX", 3), "--read"]).await,
        env.act(&["delete", &env.id("INBOX", 2)]).await,
        env.act(&["delete", "--permanently", &env.id("INBOX", 1)])
            .await,
    ];
    server.refuse("UID MOVE");
    succeeded(
        &env.act(&["move", &env.id(PRIVATE_FOLDER, 1), "--to", "archive"])
            .await,
    );
    succeeded(
        &env.act(&["move", &env.id("INBOX", 3), "--to", PRIVATE_FOLDER])
            .await,
    );
    outputs.push(sync_soon(&env).await);
    server.heal();
    outputs.push(sync_soon(&env).await);
    eventually(20, "the queue empty", || env.queue() == 0).await;

    let logs = env.logs();
    assert!(logs.contains("action"), "{logs}");
    for private in PRIVATE {
        assert!(!logs.contains(private), "the log shows {private}:\n{logs}");
        for output in &outputs {
            assert!(!stdout(output).contains(private), "{}", stdout(output));
            assert!(!stderr(output).contains(private), "{}", stderr(output));
        }
    }
}

/// How many commands of the server's log start with `command`.
fn tries(server: &FakeServer, command: &str) -> usize {
    server
        .imap_log()
        .iter()
        .filter(|c| c.starts_with(command))
        .count()
}

/// Delivers a new message to the inbox and waits until the copy has it.
async fn new_mail_is_synced(server: &FakeServer, env: &Env, n: u32) {
    let uid = server.deliver("INBOX", message(n));
    eventually(20, "the new message in the copy", || {
        env.uids("INBOX").contains(&uid)
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_action_the_server_always_answers_by_cutting_the_connection_is_refused_after_the_limit_and_new_mail_is_still_synced()
 {
    let (server, env, _daemon) = started(mailbox().cut("UID STORE", 0)).await;
    succeeded(&env.act(&["mark", &env.id("INBOX", 3), "--read"]).await);
    eventually(10, "a first cut", || tries(&server, "UID STORE") >= 1).await;

    // Each connection syncs the folders before it tries the action again.
    new_mail_is_synced(&server, &env, 5).await;
    assert_eq!(env.queue(), 1, "{}", env.logs());

    eventually(60, "the action refused", || env.queue() == 0).await;
    assert_eq!(tries(&server, "UID STORE"), 5);
    new_mail_is_synced(&server, &env, 6).await;
    // The server ran each STORE before it cut its answer: the copy shows what it has.
    succeeded(&sync_soon(&env).await);
    assert_eq!(server.flags("INBOX", 3), flags(&["\\Seen"]));
    assert_eq!(env.flags("INBOX", 3).as_deref(), Some("\\Seen"));
    let logs = env.logs();
    assert!(logs.contains("refused"), "{logs}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_action_the_server_always_answers_bad_is_refused_after_the_limit_and_new_mail_is_still_synced()
 {
    // Through the sync's reader, a BAD is an error: here on the SELECT only an action sends.
    let (server, env, _daemon) = started(mailbox().bad("SELECT")).await;
    succeeded(&env.act(&["mark", &env.id("INBOX", 3), "--read"]).await);
    eventually(10, "a first BAD", || tries(&server, "SELECT") == 1).await;

    for pass in 2..=5 {
        succeeded(&sync_soon(&env).await);
        new_mail_is_synced(&server, &env, 10 + pass).await;
        eventually(10, "one more try", || {
            tries(&server, "SELECT") == pass as usize
        })
        .await;
        if pass < 5 {
            assert_eq!(env.queue(), 1);
        }
    }
    eventually(10, "the action refused", || env.queue() == 0).await;
    succeeded(&sync_soon(&env).await);
    assert_eq!(tries(&server, "SELECT"), 5);
    assert_eq!(server.flags("INBOX", 3), Some(Vec::new()));
    eventually(10, "the mark taken back", || {
        env.flags("INBOX", 3).as_deref() == Some("")
    })
    .await;
    new_mail_is_synced(&server, &env, 20).await;
    assert_eq!(env.state("alice").as_deref(), Some("ready"));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_action_command_that_never_answers_times_out_drops_the_connection_and_counts_as_a_failure()
 {
    let (server, env, _daemon) =
        started_with(mailbox().stall("UID STORE"), &["--action-timeout", "1"]).await;
    let logins = tries(&server, "LOGIN");
    succeeded(&env.act(&["mark", &env.id("INBOX", 3), "--read"]).await);
    eventually(10, "a first STORE", || tries(&server, "UID STORE") == 1).await;
    // The connection is dropped and another one made.
    eventually(10, "a new connection", || tries(&server, "LOGIN") > logins).await;
    new_mail_is_synced(&server, &env, 5).await;

    eventually(60, "the action refused", || env.queue() == 0).await;
    // Each timeout dropped its connection, the fifth included.
    eventually(10, "a connection after each timeout", || {
        tries(&server, "LOGIN") > logins + 4
    })
    .await;
    succeeded(&sync_soon(&env).await);
    assert_eq!(tries(&server, "UID STORE"), 5);
    assert_eq!(server.flags("INBOX", 3), Some(Vec::new()));
    eventually(10, "the mark taken back", || {
        env.flags("INBOX", 3).as_deref() == Some("")
    })
    .await;
    new_mail_is_synced(&server, &env, 6).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_uid_expunge_takes_the_deleted_flag_back_on_the_server() {
    // NO, until the limit refuses the move.
    let (server, env, _daemon) = started(mailbox().without_move().refuse("UID EXPUNGE")).await;
    succeeded(
        &env.act(&["move", &env.id("INBOX", 3), "--to", "archive"])
            .await,
    );
    eventually(10, "a first NO", || tries(&server, "UID EXPUNGE") == 1).await;
    for pass in 2..=5 {
        succeeded(&sync_soon(&env).await);
        eventually(10, "one more try", || tries(&server, "UID EXPUNGE") == pass).await;
    }
    eventually(10, "the move refused", || env.queue() == 0).await;
    assert_eq!(server.uids("INBOX"), [1, 2, 3, 4]);
    assert_eq!(server.flags("INBOX", 3), Some(Vec::new()));
    // What another client marked is left alone.
    assert_eq!(server.flags("INBOX", 4), flags(&["\\Deleted"]));
    assert!(
        server
            .imap_log()
            .iter()
            .any(|c| c == "UID STORE 3 -FLAGS.SILENT (\\Deleted)"),
        "{:?}",
        server.imap_log()
    );

    // BAD: refused at once, the move and a permanent delete alike.
    let (server, env, _daemon) = started(mailbox().without_move().bad("UID EXPUNGE")).await;
    succeeded(
        &env.act(&["move", &env.id("INBOX", 3), "--to", "archive"])
            .await,
    );
    succeeded(
        &env.act(&["delete", "--permanently", &env.id("Trash", 1)])
            .await,
    );
    eventually(20, "both refused", || env.queue() == 0).await;
    succeeded(&sync_soon(&env).await);
    assert_eq!(server.uids("INBOX"), [1, 2, 3, 4]);
    assert_eq!(server.flags("INBOX", 3), Some(Vec::new()));
    assert_eq!(server.flags("INBOX", 4), flags(&["\\Deleted"]));
    assert_eq!(server.uids("Trash"), [1, 2]);
    assert_eq!(server.flags("Trash", 1), Some(Vec::new()));
    assert_eq!(server.flags("Trash", 2), flags(&["\\Deleted"]));
    assert_eq!(env.uids("Trash"), [1, 2]);
}

/// The Message-IDs of a folder on the server, in UID order.
fn message_ids(server: &FakeServer, folder: &str) -> Vec<String> {
    server
        .uids(folder)
        .into_iter()
        .map(|uid| {
            let raw = String::from_utf8(server.raw(folder, uid).unwrap()).unwrap();
            raw.lines()
                .find_map(|line| line.strip_prefix("Message-ID: "))
                .unwrap()
                .to_owned()
        })
        .collect()
}

/// Checks that the removal from the source came after the last `UID COPY`.
fn removed_after_the_last_copy(server: &FakeServer) {
    let log = server.imap_log();
    let last_copy = log.iter().rposition(|c| c.starts_with("UID COPY")).unwrap();
    let store = log
        .iter()
        .position(|c| c.contains("+FLAGS.SILENT (\\Deleted)"))
        .unwrap();
    let expunge = log
        .iter()
        .position(|c| c.starts_with("UID EXPUNGE"))
        .unwrap();
    assert!(last_copy < store && store < expunge, "{log:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_copy_whose_answer_never_came_and_that_copied_nothing_is_copied_again() {
    let (server, env, _daemon) = started_with(
        mailbox().without_move().stall("UID COPY"),
        &["--action-timeout", "1"],
    )
    .await;
    succeeded(
        &env.act(&["move", &env.id("INBOX", 3), "--to", "archive"])
            .await,
    );
    // The server never ran it, and never answered.
    eventually(10, "the COPY", || tries(&server, "UID COPY") == 1).await;
    server.heal();

    eventually(30, "the queue empty", || env.queue() == 0).await;
    succeeded(&sync_soon(&env).await);

    assert_eq!(tries(&server, "UID COPY"), 2);
    // The target's UIDNEXT had not moved: nothing was looked for there.
    assert_eq!(
        tries(&server, "UID SEARCH UID"),
        0,
        "{:?}",
        server.imap_log()
    );
    assert_eq!(message_ids(&server, "Archive"), ["<3@example.com>"]);
    assert_eq!(server.uids("INBOX"), [1, 2, 4]);
    removed_after_the_last_copy(&server);
    assert_eq!(env.uids("Archive"), server.uids("Archive"));
    assert_eq!(env.uids("INBOX"), [1, 2, 4]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_copy_whose_answer_never_came_and_that_copied_some_messages_copies_only_the_others() {
    let (server, env, _daemon) = started_with(
        mailbox().without_move().stall("UID COPY"),
        &["--action-timeout", "1"],
    )
    .await;
    succeeded(
        &env.act(&[
            "move",
            &env.id("INBOX", 2),
            &env.id("INBOX", 3),
            "--to",
            "archive",
        ])
        .await,
    );
    eventually(10, "the COPY", || tries(&server, "UID COPY") == 1).await;
    // As a server that copied one of the two before the connection was lost.
    server.deliver("Archive", message(3));
    server.heal();

    eventually(30, "the queue empty", || env.queue() == 0).await;
    succeeded(&sync_soon(&env).await);

    let log = server.imap_log();
    let copies: Vec<&String> = log.iter().filter(|c| c.starts_with("UID COPY")).collect();
    assert_eq!(
        copies,
        ["UID COPY 2:3 \"Archive\"", "UID COPY 2 \"Archive\""]
    );
    assert_eq!(
        message_ids(&server, "Archive"),
        ["<3@example.com>", "<2@example.com>"]
    );
    assert_eq!(server.uids("INBOX"), [1, 4]);
    removed_after_the_last_copy(&server);
    assert_eq!(env.uids("Archive"), server.uids("Archive"));
    assert_eq!(env.uids("INBOX"), [1, 4]);
}

#[tokio::test(flavor = "multi_thread")]
async fn after_an_action_the_sync_goes_back_to_examine_before_any_fetch() {
    let (server, env, _daemon) = started(mailbox()).await;
    succeeded(&env.act(&["mark", &env.id("INBOX", 3), "--read"]).await);
    eventually(10, "the queue empty", || env.queue() == 0).await;
    succeeded(&sync_soon(&env).await);

    let log = server.imap_log();
    let store = log.iter().position(|c| c.starts_with("UID STORE")).unwrap();
    assert_eq!(log[store - 1], "SELECT \"INBOX\"", "{log:?}");
    let after = &log[store + 1..];
    let opened_or_fetched = after
        .iter()
        .find(|c| c.starts_with("EXAMINE") || c.starts_with("SELECT") || c.contains("FETCH"));
    assert!(
        opened_or_fetched.is_some_and(|c| c.starts_with("EXAMINE")),
        "{log:?}"
    );
    assert!(!after.iter().any(|c| c.starts_with("SELECT")), "{log:?}");
    assert!(after.iter().any(|c| c == "EXAMINE \"INBOX\""), "{log:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_copy_whose_answer_never_came_is_not_trusted_to_another_message_with_its_message_id() {
    // Another message with the same Message-ID arrives in the target meanwhile: the same message
    // received again later (another INTERNALDATE), or another message (another size).
    let impostors = [
        message(3).internal_date("02-Jan-2026 10:00:00 +0000"),
        Message::new(
            "Message-ID: <3@example.com>\r\n\
             \r\n\
             Not the same message at all.\r\n",
        ),
    ];
    for impostor in impostors {
        let (server, env, _daemon) = started_with(
            mailbox().without_move().stall("UID COPY"),
            &["--action-timeout", "1"],
        )
        .await;
        let original = server.raw("INBOX", 3).unwrap();
        succeeded(
            &env.act(&["move", &env.id("INBOX", 3), "--to", "archive"])
                .await,
        );
        // The server never ran it, and never answered.
        eventually(10, "the COPY", || tries(&server, "UID COPY") == 1).await;
        server.deliver("Archive", impostor);
        server.heal();

        eventually(30, "the queue empty", || env.queue() == 0).await;
        succeeded(&sync_soon(&env).await);

        // Looked for, found, but not trusted: copied again before the source lost it.
        assert_eq!(
            tries(&server, "UID SEARCH UID"),
            1,
            "{:?}",
            server.imap_log()
        );
        assert_eq!(tries(&server, "UID COPY"), 2, "{:?}", server.imap_log());
        removed_after_the_last_copy(&server);
        assert_eq!(server.uids("Archive"), [1, 2]);
        assert_eq!(
            server.raw("Archive", 2).unwrap(),
            original,
            "the second message of the archive is the real copy"
        );
        assert_eq!(server.uids("INBOX"), [1, 2, 4]);
        assert_eq!(env.uids("Archive"), [1, 2]);
        assert_eq!(env.uids("INBOX"), [1, 2, 4]);
    }
}

impl Env {
    /// `missive status --json`, the account `alice`.
    async fn status_of_alice(&self) -> serde_json::Value {
        let output = self.act(&["status", "--json"]).await;
        succeeded(&output);
        let status: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
        status["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["id"] == "alice")
            .unwrap()
            .clone()
    }
}

/// A subscription to the daemon's events.
struct Events(tokio::io::Lines<BufReader<UnixStream>>);

impl Events {
    async fn subscribe(env: &Env) -> Events {
        let mut stream = UnixStream::connect(env.socket()).await.unwrap();
        stream
            .write_all(b"{\"protocol\":1,\"request\":\"subscribe\"}\n")
            .await
            .unwrap();
        Events(BufReader::new(stream).lines())
    }

    /// The next `action` event.
    async fn action(&mut self) -> serde_json::Value {
        loop {
            let line = tokio::time::timeout(Duration::from_secs(30), self.0.next_line())
                .await
                .expect("an event in time")
                .unwrap()
                .unwrap();
            let event: serde_json::Value = serde_json::from_str(&line).unwrap();
            if event["event"] == "action" {
                return event;
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_move_refused_after_its_copy_says_the_message_is_in_both_folders_and_removes_nothing() {
    let (server, env, _daemon) = started(mailbox().without_move().refuse("UID EXPUNGE")).await;
    let mut events = Events::subscribe(&env).await;
    let three = env.id("INBOX", 3);
    succeeded(&env.act(&["move", &three, "--to", "archive"]).await);
    for pass in 1..=5 {
        let event = events.action().await;
        assert_eq!(
            event["outcome"],
            if pass < 5 { "retried" } else { "refused" }
        );
        if pass < 5 {
            succeeded(&sync_soon(&env).await);
        } else {
            assert_eq!(event["copied"], true, "{event}");
            let message = event["message"].as_str().unwrap();
            assert!(message.contains("both"), "{message}");
        }
    }

    // Nothing was removed: the message is in both folders, on the server and in the copy.
    assert_eq!(server.uids("INBOX"), [1, 2, 3, 4]);
    assert_eq!(server.flags("INBOX", 3), Some(Vec::new()));
    assert_eq!(server.uids("Archive"), [1]);
    eventually(10, "the copy in the archive", || env.uids("Archive") == [1]).await;
    assert_eq!(env.uids("INBOX"), [1, 2, 3, 4]);
    assert_eq!(env.shown_uids("INBOX"), [1, 2, 3, 4]);

    // `status` says so, until the user removed one of the two.
    let copy = env.id("Archive", 1);
    assert_eq!(
        env.status_of_alice().await["half_moved"],
        serde_json::json!([{
            "message": three, "copy": copy, "folder": "inbox", "target": "archive",
            "maybe": false,
        }])
    );
    let text = stdout(&env.act(&["status"]).await);
    assert!(text.contains("in both inbox and archive"), "{text}");
    // A pass does not forget it.
    succeeded(&sync_soon(&env).await);
    assert_eq!(
        env.status_of_alice().await["half_moved"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // The user deletes the copy in the archive: nothing is left half moved.
    server.heal();
    succeeded(&env.act(&["delete", &copy]).await);
    eventually(20, "the queue empty", || env.queue() == 0).await;
    succeeded(&sync_soon(&env).await);
    assert_eq!(server.uids("Archive"), Vec::<u32>::new());
    assert_eq!(server.uids("INBOX"), [1, 2, 3, 4]);
    assert_eq!(
        env.status_of_alice().await["half_moved"],
        serde_json::json!([])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_move_refused_while_its_copy_is_unknown_says_the_message_may_be_in_both_folders() {
    // The copy is made, but its answer is lost; then the target cannot be read to find it.
    let (server, env, _daemon) = started(mailbox().without_move().cut("UID COPY", 0)).await;
    let mut events = Events::subscribe(&env).await;
    let three = env.id("INBOX", 3);
    succeeded(&env.act(&["move", &three, "--to", "archive"]).await);
    eventually(10, "the copy", || tries(&server, "UID COPY") == 1).await;
    server.heal();
    server.refuse("STATUS");
    let event = loop {
        let event = events.action().await;
        if event["outcome"] != "retried" {
            break event;
        }
        succeeded(&sync_soon(&env).await);
    };
    assert_eq!(event["outcome"], "refused", "{event}");
    assert_eq!(event["maybe_copied"], true, "{event}");
    assert_eq!(event.get("copied"), None, "{event}");
    let message = event["message"].as_str().unwrap();
    assert!(message.contains("may be in both folders"), "{message}");

    // It was copied: the message is in both folders, and nothing was removed.
    assert_eq!(server.uids("INBOX"), [1, 2, 3, 4]);
    assert_eq!(server.uids("Archive"), [1]);
    eventually(10, "the copy in the archive", || env.uids("Archive") == [1]).await;
    assert_eq!(env.shown_uids("INBOX"), [1, 2, 3, 4]);

    // Once the archive is synced, its copy is found by its fingerprint: no longer a maybe.
    let copy = env.id("Archive", 1);
    half_moved_soon(
        &env,
        serde_json::json!([{
            "message": three, "copy": copy, "folder": "inbox", "target": "archive",
            "maybe": false,
        }]),
    )
    .await;
    let text = stdout(&env.act(&["status"]).await);
    assert!(text.contains("is in both inbox and archive"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_maybe_half_move_whose_copy_never_landed_settles_once_the_target_is_synced() {
    // The copy is never made, and its answer never comes; then the target cannot be read.
    let (server, env, _daemon) = started_with(
        mailbox().without_move().stall("UID COPY"),
        &["--action-timeout", "1"],
    )
    .await;
    let mut events = Events::subscribe(&env).await;
    let three = env.id("INBOX", 3);
    succeeded(&env.act(&["move", &three, "--to", "archive"]).await);
    eventually(10, "the copy", || tries(&server, "UID COPY") == 1).await;
    server.heal();
    server.refuse("STATUS");
    let event = until_settled(&env, &mut events).await;
    assert_eq!(event["outcome"], "refused", "{event}");
    assert_eq!(event["maybe_copied"], true, "{event}");

    // Nothing was copied, nothing removed: the target's sync finds no copy, and the row leaves.
    // The target holds another message: an empty folder would not settle anything.
    server.heal();
    server.deliver("Archive", message(30));
    succeeded(&sync_soon(&env).await);
    assert_eq!(server.uids("Archive"), [1]);
    assert_eq!(server.uids("INBOX"), [1, 2, 3, 4]);
    half_moved_soon(&env, serde_json::json!([])).await;
    assert_eq!(env.half_move_rows(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_half_move_whose_source_was_renumbered_is_found_again_by_its_fingerprint() {
    let (server, env, _daemon) = started(mailbox().without_move().refuse("UID EXPUNGE")).await;
    let mut events = Events::subscribe(&env).await;
    let three = env.id("INBOX", 3);
    succeeded(&env.act(&["move", &three, "--to", "archive"]).await);
    let event = until_settled(&env, &mut events).await;
    assert_eq!(event["copied"], true, "{event}");
    eventually(10, "the copy in the archive", || env.uids("Archive") == [1]).await;

    // The server renumbers the inbox: the message is at UID 2 now, and still in both folders.
    server.heal();
    server.expunge("INBOX", 1);
    server.bump_uidvalidity("INBOX");
    succeeded(&sync_soon(&env).await);
    eventually(10, "the inbox rebuilt", || env.uids("INBOX") == [1, 2, 3]).await;
    let found = env.id("INBOX", 2);
    assert_ne!(found, three);
    let copy = env.id("Archive", 1);
    half_moved_soon(
        &env,
        serde_json::json!([{
            "message": found, "copy": copy, "folder": "inbox", "target": "archive",
            "maybe": false,
        }]),
    )
    .await;
    assert_eq!(server.uids("Archive"), [1]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_settled_half_move_leaves_after_a_complete_sync_but_not_on_a_renumbering() {
    let (server, env, _daemon) = started(mailbox().without_move().refuse("UID EXPUNGE")).await;
    let mut events = Events::subscribe(&env).await;
    let three = env.id("INBOX", 3);
    succeeded(&env.act(&["move", &three, "--to", "archive"]).await);
    until_settled(&env, &mut events).await;
    eventually(10, "the copy in the archive", || env.uids("Archive") == [1]).await;
    server.heal();
    // The archive holds another message, so that removing the copy does not empty it: an empty
    // folder settles nothing.
    server.deliver("Archive", message(30));

    // A renumbering of the target empties it in the copy: the row stays, and the copy is found
    // again under the new UIDVALIDITY.
    server.bump_uidvalidity("Archive");
    succeeded(&sync_soon(&env).await);
    let copy = env.id("Archive", 1);
    half_moved_soon(
        &env,
        serde_json::json!([{
            "message": three, "copy": copy, "folder": "inbox", "target": "archive",
            "maybe": false,
        }]),
    )
    .await;
    assert_eq!(env.half_move_rows(), 1);

    // Another client removes the copy: the next complete sync of the archive deletes the row.
    server.expunge("Archive", 1);
    succeeded(&sync_soon(&env).await);
    half_moved_soon(&env, serde_json::json!([])).await;
    eventually(10, "the row deleted", || env.half_move_rows() == 0).await;
    assert_eq!(server.uids("INBOX"), [1, 2, 3, 4]);
}

/// Waits for the next action event that is not a retry, running a pass after each retry.
async fn until_settled(env: &Env, events: &mut Events) -> serde_json::Value {
    loop {
        let event = events.action().await;
        if event["outcome"] != "retried" {
            return event;
        }
        succeeded(&sync_soon(env).await);
    }
}

/// Waits until `status` lists `expected` as alice's half moves.
async fn half_moved_soon(env: &Env, expected: serde_json::Value) {
    let started = Instant::now();
    loop {
        let half_moved = env.status_of_alice().await["half_moved"].clone();
        if half_moved == expected {
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "half_moved is {half_moved}, not {expected}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

impl Env {
    /// The rows of `half_move`, settled or not.
    fn half_move_rows(&self) -> i64 {
        self.copy()
            .unwrap()
            .query_row("SELECT count(*) FROM half_move", [], |row| row.get(0))
            .unwrap()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_half_move_whose_source_was_renumbered_writes_no_copy_of_another_message() {
    let (server, env, _daemon) = started(mailbox().without_move().refuse("UID EXPUNGE")).await;
    let mut events = Events::subscribe(&env).await;
    let three = env.id("INBOX", 3);
    succeeded(&env.act(&["move", &three, "--to", "archive"]).await);
    // The copy is made, the removal refused: the action waits for its next try.
    assert_eq!(events.action().await["outcome"], "retried");
    assert_eq!(server.uids("Archive"), [1]);
    let copy_on_the_server = server.raw("Archive", 1).unwrap();

    // The server renumbers the inbox before that try; the UID 3 now names another message.
    server.expunge("INBOX", 1);
    server.bump_uidvalidity("INBOX");
    let other = server.raw("INBOX", 3).unwrap();
    assert!(other != copy_on_the_server);
    succeeded(&sync_soon(&env).await);
    let event = loop {
        let event = events.action().await;
        if event["outcome"] != "retried" {
            break event;
        }
        succeeded(&sync_soon(&env).await);
    };
    assert_eq!(event["outcome"], "refused", "{event}");
    assert_eq!(event["reason"], "renumbered", "{event}");
    assert_eq!(event["copied"], true, "{event}");

    // The archive of the copy is the one the server has, never the other message.
    succeeded(&sync_soon(&env).await);
    eventually(20, "the archive in the copy", || env.uids("Archive") == [1]).await;
    assert!(
        env.raw("Archive", 1).unwrap() == copy_on_the_server,
        "the copy of the archive is not the one the server has"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_half_move_refused_after_its_source_was_renumbered_is_found_again_by_its_fingerprint() {
    let (server, env, _daemon) = started(mailbox().without_move().refuse("UID EXPUNGE")).await;
    let mut events = Events::subscribe(&env).await;
    let three = env.id("INBOX", 3);
    succeeded(&env.act(&["move", &three, "--to", "archive"]).await);
    // The copy is made, the removal refused: the action waits for its next try.
    assert_eq!(events.action().await["outcome"], "retried");

    // The server renumbers the inbox before that try: the message is at UID 2 now.
    server.expunge("INBOX", 1);
    server.bump_uidvalidity("INBOX");
    succeeded(&sync_soon(&env).await);
    let event = until_settled(&env, &mut events).await;
    assert_eq!(event["reason"], "renumbered", "{event}");
    assert_eq!(event["copied"], true, "{event}");

    // What tells the message from another was taken when the action was recorded: the next
    // complete syncs find it in both folders, under the UIDs the server gives it now.
    succeeded(&sync_soon(&env).await);
    let found = env.id("INBOX", 2);
    assert_ne!(found, three);
    let copy = env.id("Archive", 1);
    half_moved_soon(
        &env,
        serde_json::json!([{
            "message": found, "copy": copy, "folder": "inbox", "target": "archive",
            "maybe": false,
        }]),
    )
    .await;
    assert_eq!(server.uids("Archive"), [1]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_maybe_half_move_refused_after_its_source_was_renumbered_settles_once_the_target_is_synced()
 {
    // The copy is never made, and its answer never comes.
    let (server, env, _daemon) = started_with(
        mailbox().without_move().stall("UID COPY"),
        &["--action-timeout", "1"],
    )
    .await;
    let mut events = Events::subscribe(&env).await;
    let three = env.id("INBOX", 3);
    succeeded(&env.act(&["move", &three, "--to", "archive"]).await);
    eventually(10, "the copy", || tries(&server, "UID COPY") == 1).await;

    // The server renumbers the inbox, and the target cannot be read: whether the message was
    // copied is not known.
    server.heal();
    server.refuse("STATUS");
    server.expunge("INBOX", 1);
    server.bump_uidvalidity("INBOX");
    succeeded(&sync_soon(&env).await);
    let event = until_settled(&env, &mut events).await;
    assert_eq!(event["outcome"], "refused", "{event}");
    assert_eq!(event["maybe_copied"], true, "{event}");

    // Nothing was copied: the target's sync finds no copy by the fingerprint taken when the
    // action was recorded, and the row leaves. The target holds another message: an empty folder
    // would not settle anything.
    server.heal();
    server.deliver("Archive", message(30));
    succeeded(&sync_soon(&env).await);
    succeeded(&sync_soon(&env).await);
    half_moved_soon(&env, serde_json::json!([])).await;
    eventually(10, "the row deleted", || env.half_move_rows() == 0).await;
    assert_eq!(server.uids("Archive"), [1]);
}

/// The JSON object a `--json` command printed.
fn json(output: &Output) -> serde_json::Value {
    serde_json::from_str(&stdout(output))
        .unwrap_or_else(|e| panic!("{e}: {}\n{}", stdout(output), stderr(output)))
}

#[tokio::test(flavor = "multi_thread")]
async fn wait_answers_once_the_server_applied_a_mark_a_move_and_a_delete() {
    let (server, env, _daemon) = started(mailbox()).await;
    let three = env.id("INBOX", 3);
    let two = env.id("INBOX", 2);
    let one = env.id("INBOX", 1);

    let output = env
        .act(&["mark", &three, "--read", "--wait", "--json"])
        .await;
    succeeded(&output);
    // Already on the server when the command answers.
    assert_eq!(server.flags("INBOX", 3), flags(&["\\Seen"]));
    let answer = json(&output);
    assert_eq!(answer["messages"], serde_json::json!([three]));
    assert_eq!(answer["outcome"], "applied");
    let actions = answer["actions"].as_array().unwrap();
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0]["outcome"], "applied");
    assert!(actions[0]["id"].is_number(), "{answer}");

    let output = env.act(&["move", &two, "--to", "archive", "--wait"]).await;
    succeeded(&output);
    assert_eq!(server.uids("INBOX"), [1, 3, 4]);
    assert_eq!(server.uids("Archive"), [1]);
    let text = stdout(&output);
    assert!(text.contains("applied on the server"), "{text}");

    let output = env.act(&["delete", &one, "--wait", "--json"]).await;
    succeeded(&output);
    assert_eq!(server.uids("INBOX"), [3, 4]);
    assert_eq!(server.uids("Trash"), [1, 2, 3]);
    assert_eq!(json(&output)["outcome"], "applied");
}

/// The exit status of `--wait` when the server refused an action.
const REFUSED: i32 = 5;
/// The exit status of `--wait` when an action is still in the queue.
const QUEUED: i32 = 4;

#[tokio::test(flavor = "multi_thread")]
async fn wait_tells_an_action_the_server_refused_apart() {
    let (_server, env, _daemon) = started(mailbox().bad("UID MOVE")).await;
    let three = env.id("INBOX", 3);

    let output = env
        .act(&["move", &three, "--to", "archive", "--wait", "--json"])
        .await;
    assert_eq!(output.status.code(), Some(REFUSED), "{}", stderr(&output));
    let answer = json(&output);
    assert_eq!(answer["outcome"], "refused");
    assert_eq!(answer["actions"][0]["outcome"], "refused");
    assert_eq!(answer["actions"][0]["reason"], "refused");
    assert!(answer["actions"][0]["message"].is_string(), "{answer}");

    let output = env
        .act(&["move", &three, "--to", "archive", "--wait"])
        .await;
    assert_eq!(output.status.code(), Some(REFUSED));
    assert!(stdout(&output).contains("refused"), "{}", stdout(&output));
}

#[tokio::test(flavor = "multi_thread")]
async fn wait_on_an_account_that_is_offline_says_the_action_is_still_queued_at_once() {
    let (_server, _relay, env) = offline().await;
    let _daemon = env.daemon(&["--check-interval", "3600"]).await;
    eventually(10, "unreachable", || {
        env.state("alice").as_deref() == Some("unreachable")
    })
    .await;

    let started = Instant::now();
    let output = env
        .act(&["mark", &env.id("INBOX", 3), "--read", "--wait", "--json"])
        .await;
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(output.status.code(), Some(QUEUED), "{}", stderr(&output));
    let answer = json(&output);
    assert_eq!(answer["outcome"], "queued");
    assert_eq!(answer["actions"][0]["outcome"], "queued");
    assert_eq!(answer["actions"][0]["reason"], "unreachable");
    // Recorded all the same, and shown at once.
    assert_eq!(env.queue(), 1);
    assert_eq!(env.flags("INBOX", 3).as_deref(), Some("\\Seen"));
}

#[tokio::test(flavor = "multi_thread")]
async fn wait_gives_up_after_its_timeout_while_the_server_keeps_refusing_for_now() {
    let (server, env, _daemon) = started(mailbox()).await;
    server.refuse("UID STORE");

    let started = Instant::now();
    let output = env
        .act(&[
            "mark",
            &env.id("INBOX", 3),
            "--read",
            "--wait",
            "--timeout",
            "2",
            "--json",
        ])
        .await;
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_secs(2) && waited < Duration::from_secs(10),
        "{waited:?}"
    );
    assert_eq!(output.status.code(), Some(QUEUED), "{}", stderr(&output));
    let answer = json(&output);
    assert_eq!(answer["outcome"], "queued");
    assert_eq!(answer["actions"][0]["reason"], "timeout");
    assert_eq!(env.queue(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn wait_with_no_daemon_running_does_not_believe_the_state_the_last_one_left() {
    let (server, relay, env) = offline().await;
    let mut daemon = env.daemon(&["--check-interval", "3600"]).await;
    eventually(10, "unreachable", || {
        env.state("alice").as_deref() == Some("unreachable")
    })
    .await;
    assert!(daemon.stop("-TERM").await.success());
    assert_eq!(env.state("alice").as_deref(), Some("unreachable"));
    // The server is back; the command starts a daemon, which connects.
    relay.up.store(true, Ordering::SeqCst);

    let output = env
        .act(&["mark", &env.id("INBOX", 3), "--read", "--wait", "--json"])
        .await;
    succeeded(&output);
    assert_eq!(json(&output)["outcome"], "applied");
    assert_eq!(server.flags("INBOX", 3), flags(&["\\Seen"]));
}

#[tokio::test(flavor = "multi_thread")]
async fn wait_does_not_take_the_state_the_last_daemon_left_for_the_new_ones() {
    let (_server, relay, env) = offline().await;
    let mut daemon = env.daemon(&["--check-interval", "3600"]).await;
    eventually(10, "unreachable", || {
        env.state("alice").as_deref() == Some("unreachable")
    })
    .await;
    assert!(daemon.stop("-TERM").await.success());
    // The server is back, and the new daemon has not told any state yet: its worker waits for
    // the keyring.
    relay.up.store(true, Ordering::SeqCst);
    env.keyring.stall_secret_reads();
    let _daemon = env.daemon(&["--check-interval", "3600"]).await;
    assert_eq!(env.state("alice").as_deref(), Some("unreachable"));

    let output = env
        .act(&[
            "mark",
            &env.id("INBOX", 3),
            "--read",
            "--wait",
            "--timeout",
            "2",
            "--json",
        ])
        .await;
    assert_eq!(output.status.code(), Some(QUEUED), "{}", stderr(&output));
    let answer = json(&output);
    assert_eq!(answer["actions"][0]["reason"], "timeout", "{answer}");
}

#[tokio::test(flavor = "multi_thread")]
async fn wait_with_a_daemon_older_than_the_command_says_it_cannot_tell() {
    let env = Env::new().await;
    std::fs::create_dir_all(env.socket().parent().unwrap()).unwrap();
    // A daemon from before `--wait`: its answer to `mark` has no `actions`.
    let listener = tokio::net::UnixListener::bind(env.socket()).unwrap();
    let old_daemon = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut lines = BufReader::new(read).lines();
            lines.next_line().await.unwrap();
            write
                .write_all(b"{\"protocol\":1,\"answer\":\"subscribe\",\"ok\":true}\n")
                .await
                .unwrap();
            lines.next_line().await.unwrap();
            write
                .write_all(
                    b"{\"protocol\":1,\"answer\":\"mark\",\"ok\":true,\
                      \"messages\":[\"alice/1/100/3\"]}\n",
                )
                .await
                .unwrap();
            // The daemon stays connected: the command must not wait for its timeout.
            let _ = lines.next_line().await;
        }
    });

    let started = Instant::now();
    let output = env
        .act(&["mark", "alice/1/100/3", "--read", "--wait", "--json"])
        .await;
    assert!(started.elapsed() < Duration::from_secs(20));
    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
    let answer = json(&output);
    assert_eq!(answer["outcome"], "queued", "{answer}");
    assert_eq!(answer["messages"], serde_json::json!(["alice/1/100/3"]));

    let output = env
        .act(&["mark", "alice/1/100/3", "--read", "--wait"])
        .await;
    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("older") && text.contains("--wait"), "{text}");
    assert!(!text.contains("applied"), "{text}");
    old_daemon.abort();
}

impl Env {
    /// Makes the copy forget the server's capabilities, as a copy written before it recorded them.
    fn forget_capabilities(&self) {
        Connection::open(self.copy_path())
            .unwrap()
            .execute("UPDATE account SET can_move = NULL, can_uidplus = NULL", [])
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn what_the_server_cannot_do_is_refused_when_asked_once_its_capabilities_are_known() {
    let (server, env, _daemon) = started(mailbox().without_move().without_uidplus()).await;
    let before = env.messages();
    let from = server.imap_log().len();

    for args in [
        vec!["move", "--to", "archive"],
        vec!["delete"],
        vec!["delete", "--permanently"],
    ] {
        let folder = if args.contains(&"--permanently") {
            "Trash"
        } else {
            "INBOX"
        };
        let id = env.id(folder, 1);
        let mut args = args.clone();
        args.push(&id);
        let output = env.act(&args).await;
        failed(&output);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert!(stderr(&output).contains("UIDPLUS"), "{}", stderr(&output));
        assert_eq!(env.queue(), 0, "{args:?}");
    }
    assert_eq!(env.messages(), before);
    assert!(
        writes(&server, from).is_empty(),
        "{:?}",
        writes(&server, from)
    );
    // A mark needs neither.
    succeeded(&env.act(&["mark", &env.id("INBOX", 3), "--read"]).await);

    // With MOVE but without UIDPLUS, a move is accepted and a permanent delete refused.
    let (_server, env, _daemon) = started(mailbox().without_uidplus()).await;
    let output = env
        .act(&["delete", "--permanently", "--json", &env.id("Trash", 1)])
        .await;
    failed(&output);
    assert!(stderr(&output).contains("UIDPLUS"), "{}", stderr(&output));
    succeeded(
        &env.act(&["move", &env.id("INBOX", 3), "--to", "archive"])
            .await,
    );
    eventually(10, "the queue empty", || env.queue() == 0).await;
}

/// [`offline`], with a daemon running that found the server unreachable.
async fn offline_with_a_daemon() -> (FakeServer, Relay, Env, Daemon) {
    let (server, relay, env) = offline().await;
    let daemon = env.daemon(&["--check-interval", "3600"]).await;
    eventually(10, "unreachable", || {
        env.state("alice").as_deref() == Some("unreachable")
    })
    .await;
    (server, relay, env, daemon)
}

#[tokio::test(flavor = "multi_thread")]
async fn actions_asked_while_a_move_waits_follow_the_message_to_its_new_uid() {
    let (server, relay, env, _daemon) = offline_with_a_daemon().await;
    let from = server.imap_log().len();
    let three = env.id("INBOX", 3);

    for args in [
        vec!["move", &three, "--to", "archive"],
        vec!["mark", &three, "--read"],
        vec!["mark", &three, "--flagged"],
        vec!["delete", &three],
    ] {
        succeeded(&env.act(&args).await);
    }
    assert_eq!(env.queue(), 4);
    // Shown where the last move takes it, with its flags.
    assert_eq!(env.shown_uids("Trash"), [1, 2, 3]);
    assert_eq!(env.flags("INBOX", 3).as_deref(), Some("\\Flagged \\Seen"));
    relay.up.store(true, Ordering::SeqCst);

    // COPYUID said the message is the archive's UID 1: what was asked after the move goes there.
    wrote(
        &server,
        from,
        &[
            "SELECT \"INBOX\"",
            "UID MOVE 3 \"Archive\"",
            "SELECT \"Archive\"",
            "UID STORE 1 +FLAGS (\\Seen)",
            "SELECT \"Archive\"",
            "UID STORE 1 +FLAGS (\\Flagged)",
            "SELECT \"Archive\"",
            "UID MOVE 1 \"Trash\"",
        ],
    )
    .await;
    assert_eq!(server.uids("INBOX"), [1, 2, 4]);
    assert_eq!(server.uids("Archive"), Vec::<u32>::new());
    assert_eq!(server.uids("Trash"), [1, 2, 3]);
    let mut moved = server.flags("Trash", 3).unwrap();
    moved.sort();
    assert_eq!(moved, ["\\Flagged", "\\Seen"]);
    eventually(10, "the queue empty", || env.queue() == 0).await;
    succeeded(&sync_soon(&env).await);
    assert_eq!(env.uids("Trash"), [1, 2, 3]);
    assert_eq!(env.flags("Trash", 3).as_deref(), Some("\\Flagged \\Seen"));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_action_behind_a_move_whose_copyuid_came_in_parts_follows_every_message() {
    let (server, relay, env) = offline_from(mailbox().split_copyuid()).await;
    let _daemon = env.daemon(&["--check-interval", "3600"]).await;
    eventually(10, "unreachable", || {
        env.state("alice").as_deref() == Some("unreachable")
    })
    .await;
    let from = server.imap_log().len();
    let (two, three) = (env.id("INBOX", 2), env.id("INBOX", 3));
    succeeded(&env.act(&["move", &two, &three, "--to", "archive"]).await);
    succeeded(&env.act(&["mark", &two, &three, "--read"]).await);
    relay.up.store(true, Ordering::SeqCst);

    // One untagged COPYUID per message: together they tell both new UIDs.
    wrote(
        &server,
        from,
        &[
            "SELECT \"INBOX\"",
            "UID MOVE 2:3 \"Archive\"",
            "SELECT \"Archive\"",
            "UID STORE 1:2 +FLAGS (\\Seen)",
        ],
    )
    .await;
    eventually(10, "the queue empty", || env.queue() == 0).await;
    for uid in [1, 2] {
        assert!(
            server
                .flags("Archive", uid)
                .unwrap()
                .contains(&"\\Seen".to_owned())
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_permanent_delete_asked_while_the_move_to_the_trash_waits_removes_that_message_only() {
    let (server, relay, env, _daemon) = offline_with_a_daemon().await;
    let from = server.imap_log().len();
    let three = env.id("INBOX", 3);

    succeeded(&env.act(&["delete", &three]).await);
    // Shown in the trash already: deleting it again is refused, for good is accepted.
    let output = env.act(&["delete", &three]).await;
    failed(&output);
    assert!(stderr(&output).contains("trash"), "{}", stderr(&output));
    succeeded(&env.act(&["delete", "--permanently", &three]).await);
    assert_eq!(env.queue(), 2);
    relay.up.store(true, Ordering::SeqCst);

    wrote(
        &server,
        from,
        &[
            "SELECT \"INBOX\"",
            "UID MOVE 3 \"Trash\"",
            "SELECT \"Trash\"",
            "UID SEARCH DELETED UID 3",
            "UID STORE 3 +FLAGS.SILENT (\\Deleted)",
            "UID EXPUNGE 3",
        ],
    )
    .await;
    assert_eq!(server.uids("INBOX"), [1, 2, 4]);
    assert_eq!(
        message_ids(&server, "Trash"),
        ["<10@example.com>", "<11@example.com>"]
    );
    eventually(10, "the queue empty", || env.queue() == 0).await;
    succeeded(&sync_soon(&env).await);
    assert_eq!(env.uids("Trash"), [1, 2]);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_action_on_messages_some_of_which_wait_for_a_move_is_split_where_they_are() {
    let (server, relay, env, _daemon) = offline_with_a_daemon().await;
    let from = server.imap_log().len();
    let two = env.id("INBOX", 2);
    let three = env.id("INBOX", 3);

    succeeded(&env.act(&["move", &three, "--to", "archive"]).await);
    succeeded(&env.act(&["mark", &two, &three, "--read"]).await);
    // One action for the message in the inbox, one for the message on its way.
    assert_eq!(env.queue(), 3);
    relay.up.store(true, Ordering::SeqCst);

    wrote(
        &server,
        from,
        &[
            "SELECT \"INBOX\"",
            "UID MOVE 3 \"Archive\"",
            "SELECT \"INBOX\"",
            "UID STORE 2 +FLAGS (\\Seen)",
            "SELECT \"Archive\"",
            "UID STORE 1 +FLAGS (\\Seen)",
        ],
    )
    .await;
    let mut two = server.flags("INBOX", 2).unwrap();
    two.sort();
    assert_eq!(two, ["\\Flagged", "\\Seen"]);
    assert_eq!(server.flags("Archive", 1), flags(&["\\Seen"]));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_action_behind_a_move_leaves_out_the_messages_the_server_did_not_move() {
    let (server, relay, env, _daemon) = offline_with_a_daemon().await;
    let from = server.imap_log().len();
    let two = env.id("INBOX", 2);
    let three = env.id("INBOX", 3);

    succeeded(&env.act(&["move", &two, &three, "--to", "archive"]).await);
    succeeded(&env.act(&["mark", &two, &three, "--read"]).await);
    // Another client removes one of the two before the move reaches the server.
    server.expunge("INBOX", 2);
    relay.up.store(true, Ordering::SeqCst);

    wrote(
        &server,
        from,
        &[
            "SELECT \"INBOX\"",
            "UID MOVE 2:3 \"Archive\"",
            "SELECT \"Archive\"",
            "UID STORE 1 +FLAGS (\\Seen)",
        ],
    )
    .await;
    assert_eq!(server.uids("Archive"), [1]);
    assert_eq!(server.flags("Archive", 1), flags(&["\\Seen"]));
    eventually(10, "the queue empty", || env.queue() == 0).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn without_copyuid_an_action_behind_a_move_is_refused_and_touches_nothing() {
    // MOVE without UIDPLUS: a move says nothing of the new UIDs.
    let (server, env, _daemon) = started(mailbox().without_uidplus().refuse("UID MOVE")).await;
    let mut events = Events::subscribe(&env).await;
    let three = env.id("INBOX", 3);
    succeeded(&env.act(&["move", &three, "--to", "archive"]).await);
    assert_eq!(events.action().await["outcome"], "retried");

    // The server is known to tell no new UID: refused when asked.
    let output = env.act(&["mark", &three, "--read"]).await;
    failed(&output);
    assert!(stderr(&output).contains("UIDPLUS"), "{}", stderr(&output));
    assert_eq!(env.queue(), 1);

    // Not known yet: recorded, then refused once the move is applied without COPYUID.
    env.forget_capabilities();
    succeeded(&env.act(&["mark", &three, "--read"]).await);
    assert_eq!(env.queue(), 2);
    let from = server.imap_log().len();
    server.heal();
    succeeded(&sync_soon(&env).await);
    assert_eq!(events.action().await["outcome"], "applied");
    let event = events.action().await;
    assert_eq!(event["outcome"], "refused", "{event}");
    assert_eq!(event["reason"], "moved", "{event}");
    assert_eq!(event["action"], "read", "{event}");
    eventually(10, "the queue empty", || env.queue() == 0).await;
    let log = writes(&server, from);
    assert!(!log.iter().any(|c| c.starts_with("UID STORE")), "{log:?}");
    assert_eq!(server.uids("Archive"), [1]);
    assert_eq!(server.flags("Archive", 1), Some(Vec::new()));
    succeeded(&sync_soon(&env).await);
    assert_eq!(env.flags("Archive", 1).as_deref(), Some(""));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_permanent_delete_behind_a_move_to_the_trash_the_server_refused_removes_nothing() {
    let (server, env, _daemon) = started(mailbox().refuse("UID MOVE")).await;
    let mut events = Events::subscribe(&env).await;
    let three = env.id("INBOX", 3);
    succeeded(&env.act(&["delete", &three]).await);
    assert_eq!(events.action().await["outcome"], "retried");
    succeeded(&env.act(&["delete", "--permanently", &three]).await);

    for pass in 2..=5 {
        succeeded(&sync_soon(&env).await);
        let event = events.action().await;
        assert_eq!(event["action"], "delete", "{event}");
        assert_eq!(
            event["outcome"],
            if pass < 5 { "retried" } else { "refused" },
            "{event}"
        );
    }
    // The message never reached the trash: it is not deleted for good from the inbox.
    let event = events.action().await;
    assert_eq!(event["action"], "delete-permanently", "{event}");
    assert_eq!(event["outcome"], "refused", "{event}");
    assert_eq!(event["reason"], "not-moved", "{event}");
    eventually(10, "the queue empty", || env.queue() == 0).await;
    assert_eq!(server.uids("INBOX"), [1, 2, 3, 4]);
    assert_eq!(server.flags("INBOX", 3), Some(Vec::new()));
    assert_eq!(tries(&server, "UID STORE"), 0);
    assert_eq!(tries(&server, "UID EXPUNGE"), 0);
    succeeded(&sync_soon(&env).await);
    assert_eq!(env.shown_uids("INBOX"), [1, 2, 3, 4]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_move_back_asked_while_the_first_move_waits_brings_the_message_back() {
    let (server, relay, env, _daemon) = offline_with_a_daemon().await;
    let from = server.imap_log().len();
    let three = env.id("INBOX", 3);

    succeeded(&env.act(&["move", &three, "--to", "archive"]).await);
    succeeded(&env.act(&["move", &three, "--to", "inbox"]).await);
    assert_eq!(env.shown_uids("INBOX"), [1, 2, 3, 4]);
    relay.up.store(true, Ordering::SeqCst);

    wrote(
        &server,
        from,
        &[
            "SELECT \"INBOX\"",
            "UID MOVE 3 \"Archive\"",
            "SELECT \"Archive\"",
            "UID MOVE 1 \"INBOX\"",
        ],
    )
    .await;
    assert_eq!(server.uids("INBOX"), [1, 2, 4, 5]);
    assert_eq!(server.uids("Archive"), Vec::<u32>::new());
    eventually(10, "the queue empty", || env.queue() == 0).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_move_back_behind_a_move_the_server_refused_sends_nothing() {
    let (server, env, _daemon) = started(mailbox().refuse("UID MOVE")).await;
    let mut events = Events::subscribe(&env).await;
    let three = env.id("INBOX", 3);
    succeeded(&env.act(&["move", &three, "--to", "archive"]).await);
    assert_eq!(events.action().await["outcome"], "retried");
    succeeded(&env.act(&["move", &three, "--to", "inbox"]).await);
    // The first is refused for good; the second has nothing left to do.
    for pass in 2..=5 {
        succeeded(&sync_soon(&env).await);
        let event = events.action().await;
        assert_eq!(event["action"], "move", "{event}");
        assert_eq!(
            event["outcome"],
            if pass < 5 { "retried" } else { "refused" },
            "{event}"
        );
    }
    let event = events.action().await;
    assert_eq!(event["outcome"], "applied", "{event}");
    eventually(10, "the queue empty", || env.queue() == 0).await;
    assert_eq!(tries(&server, "UID MOVE"), 5);
    assert_eq!(server.uids("INBOX"), [1, 2, 3, 4]);
    succeeded(&sync_soon(&env).await);
    assert_eq!(env.shown_uids("INBOX"), [1, 2, 3, 4]);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_action_behind_a_move_whose_copy_was_only_found_again_is_not_remapped() {
    let (server, env, _daemon) = started_with(
        mailbox().without_move().stall("UID COPY"),
        &["--action-timeout", "1"],
    )
    .await;
    let mut events = Events::subscribe(&env).await;
    let three = env.id("INBOX", 3);
    succeeded(&env.act(&["move", &three, "--to", "archive"]).await);
    eventually(10, "the COPY", || tries(&server, "UID COPY") == 1).await;
    // The next try waits: it cannot read the target's UIDNEXT.
    server.refuse("STATUS");
    assert_eq!(events.action().await["outcome"], "retried");
    // As a server that copied it before the connection was lost.
    server.deliver("Archive", message(3));
    succeeded(&env.act(&["delete", &three]).await);
    succeeded(&env.act(&["mark", &three, "--read"]).await);
    let from = server.imap_log().len();
    server.heal();
    succeeded(&sync_soon(&env).await);

    // Found by its Message-ID, size and date, not told by COPYUID: not proof enough to move it again.
    let mut outcomes = Vec::new();
    while outcomes.len() < 3 {
        let event = events.action().await;
        if event["outcome"] != "retried" {
            outcomes.push((
                event["action"].clone(),
                event["outcome"].clone(),
                event["reason"].clone(),
            ));
        }
    }
    assert_eq!(
        outcomes,
        [
            ("move".into(), "applied".into(), serde_json::Value::Null),
            ("delete".into(), "refused".into(), "moved".into()),
            ("read".into(), "refused".into(), "moved".into()),
        ]
    );
    let log = writes(&server, from);
    assert!(
        !log.iter()
            .any(|c| c.contains("Archive") && c.starts_with("SELECT")),
        "{log:?}"
    );
    assert_eq!(server.uids("Archive"), [1]);
    assert_eq!(server.flags("Archive", 1), Some(Vec::new()));
    assert_eq!(server.uids("INBOX"), [1, 2, 4]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deleted_flag_left_by_a_cut_connection_is_reported_and_taken_back_at_the_next_connection()
{
    // The flag is never taken back: the server never answers, nor runs, that command.
    let (server, env, _daemon) = started_with(
        mailbox()
            .without_move()
            .refuse("UID EXPUNGE")
            .stall("UID STORE 3 -FLAGS"),
        &["--action-timeout", "1"],
    )
    .await;
    let mut events = Events::subscribe(&env).await;
    let three = env.id("INBOX", 3);
    succeeded(&env.act(&["move", &three, "--to", "archive"]).await);
    let event = loop {
        let event = events.action().await;
        if event["outcome"] != "retried" {
            break event;
        }
        succeeded(&sync_soon(&env).await);
    };
    assert_eq!(event["outcome"], "refused", "{event}");
    assert_eq!(event["reason"], "connection", "{event}");
    let message = event["message"].as_str().unwrap();
    assert!(message.contains("\\Deleted"), "{message}");
    // Every try set it again and tried to take it back, and none could: it is still there.
    assert_eq!(server.flags("INBOX", 3), flags(&["\\Deleted"]));
    assert_eq!(
        env.status_of_alice().await["deleted_left"],
        serde_json::json!([three])
    );
    let text = stdout(&env.act(&["status"]).await);
    assert!(text.contains("\\Deleted"), "{text}");

    server.heal();
    succeeded(&sync_soon(&env).await);
    eventually(20, "the flag taken back", || {
        server.flags("INBOX", 3) == Some(Vec::new())
    })
    .await;
    assert_eq!(
        env.status_of_alice().await["deleted_left"],
        serde_json::json!([])
    );
    // What another client marked is left alone.
    assert_eq!(server.flags("INBOX", 4), flags(&["\\Deleted"]));
    assert_eq!(server.uids("INBOX"), [1, 2, 3, 4]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_move_after_a_refusal_that_left_its_deleted_flag_does_not_carry_it_to_another_folder() {
    // The removal is refused, and taking its flag back too, until the limit refuses the move.
    let (server, env, _daemon) = started(
        mailbox()
            .without_move()
            .refuse("UID EXPUNGE")
            .refuse("UID STORE 3 -FLAGS"),
    )
    .await;
    let mut events = Events::subscribe(&env).await;
    let three = env.id("INBOX", 3);
    succeeded(&env.act(&["move", &three, "--to", "archive"]).await);
    // Asked while the first waits: once that one is refused, it applies where the message is.
    succeeded(&env.act(&["move", &three, "--to", PRIVATE_FOLDER]).await);
    let first = loop {
        let event = events.action().await;
        if event["outcome"] != "retried" {
            break event;
        }
        succeeded(&sync_soon(&env).await);
    };
    assert_eq!(first["outcome"], "refused", "{first}");
    assert_eq!(first["copied"], true, "{first}");
    assert_eq!(server.flags("INBOX", 3), flags(&["\\Deleted"]));

    // `UID COPY` keeps the flags: copied now, the message would carry the `\Deleted` Missive set
    // into a folder where no record of it is kept, and any client's EXPUNGE would remove it.
    let second = events.action().await;
    assert_eq!(second["outcome"], "refused", "{second}");
    assert_eq!(second["reason"], "deleted-left", "{second}");
    let message = second["message"].as_str().unwrap();
    assert!(message.contains("\\Deleted"), "{message}");
    assert_eq!(server.uids(PRIVATE_FOLDER), [1]);
    assert_eq!(tries(&server, "UID COPY"), 1);
    assert_eq!(env.queue(), 0);

    // The flag is taken back at the next connection, and still nothing was moved.
    server.heal();
    succeeded(&sync_soon(&env).await);
    eventually(20, "the flag taken back", || {
        server.flags("INBOX", 3) == Some(Vec::new())
    })
    .await;
    assert_eq!(server.uids(PRIVATE_FOLDER), [1]);
    assert_eq!(server.uids("INBOX"), [1, 2, 3, 4]);
    assert_eq!(server.flags("INBOX", 4), flags(&["\\Deleted"]));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deleted_flag_left_in_a_folder_since_deleted_on_the_server_is_let_go() {
    // The removal is refused, and taking its flag back too, until the limit refuses the move.
    let (server, env, _daemon) = started(
        mailbox()
            .without_move()
            .refuse("UID EXPUNGE")
            .refuse("UID STORE 1 -FLAGS"),
    )
    .await;
    let mut events = Events::subscribe(&env).await;
    let one = env.id(PRIVATE_FOLDER, 1);
    succeeded(&env.act(&["move", &one, "--to", "archive"]).await);
    loop {
        let event = events.action().await;
        if event["outcome"] != "retried" {
            assert_eq!(event["outcome"], "refused", "{event}");
            break;
        }
        succeeded(&sync_soon(&env).await);
    }
    assert_eq!(
        env.status_of_alice().await["deleted_left"],
        serde_json::json!([one])
    );

    // Another client deletes the folder: no message is left to take the flag back from.
    server.delete_folder(PRIVATE_FOLDER);
    server.heal();
    succeeded(&sync_soon(&env).await);
    let started = Instant::now();
    while env.status_of_alice().await["deleted_left"] != serde_json::json!([]) {
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the flag of a deleted folder is still listed"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // What another client marked is left alone.
    assert_eq!(server.flags("INBOX", 4), flags(&["\\Deleted"]));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deleted_flag_left_is_kept_while_the_server_lists_no_folder_at_all() {
    let (server, env, _daemon) = started(
        mailbox()
            .without_move()
            .refuse("UID EXPUNGE")
            .refuse("UID STORE 1 -FLAGS"),
    )
    .await;
    let mut events = Events::subscribe(&env).await;
    let one = env.id(PRIVATE_FOLDER, 1);
    succeeded(&env.act(&["move", &one, "--to", "archive"]).await);
    let event = until_settled(&env, &mut events).await;
    assert_eq!(event["outcome"], "refused", "{event}");
    assert_eq!(
        env.status_of_alice().await["deleted_left"],
        serde_json::json!([one])
    );

    // A bridge still syncing: the folder cannot be selected, and LIST answers OK with nothing,
    // not even the inbox. That does not tell the folder is gone.
    server.heal();
    server.refuse(&format!("SELECT \"{PRIVATE_FOLDER}\""));
    server.truncate("LIST", 0);
    // The folder is asked for only once it cannot be selected.
    let started = Instant::now();
    while tries(&server, &format!("LIST \"\" \"{PRIVATE_FOLDER}\"")) < 1 {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the folder was not asked for"
        );
        succeeded(&sync_soon(&env).await);
    }
    assert_eq!(
        env.status_of_alice().await["deleted_left"],
        serde_json::json!([one])
    );

    // Once the server lists its folders again without this one, it is let go.
    server.heal();
    server.delete_folder(PRIVATE_FOLDER);
    succeeded(&sync_soon(&env).await);
    let started = Instant::now();
    while env.status_of_alice().await["deleted_left"] != serde_json::json!([]) {
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the flag of a deleted folder is still listed"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deleted_flag_the_server_never_takes_back_is_said_and_leaves_once_cleared_elsewhere() {
    let (server, env, _daemon) = started(
        mailbox()
            .without_move()
            .refuse("UID EXPUNGE")
            .refuse("UID STORE 3 -FLAGS"),
    )
    .await;
    let mut events = Events::subscribe(&env).await;
    let three = env.id("INBOX", 3);
    succeeded(&env.act(&["move", &three, "--to", "archive"]).await);
    let event = until_settled(&env, &mut events).await;
    assert_eq!(event["outcome"], "refused", "{event}");
    assert_eq!(server.flags("INBOX", 3), flags(&["\\Deleted"]));
    let status = env.status_of_alice().await;
    assert_eq!(status["deleted_left"], serde_json::json!([three]));
    assert_eq!(status["deleted_refused"], serde_json::json!([]));

    // Taking it back is refused at each pass: after the limit, `status` says so.
    let started = Instant::now();
    while env.status_of_alice().await["deleted_refused"] != serde_json::json!([three]) {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "a flag the server never takes back is not said"
        );
        succeeded(&sync_soon(&env).await);
    }
    let text = stdout(&env.act(&["status"]).await);
    assert!(text.contains("refused to take it back"), "{text}");

    // Another client clears it: the next sync of the folder sees it gone, and the row leaves.
    server.set_flags("INBOX", 3, &[]);
    succeeded(&sync_soon(&env).await);
    let started = Instant::now();
    loop {
        let status = env.status_of_alice().await;
        if status["deleted_left"] == serde_json::json!([])
            && status["deleted_refused"] == serde_json::json!([])
        {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "a flag cleared elsewhere is still listed: {status}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Nothing holds a move of the message back any more.
    server.heal();
    succeeded(&env.act(&["move", &three, "--to", PRIVATE_FOLDER]).await);
    eventually(20, "the message moved", || {
        server.uids(PRIVATE_FOLDER) == [1, 2]
    })
    .await;
    assert_eq!(server.flags(PRIVATE_FOLDER, 2), Some(Vec::new()));
}

impl Env {
    /// The rows of `deleted_left`: the `\Deleted` flags Missive still has to take back.
    fn deleted_left_rows(&self) -> i64 {
        self.copy()
            .unwrap()
            .query_row("SELECT count(*) FROM deleted_left", [], |row| row.get(0))
            .unwrap()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_half_move_is_kept_when_a_renumbered_folder_answers_empty_and_is_found_when_it_fills() {
    let (server, env, _daemon) = started(mailbox().without_move().refuse("UID EXPUNGE")).await;
    let mut events = Events::subscribe(&env).await;
    let three = env.id("INBOX", 3);
    succeeded(&env.act(&["move", &three, "--to", "archive"]).await);
    until_settled(&env, &mut events).await;
    eventually(10, "the copy in the archive", || env.uids("Archive") == [1]).await;
    server.heal();
    assert_eq!(env.half_move_rows(), 1);

    // A bridge still syncing: the inbox has a new UIDVALIDITY and nothing in it yet.
    for uid in 1..=4 {
        server.expunge("INBOX", uid);
    }
    server.bump_uidvalidity("INBOX");
    succeeded(&sync_soon(&env).await);
    eventually(10, "the inbox emptied", || env.uids("INBOX").is_empty()).await;
    // An empty listing settles nothing: the message may come back.
    assert_eq!(env.half_move_rows(), 1);

    // It refills: the message is in both folders, and `status` says so.
    server.deliver("INBOX", message(1).flag("\\Seen"));
    server.deliver("INBOX", message(2).flag("\\Flagged"));
    server.deliver("INBOX", message(3));
    server.deliver("INBOX", message(4).flag("\\Deleted"));
    succeeded(&sync_soon(&env).await);
    eventually(10, "the inbox refilled", || {
        env.uids("INBOX") == [1, 2, 3, 4]
    })
    .await;
    let found = env.id("INBOX", 3);
    let copy = env.id("Archive", 1);
    half_moved_soon(
        &env,
        serde_json::json!([{
            "message": found, "copy": copy, "folder": "inbox", "target": "archive",
            "maybe": false,
        }]),
    )
    .await;
    assert_eq!(server.uids("Archive"), [1]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_half_move_is_kept_while_a_permanent_delete_of_its_copy_waits() {
    let (server, env, _daemon) = started(mailbox().without_move().refuse("UID EXPUNGE")).await;
    let mut events = Events::subscribe(&env).await;
    let three = env.id("INBOX", 3);
    succeeded(&env.act(&["delete", &three]).await);
    let event = until_settled(&env, &mut events).await;
    assert_eq!(event["copied"], true, "{event}");
    eventually(10, "the copy in the trash", || {
        env.uids("Trash") == [1, 2, 3]
    })
    .await;
    assert_eq!(env.half_move_rows(), 1);

    // The copy in the trash is deleted for good, and the server keeps refusing: the first try is
    // retried, and a sync runs meanwhile. The copy is not in the copy then, but only because it
    // is being deleted.
    let copy = env.id("Trash", 3);
    succeeded(&env.act(&["delete", "--permanently", &copy]).await);
    assert_eq!(events.action().await["outcome"], "retried");
    succeeded(&sync_soon(&env).await);
    assert_eq!(env.half_move_rows(), 1);

    // Refused for good: the copy is back, and the message is still said to be in both folders.
    let event = until_settled(&env, &mut events).await;
    assert_eq!(event["outcome"], "refused", "{event}");
    server.heal();
    succeeded(&sync_soon(&env).await);
    eventually(10, "the copy back in the trash", || {
        env.uids("Trash") == [1, 2, 3]
    })
    .await;
    half_moved_soon(
        &env,
        serde_json::json!([{
            "message": three, "copy": copy, "folder": "inbox", "target": "trash",
            "maybe": false,
        }]),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deleted_flag_left_is_kept_when_the_folder_answers_empty() {
    let (server, env, _daemon) = started(
        mailbox()
            .without_move()
            .refuse("UID EXPUNGE")
            .refuse("UID STORE 3 -FLAGS"),
    )
    .await;
    let mut events = Events::subscribe(&env).await;
    let three = env.id("INBOX", 3);
    succeeded(&env.act(&["move", &three, "--to", "archive"]).await);
    until_settled(&env, &mut events).await;
    assert_eq!(env.deleted_left_rows(), 1);
    assert_eq!(server.flags("INBOX", 3), flags(&["\\Deleted"]));

    // The same UIDVALIDITY, and an empty listing: nothing says the flag is gone.
    for uid in 1..=4 {
        server.expunge("INBOX", uid);
    }
    succeeded(&sync_soon(&env).await);
    eventually(10, "the inbox emptied", || env.uids("INBOX").is_empty()).await;
    assert_eq!(env.deleted_left_rows(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deleted_flag_left_is_kept_while_a_permanent_delete_of_its_message_waits() {
    let server = FakeServer::builder()
        .user(USER, PASSWORD)
        .folder(Folder::new("INBOX").message(message(1)).message(message(2)))
        .refuse("UID EXPUNGE")
        .refuse("UID STORE 1 -FLAGS")
        .start()
        .await;
    let env = Env::new().await;
    env.add_account("alice", &server, USER, Some(PASSWORD))
        .await;
    let _daemon = env.daemon(&["--check-interval", "3600"]).await;
    let mut events = Events::subscribe(&env).await;
    succeeded(&sync_soon(&env).await);

    // The first delete is refused for good and leaves its flag on the server.
    let one = env.id("INBOX", 1);
    succeeded(&env.act(&["delete", "--permanently", &one]).await);
    let event = until_settled(&env, &mut events).await;
    assert_eq!(event["outcome"], "refused", "{event}");
    assert_eq!(server.flags("INBOX", 1), flags(&["\\Deleted"]));
    assert_eq!(env.deleted_left_rows(), 1);

    // A second one waits for its next try, and a sync runs meanwhile: the message is not in the
    // copy then, but only because it is being deleted.
    succeeded(&env.act(&["delete", "--permanently", &one]).await);
    assert_eq!(events.action().await["outcome"], "retried");
    succeeded(&sync_soon(&env).await);
    assert_eq!(env.deleted_left_rows(), 1);

    // Refused for good again: the flag is still there, and still said.
    let event = until_settled(&env, &mut events).await;
    assert_eq!(event["outcome"], "refused", "{event}");
    assert_eq!(server.flags("INBOX", 1), flags(&["\\Deleted"]));
    succeeded(&sync_soon(&env).await);
    eventually(10, "the message back in the copy", || {
        env.uids("INBOX") == [1, 2]
    })
    .await;
    assert_eq!(env.deleted_left_rows(), 1);
}
