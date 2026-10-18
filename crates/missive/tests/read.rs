//! `missive status`, `unread`, `search`, `list`, `show` and `open`, which answer from the local
//! copy alone. The copy is filled by a daemon from the fake IMAP server; the reading commands then run with the daemon
//! stopped, the server gone and no session bus at all, so that whatever they would ask of the
//! network or the keyring fails. Everything lives in temporary folders.

use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::{Duration, Instant};

use missive_core::account::{Account, AccountStore, Password, Security, Server};
use missive_core::fake::{
    FakeKeyring, FakePortal, FakeServer, Folder, Message, PortalAnswer, SpecialUse,
};
use missive_core::keyring::Keyring;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::UnixStream;
use tokio::process::{Child, Command};

const USER: &str = "alice@example.org";
const PASSWORD: &str = "Recognisable-Pa55word-4c1d";
const SENDER: &str = "carol.sender@example.com";
const RECIPIENT: &str = "dave.recipient@example.net";

fn mail(headers: &str, body: &str) -> String {
    format!(
        "From: Carol Sender <{SENDER}>\r\nTo: Dave Recipient <{RECIPIENT}>\r\n\
         Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\
         {headers}\r\n{body}\r\n"
    )
}

/// A message whose subject is `Plan <n>` and whose date is `n` minutes after 09:00 (+0200) on
/// 17 October 2026.
fn plan(n: u32, extra: &str, body: &str) -> Message {
    Message::new(mail(
        &format!(
            "Subject: Plan {n}\r\nMessage-ID: <{n}@example.com>\r\n\
             Date: Sat, 17 Oct 2026 09:{n:02}:00 +0200\r\n{extra}"
        ),
        body,
    ))
}

/// INBOX: 1 is seen; 2 has its word in the subject only; 3 in the body only; 4 in an HTML body
/// only; 5 is full of accents and sent by someone else. Sent holds one seen message, and a folder
/// of the user's own two unread ones.
fn mailbox() -> missive_core::fake::Builder {
    let subject_only = Message::new(mail(
        "Subject: Pelican migration\r\nMessage-ID: <2@example.com>\r\n\
         Date: Sat, 17 Oct 2026 09:02:00 +0200\r\n",
        "Nothing else here.",
    ))
    .flag("\\Flagged");
    let html_only = Message::new(
        "From: Carol Sender <carol.sender@example.com>\r\n\
         Subject: Plan 4\r\nMessage-ID: <4@example.com>\r\n\
         Date: Sat, 17 Oct 2026 09:04:00 +0200\r\nContent-Type: text/html; charset=utf-8\r\n\
         \r\n<html><head><style>.hidden { color: red }</style></head><body>\
         <p>The <b>Quokka</b> waits.</p><a href=\"https://tracker.example.net/blockquote\">\
         <span>read</span></a></body></html>\r\n",
    );
    let accents = Message::new(
        "From: Zoé Müller <zoe.muller@example.org>\r\nTo: Dave Recipient <dave.recipient@example.net>\r\n\
         Subject: Café Crème\r\nMessage-ID: <5@example.com>\r\n\
         Date: Sat, 17 Oct 2026 09:05:00 +0200\r\n\
         Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\
         \r\nLes Éléphants d'Élodie sont naïfs.\r\n",
    );
    FakeServer::builder()
        .user(USER, PASSWORD)
        .folder(
            Folder::new("INBOX")
                .message(plan(1, "", "Nothing special.").flag("\\Seen"))
                .message(subject_only)
                .message(plan(3, "", "The Zanzibar archive."))
                .message(html_only)
                .message(accents),
        )
        .folder(
            Folder::new("Sent")
                .special_use(SpecialUse::Sent)
                .message(plan(6, "", "Sent Zanzibar.").flag("\\Seen")),
        )
        .folder(
            Folder::new("Projects")
                .message(plan(7, "", "Nothing special.").uid(5))
                .message(plan(8, "", "Nothing special.").uid(9)),
        )
}

struct Env {
    home: TempDir,
    keyring: FakeKeyring,
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
        }
    }

    fn path(&self, folder: &str) -> PathBuf {
        self.home.path().join(folder)
    }

    async fn add_account(&self, id: &str, server: &FakeServer) {
        let account = Account {
            name: "Alice Example".to_owned(),
            address: USER.to_owned(),
            user: USER.to_owned(),
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
        };
        AccountStore::new(self.path("config").join("missive"))
            .save(id, &account, Some(&server.certificate_der()))
            .unwrap();
        let connection = zbus::connection::Builder::address(self.keyring.address())
            .unwrap()
            .build()
            .await
            .unwrap();
        Keyring::open_on(connection)
            .await
            .unwrap()
            .store(id, &Password::new(PASSWORD.to_owned()))
            .await
            .unwrap();
    }

    fn command(&self, args: &[&str], bus: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_missive"));
        command
            .args(args)
            .env_clear()
            .env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.path("config"))
            .env("XDG_DATA_HOME", self.path("data"))
            .env("XDG_STATE_HOME", self.path("state"))
            .env("XDG_RUNTIME_DIR", self.path("runtime"))
            .env("DBUS_SESSION_BUS_ADDRESS", bus)
            .stdin(Stdio::null());
        command
    }

    /// A command of the reading kind: there is no session bus for it to find, so it fails if it
    /// asks for the keyring.
    async fn read(&self, args: &[&str]) -> Output {
        let nowhere = format!("unix:path={}", self.path("no-bus").display());
        self.command(args, &nowhere).output().await.unwrap()
    }

    async fn sync(&self) -> Output {
        self.command(&["sync"], self.keyring.address())
            .output()
            .await
            .unwrap()
    }

    /// Starts `missive daemon`, which has the keyring, and waits until it answers.
    async fn daemon(&self, args: &[&str]) -> Daemon {
        let mut arguments = vec!["daemon"];
        arguments.extend_from_slice(args);
        let child = self
            .command(&arguments, self.keyring.address())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let started = Instant::now();
        while UnixStream::connect(self.path("runtime").join("missive/daemon.sock"))
            .await
            .is_err()
        {
            assert!(started.elapsed() < Duration::from_secs(10), "no daemon");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Daemon { child }
    }

    fn copy_path(&self) -> PathBuf {
        self.path("data").join("missive/copy.sqlite")
    }

    /// The daemon fills the copy from the fake server, then stops, and the server goes: what
    /// follows is a machine with the network down and no daemon.
    async fn filled() -> Env {
        Env::filled_with(vec![("alice", mailbox())]).await
    }

    /// As [`Env::filled`], for these accounts, each with its own fake server.
    async fn filled_with(accounts: Vec<(&str, missive_core::fake::Builder)>) -> Env {
        let env = Env::new().await;
        let mut servers = Vec::new();
        for (id, builder) in accounts {
            let server = builder.start().await;
            env.add_account(id, &server).await;
            servers.push(server);
        }
        let mut daemon = env.daemon(&[]).await;
        succeeded(&env.sync().await);
        daemon.stop().await;
        drop(servers);
        env
    }
}

struct Daemon {
    child: Child,
}

