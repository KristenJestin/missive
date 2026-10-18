//! The window's state, without a window, against a copy filled through the fake server.

mod common;

use std::time::Duration;

use common::{ID, Mail, hostile, mailbox, plan};
use missive_core::events::{Event, NewMessage, Unread};
use missive_core::fake::Folder;
use missive_window::model::{Body, Daemon, Model, Reader};

fn subjects(model: &Model) -> Vec<String> {
    model
        .messages
        .iter()
        .map(|m| m.hit.subject.clone().unwrap_or_default())
        .collect()
}

fn unread_of(model: &Model, role: &str) -> u64 {
    model.accounts[0]
        .folders
        .iter()
        .find(|f| f.folder.role.as_deref() == Some(role))
        .unwrap()
        .unread
}

fn opened_subject(model: &Model) -> Option<String> {
    match &model.reader {
        Reader::Open(opened) => opened.subject.clone(),
        _ => None,
    }
}

fn opened_body(model: &Model) -> Option<Body> {
    match &model.reader {
        Reader::Open(opened) => Some(opened.body.clone()),
        _ => None,
    }
}

/// Runs the reduction the model asked for, as the window does off its thread, and hands it back.
fn prepare(model: &mut Model) {
    let reduction = model.take_reduction().expect("a reduction to run");
    model.reduced(reduction.run());
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

fn new_message(id: &str) -> Event {
    Event::NewMessage(NewMessage {
        account: ID.to_owned(),
        folder: Some("inbox".to_owned()),
        message: id.to_owned(),
        sender: None,
        subject: None,
    })
}

fn unread() -> Event {
    Event::Unread(Unread {
        total: 0,
        accounts: Vec::new(),
    })
}

#[tokio::test]
async fn the_window_opens_on_the_inbox_newest_first_with_its_unread_counts() {
    let mail = Mail::new(mailbox()).await;
    let model = mail.model();
    assert_eq!(model.accounts.len(), 1);
    assert_eq!(model.accounts[0].id, ID);
    assert_eq!(model.accounts[0].state.as_deref(), Some("ready"));
    assert_eq!(unread_of(&model, "inbox"), 2);
    assert_eq!(unread_of(&model, "sent"), 0);
    assert_eq!(subjects(&model), ["Plan 3", "Plan 2", "Plan 1"]);
    assert_eq!(model.cursor, Some(0));
    assert_eq!(model.reader, Reader::Empty);
    let unread: Vec<bool> = model.messages.iter().map(|m| m.hit.unread).collect();
    assert_eq!(unread, [true, true, false]);
}

#[tokio::test]
async fn selecting_a_folder_shows_its_messages() {
    let mail = Mail::new(mailbox()).await;
    let mut model = mail.model();
    select(&mut model, "sent");
    assert_eq!(subjects(&model), ["Plan 4"]);
    assert_eq!(model.cursor, Some(0));
}

#[tokio::test]
async fn up_and_down_move_in_the_list_and_enter_opens_the_message() {
    let mail = Mail::new(mailbox()).await;
    let mut model = mail.model();
    model.move_cursor(1);
    model.move_cursor(1);
    model.move_cursor(1);
    assert_eq!(model.cursor, Some(2), "the cursor stays in the list");
    model.move_cursor(-1);
    model.open_cursor();
    assert_eq!(opened_subject(&model).as_deref(), Some("Plan 2"));
    model.move_cursor(-5);
    assert_eq!(model.cursor, Some(0));
}

#[tokio::test]
async fn escape_closes_the_reader() {
    let mail = Mail::new(mailbox()).await;
    let mut model = mail.model();
    model.open_cursor();
    assert!(opened_subject(&model).is_some());
    model.back();
    assert_eq!(model.reader, Reader::Empty);
}

#[tokio::test]
async fn the_reader_shows_the_headers_and_the_body_in_the_reduced_form() {
    let mail = Mail::new(mailbox()).await;
    let mut model = mail.model();
    let id = model.messages[1].hit.id.clone();
    model.open(&id);
    let Reader::Open(opened) = &model.reader else {
        panic!("not open");
    };
    assert_eq!(
        opened.from[0].address.as_deref(),
        Some("carol.sender@example.com")
    );
    assert_eq!(opened.to[0].address.as_deref(), Some("dave@example.net"));
    assert!(opened.date.is_some());
    assert_eq!(opened.body, Body::Preparing);
    prepare(&mut model);
    let Some(Body::Reduced(html)) = opened_body(&model) else {
        panic!("not reduced: {:?}", opened_body(&model));
    };
    assert!(
        html.contains("<a href=\"https://www.example.org/news\">the news</a>"),
        "{html}"
    );
    assert!(html.contains("[Our logo]"), "{html}");
    assert!(!html.contains("images.example.org"), "{html}");
    assert!(!html.contains("track.example.net"), "{html}");
    assert!(!html.contains("Preheader"), "{html}");
}

#[tokio::test]
async fn opening_a_message_does_not_mark_it_read() {
    let mail = Mail::new(mailbox()).await;
    let mut model = mail.model();
    model.open_cursor();
    assert_eq!(opened_subject(&model).as_deref(), Some("Plan 3"));
    model.refresh();
    assert!(model.messages[0].hit.unread, "still unread in the copy");
    assert_eq!(unread_of(&model, "inbox"), 2);
    assert!(
        !mail.server_flags(3).iter().any(|f| f == "\\Seen"),
        "still unread on the server"
    );
}

#[tokio::test]
async fn a_new_message_event_refreshes_the_list_and_the_counts() {
    let mail = Mail::new(mailbox()).await;
    let mut model = mail.model();
    model.move_cursor(1);
    let uid = mail.server.deliver("INBOX", plan(5, "", "Fresh."));
    mail.sync().await;
    assert_eq!(
        subjects(&model).len(),
        3,
        "nothing is read before the event"
    );

    let (folder, _) = model.messages[0].hit.id.rsplit_once('/').unwrap();
    let id = format!("{folder}/{uid}");
    model.apply(&new_message(&id));
    assert_eq!(subjects(&model), ["Plan 5", "Plan 3", "Plan 2", "Plan 1"]);
    assert_eq!(unread_of(&model, "inbox"), 3);
    assert_eq!(model.daemon, Daemon::Running);
    assert_eq!(
        model.under_cursor(),
        Some(model.messages[2].hit.id.as_str()),
        "the cursor stays on the message it was on"
    );
}

#[tokio::test]
async fn an_unread_event_refreshes_the_counts_and_the_bold() {
    let mail = Mail::new(mailbox()).await;
    let mut model = mail.model();
    mail.server.set_flags("INBOX", 3, &["\\Seen"]);
    mail.sync().await;
    model.apply(&unread());
    assert_eq!(unread_of(&model, "inbox"), 1);
    assert!(!model.messages[0].hit.unread);
}

#[tokio::test]
async fn a_message_removed_while_shown_says_it_is_gone() {
    let mail = Mail::new(mailbox()).await;
    let mut model = mail.model();
    model.open_cursor();
    assert_eq!(opened_subject(&model).as_deref(), Some("Plan 3"));
    mail.server.expunge("INBOX", 3);
    mail.sync().await;
    model.apply(&Event::SyncEnded {
        account: ID.to_owned(),
        folders: 2,
        new: 0,
        removed: 1,
        flags: 0,
        skipped: Vec::new(),
    });
    assert_eq!(model.reader, Reader::Gone);
    assert_eq!(subjects(&model), ["Plan 2", "Plan 1"]);
    assert_eq!(
        model.cursor,
        Some(0),
        "the cursor moves to the next message"
    );
}

#[tokio::test]
async fn an_unreachable_account_is_told_apart() {
    let mail = Mail::new(mailbox()).await;
    let mut model = mail.model();
    model.apply(&Event::State {
        account: ID.to_owned(),
        state: "unreachable".to_owned(),
    });
    assert_eq!(model.accounts[0].state.as_deref(), Some("unreachable"));
    assert_eq!(model.daemon, Daemon::Running);
    assert_eq!(subjects(&model).len(), 3, "the copy is still shown");
}

#[tokio::test]
async fn without_a_daemon_the_copy_is_shown_and_the_daemon_said_stopped() {
    let mail = Mail::new(mailbox()).await;
    // A daemon killed mid-sync leaves `syncing` behind.
    mail.set_state("syncing");
    let mut model = mail.model();
    assert_eq!(model.daemon, Daemon::Unknown);
    model.apply(&Event::Gone);
    assert_eq!(model.daemon, Daemon::Stopped);
    assert_eq!(
        model.accounts[0].state, None,
        "syncing means nothing without a daemon"
    );
    assert_eq!(subjects(&model).len(), 3);

    model.apply(&Event::State {
        account: ID.to_owned(),
        state: "syncing".to_owned(),
    });
    assert_eq!(model.accounts[0].state.as_deref(), Some("syncing"));
    model.apply(&Event::Gone);
    assert_eq!(
        model.accounts[0].state, None,
        "what the daemon told goes with it"
    );
}

#[tokio::test]
async fn without_a_copy_the_window_shows_the_accounts_and_no_message() {
    let mail = Mail::new(mailbox()).await;
    std::fs::remove_file(mail.copy_path()).unwrap();
    let model = mail.model();
    assert_eq!(model.accounts.len(), 1);
    assert_eq!(model.accounts[0].state, None);
    assert!(model.messages.is_empty());
    assert_eq!(model.problem, None);
}

#[tokio::test]
async fn a_body_that_cannot_be_reduced_shows_its_text_and_does_not_block_other_messages() {
    let mail = Mail::new(mailbox().folder(Folder::new("Hostile").message(hostile()))).await;
    let mut model = mail.model();
    let folder = model.accounts[0]
        .folders
        .iter()
        .find(|f| f.folder.name == "Hostile")
        .unwrap()
        .folder
        .id;
    model.select_folder(ID, folder);
    let hostile_id = model.messages[0].hit.id.clone();
    model.open(&hostile_id);
    assert_eq!(opened_body(&model), Some(Body::Preparing));
    let late = model.take_reduction().unwrap();

    // Another message opens while the first is still being prepared, and is shown.
    select(&mut model, "inbox");
    model.open_cursor();
    assert_eq!(opened_subject(&model).as_deref(), Some("Plan 3"));
    prepare(&mut model);
    assert!(matches!(opened_body(&model), Some(Body::Reduced(_))));
    // The first one's end, coming late, changes nothing.
    model.reduced(late.run());
    assert_eq!(opened_subject(&model).as_deref(), Some("Plan 3"));
    assert!(matches!(opened_body(&model), Some(Body::Reduced(_))));

    model.select_folder(ID, folder);
    model.open(&hostile_id);
    prepare(&mut model);
    let Some(Body::Text(text)) = opened_body(&model) else {
        panic!("not the text: {:?}", opened_body(&model));
    };
    assert!(text.contains("The hostile words."), "{text}");
}

#[tokio::test]
async fn a_reduction_over_its_budget_shows_the_text() {
    let mail = Mail::new(mailbox()).await;
    let mut model = mail.model();
    let id = model.messages[1].hit.id.clone();
    model.open(&id);
    let mut reduction = model.take_reduction().unwrap();
    reduction.budget = Duration::ZERO;
    model.reduced(reduction.run());
    let Some(Body::Text(text)) = opened_body(&model) else {
        panic!("not the text: {:?}", opened_body(&model));
    };
    assert!(text.contains("the news"), "{text}");
}
