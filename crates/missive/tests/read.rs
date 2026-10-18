//! `missive status`, `unread` and `search`, which answer from the local copy alone. The copy is
//! filled by a daemon from the fake IMAP server; the reading commands then run with the daemon
//! stopped, the server gone and no session bus at all, so that whatever they would ask of the
//! network or the keyring fails. Everything lives in temporary folders.

use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::{Duration, Instant};

use missive_core::account::{Account, AccountStore, Password, Security, Server};
use missive_core::fake::{FakeKeyring, FakeServer, Folder, Message, SpecialUse};
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
        let env = Env::new().await;
        let server = mailbox().start().await;
        env.add_account("alice", &server).await;
        let mut daemon = env.daemon(&[]).await;
        succeeded(&env.sync().await);
        daemon.stop().await;
        drop(server);
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
