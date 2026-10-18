//! What the window asks the daemon to do to mail, without a window: the requests the model
//! queues for the keys and buttons, and how it follows their answers and `action` events. The
//! daemon's part is played by the store's own queue, as the daemon calls it.

mod common;

use common::{ID, Mail, mailbox, sorted_mailbox};
use missive_core::actions::Wanted;
use missive_core::events::{ActionEvent, Event};
use missive_core::protocol::{Answer, Command, PROTOCOL};
use missive_core::store::Store;
use missive_window::model::{Model, READ_AFTER, Reader};

fn subjects(model: &Model) -> Vec<String> {
    model
        .messages
        .iter()
        .map(|m| m.hit.subject.clone().unwrap_or_default())
        .collect()
}

fn requests(model: &mut Model) -> Vec<Command> {
    std::iter::from_fn(|| model.take_request()).collect()
}

/// Opens the message of the list whose subject is `subject`.
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

fn mark(id: &str, mark_as: &str) -> Command {
    Command::Mark {
        messages: vec![id.to_owned()],
        mark_as: mark_as.to_owned(),
    }
}

fn moved(id: &str, to: &str) -> Command {
    Command::Move {
        messages: vec![id.to_owned()],
        to: to.to_owned(),
    }
}

fn deleted(id: &str, permanently: bool) -> Command {
    Command::Delete {
        messages: vec![id.to_owned()],
        permanently,
    }
}

/// The daemon's answer to an action recorded.
fn recorded(name: &str, id: &str) -> Answer {
    Answer {
        protocol: PROTOCOL,
        answer: Some(name.to_owned()),
        ok: true,
        messages: Some(vec![id.to_owned()]),
        ..Answer::default()
    }
}

#[tokio::test]
async fn opening_a_message_for_two_seconds_queues_one_read_request() {
    let mail = Mail::new(sorted_mailbox()).await;
    let mut model = mail.model();
    let id = open(&mut model, "Plan 3");
    let timer = model
        .take_read_timer()
        .expect("a timer for an unread message");
    assert_eq!(timer.after, READ_AFTER);
    assert_eq!(READ_AFTER.as_secs(), 2);
    assert!(requests(&mut model).is_empty(), "nothing before the timer");
    model.read_timer_elapsed(timer);
    assert_eq!(requests(&mut model), [mark(&id, "read")]);
    assert!(model.take_read_timer().is_none(), "one timer, one request");
}

#[tokio::test]
async fn moving_on_before_two_seconds_does_not_mark_it_read() {
    let mail = Mail::new(sorted_mailbox()).await;
    let mut model = mail.model();
    open(&mut model, "Plan 3");
    let first = model.take_read_timer().unwrap();
    open(&mut model, "Plan 2");
    let second = model.take_read_timer().unwrap();
    model.read_timer_elapsed(first);
    assert!(
        requests(&mut model).is_empty(),
        "Plan 3 was left before 2 s"
    );
    model.back();
    model.read_timer_elapsed(second);
    assert!(
        requests(&mut model).is_empty(),
        "Plan 2 was closed before 2 s"
    );
}

#[tokio::test]
async fn a_message_already_read_starts_no_timer() {
    let mail = Mail::new(sorted_mailbox()).await;
    let mut model = mail.model();
    open(&mut model, "Plan 1");
    assert!(model.take_read_timer().is_none());
}

#[tokio::test]
async fn a_message_marked_unread_while_open_is_not_marked_read_by_the_timer() {
    let mail = Mail::new(sorted_mailbox()).await;
    let mut model = mail.model();
    let id = open(&mut model, "Plan 3");
    let timer = model.take_read_timer().unwrap();
    model.mark_unread();
    model.read_timer_elapsed(timer);
    assert!(
        requests(&mut model).is_empty(),
        "already unread: nothing to ask, and the timer is off"
    );

    // Read, then marked unread again by the user while open.
    let id1 = open(&mut model, "Plan 1");
    model.mark_unread();
    assert_eq!(requests(&mut model), [mark(&id1, "unread")]);
    assert_ne!(id, id1);
}

#[tokio::test]
async fn each_key_sends_the_right_request() {
    let mail = Mail::new(sorted_mailbox()).await;
    let mut model = mail.model();

    let id = open(&mut model, "Plan 1");
    model.mark_unread();
    assert_eq!(requests(&mut model), [mark(&id, "unread")], "U");

    let id = open(&mut model, "Plan 3");
    model.toggle_flag();
    assert_eq!(requests(&mut model), [mark(&id, "flagged")], "S");
    let id = open(&mut model, "Plan 2");
    model.toggle_flag();
    assert_eq!(requests(&mut model), [mark(&id, "unflagged")], "S again");

    let id = open(&mut model, "Plan 3");
    assert!(model.can_archive());
    model.archive();
    assert_eq!(requests(&mut model), [moved(&id, "archive")], "A");
    assert_eq!(model.reader, Reader::Empty, "the message left the reader");

    let id = open(&mut model, "Plan 3");
    model.pick_folder();
    let picker = model.picker.as_ref().expect("the folder picker");
    let names: Vec<&str> = picker.folders.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(
        names,
        ["Archive", "Trash", "Projects"],
        "every folder but the message's own, as the folder pane lists them"
    );
    model.picker_move(5);
    model.choose_folder();
    assert!(model.picker.is_none());
    assert_eq!(requests(&mut model), [moved(&id, "Projects")], "M");

    let id = open(&mut model, "Plan 3");
    model.pick_folder();
    model.choose_folder();
    assert_eq!(
        requests(&mut model),
        [moved(&id, "archive")],
        "a folder with a role is named by its role"
    );

    let id = open(&mut model, "Plan 3");
    model.delete();
    assert!(model.confirm.is_none(), "a move to the trash asks nothing");
    assert_eq!(requests(&mut model), [deleted(&id, false)], "Delete");
}

