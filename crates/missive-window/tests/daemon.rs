//! The window's requests through a real `missive daemon`, against the fake IMAP server and a fake
//! Secret Service on a private bus. The daemon runs with a cleared environment in temporary
//! folders, and is killed when the test ends, even when it panics.
//!
//! The daemon is the `missive` binary of the same build: `cargo test` at the workspace's root
//! builds it with this test.

mod common;

use std::path::PathBuf;
use std::process::{Child, Command as Process, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use common::{ID, Mail, PASSWORD, plan, sorted_mailbox};
use missive_core::account::Password;
use missive_core::events::Event;
use missive_core::fake::{FakeKeyring, FakeServer, Folder, SpecialUse};
use missive_core::keyring::Keyring;
use missive_window::ask;
use missive_window::compose::{Compose, Field};
use missive_window::model::{Model, Reader, Start};
use missive_window::view::Asker;

/// The `missive` binary beside this test's folder of the build.
fn missive() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let program = exe.parent().unwrap().parent().unwrap().join("missive");
    assert!(
        program.is_file(),
        "{} is not built: run cargo test from the workspace's root",
        program.display()
    );
    program
}

struct Daemon {
    child: Child,
    socket: PathBuf,
    log: PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Daemon {
    /// Starts `missive daemon` on the mail's folders and waits until it answers on its socket.
    async fn start(mail: &Mail, keyring: &FakeKeyring) -> Daemon {
        let home = mail.home.path();
        for folder in ["state", "runtime"] {
            std::fs::create_dir_all(home.join(folder)).unwrap();
        }
        let log = home.join("daemon.log");
        let child = Process::new(missive())
            .arg("daemon")
            .env_clear()
            .env("HOME", home)
            .env("XDG_CONFIG_HOME", home.join("config"))
            .env("XDG_DATA_HOME", home.join("data"))
            .env("XDG_STATE_HOME", home.join("state"))
            .env("XDG_RUNTIME_DIR", home.join("runtime"))
            .env("DBUS_SESSION_BUS_ADDRESS", keyring.address())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap())
            .spawn()
            .unwrap();
        let daemon = Daemon {
            child,
            socket: home.join("runtime/missive/daemon.sock"),
            log,
        };
        let started = Instant::now();
        while std::os::unix::net::UnixStream::connect(&daemon.socket).is_err() {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "the daemon did not answer; its log:\n{}",
                daemon.log()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        daemon
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// The window's own asker, on this daemon's socket.
    fn asker(&self) -> Asker {
        ask::asker(self.socket.clone(), missive())
    }

    /// The daemon's events, read by the window's own subscription.
    fn events(&self) -> mpsc::Receiver<Event> {
        let (send, receive) = mpsc::channel();
        let socket = self.socket.clone();
        std::thread::spawn(move || {
            missive_window::watch::follow(socket, |event| send.send(event).is_ok())
        });
        receive
    }
}

async fn keyring() -> FakeKeyring {
    let keyring = FakeKeyring::start().await;
    let connection = zbus::connection::Builder::address(keyring.address())
        .unwrap()
        .build()
        .await
        .unwrap();
    Keyring::open_on(connection)
        .await
        .unwrap()
        .store(ID, &Password::new(PASSWORD.to_owned()))
        .await
        .unwrap();
    keyring
}

/// Hands every request the model has to the window's asker, and the answers back to the model.
/// The asker blocks, as it does on the window's background thread.
fn ask_all(model: &mut Model, asker: &Asker) {
    tokio::task::block_in_place(|| {
        while let Some(command) = model.take_request() {
            let answer = asker(command);
            model.answered(answer);
        }
    })
}

/// Applies the daemon's events to the model until one is an `action` event that is `outcome`.
fn until_action(model: &mut Model, events: &mpsc::Receiver<Event>, outcome: &str, daemon: &Daemon) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let event = tokio::task::block_in_place(|| events.recv_timeout(left))
            .unwrap_or_else(|_| panic!("no {outcome} action; the log:\n{}", daemon.log()));
        model.apply(&event);
        if let Event::Action(action) = &event
            && action.outcome == outcome
        {
            return;
        }
    }
}

fn subjects(model: &Model) -> Vec<String> {
    model
        .messages
        .iter()
        .map(|m| m.hit.subject.clone().unwrap_or_default())
        .collect()
}

fn open(model: &mut Model, subject: &str) -> String {
    let id = model
        .messages
        .iter()
        .find(|m| m.hit.subject.as_deref() == Some(subject))
        .unwrap()
        .hit
        .id
        .clone();
    model.open(&id);
    id
}