impl Daemon {
    async fn stop(&mut self) {
        let pid = self.child.id().expect("the daemon is running").to_string();
        assert!(
            std::process::Command::new("kill")
                .args(["-TERM", &pid])
                .status()
                .unwrap()
                .success()
        );
        tokio::time::timeout(Duration::from_secs(10), self.child.wait())
            .await
            .expect("the daemon exits within 10 seconds")
            .unwrap();
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

fn json(output: &Output) -> Value {
    succeeded(output);
    serde_json::from_str(&stdout(output))
        .unwrap_or_else(|e| panic!("not JSON ({e}): {}", stdout(output)))
}

/// The `(folder name, uid)` of what a search found, in order.
async fn found(env: &Env, words: &[&str]) -> Vec<(String, u64)> {
    let mut args = vec!["search", "--json"];
    args.extend_from_slice(words);
    json(&env.read(&args).await)
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| {
            (
                hit["folder"]["name"].as_str().unwrap().to_owned(),
                hit["uid"].as_u64().unwrap(),
            )
        })
        .collect()
}

fn inbox(uid: u64) -> (String, u64) {
    ("INBOX".to_owned(), uid)
}

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

fn state_in_copy(path: &Path) -> Option<String> {
    let copy =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()?;
    copy.query_row("SELECT state FROM account", [], |row| row.get(0))
        .ok()
}

#[tokio::test(flavor = "multi_thread")]
async fn unread_matches_the_unseen_messages_of_the_fake_server() {
    let env = Env::filled().await;

    let unread = json(&env.read(&["unread", "--json"]).await);

    assert_eq!(unread["total"], 6);
    let alice = &unread["accounts"][0];
    assert_eq!(alice["id"], "alice");
    assert_eq!(alice["unread"], 6);
    let folders: Vec<(&str, Option<&str>, u64)> = alice["folders"]
        .as_array()
        .unwrap()
        .iter()
        .map(|folder| {
            (
                folder["name"].as_str().unwrap(),
                folder["role"].as_str(),
                folder["unread"].as_u64().unwrap(),
            )
        })
        .collect();
    // The inbox first, then the other roles, then the user's own folders.
    assert_eq!(
        folders,
        [
            ("INBOX", Some("inbox"), 4),
            ("Sent", Some("sent"), 0),
            ("Projects", None, 2),
        ]
    );
    assert!(alice["folders"][0]["id"].is_u64());
}

#[tokio::test(flavor = "multi_thread")]
async fn unread_says_it_in_words_too() {
    let env = Env::filled().await;

    let output = env.read(&["unread"]).await;

    succeeded(&output);
    assert_eq!(
        stdout(&output),
        "alice: 6 unread\n  inbox: 4\n  Projects: 2\ntotal: 6\n"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn search_finds_a_word_only_in_a_body() {
    let env = Env::filled().await;
    // Newest first: the sent one is the later message.
    assert_eq!(
        found(&env, &["Zanzibar"]).await,
        [("Sent".to_owned(), 1), inbox(3)]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn search_finds_a_word_only_in_an_html_body() {
    let env = Env::filled().await;
    assert_eq!(found(&env, &["quokka"]).await, [inbox(4)]);
    // The markup is not text: tags, styles, and the address a link goes to.
    for markup in ["html", "span", "style", "hidden", "blockquote", "tracker"] {
        assert!(found(&env, &[markup]).await.is_empty(), "{markup}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn search_finds_a_word_only_in_a_subject() {
    let env = Env::filled().await;
    assert_eq!(found(&env, &["pelican"]).await, [inbox(2)]);
}

#[tokio::test(flavor = "multi_thread")]
async fn search_does_not_mind_accents_or_case() {
    let env = Env::filled().await;
    for words in [
        &["cafe", "creme"][..],
        &["CAFÉ", "CRÈME"],
        &["Cafe"],
        &["elephants"],
        &["ÉLÉPHANTS"],
        &["elodie", "naifs"],
        &["zoe", "muller"],
        &["ZOÉ"],
    ] {
        assert_eq!(found(&env, words).await, [inbox(5)], "{words:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn search_finds_by_sender_and_by_recipient() {
    let env = Env::filled().await;
    let from_carol = found(&env, &["carol.sender@example.com"]).await;
    assert_eq!(from_carol.len(), 7);
    assert!(!from_carol.contains(&inbox(5)));
    assert_eq!(found(&env, &["Dave", "Müller"]).await, [inbox(5)]);
    assert_eq!(
        found(&env, &["dave.recipient@example.net", "zoe"]).await,
        [inbox(5)]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn search_can_be_narrowed_to_a_folder_role_and_limited() {
    let env = Env::filled().await;

    let sent = json(
        &env.read(&["search", "--json", "--folder", "sent", "carol"])
            .await,
    );
    assert_eq!(sent.as_array().unwrap().len(), 1);
    assert_eq!(sent[0]["folder"]["role"], "sent");

    let limited = json(
        &env.read(&["search", "--json", "--limit", "2", "carol"])
            .await,
    );
    // Newest first: Plan 8, then Plan 7.
    assert_eq!(limited[0]["subject"], "Plan 8");
    assert_eq!(limited[1]["subject"], "Plan 7");
    assert_eq!(limited.as_array().unwrap().len(), 2);

    let account = json(
        &env.read(&["search", "--json", "--account", "alice", "pelican"])
            .await,
    );
    assert_eq!(account.as_array().unwrap().len(), 1);

    let nobody = env
        .read(&["search", "--account", "nobody", "pelican"])
        .await;
    assert!(!nobody.status.success());
    assert!(
        stderr(&nobody).contains("no account nobody"),
        "{}",
        stderr(&nobody)
    );

    // Only a role can be asked for: a usage error.
    let bad = env.read(&["search", "--folder", "Projects", "carol"]).await;
    assert_eq!(bad.status.code(), Some(2));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hit_carries_the_identifier_a_later_open_will_take() {
    let env = Env::filled().await;

    let hits = json(&env.read(&["search", "--json", "quokka"]).await);

    let hit = &hits[0];
    let folder = hit["folder"]["id"].as_u64().unwrap();
    let uidvalidity = hit["uidvalidity"].as_u64().unwrap();
    assert_eq!(hit["id"], format!("alice/{folder}/{uidvalidity}/4"));
    assert_eq!(hit["account"], "alice");
    assert_eq!(hit["uid"], 4);
    assert_eq!(hit["subject"], "Plan 4");
    assert_eq!(
        hit["from"],
        json!([{ "name": "Carol Sender", "address": SENDER }])
    );
    assert_eq!(hit["to"], json!([]));
    assert_eq!(hit["unread"], true);
    // 17 October 2026, 09:04 at +0200.
    assert_eq!(hit["date"], 1_792_220_640);
    assert_eq!(hit["folder"]["role"], "inbox");
    assert_eq!(hit["folder"]["name"], "INBOX");
}

#[tokio::test(flavor = "multi_thread")]
async fn search_says_it_in_words_too() {
    let env = Env::filled().await;

    let output = env.read(&["search", "pelican"]).await;

    succeeded(&output);
    let line = stdout(&output);
    let fields: Vec<&str> = line.trim_end().split('\t').collect();
    assert_eq!(line.matches('\n').count(), 1, "{line}");
    assert!(
        fields[0].starts_with("alice/") && fields[0].ends_with("/2"),
        "{line}"
    );
    assert_eq!(
        &fields[1..],
        ["2026-10-17 07:02 UTC", "Carol Sender", "Pelican migration"]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_query_made_of_search_syntax_is_searched_as_words() {
    let env = Env::filled().await;
    for query in [
        &["AND"][..],
        &["OR", "NOT"],
        &["NEAR(", "plan"],
        &["\""],
        &["\"unbalanced"],
        &["plan*"],
        &["subject:plan"],
        &["-plan"],
        &["(", ")"],
        &["'; DROP TABLE message; --"],
        &["!!!"],
    ] {
        // After `--`, so that a word starting with a dash is not taken for an option.
        let mut args = vec!["search", "--json", "--"];
        args.extend_from_slice(query);
        let output = env.read(&args).await;
        succeeded(&output);
        assert!(json(&output).is_array(), "{query:?}");
    }
    // A star is no prefix search: "pel" is no word of the subject, "pelican" is.
    assert!(found(&env, &["pel*"]).await.is_empty());
    assert_eq!(found(&env, &["pelican*"]).await, [inbox(2)]);
    assert!(found(&env, &["subject:pelican"]).await.is_empty());
    assert!(found(&env, &["pelican", "OR", "quokka"]).await.is_empty());
    // And the copy is whole.
    assert_eq!(found(&env, &["pelican"]).await, [inbox(2)]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_search_that_finds_nothing_prints_an_empty_list() {
    let env = Env::filled().await;
    let output = env.read(&["search", "--json", "absent"]).await;
    succeeded(&output);
    assert_eq!(stdout(&output), "[]\n");
    let output = env.read(&["search", "absent"]).await;
    succeeded(&output);
    assert_eq!(stdout(&output), "");
}

#[tokio::test(flavor = "multi_thread")]
async fn status_says_the_state_the_last_sync_and_the_message_count() {
    let env = Env::filled().await;

    let status = json(&env.read(&["status", "--json"]).await);

    assert_eq!(status["daemon"], "stopped");
    let alice = &status["accounts"][0];
    assert_eq!(alice["id"], "alice");
    assert_eq!(alice["state"], "ready");
    assert_eq!(alice["syncing"], false);
    assert_eq!(alice["messages"], 8);
    let last_sync = alice["last_sync"].as_i64().unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    assert!((now - 60..=now).contains(&last_sync), "{last_sync} / {now}");

    let words = env.read(&["status"]).await;
    succeeded(&words);
    let text = stdout(&words);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "daemon: stopped");
    assert!(
        lines[1].starts_with("alice\tready\tlast sync 20") && lines[1].ends_with("UTC\t8 messages"),
        "{text}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_account_not_yet_in_the_copy_is_never_synced_and_a_removed_one_is_not_shown() {
    let env = Env::filled().await;
    let server = mailbox().start().await;
    env.add_account("bob", &server).await;
    AccountStore::new(env.path("config").join("missive"))
        .remove("alice")
        .unwrap();

    let status = json(&env.read(&["status", "--json"]).await);

    assert_eq!(
        status["accounts"],
        json!([{ "id": "bob", "state": null, "syncing": false, "last_sync": null, "messages": 0 }])
    );
    let unread = json(&env.read(&["unread", "--json"]).await);
    assert_eq!(
        unread,
        json!({ "total": 0, "accounts": [{ "id": "bob", "unread": 0, "folders": [] }] })
    );
    assert_eq!(found(&env, &["pelican"]).await, []);
    let words = env.read(&["status"]).await;
    assert!(
        stdout(&words).contains("bob\tnever synced\t0 messages"),
        "{}",
        stdout(&words)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_killed_mid_sync_is_not_reported_as_syncing() {
    let mut inbox = Folder::new("INBOX");
    for n in 1..=40 {
        inbox = inbox.message(plan(n % 60, "", "Nothing special."));
    }
    let server = FakeServer::builder()
        .user(USER, PASSWORD)
        .folder(inbox)
        .fetch_delay(Duration::from_millis(40))
        .start()
        .await;
    let env = Env::new().await;
    env.add_account("alice", &server).await;
    let mut daemon = env.daemon(&["--batch-size", "2"]).await;
    let copy = env.copy_path();
    eventually(20, "the sync to begin", || {
        state_in_copy(&copy).as_deref() == Some("syncing")
    })
    .await;

    let running = json(&env.read(&["status", "--json"]).await);
    assert_eq!(running["daemon"], "running");
    assert_eq!(running["accounts"][0]["state"], "syncing");
    assert_eq!(running["accounts"][0]["syncing"], true);

    daemon.child.kill().await.unwrap();
    assert_eq!(state_in_copy(&copy).as_deref(), Some("syncing"));
    let killed = json(&env.read(&["status", "--json"]).await);
    assert_eq!(killed["daemon"], "stopped");
    assert_eq!(killed["accounts"][0]["state"], Value::Null);
    assert_eq!(killed["accounts"][0]["syncing"], false);
    let words = env.read(&["status"]).await;
    assert!(
        stdout(&words).contains("alice\tunknown (the daemon is not running)\t"),
        "{}",
        stdout(&words)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn status_does_not_wait_for_a_daemon_that_is_busy() {
    let env = Env::filled().await;
    let _daemon_is_running = env.daemon(&[]).await;
    let status = tokio::time::timeout(Duration::from_secs(5), env.read(&["status", "--json"]))
        .await
        .expect("status answers at once");
    assert_eq!(json(&status)["daemon"], "running");
}

#[tokio::test(flavor = "multi_thread")]
async fn with_no_copy_yet_the_commands_say_so_and_give_zeros_and_create_nothing() {
    let env = Env::new().await;
    let server = mailbox().start().await;
    env.add_account("alice", &server).await;

    let status = env.read(&["status", "--json"]).await;
    assert_eq!(
        json(&status),
        json!({ "daemon": "stopped", "accounts": [
            { "id": "alice", "state": null, "syncing": false, "last_sync": null, "messages": 0 }
        ]})
    );
    assert!(
        stderr(&status).contains("no local copy yet"),
        "{}",
        stderr(&status)
    );
    let unread = env.read(&["unread", "--json"]).await;
    assert_eq!(
        json(&unread),
        json!({ "total": 0, "accounts": [{ "id": "alice", "unread": 0, "folders": [] }] })
    );
    assert!(stderr(&unread).contains("no local copy yet"));
    let search = env.read(&["search", "--json", "anything"]).await;
    assert_eq!(stdout(&search), "[]\n");
    assert!(stderr(&search).contains("no local copy yet"));
    let words = env.read(&["unread"]).await;
    succeeded(&words);
    assert!(stderr(&words).contains("no local copy yet"));

    assert!(!env.copy_path().exists());
    assert!(!env.path("runtime").join("missive").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn with_no_account_at_all_everything_is_empty() {
    let env = Env::new().await;
    assert_eq!(
        json(&env.read(&["status", "--json"]).await),
        json!({ "daemon": "stopped", "accounts": [] })
    );
    assert_eq!(
        json(&env.read(&["unread", "--json"]).await),
        json!({ "total": 0, "accounts": [] })
    );
    assert_eq!(json(&env.read(&["search", "--json", "x"]).await), json!([]));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unreadable_account_is_named_on_standard_error_and_the_others_answer() {
    let env = Env::filled().await;
    let broken = env.path("config").join("missive/accounts/broken");
    std::fs::create_dir_all(&broken).unwrap();
    std::fs::write(broken.join("account.toml"), "not = [valid").unwrap();

    let status = env.read(&["status", "--json"]).await;

    assert_eq!(json(&status)["accounts"][0]["id"], "alice");
    assert!(
        stderr(&status).contains("skipped the account folder broken"),
        "{}",
        stderr(&status)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn search_follows_the_copy_as_mail_arrives_and_leaves() {
    let server = mailbox().start().await;
    let env = Env::new().await;
    env.add_account("alice", &server).await;
    let _daemon = env.daemon(&["--check-interval", "3600"]).await;
    succeeded(&env.sync().await);
    assert_eq!(found(&env, &["narwhal"]).await, []);

    let uid = server.deliver("INBOX", plan(9, "", "A narwhal appears."));
    succeeded(&env.sync().await);
    assert_eq!(found(&env, &["narwhal"]).await, [inbox(uid.into())]);

    server.expunge("INBOX", uid);
    succeeded(&env.sync().await);
    assert_eq!(found(&env, &["narwhal"]).await, []);
    // The others are still there.
    assert_eq!(found(&env, &["pelican"]).await, [inbox(2)]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_folder_renumbered_by_the_server_leaves_the_index() {
    let server = mailbox().start().await;
    let env = Env::new().await;
    env.add_account("alice", &server).await;
    let _daemon = env.daemon(&["--check-interval", "3600"]).await;
    succeeded(&env.sync().await);
    assert_eq!(found(&env, &["pelican"]).await, [inbox(2)]);

    server.bump_uidvalidity("INBOX");
    server.expunge("INBOX", 2);
    succeeded(&env.sync().await);

    // Refilled under the new UIDVALIDITY, without the expunged message.
    assert_eq!(found(&env, &["pelican"]).await, []);
    let hits = json(&env.read(&["search", "--json", "quokka"]).await);
    assert_eq!(hits.as_array().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_copy_from_before_the_search_index_can_be_counted_and_asks_for_the_daemon_to_search() {
    let env = Env::new().await;
    let server = mailbox().start().await;
    env.add_account("alice", &server).await;
    std::fs::create_dir_all(env.copy_path().parent().unwrap()).unwrap();
    {
        let connection = rusqlite::Connection::open(env.copy_path()).unwrap();
        connection
            .execute_batch(include_str!("../../missive-core/src/store/schema-1.sql"))
            .unwrap();
        connection.pragma_update(None, "user_version", 1).unwrap();
        connection
            .execute_batch("INSERT INTO account (key, id, state) VALUES (1, 'alice', 'ready')")
            .unwrap();
    }

    assert_eq!(json(&env.read(&["unread", "--json"]).await)["total"], 0);
    let search = env.read(&["search", "anything"]).await;
    assert_eq!(search.status.code(), Some(1));
    assert!(
        stderr(&search).contains("no search index yet"),
        "{}",
        stderr(&search)
    );

    // The daemon brings the copy up to date when it starts, and the same copy then searches.
    let mut daemon = env.daemon(&["--check-interval", "3600"]).await;
    daemon.stop().await;
    assert_eq!(found(&env, &["anything"]).await, []);
}

// ---- `missive list` and `missive show` ----

/// The raw message that is none: not a header, not a line, bytes that are not UTF-8.
const MALFORMED: &[u8] = b"\x00\x01\xff\xfe this is not a mail\r\n\x80\x81";

/// A message with a text part and a PDF of twelve bytes once decoded.
fn with_attachment() -> Message {
    Message::new(format!(
        "From: Carol Sender <{SENDER}>\r\nTo: Dave Recipient <{RECIPIENT}>\r\n\
         Subject: Plan 3\r\nMessage-ID: <3@example.com>\r\n\
         Date: Sat, 17 Oct 2026 09:03:00 +0200\r\nMIME-Version: 1.0\r\n\
         Content-Type: multipart/mixed; boundary=\"b1\"\r\n\r\n\
         --b1\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nThe report is attached.\r\n\
         --b1\r\nContent-Type: application/pdf; name=\"report.pdf\"\r\n\
         Content-Disposition: attachment; filename=\"report.pdf\"\r\n\
         Content-Transfer-Encoding: base64\r\n\r\nSGVsbG8gd29ybGQh\r\n--b1--\r\n"
    ))
}

fn html_only() -> Message {
    Message::new(format!(
        "From: Carol Sender <{SENDER}>\r\nSubject: Plan 4\r\nMessage-ID: <4@example.com>\r\n\
         Date: Sat, 17 Oct 2026 09:04:00 +0200\r\nContent-Type: text/html; charset=utf-8\r\n\
         \r\n<html><head><style>.hidden {{ color: red }}</style></head><body>\
         <script>alert('never run')</script><p>The <b>Quokka</b> waits.</p>\
         <p>Second paragraph.</p><a href=\"https://example.net/read\">read it</a>\
         </body></html>\r\n"
    ))
}

fn alternative() -> Message {
    Message::new(format!(
        "From: Carol Sender <{SENDER}>\r\nTo: Dave Recipient <{RECIPIENT}>\r\n\
         Cc: Erin Copy <erin.copy@example.org>\r\nReply-To: Carol Elsewhere <carol.elsewhere@example.com>\r\n\
         Subject: Plan 5\r\nMessage-ID: <5@example.com>\r\n\
         Date: Sat, 17 Oct 2026 09:05:00 +0200\r\nMIME-Version: 1.0\r\n\
         Content-Type: multipart/alternative; boundary=\"b2\"\r\n\r\n\
         --b2\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nPlain version.\r\n\
         --b2\r\nContent-Type: text/html; charset=utf-8\r\n\r\n\
         <p>HTML <b>version</b>.</p>\r\n--b2--\r\n"
    ))
}

/// A body in ISO-8859-1, a subject in an encoded word of the same charset.
fn latin1_raw() -> Vec<u8> {
    let mut raw = format!(
        "From: Carol Sender <{SENDER}>\r\nSubject: =?iso-8859-1?Q?Caf=E9?=\r\n\
         Message-ID: <6@example.com>\r\nDate: Sat, 17 Oct 2026 09:06:00 +0200\r\n\
         Content-Type: text/plain; charset=iso-8859-1\r\nContent-Transfer-Encoding: 8bit\r\n\r\n"
    )
    .into_bytes();
    raw.extend_from_slice(b"Un caf\xe9 cr\xe8me, s'il vous pla\xeet.\r\n");
    raw
}

/// alice: INBOX holds seven messages (1 seen; 2 flagged and answered; 3 with an attachment; 4 only
/// HTML; 5 in two versions; 6 in Latin-1; 7 not a message, with no `Date` and the newest internal
/// date), Sent one seen message, and Projects (a folder of the user's own) two.
fn alice() -> missive_core::fake::Builder {
    FakeServer::builder()
        .user(USER, PASSWORD)
        .folder(
            Folder::new("INBOX")
                .message(plan(1, "", "Nothing special.").flag("\\Seen"))
                .message(
                    plan(2, "", "Flagged and answered.")
                        .flag("\\Flagged")
                        .flag("\\Answered"),
                )
                .message(with_attachment())
                .message(html_only())
                .message(alternative())
                .message(Message::new(latin1_raw()))
                .message(Message::new(MALFORMED).internal_date("20-Oct-2026 10:00:00 +0000")),
        )
        .folder(
            Folder::new("Sent")
                .special_use(SpecialUse::Sent)
                .message(plan(8, "", "Sent.").flag("\\Seen")),
        )
        .folder(
            Folder::new("Projects")
                .message(plan(9, "", "Nothing special.").uid(5))
                .message(plan(10, "", "Nothing special.").uid(9)),
        )
}

/// bob: one message in the inbox, dated between alice's.
fn bob() -> missive_core::fake::Builder {
    FakeServer::builder()
        .user(USER, PASSWORD)
        .folder(Folder::new("INBOX").message(plan(11, "", "From bob's side.")))
}

/// What a list answered, as `(account, folder name, uid)` in order.
fn listed(output: &Output) -> Vec<(String, String, u64)> {
    json(output)
        .as_array()
        .unwrap()
        .iter()
        .map(|message| {
            (
                message["account"].as_str().unwrap().to_owned(),
                message["folder"]["name"].as_str().unwrap().to_owned(),
                message["uid"].as_u64().unwrap(),
            )
        })
        .collect()
}

fn in_alice(folder: &str, uid: u64) -> (String, String, u64) {
    ("alice".to_owned(), folder.to_owned(), uid)
}

/// The identifier `list` gives to a message of alice's inbox.
async fn inbox_id(env: &Env, uid: u64) -> String {
    let all = json(&env.read(&["list", "--json", "--limit", "100"]).await);
    all.as_array()
        .unwrap()
        .iter()
        .find(|m| m["account"] == "alice" && m["uid"] == uid)
        .unwrap_or_else(|| panic!("no message {uid} in the inbox: {all}"))["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn show(env: &Env, uid: u64) -> Value {
    let id = inbox_id(env, uid).await;
    json(&env.read(&["show", "--json", &id]).await)
}

#[tokio::test(flavor = "multi_thread")]
async fn list_gives_the_inbox_newest_first_by_date_else_internal_date() {
    let env = Env::filled_with(vec![("alice", alice())]).await;

    let output = env.read(&["list", "--json"]).await;

    // 7 has no Date: its internal date (20 October) puts it first. The others follow their Date,
    // minute by minute.
    assert_eq!(
        listed(&output),
        [7, 6, 5, 4, 3, 2, 1].map(|uid| in_alice("INBOX", uid))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_listed_message_carries_what_search_prints_and_more() {
    let env = Env::filled_with(vec![("alice", alice())]).await;

    let list = json(&env.read(&["list", "--json"]).await);
    let flagged = list
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["uid"] == 2)
        .unwrap();
    let plain = list
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["uid"] == 1)
        .unwrap();
    let hits = json(&env.read(&["search", "--json", "special"]).await);

    // The very fields of a search hit, so that one reader reads both.
    let hit = hits
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["uid"] == 1)
        .unwrap();
    for field in [
        "id",
        "account",
        "folder",
        "uidvalidity",
        "uid",
        "date",
        "subject",
        "from",
        "to",
    ] {
        assert_eq!(plain[field], hit[field], "{field}");
    }
    assert_eq!(plain["subject"], "Plan 1");
    assert_eq!(
        plain["from"],
        json!([{ "name": "Carol Sender", "address": SENDER }])
    );
    // 17 October 2026, 09:01 at +0200.
    assert_eq!(plain["date"], 1_792_220_460);
    assert_eq!(plain["unread"], false);
    assert_eq!(plain["flagged"], false);
    assert_eq!(plain["answered"], false);
    assert_eq!(plain["has_attachments"], false);
    assert!(plain["size"].as_u64().unwrap() > 100, "{plain}");
    assert_eq!(flagged["unread"], true);
    assert_eq!(flagged["flagged"], true);
    assert_eq!(flagged["answered"], true);
    let with = list
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["uid"] == 3)
        .unwrap();
    assert_eq!(with["has_attachments"], true);
    // A message in two versions, or in HTML, is not an attachment.
    for uid in [4, 5] {
        let message = list
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["uid"] == uid)
            .unwrap();
        assert_eq!(message["has_attachments"], false, "{message}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_newsletter_with_inline_images_has_no_attachment_but_a_file_makes_one() {
    let newsletter = |extra: &str| {
        Message::new(format!(
            "From: Carol Sender <{SENDER}>\r\nSubject: News\r\n\
             Date: Sat, 17 Oct 2026 09:20:00 +0200\r\nMIME-Version: 1.0\r\n\
             Content-Type: multipart/mixed; boundary=\"m\"\r\n\r\n\
             --m\r\nContent-Type: multipart/related; boundary=\"r\"\r\n\r\n\
             --r\r\nContent-Type: text/html; charset=utf-8\r\n\r\n\
             <p>News</p><img src=\"cid:logo@example.com\">\r\n\
             --r\r\nContent-Type: image/png\r\nContent-ID: <logo@example.com>\r\n\
             Content-Transfer-Encoding: base64\r\n\r\nAAEC\r\n--r--\r\n{extra}--m--\r\n"
        ))
    };
    let with_file = "--m\r\nContent-Type: application/pdf; name=\"a.pdf\"\r\n\
        Content-Disposition: attachment; filename=\"a.pdf\"\r\n\r\nPDF\r\n";
    let env = Env::filled_with(vec![(
        "alice",
        FakeServer::builder().user(USER, PASSWORD).folder(
            Folder::new("INBOX")
                .message(newsletter(""))
                .message(newsletter(with_file)),
        ),
    )])
    .await;

    let list = json(&env.read(&["list", "--json"]).await);
    let marks: Vec<(u64, bool)> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["uid"].as_u64().unwrap(),
                m["has_attachments"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(marks, [(2, true), (1, false)]);
    let text = stdout(&env.read(&["list"]).await);
    let flags: Vec<&str> = text
        .lines()
        .map(|l| l.split('\t').nth(1).unwrap())
        .collect();
    assert_eq!(flags, ["u..@", "u..."]);
}

#[tokio::test(flavor = "multi_thread")]
async fn list_can_keep_the_unread_messages_only() {
    let env = Env::filled_with(vec![("alice", alice())]).await;

    let output = env.read(&["list", "--json", "--unread"]).await;

    assert_eq!(
        listed(&output),
        [7, 6, 5, 4, 3, 2].map(|uid| in_alice("INBOX", uid))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn list_stops_at_the_limit_with_the_newest() {
    let env = Env::filled_with(vec![("alice", alice())]).await;

    let output = env.read(&["list", "--json", "--limit", "3"]).await;

    assert_eq!(listed(&output), [7, 6, 5].map(|uid| in_alice("INBOX", uid)));
    // A limit of nothing is a usage error, as in `search`.
    assert_eq!(
        env.read(&["list", "--limit", "0"]).await.status.code(),
        Some(2)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn list_takes_a_folder_by_role_or_by_name() {
    let env = Env::filled_with(vec![("alice", alice())]).await;

    let by_role = env.read(&["list", "--json", "--folder", "sent"]).await;
    let by_name = env.read(&["list", "--json", "--folder", "Sent"]).await;
    let own = env.read(&["list", "--json", "--folder", "Projects"]).await;

    assert_eq!(listed(&by_role), [in_alice("Sent", 1)]);
    assert_eq!(listed(&by_name), [in_alice("Sent", 1)]);
    // Newest first: Plan 10 (UID 9), then Plan 9 (UID 5).
    assert_eq!(
        listed(&own),
        [in_alice("Projects", 9), in_alice("Projects", 5)]
    );
    // The inbox, by its name as well.
    let inbox = env.read(&["list", "--json", "--folder", "INBOX"]).await;
    assert_eq!(listed(&inbox).len(), 7);
}

#[tokio::test(flavor = "multi_thread")]
async fn list_of_a_folder_that_does_not_exist_is_an_error() {
    let env = Env::filled_with(vec![("alice", alice())]).await;

    let output = env.read(&["list", "--folder", "Nowhere"]).await;

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("no folder Nowhere"),
        "{}",
        stderr(&output)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn list_of_several_accounts_merges_their_inboxes_or_takes_one() {
    let env = Env::filled_with(vec![("alice", alice()), ("bob", bob())]).await;

    let both = env.read(&["list", "--json", "--limit", "3"]).await;
    let only_bob = env.read(&["list", "--json", "--account", "bob"]).await;
    let sent = env.read(&["list", "--json", "--folder", "sent"]).await;

    // Bob's Plan 11 (09:11) comes between alice's message without a Date and her Plan 6.
    assert_eq!(
        listed(&both),
        [
            in_alice("INBOX", 7),
            ("bob".to_owned(), "INBOX".to_owned(), 1),
            in_alice("INBOX", 6)
        ]
    );
    assert_eq!(
        listed(&only_bob),
        [("bob".to_owned(), "INBOX".to_owned(), 1)]
    );
    // Bob has no Sent folder: that is not an error while another account has one.
    assert_eq!(listed(&sent), [in_alice("Sent", 1)]);
    let nobody = env.read(&["list", "--account", "nobody"]).await;
    assert_eq!(nobody.status.code(), Some(1));
    assert!(
        stderr(&nobody).contains("no account nobody"),
        "{}",
        stderr(&nobody)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn list_says_it_in_words_too() {
    let env = Env::filled_with(vec![("alice", alice())]).await;

    let output = env.read(&["list", "--limit", "7"]).await;

    succeeded(&output);
    let text = stdout(&output);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 7, "{text}");
    let id = inbox_id(&env, 2).await;
    // The identifier, four flags (u unread, f flagged, a answered, @ attachments; a dot for
    // none), the date in UTC, the first sender and the subject, separated by tabs.
    let line = lines.iter().find(|l| l.starts_with(&id)).unwrap();
    let columns: Vec<&str> = line.split('\t').collect();
    assert_eq!(
        columns[1..],
        ["ufa.", "2026-10-17 07:02 UTC", "Carol Sender", "Plan 2"]
    );
    let with = lines.iter().find(|l| l.ends_with("Plan 3")).unwrap();
    assert_eq!(with.split('\t').nth(1), Some("u..@"));
    // The message that is none has no subject and no sender.
    assert!(lines[0].ends_with("\t(no subject)"), "{}", lines[0]);
}

#[tokio::test(flavor = "multi_thread")]
async fn show_a_text_only_message() {
    let env = Env::filled_with(vec![("alice", alice())]).await;

    let message = show(&env, 1).await;

    assert_eq!(message["id"], inbox_id(&env, 1).await);
    assert_eq!(message["account"], "alice");
    assert_eq!(message["folder"]["role"], "inbox");
    assert_eq!(message["uid"], 1);
    assert_eq!(message["subject"], "Plan 1");
    assert_eq!(message["date"], 1_792_220_460);
    assert_eq!(
        message["from"],
        json!([{ "name": "Carol Sender", "address": SENDER }])
    );
    assert_eq!(
        message["to"],
        json!([{ "name": "Dave Recipient", "address": RECIPIENT }])
    );
    assert_eq!(message["cc"], json!([]));
    assert_eq!(message["reply_to"], json!([]));
    assert_eq!(message["message_id"], "1@example.com");
    assert_eq!(message["unread"], false);
    assert_eq!(message["text"], "Nothing special.\r\n");
    assert_eq!(message["html"], Value::Null);
    assert_eq!(message["attachments"], json!([]));
}

#[tokio::test(flavor = "multi_thread")]
async fn show_a_message_with_only_html_gives_it_as_written_and_prints_it_as_text() {
    let env = Env::filled_with(vec![("alice", alice())]).await;

    let message = show(&env, 4).await;
    let id = inbox_id(&env, 4).await;
    let words = env.read(&["show", &id]).await;

    // The HTML is the sender's, tags and script included; nothing interprets it.
    let html = message["html"].as_str().unwrap();
    assert!(
        html.contains("<script>alert('never run')</script>"),
        "{html}"
    );
    assert!(html.contains("<b>Quokka</b>"), "{html}");
    // No text part: the JSON does not make one up.
    assert_eq!(message["text"], Value::Null);
    // For people, the HTML as text: no tag, no style, no script, and the paragraphs stay apart.
    // A link keeps its words and gets its target after them; the JSON holds the HTML as written.
    succeeded(&words);
    // After the headers.
    let text = stdout(&words).split_once("\n\n").unwrap().1.to_owned();
    assert!(text.contains("The Quokka waits."), "{text}");
    assert!(text.contains("Second paragraph."), "{text}");
    assert!(!text.contains('<'), "{text}");
    assert!(!text.contains("color: red"), "{text}");
    assert!(!text.contains("never run"), "{text}");
    assert!(
        text.contains("read it [https://example.net/read]"),
        "{text}"
    );
    assert!(
        text.lines()
            .any(|l| l.contains("Quokka waits") && !l.contains("Second paragraph")),
        "{text}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn show_a_message_in_two_versions_gives_both_and_prints_the_text() {
    let env = Env::filled_with(vec![("alice", alice())]).await;

    let message = show(&env, 5).await;
    let id = inbox_id(&env, 5).await;
    let words = env.read(&["show", &id]).await;

    // The line break before a boundary belongs to the boundary.
    assert_eq!(message["text"], "Plain version.");
    assert_eq!(message["html"], "<p>HTML <b>version</b>.</p>");
    assert_eq!(
        message["cc"],
        json!([{ "name": "Erin Copy", "address": "erin.copy@example.org" }])
    );
    assert_eq!(
        message["reply_to"],
        json!([{ "name": "Carol Elsewhere", "address": "carol.elsewhere@example.com" }])
    );
    assert_eq!(message["attachments"], json!([]));
    // The text version, not the HTML turned into text.
    succeeded(&words);
    let text = stdout(&words);
    assert!(text.contains("Plain version."), "{text}");
    assert!(!text.contains("HTML"), "{text}");
    // The headers come first, then a blank line.
    assert!(text.starts_with("From: Carol Sender <"), "{text}");
    assert!(
        text.contains("\nCc: Erin Copy <erin.copy@example.org>\n"),
        "{text}"
    );
    assert!(
        text.contains("\nReply-To: Carol Elsewhere <carol.elsewhere@example.com>\n"),
        "{text}"
    );
    assert!(text.contains("\nSubject: Plan 5\n"), "{text}");
    assert!(text.contains("\nDate: 2026-10-17 07:05 UTC\n"), "{text}");
    assert!(
        text.contains("\nMessage-ID: 5@example.com\n\nPlain version."),
        "{text}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn show_a_message_with_an_attachment_lists_it() {
    let env = Env::filled_with(vec![("alice", alice())]).await;

    let message = show(&env, 3).await;
    let id = inbox_id(&env, 3).await;
    let words = env.read(&["show", &id]).await;

    assert_eq!(message["text"], "The report is attached.");
    // The part's number in the message (0 is the multipart itself, 1 the text), its decoded size.
    assert_eq!(
        message["attachments"],
        json!([{
            "name": "report.pdf",
            "media_type": "application/pdf",
            "size": 12,
            "part": 2
        }])
    );
    succeeded(&words);
    assert!(
        stdout(&words).contains("\nAttachments: report.pdf (application/pdf, 12 bytes)\n"),
        "{}",
        stdout(&words)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn show_a_message_in_another_charset_decodes_it() {
    let env = Env::filled_with(vec![("alice", alice())]).await;

    let message = show(&env, 6).await;
    let id = inbox_id(&env, 6).await;
    let words = env.read(&["show", &id]).await;

    assert_eq!(message["subject"], "Café");
    assert_eq!(message["text"], "Un café crème, s'il vous plaît.\r\n");
    assert!(
        stdout(&words).contains("Un café crème, s'il vous plaît."),
        "{}",
        stdout(&words)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn show_a_malformed_message_prints_what_can_be_read() {
    let env = Env::filled_with(vec![("alice", alice())]).await;
    let id = inbox_id(&env, 7).await;

    let as_json = env.read(&["show", "--json", &id]).await;
    let words = env.read(&["show", &id]).await;

    let message = json(&as_json);
    assert_eq!(message["uid"], 7);
    assert_eq!(message["subject"], Value::Null);
    assert_eq!(message["from"], json!([]));
    assert_eq!(message["attachments"], json!([]));
    assert_eq!(message["unread"], true);
    // It did not panic, and the command did not fail: it prints a message without headers.
    succeeded(&words);
    assert!(
        stdout(&words).contains("Subject: (no subject)"),
        "{}",
        stdout(&words)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn show_raw_gives_back_the_exact_bytes_stored() {
    let env = Env::filled_with(vec![("alice", alice())]).await;

    for (uid, expected) in [(7_u64, MALFORMED.to_vec()), (6, latin1_raw())] {
        let id = inbox_id(&env, uid).await;
        let output = env.read(&["show", "--raw", &id]).await;
        succeeded(&output);
        assert_eq!(output.stdout, expected, "message {uid}");
        assert_eq!(stderr(&output), "");
    }
    // The one or the other, not both.
    let id = inbox_id(&env, 1).await;
    let both = env.read(&["show", "--raw", "--json", &id]).await;
    assert_eq!(both.status.code(), Some(2));
}

#[tokio::test(flavor = "multi_thread")]
async fn show_does_not_print_the_control_characters_of_a_message_on_a_terminal() {
    let escape = Message::new(format!(
        "From: Carol Sender <{SENDER}>\r\nSubject: Plan\x1b[31m 12\r\n\
         Date: Sat, 17 Oct 2026 09:12:00 +0200\r\nContent-Type: text/plain; charset=utf-8\r\n\
         Content-Transfer-Encoding: 8bit\r\n\r\nLine one\x1b]0;owned\x07\r\nLine\ttwo\r\n"
    ));
    let env = Env::filled_with(vec![(
        "alice",
        FakeServer::builder()
            .user(USER, PASSWORD)
            .folder(Folder::new("INBOX").message(escape)),
    )])
    .await;
    let id = inbox_id(&env, 1).await;

    let output = env.read(&["show", &id]).await;

    succeeded(&output);
    let text = stdout(&output);
    assert!(!text.contains('\x1b') && !text.contains('\x07'), "{text:?}");
    assert!(text.contains("Line one"), "{text:?}");
    // A tab and the line breaks are text.
    assert!(text.contains("Line\ttwo"), "{text:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn show_does_not_print_the_characters_that_make_text_look_like_something_else() {
    // A right-to-left override, zero-width characters, an isolate and a byte order mark are no
    // control characters, yet they hide or reorder what a person reads.
    let tricky = Message::new(format!(
        "From: Carol Sender <{SENDER}>\r\nSubject: Invoice\u{202e}fdp.exe\u{200b} 12\r\n\
         Date: Sat, 17 Oct 2026 09:12:00 +0200\r\nContent-Type: text/plain; charset=utf-8\r\n\
         Content-Transfer-Encoding: 8bit\r\n\r\n\
         Pay\u{202e}gnissim\u{202c} to\u{2066}x\u{2069} the\u{feff}\u{2060}\u{200e} account\r\n\
         Caf\u{e9} cr\u{e8}me, \u{65e5}\u{672c}\u{8a9e}\r\n"
    ));
    let env = Env::filled_with(vec![(
        "alice",
        FakeServer::builder()
            .user(USER, PASSWORD)
            .folder(Folder::new("INBOX").message(tricky)),
    )])
    .await;
    let id = inbox_id(&env, 1).await;

    let output = env.read(&["show", &id]).await;

    succeeded(&output);
    let text = stdout(&output);
    for hidden in [
        '\u{202e}', '\u{202c}', '\u{200b}', '\u{2066}', '\u{2069}', '\u{feff}', '\u{2060}',
        '\u{200e}',
    ] {
        assert!(!text.contains(hidden), "{hidden:?} in {text:?}");
    }
    assert!(text.contains("Invoice"), "{text:?}");
    assert!(text.contains("account"), "{text:?}");
    // Letters of every script and accents are text.
    assert!(
        text.contains("Caf\u{e9} cr\u{e8}me, \u{65e5}\u{672c}\u{8a9e}"),
        "{text:?}"
    );
    // `list` prints the subject too.
    let list = env.read(&["list"]).await;
    assert!(!stdout(&list).contains('\u{202e}'), "{:?}", stdout(&list));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_identifier_that_names_nothing_is_not_found_with_its_own_exit_code() {
    let env = Env::filled_with(vec![("alice", alice())]).await;
    let id = inbox_id(&env, 1).await;
    let parts: Vec<&str> = id.split('/').collect();
    let (folder, uidvalidity) = (parts[1], parts[2]);

    for (what, wrong) in [
        ("an unknown UID", format!("alice/{folder}/{uidvalidity}/99")),
        ("an unknown folder", format!("alice/999/{uidvalidity}/1")),
        (
            "an unknown account",
            format!("carol/{folder}/{uidvalidity}/1"),
        ),
        ("a changed UIDVALIDITY", format!("alice/{folder}/1/1")),
        ("not an identifier", "alice/1".to_owned()),
        (
            "a number with a sign",
            format!("alice/{folder}/{uidvalidity}/+1"),
        ),
        (
            "a number with a zero",
            format!("alice/{folder}/{uidvalidity}/01"),
        ),
        ("a fifth part", format!("{id}/1")),
        ("no account", format!("/{folder}/{uidvalidity}/1")),
    ] {
        for flags in [&["show"][..], &["show", "--json"], &["show", "--raw"]] {
            let mut args = flags.to_vec();
            args.push(&wrong);
            let output = env.read(&args).await;
            assert_eq!(output.status.code(), Some(3), "{what}: {wrong}");
            assert_eq!(stdout(&output), "", "{what}");
            assert!(
                stderr(&output).starts_with("missive: "),
                "{what}: {}",
                stderr(&output)
            );
        }
    }
    // Same folder and UID, other account: not that message.
    let bob = env
        .read(&["show", &format!("bob/{folder}/{uidvalidity}/1")])
        .await;
    assert_eq!(bob.status.code(), Some(3));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_identifier_from_before_the_server_renumbered_the_folder_is_stale() {
    let server = alice().start().await;
    let env = Env::new().await;
    env.add_account("alice", &server).await;
    let _daemon = env.daemon(&["--check-interval", "3600"]).await;
    succeeded(&env.sync().await);
    let id = inbox_id(&env, 1).await;
    succeeded(&env.read(&["show", &id]).await);

    server.bump_uidvalidity("INBOX");
    succeeded(&env.sync().await);

    // UID 1 is there again, under the new UIDVALIDITY: the old identifier must not show it.
    let output = env.read(&["show", &id]).await;
    assert_eq!(output.status.code(), Some(3));
    assert!(
        stderr(&output).contains("renumbered"),
        "{}",
        stderr(&output)
    );
    let fresh = inbox_id(&env, 1).await;
    assert_ne!(fresh, id);
    succeeded(&env.read(&["show", &fresh]).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_of_a_removed_account_is_not_found_and_not_listed() {
    let env = Env::filled_with(vec![("alice", alice()), ("bob", bob())]).await;
    let id = inbox_id(&env, 1).await;
    succeeded(&env.read(&["show", &id]).await);
    AccountStore::new(env.path("config").join("missive"))
        .remove("alice")
        .unwrap();

    let output = env.read(&["show", &id]).await;
    let list = env.read(&["list", "--json", "--limit", "100"]).await;

    assert_eq!(output.status.code(), Some(3));
    assert_eq!(listed(&list), [("bob".to_owned(), "INBOX".to_owned(), 1)]);
}

#[tokio::test(flavor = "multi_thread")]
async fn list_and_show_with_no_copy_yet_say_so() {
    let env = Env::new().await;

    let list = env.read(&["list", "--json"]).await;
    let show = env.read(&["show", "alice/1/1/1"]).await;

    assert_eq!(json(&list), json!([]));
    assert!(
        stderr(&list).contains("no local copy yet"),
        "{}",
        stderr(&list)
    );
    assert_eq!(show.status.code(), Some(3));
    assert!(
        stderr(&show).contains("no local copy yet"),
        "{}",
        stderr(&show)
    );
    assert!(!env.copy_path().exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_copy_made_before_the_attachment_marker_is_listed_after_the_daemon_has_started() {
    let env = Env::new().await;
    let server = alice().start().await;
    env.add_account("alice", &server).await;
    // A copy of version 2 as the previous build left it, with one message.
    std::fs::create_dir_all(env.copy_path().parent().unwrap()).unwrap();
    {
        let connection = rusqlite::Connection::open(env.copy_path()).unwrap();
        connection
            .execute_batch(include_str!("../../missive-core/src/store/schema-1.sql"))
            .unwrap();
        connection
            .execute_batch(include_str!("../../missive-core/src/store/schema-2.sql"))
            .unwrap();
        connection.pragma_update(None, "user_version", 2).unwrap();
        connection
            .execute_batch(
                "INSERT INTO account (key, id, state) VALUES (1, 'alice', 'ready');
                 INSERT INTO folder (id, account, name, role, uidvalidity)
                     VALUES (1, 1, 'INBOX', 'inbox', 7);
                 INSERT INTO message (id, folder, uid, flags, size, sender, recipients, cc)
                     VALUES (1, 1, 1, '', 10, '[]', '[]', '[]');
                 INSERT INTO message_raw (message, raw) VALUES (1, x'41');",
            )
            .unwrap();
    }

    let list = env.read(&["list"]).await;
    let show = env.read(&["show", "alice/1/7/1"]).await;

    assert_eq!(list.status.code(), Some(1));
    assert!(
        stderr(&list).contains("no attachment marker yet"),
        "{}",
        stderr(&list)
    );
    // `show` reads the raw message and needs no marker.
    succeeded(&show);
    let mut daemon = env.daemon(&["--check-interval", "3600"]).await;
    daemon.stop().await;
    succeeded(&env.read(&["list"]).await);
}

// ---- `missive open` ----

/// Puts in `bin` an `xdg-open` that records its arguments and its environment, then exits.
fn fake_xdg_open(env: &Env) {
    let bin = env.path("bin");
    std::fs::create_dir(&bin).unwrap();
    let script = bin.join("xdg-open");
    // The record is complete once `args` exists.
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\n/usr/bin/env > '{bin}/env'\nprintf '%s\\n' \"$@\" > '{bin}/args.tmp'\n\
             mv '{bin}/args.tmp' '{bin}/args'\n",
            bin = bin.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// `missive open` with the fake `xdg-open` first on `PATH`, and one variable of the user's own.
async fn open(env: &Env, args: &[&str], bus: &str) -> Output {
    let mut arguments = vec!["open"];
    arguments.extend_from_slice(args);
    env.command(&arguments, bus)
        .env("PATH", path_variable(env))
        .env("MISSIVE_TEST_MARK", "the user's own")
        .output()
        .await
        .unwrap()
}

fn path_variable(env: &Env) -> String {
    format!("{}:/usr/bin:/bin", env.path("bin").display())
}

fn nowhere(env: &Env) -> String {
    format!("unix:path={}", env.path("no-bus").display())
}

/// What the fake `xdg-open` was given: its arguments and its environment.
async fn xdg_opened(env: &Env) -> (Vec<String>, Vec<String>) {
    let args = env.path("bin").join("args");
    eventually(10, "xdg-open", || args.exists()).await;
    let lines = |path: PathBuf| -> Vec<String> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    };
    (lines(args), lines(env.path("bin").join("env")))
}

fn originals(env: &Env) -> PathBuf {
    env.path("runtime").join("missive/original")
}

fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[tokio::test(flavor = "multi_thread")]
async fn open_writes_a_private_cleaned_page_under_the_runtime_folder() {
    let env = Env::filled_with(vec![("alice", alice())]).await;
    fake_xdg_open(&env);
    let id = inbox_id(&env, 4).await;

    let output = open(&env, &[&id, "--original"], &nowhere(&env)).await;

    succeeded(&output);
    let printed = stdout(&output);
    let path = PathBuf::from(printed.trim_end());
    assert_eq!(printed, format!("{}\n", path.display()));
    assert_eq!(path.parent().unwrap(), originals(&env));
    assert_eq!(path.extension().unwrap(), "html");
    assert_eq!(mode(&path), 0o600);
    assert_eq!(mode(&originals(&env)), 0o700);
    let page = std::fs::read_to_string(&path).unwrap();
    assert!(page.contains("Content-Security-Policy"), "{page}");
    assert!(page.contains("<b>Quokka</b>"), "{page}");
    assert!(
        !page.contains("<script") && !page.contains("never run"),
        "{page}"
    );
    // Nothing of the message is said, only where its page is.
    for secret in ["Plan 4", SENDER, "Quokka"] {
        assert!(!printed.contains(secret) && !stderr(&output).contains(secret));
    }
    let (args, _) = xdg_opened(&env).await;
    assert_eq!(args, [path.display().to_string()]);
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_portal_xdg_open_gets_exactly_the_environment_missive_received() {
    let env = Env::filled_with(vec![("alice", alice())]).await;
    fake_xdg_open(&env);
    let id = inbox_id(&env, 4).await;
    let bus = nowhere(&env);

    succeeded(&open(&env, &[&id], &bus).await);

    let (_, environment) = xdg_opened(&env).await;
    let mut received: Vec<String> = environment
        .into_iter()
        // What the fake's own shell sets for itself.
        .filter(|line| {
            !["PWD=", "OLDPWD=", "SHLVL=", "_="]
                .iter()
                .any(|v| line.starts_with(v))
        })
        .collect();
    received.sort();
    let path = |folder: &str| env.path(folder).display().to_string();
    let mut given = vec![
        format!("HOME={}", env.home.path().display()),
        format!("XDG_CONFIG_HOME={}", path("config")),
        format!("XDG_DATA_HOME={}", path("data")),
        format!("XDG_STATE_HOME={}", path("state")),
        format!("XDG_RUNTIME_DIR={}", path("runtime")),
        format!("DBUS_SESSION_BUS_ADDRESS={bus}"),
        format!("PATH={}", path_variable(&env)),
        "MISSIVE_TEST_MARK=the user's own".to_owned(),
    ];
    given.sort();
    assert_eq!(received, given);
}

#[tokio::test(flavor = "multi_thread")]
async fn open_hands_the_page_to_the_desktop_portal() {
    let env = Env::filled_with(vec![("alice", alice())]).await;
    fake_xdg_open(&env);
    let portal = FakePortal::start(PortalAnswer::Opened).await;
    let id = inbox_id(&env, 4).await;

    let output = open(&env, &[&id, "--original"], portal.address()).await;

    succeeded(&output);
    let path = PathBuf::from(stdout(&output).trim_end());
    let calls = portal.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    let call = &calls[0];
    assert_eq!(call.path, path);
    assert_eq!(call.content, std::fs::read(&path).unwrap());
    assert!(call.read_only);
    assert_eq!(call.parent_window, "");
    assert!(call.handle_token.is_some());
    // The portal opened it: xdg-open is not run as well.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!env.path("bin").join("args").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_portal_that_cannot_open_the_page_leaves_it_to_xdg_open() {
    let env = Env::filled_with(vec![("alice", alice())]).await;
    fake_xdg_open(&env);
    let portal = FakePortal::start(PortalAnswer::Failed).await;
    let id = inbox_id(&env, 4).await;

    let output = open(&env, &[&id], portal.address()).await;

    succeeded(&output);
    assert_eq!(portal.calls().len(), 1);
    let (args, _) = xdg_opened(&env).await;
    assert_eq!(args, [stdout(&output).trim_end()]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_portal_that_does_not_answer_is_not_waited_for() {
    let env = Env::filled_with(vec![("alice", alice())]).await;
    fake_xdg_open(&env);
    let portal = FakePortal::start(PortalAnswer::Silent).await;
    let id = inbox_id(&env, 4).await;

    let started = Instant::now();
    let output = open(&env, &[&id], portal.address()).await;

    succeeded(&output);
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(portal.calls().len(), 1);
    // It took the file: it may still be asking the user.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!env.path("bin").join("args").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_text_only_message_is_opened_as_plain_text() {
    let env = Env::filled_with(vec![("alice", alice())]).await;
    fake_xdg_open(&env);
    let id = inbox_id(&env, 1).await;

    let output = open(&env, &[&id], &nowhere(&env)).await;

    succeeded(&output);
    let page = std::fs::read_to_string(stdout(&output).trim_end()).unwrap();
    assert!(page.contains("<pre>Nothing special."), "{page}");
}

#[tokio::test(flavor = "multi_thread")]
async fn open_of_an_identifier_that_names_nothing_is_not_found() {
    let env = Env::filled_with(vec![("alice", alice())]).await;
    fake_xdg_open(&env);
    let id = inbox_id(&env, 1).await;
    let parts: Vec<&str> = id.split('/').collect();

    for wrong in [
        format!("alice/{}/{}/99", parts[1], parts[2]),
        "alice/1".to_owned(),
    ] {
        let output = open(&env, &[&wrong], &nowhere(&env)).await;
        assert_eq!(output.status.code(), Some(3), "{wrong}");
        assert_eq!(stdout(&output), "");
        assert!(
            stderr(&output).starts_with("missive: "),
            "{}",
            stderr(&output)
        );
    }
    assert!(!originals(&env).exists());
    assert!(!env.path("bin").join("args").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn open_without_a_runtime_folder_refuses() {
    let env = Env::filled_with(vec![("alice", alice())]).await;
    fake_xdg_open(&env);
    let id = inbox_id(&env, 4).await;

    let output = env
        .command(&["open", &id], &nowhere(&env))
        .env_remove("XDG_RUNTIME_DIR")
        .env("PATH", path_variable(&env))
        .output()
        .await
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("XDG_RUNTIME_DIR"),
        "{}",
        stderr(&output)
    );
    assert!(!env.path("bin").join("args").exists());
}

/// Runs `missive open` on a runtime folder prepared by `prepare`, and checks that it refused with
/// `needle` in its message, that nothing was handed to `xdg-open`, and that no page was written
/// under `watched`. A folder owned by another user cannot be made without root: the tests stand
/// in with the folder's permissions and with symlinks, the two other ways it is not Missive's own.
async fn open_refused_on(prepare: impl FnOnce(&Env), watched: impl FnOnce(&Env) -> PathBuf) {
    let env = Env::filled_with(vec![("alice", alice())]).await;
    fake_xdg_open(&env);
    let id = inbox_id(&env, 4).await;
    prepare(&env);

    let output = open(&env, &[&id], &nowhere(&env)).await;

    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("not private to you"),
        "{}",
        stderr(&output)
    );
    assert!(stdout(&output).is_empty());
    let watched = watched(&env);
    assert!(
        std::fs::read_dir(&watched).unwrap().next().is_none(),
        "{watched:?} holds a file"
    );
    assert!(!env.path("bin").join("args").exists());
}

fn chmod(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn open_refuses_a_missive_folder_others_can_write() {
    for mode in [0o770, 0o707] {
        open_refused_on(
            |env| {
                std::fs::create_dir_all(originals(env)).unwrap();
                chmod(originals(env).parent().unwrap(), mode);
            },
            originals,
        )
        .await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn open_refuses_an_original_folder_others_can_write() {
    for mode in [0o770, 0o707] {
        open_refused_on(
            |env| {
                std::fs::create_dir_all(originals(env)).unwrap();
                chmod(&originals(env), mode);
            },
            originals,
        )
        .await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn open_refuses_an_original_folder_that_is_a_symlink() {
    open_refused_on(
        |env| {
            let elsewhere = env.path("elsewhere");
            std::fs::create_dir(&elsewhere).unwrap();
            std::fs::create_dir_all(originals(env).parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(&elsewhere, originals(env)).unwrap();
        },
        |env| env.path("elsewhere"),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn open_refuses_a_missive_folder_that_is_a_symlink() {
    open_refused_on(
        |env| {
            let elsewhere = env.path("elsewhere");
            std::fs::create_dir_all(elsewhere.join("original")).unwrap();
            let missive = env.path("runtime").join("missive");
            let _ = std::fs::remove_dir_all(&missive);
            std::os::unix::fs::symlink(&elsewhere, missive).unwrap();
        },
        |env| env.path("elsewhere").join("original"),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_portal_slow_to_take_the_call_does_not_get_xdg_open_as_well() {
    let env = Env::filled_with(vec![("alice", alice())]).await;
    fake_xdg_open(&env);
    // Slower than the time `open` gives the call: the portal may well be showing a chooser.
    let portal = FakePortal::start(PortalAnswer::Slow(Duration::from_secs(7))).await;
    let id = inbox_id(&env, 4).await;

    let output = open(&env, &[&id], portal.address()).await;

    succeeded(&output);
    assert_eq!(portal.calls().len(), 1);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!env.path("bin").join("args").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_portal_answering_after_the_wait_does_not_get_xdg_open_as_well() {
    let env = Env::filled_with(vec![("alice", alice())]).await;
    fake_xdg_open(&env);
    let portal = FakePortal::start(PortalAnswer::Late(Duration::from_secs(3))).await;
    let id = inbox_id(&env, 4).await;

    let output = open(&env, &[&id], portal.address()).await;

    succeeded(&output);
    assert_eq!(portal.calls().len(), 1);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!env.path("bin").join("args").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_page_older_than_an_hour_is_removed_at_the_next_open() {
    let env = Env::filled_with(vec![("alice", alice())]).await;
    fake_xdg_open(&env);
    let id = inbox_id(&env, 4).await;
    std::fs::create_dir_all(originals(&env)).unwrap();
    let old = originals(&env).join("old.html");
    let recent = originals(&env).join("recent.html");
    for (path, age) in [(&old, 61 * 60), (&recent, 59 * 60)] {
        let file = std::fs::File::create(path).unwrap();
        file.set_modified(std::time::SystemTime::now() - Duration::from_secs(age))
            .unwrap();
    }

    succeeded(&open(&env, &[&id], &nowhere(&env)).await);

    assert!(!old.exists());
    assert!(recent.exists());
}

/// What the copy holds for each account, by identifier: `(folders, messages, raw messages,
/// index rows, queued actions)`, read straight from the file.
fn rows_in_copy(path: &Path) -> std::collections::BTreeMap<String, [i64; 5]> {
    let copy =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let mut statement = copy
        .prepare(
            "SELECT account.id,
                (SELECT count(*) FROM folder WHERE folder.account = account.key),
                (SELECT count(*) FROM message JOIN folder ON folder.id = message.folder
                  WHERE folder.account = account.key),
                (SELECT count(*) FROM message_raw
                   JOIN message ON message.id = message_raw.message
                   JOIN folder ON folder.id = message.folder
                  WHERE folder.account = account.key),
                (SELECT count(*) FROM message_search
                   JOIN message ON message.id = message_search.rowid
                   JOIN folder ON folder.id = message.folder
                  WHERE folder.account = account.key),
                (SELECT count(*) FROM action WHERE action.account = account.key)
               FROM account",
        )
        .unwrap();
    statement
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                [r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?],
            ))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn whole_account_in_copy(rows: &std::collections::BTreeMap<String, [i64; 5]>, id: &str) -> bool {
    rows.get(id)
        .is_some_and(|[folders, messages, raws, indexed, _]| {
            *folders > 0 && *messages > 0 && messages == raws && messages == indexed
        })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_removed_account_leaves_the_copy_when_the_daemon_starts_and_the_other_stays() {
    let env = Env::filled_with(vec![("alice", alice()), ("bob", bob())]).await;
    let before = rows_in_copy(&env.copy_path());
    assert!(whole_account_in_copy(&before, "alice"));
    assert!(whole_account_in_copy(&before, "bob"));
    AccountStore::new(env.path("config").join("missive"))
        .remove("alice")
        .unwrap();
    // Without a daemon the copy is not touched (the read commands skip the account meanwhile).
    assert_eq!(rows_in_copy(&env.copy_path()), before);

    let mut daemon = env.daemon(&[]).await;
    eventually(10, "the removed account leaving the copy", || {
        !rows_in_copy(&env.copy_path()).contains_key("alice")
    })
    .await;
    daemon.stop().await;

    let after = rows_in_copy(&env.copy_path());
    assert_eq!(after.keys().collect::<Vec<_>>(), ["bob"]);
    assert_eq!(after["bob"], before["bob"]);
    let status = json(&env.read(&["status", "--json"]).await);
    assert_eq!(status["accounts"].as_array().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_account_removed_while_the_daemon_runs_leaves_the_copy_at_the_next_reload() {
    let env = Env::filled_with(vec![("alice", alice()), ("bob", bob())]).await;
    let before = rows_in_copy(&env.copy_path());
    let mut daemon = env.daemon(&[]).await;
    assert!(rows_in_copy(&env.copy_path()).contains_key("alice"));

    AccountStore::new(env.path("config").join("missive"))
        .remove("alice")
        .unwrap();
    // `sync` is one of the requests that reload the configuration.
    env.sync().await;
    eventually(10, "the removed account leaving the copy", || {
        !rows_in_copy(&env.copy_path()).contains_key("alice")
    })
    .await;
    daemon.stop().await;

    assert_eq!(rows_in_copy(&env.copy_path())["bob"], before["bob"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_account_whose_settings_cannot_be_read_keeps_its_rows() {
    let env = Env::filled_with(vec![("alice", alice()), ("bob", bob())]).await;
    let before = rows_in_copy(&env.copy_path());
    std::fs::write(
        env.path("config")
            .join("missive/accounts/alice/account.toml"),
        "not = [valid",
    )
    .unwrap();

    let mut daemon = env.daemon(&[]).await;
    // bob's server is gone, so this ends in an error; the reload it makes is what counts.
    env.sync().await;
    daemon.stop().await;

    assert_eq!(rows_in_copy(&env.copy_path()), before);
}

/// Both ways the configuration can hold no account at all: its folder is gone (a wrong
/// `XDG_CONFIG_HOME`, a folder unmounted) or it is there and empty.
async fn no_account_configured_leaves_the_copy_and_its_queue_alone(folder_missing: bool) {
    let env = Env::filled_with(vec![("alice", alice())]).await;
    {
        let copy = rusqlite::Connection::open(env.copy_path()).unwrap();
        copy.execute(
            "INSERT INTO action (account, folder, uidvalidity, uids, kind)
             SELECT folder.account, folder.id, 1, '[1]', 'read' FROM folder
              WHERE folder.name = 'INBOX'",
            [],
        )
        .unwrap();
    }
    let before = rows_in_copy(&env.copy_path());
    assert_eq!(before["alice"][4], 1);
    let accounts = env.path("config").join("missive");
    let store = AccountStore::new(accounts.clone());
    store.remove("alice").unwrap();
    if folder_missing {
        std::fs::remove_dir_all(&accounts).unwrap();
    } else {
        std::fs::create_dir_all(accounts.join("accounts")).unwrap();
    }

    // At the start of the daemon, then at a reload.
    let mut daemon = env.daemon(&[]).await;
    env.sync().await;
    daemon.stop().await;

    assert_eq!(rows_in_copy(&env.copy_path()), before);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_configuration_folder_drops_nothing_from_the_copy() {
    no_account_configured_leaves_the_copy_and_its_queue_alone(true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_configuration_with_no_account_drops_nothing_from_the_copy() {
    no_account_configured_leaves_the_copy_and_its_queue_alone(false).await;
}