#[tokio::test]
async fn the_picker_closes_without_moving_anything() {
    let mail = Mail::new(sorted_mailbox()).await;
    let mut model = mail.model();
    open(&mut model, "Plan 3");
    model.pick_folder();
    model.dismiss();
    assert!(model.picker.is_none());
    assert!(requests(&mut model).is_empty());
    assert!(matches!(model.reader, Reader::Open(_)));
}

#[tokio::test]
async fn archive_is_hidden_when_the_account_has_none() {
    let mail = Mail::new(mailbox()).await;
    let mut model = mail.model();
    open(&mut model, "Plan 3");
    assert!(!model.can_archive());
    model.archive();
    assert!(requests(&mut model).is_empty());
}

#[tokio::test]
async fn delete_in_the_trash_asks_first_and_deletes_for_good_only_once_confirmed() {
    let mail = Mail::new(sorted_mailbox()).await;
    let mut model = mail.model();
    select(&mut model, "trash");
    let id = open(&mut model, "Plan 6");

    model.delete();
    assert_eq!(model.confirm.as_deref(), Some(id.as_str()));
    assert!(requests(&mut model).is_empty(), "nothing before the answer");
    model.dismiss();
    assert!(model.confirm.is_none());
    assert!(requests(&mut model).is_empty(), "cancelled: nothing asked");

    model.delete();
    model.confirm_delete();
    assert!(model.confirm.is_none());
    assert_eq!(requests(&mut model), [deleted(&id, true)]);
}

#[tokio::test]
async fn without_a_trash_delete_is_for_good_and_asks_first() {
    let mail = Mail::new(mailbox()).await;
    let mut model = mail.model();
    let id = open(&mut model, "Plan 3");
    model.delete();
    assert!(model.confirm.is_some());
    model.confirm_delete();
    assert_eq!(requests(&mut model), [deleted(&id, true)]);
}

#[tokio::test]
async fn an_answer_shows_the_change_at_once() {
    let mail = Mail::new(sorted_mailbox()).await;
    let mut model = mail.model();
    let id = open(&mut model, "Plan 3");
    model.archive();
    let request = model.take_request().unwrap();
    assert_eq!(request, moved(&id, "archive"));
    Store::open(&mail.copy_path())
        .unwrap()
        .queue(
            std::slice::from_ref(&id),
            &Wanted::Move("archive".to_owned()),
        )
        .unwrap()
        .unwrap();
    model.answered(Ok(recorded("move", &id)));
    assert_eq!(subjects(&model), ["Plan 2", "Plan 1"]);
    assert!(model.notice.is_none());
}

#[tokio::test]
async fn a_refused_event_shows_the_notice_and_the_message_comes_back() {
    let mail = Mail::new(sorted_mailbox()).await;
    let mut model = mail.model();
    let id = open(&mut model, "Plan 3");
    model.archive();
    model.take_request().unwrap();
    let mut store = Store::open(&mail.copy_path()).unwrap();
    store
        .queue(
            std::slice::from_ref(&id),
            &Wanted::Move("archive".to_owned()),
        )
        .unwrap()
        .unwrap();
    model.answered(Ok(recorded("move", &id)));
    assert_eq!(subjects(&model), ["Plan 2", "Plan 1"]);

    // The server refused it: the daemon takes the move back and tells it.
    let key = store.account(ID).unwrap();
    let action = store.next_action(key).unwrap().unwrap();
    store.refused(&action).unwrap();
    model.apply(&Event::Action(ActionEvent {
        account: ID.to_owned(),
        action: "move".to_owned(),
        outcome: "refused".to_owned(),
        messages: vec![id.clone()],
        reason: Some("no-such-folder".to_owned()),
        attempts: None,
        message: Some("the server has no such target folder".to_owned()),
    }));
    let notice = model.notice.clone().expect("a notice");
    assert!(notice.contains("no-such-folder"), "{notice}");
    assert_eq!(subjects(&model), ["Plan 3", "Plan 2", "Plan 1"]);
}

#[tokio::test]
async fn an_applied_or_retried_action_shows_no_notice() {
    let mail = Mail::new(sorted_mailbox()).await;
    let mut model = mail.model();
    for (outcome, reason) in [("applied", None), ("retried", Some("connection"))] {
        model.apply(&Event::Action(ActionEvent {
            account: ID.to_owned(),
            action: "read".to_owned(),
            outcome: outcome.to_owned(),
            messages: vec![model.messages[0].hit.id.clone()],
            reason: reason.map(str::to_owned),
            attempts: reason.map(|_| 1),
            message: None,
        }));
        assert!(model.notice.is_none(), "{outcome}");
    }
}

#[tokio::test]
async fn a_request_the_daemon_refuses_shows_the_notice() {
    let mail = Mail::new(sorted_mailbox()).await;
    let mut model = mail.model();
    open(&mut model, "Plan 3");
    model.archive();
    model.take_request().unwrap();
    model.answered(Ok(Answer::error(
        Some("move"),
        "busy",
        "the message waits for its move",
    )));
    let notice = model.notice.clone().expect("a notice");
    assert!(notice.contains("busy"), "{notice}");
    assert_eq!(subjects(&model), ["Plan 3", "Plan 2", "Plan 1"]);

    model.answered(Err("the daemon did not start".to_owned()));
    let notice = model.notice.clone().expect("a notice");
    assert!(notice.contains("the daemon did not start"), "{notice}");
}