fn select(model: &mut Model, role: &str) {
    let folder = model.accounts[0]
        .folders
        .iter()
        .find(|f| f.folder.role.as_deref() == Some(role))
        .unwrap()
        .folder
        .id;
    model.select_folder(ID, folder);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_action_from_the_window_reaches_the_server_and_the_window_shows_it() {
    let mail = Mail::new(sorted_mailbox()).await;
    let keyring = keyring().await;
    let daemon = Daemon::start(&mail, &keyring).await;
    let asker = daemon.asker();
    let events = daemon.events();
    let mut model = mail.model();

    // Plan 3 shown for its 2 s: marked read, on the server too.
    open(&mut model, "Plan 3");
    let timer = model.take_read_timer().unwrap();
    model.read_timer_elapsed(timer);
    ask_all(&mut model, &asker);
    assert!(model.notice.is_none(), "{:?}", model.notice);
    assert!(!model.messages[0].hit.unread, "read in the copy at once");
    until_action(&mut model, &events, "applied", &daemon);
    assert!(
        mail.server_flags(3).iter().any(|f| f == "\\Seen"),
        "{:?}",
        mail.server_flags(3)
    );

    // Archived from the reader.
    open(&mut model, "Plan 3");
    model.archive();
    ask_all(&mut model, &asker);
    assert_eq!(subjects(&model), ["Plan 2", "Plan 1"], "gone at once");
    until_action(&mut model, &events, "applied", &daemon);
    let log = mail.server.imap_log().join("\n");
    assert!(log.contains("UID STORE 3 +FLAGS (\\Seen)"), "{log}");
    assert!(log.contains("UID MOVE 3 \"Archive\""), "{log}");
    assert_eq!(mail.server.uids("INBOX").len(), 2);
    assert_eq!(mail.server.uids("Archive").len(), 1);
    select(&mut model, "archive");
    assert_eq!(subjects(&model), ["Plan 3"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn offline_an_action_from_the_window_is_queued_and_shown_at_once() {
    let mail = Mail::new(sorted_mailbox()).await;
    let keyring = keyring().await;
    // The server stops answering: the copy is all the daemon has.
    mail.server.refuse("LOGIN");
    let daemon = Daemon::start(&mail, &keyring).await;
    let asker = daemon.asker();
    let mut model = mail.model();

    open(&mut model, "Plan 3");
    let timer = model.take_read_timer().unwrap();
    model.read_timer_elapsed(timer);
    model.archive();
    let started = Instant::now();
    ask_all(&mut model, &asker);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "answered without the server"
    );
    assert!(model.notice.is_none(), "{:?}", model.notice);
    assert_eq!(subjects(&model), ["Plan 2", "Plan 1"]);
    assert_eq!(model.reader, Reader::Empty);
    select(&mut model, "archive");
    assert_eq!(subjects(&model), ["Plan 3"]);
    assert!(!model.messages[0].hit.unread, "read, while queued");
    assert_eq!(
        mail.server.uids("INBOX").len(),
        3,
        "the server has not moved"
    );
}

/// Hands every request the compose window has to the asker, and the answers back.
fn ask_compose(compose: &mut Compose, asker: &Asker) {
    tokio::task::block_in_place(|| {
        while let Some(command) = compose.take_request() {
            let answer = asker(command);
            compose.answered(answer);
        }
    })
}

async fn eventually(what: &str, daemon: &Daemon, mut condition: impl FnMut() -> bool) {
    let started = Instant::now();
    while !condition() {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "{what} did not happen; the log:\n{}",
            daemon.log()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reply_written_in_the_window_arrives_with_its_attachment_and_its_draft_is_gone() {
    let mail = Mail::new(
        FakeServer::builder()
            .user(common::USER, PASSWORD)
            .folder(Folder::new("INBOX").message(plan(3, "", "The Zanzibar archive.")))
            .folder(Folder::new("Sent").special_use(SpecialUse::Sent))
            .folder(Folder::new("Drafts").special_use(SpecialUse::Drafts)),
    )
    .await;
    let keyring = keyring().await;
    let daemon = Daemon::start(&mail, &keyring).await;
    let asker = daemon.asker();
    let mut model = mail.model();
    open(&mut model, "Plan 3");
    let mut compose = model.compose(Start::Reply).unwrap();
    let file = mail.home.path().join("minutes.txt");
    std::fs::write(&file, "Minutes of the meeting").unwrap();

    // Typed, attached, and saved once the typing stopped.
    compose.attach(missive_window::compose::load(vec![file]));
    let body = format!("Noon works.{}", compose.body);
    compose.edit(Field::Body, body);
    let timer = compose.take_save_timer().unwrap();
    compose.save_timer_elapsed(timer);
    ask_compose(&mut compose, &asker);
    assert!(compose.notice.is_none(), "{:?}", compose.notice);
    eventually("the draft on the server", &daemon, || {
        mail.server.uids("Drafts").len() == 1
    })
    .await;

    // Sent, Send pressed twice.
    compose.send();
    compose.send();
    ask_compose(&mut compose, &asker);
    assert!(compose.done(), "{:?}", compose.notice);
    eventually("the message sent", &daemon, || {
        !mail.server.smtp_messages().is_empty()
    })
    .await;
    eventually("the draft gone", &daemon, || {
        mail.server.uids("Drafts").is_empty()
    })
    .await;
    eventually("the original answered", &daemon, || {
        mail.server_flags(1).iter().any(|f| f == "\\Answered")
    })
    .await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let sent = mail.server.smtp_messages();
    assert_eq!(sent.len(), 1, "sent once");
    assert_eq!(sent[0].to, ["carol.sender@example.com"]);
    let parsed = mail_parser::MessageParser::default()
        .parse(&sent[0].data)
        .unwrap();
    assert_eq!(parsed.subject(), Some("Re: Plan 3"));
    assert!(
        parsed
            .body_text(0)
            .unwrap()
            .starts_with("Noon works.\r\n\r\nOn 2026-10-17 09:03 +0200, Carol Sender wrote:")
    );
    let attachment = parsed.attachment(0).expect("the attachment");
    assert_eq!(attachment.contents(), b"Minutes of the meeting");
    use mail_parser::MimeHeaders;
    assert_eq!(attachment.attachment_name(), Some("minutes.txt"));
}
