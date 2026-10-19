//! What the window asks the daemon to do to mail, without a window: the requests the model
//! queues for the keys and buttons, and how it follows their answers and `action` events. The
//! daemon's part is played by the store's own queue, as the daemon calls it.

mod common;

use common::{ID, Mail, mailbox, sorted_mailbox};
use missive_core::actions::Wanted;
use missive_core::events::{ActionEvent, Event};
use missive_core::protocol::{Answer, Command, PROTOCOL};
use missive_core::store::Store;
use missive_window::model::{Model, READ_AFTER, Reader, Start};

fn subjects(model: &Model) -> Vec<String> {
    model
        .messages
        .iter()
        .map(|m| m.hit.subject.clone().unwrap_or_default())
        .collect()
}

/// The subject of the message open in the reader.
fn shown(model: &Model) -> Option<String> {
    match &model.reader {
        Reader::Open(opened) => opened.subject.clone(),
        _ => None,
    }
}

/// The subject of the message under the list's cursor.
fn under_cursor(model: &Model) -> Option<String> {
    model.messages[model.cursor?].hit.subject.clone()
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
    assert_eq!(
        shown(&model).as_deref(),
        Some("Plan 2"),
        "the message left the reader for the next one"
    );

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
async fn without_a_trash_delete_is_never_for_good() {
    // The daemon refuses it (`no-trash`), and the refusal shows as a notice.
    let mail = Mail::new(mailbox()).await;
    let mut model = mail.model();
    let id = open(&mut model, "Plan 3");
    model.delete();
    assert!(model.confirm.is_none());
    assert_eq!(requests(&mut model), [deleted(&id, false)]);
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
        id: 1,
        reason: Some("no-such-folder".to_owned()),
        attempts: None,
        message: Some("the server has no such target folder".to_owned()),
        copied: None,
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
            id: 1,
            reason: reason.map(str::to_owned),
            attempts: reason.map(|_| 1),
            message: None,
            copied: None,
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

#[tokio::test]
async fn after_a_move_or_a_delete_the_message_below_opens() {
    let mail = Mail::new(sorted_mailbox()).await;
    let mut model = mail.model();
    let id = open(&mut model, "Plan 3");
    model.take_read_timer();
    model.archive();
    assert_eq!(requests(&mut model), [moved(&id, "archive")]);
    assert_eq!(shown(&model).as_deref(), Some("Plan 2"));
    assert_eq!(under_cursor(&model).as_deref(), Some("Plan 2"));
    assert!(
        model.take_read_timer().is_some(),
        "the message that opens is read like any other"
    );

    let id = open(&mut model, "Plan 2");
    model.delete();
    assert_eq!(requests(&mut model), [deleted(&id, false)]);
    assert_eq!(shown(&model).as_deref(), Some("Plan 1"));
}

#[tokio::test]
async fn the_last_message_of_the_list_opens_the_one_above() {
    let mail = Mail::new(sorted_mailbox()).await;
    let mut model = mail.model();
    let id = open(&mut model, "Plan 1");
    model.archive();
    assert_eq!(requests(&mut model), [moved(&id, "archive")]);
    assert_eq!(shown(&model).as_deref(), Some("Plan 2"));
    assert_eq!(under_cursor(&model).as_deref(), Some("Plan 2"));
}

#[tokio::test]
async fn the_only_message_of_the_list_closes_the_reader() {
    let mail = Mail::new(sorted_mailbox()).await;
    let mut model = mail.model();
    select(&mut model, "trash");
    let id = open(&mut model, "Plan 6");
    model.delete();
    model.confirm_delete();
    assert_eq!(requests(&mut model), [deleted(&id, true)]);
    assert_eq!(model.reader, Reader::Empty);
}

#[tokio::test]
async fn a_message_chosen_in_the_picker_opens_the_next_one_too() {
    let mail = Mail::new(sorted_mailbox()).await;
    let mut model = mail.model();
    let id = open(&mut model, "Plan 2");
    model.pick_folder();
    model.picker_move(5);
    model.choose_folder();
    assert_eq!(requests(&mut model), [moved(&id, "Projects")]);
    assert_eq!(shown(&model).as_deref(), Some("Plan 1"));
}

#[tokio::test]
async fn a_second_move_before_the_first_is_answered_acts_on_the_new_message() {
    let mail = Mail::new(sorted_mailbox()).await;
    let mut model = mail.model();
    let first = open(&mut model, "Plan 3");
    model.archive();
    let second = model
        .messages
        .iter()
        .find(|m| m.hit.subject.as_deref() == Some("Plan 2"))
        .unwrap()
        .hit
        .id
        .clone();
    model.archive();
    assert_eq!(
        requests(&mut model),
        [moved(&first, "archive"), moved(&second, "archive")]
    );
    assert_eq!(shown(&model).as_deref(), Some("Plan 1"));
}

#[tokio::test]
async fn with_no_message_open_the_keys_act_on_the_cursor() {
    let mail = Mail::new(sorted_mailbox()).await;
    let mut model = mail.model();
    assert_eq!(model.reader, Reader::Empty);
    let id_of = |model: &Model, at: usize| model.messages[at].hit.id.clone();

    // Plan 3 is unread and not flagged; Plan 2 is flagged; Plan 1 is read.
    let plan3 = id_of(&model, 0);
    model.toggle_flag();
    assert_eq!(requests(&mut model), [mark(&plan3, "flagged")], "S");
    model.move_cursor(1);
    let plan2 = id_of(&model, 1);
    model.toggle_flag();
    assert_eq!(requests(&mut model), [mark(&plan2, "unflagged")], "S again");
    model.move_cursor(1);
    let plan1 = id_of(&model, 2);
    model.mark_unread();
    assert_eq!(requests(&mut model), [mark(&plan1, "unread")], "U");
    assert_eq!(model.reader, Reader::Empty, "nothing was opened");

    model.pick_folder();
    assert!(model.picker.is_some(), "M");
    model.choose_folder();
    assert_eq!(requests(&mut model), [moved(&plan1, "archive")]);

    // The cursor went up to Plan 2 when Plan 1 left.
    assert!(model.can_archive());
    model.archive();
    assert_eq!(requests(&mut model), [moved(&plan2, "archive")], "A");
    model.move_cursor(-2);
    model.delete();
    assert_eq!(requests(&mut model), [deleted(&plan3, false)], "Delete");
    assert_eq!(model.reader, Reader::Empty);
}

#[tokio::test]
async fn the_open_message_wins_over_the_cursor() {
    let mail = Mail::new(sorted_mailbox()).await;
    let mut model = mail.model();
    let id = open(&mut model, "Plan 3");
    model.move_cursor(1);
    assert_eq!(under_cursor(&model).as_deref(), Some("Plan 2"));
    model.toggle_flag();
    assert_eq!(requests(&mut model), [mark(&id, "flagged")]);
}

#[tokio::test]
async fn a_cursor_action_that_removes_a_message_moves_the_cursor_on() {
    let mail = Mail::new(sorted_mailbox()).await;
    let mut model = mail.model();
    let first = model.messages[0].hit.id.clone();
    model.archive();
    assert_eq!(requests(&mut model), [moved(&first, "archive")]);
    assert_eq!(
        under_cursor(&model).as_deref(),
        Some("Plan 2"),
        "a second press does not ask for the same message again"
    );
    assert_eq!(model.reader, Reader::Empty);

    select(&mut model, "archive");
    assert_eq!(model.cursor, None);
    model.delete();
    model.archive();
    model.toggle_flag();
    assert!(requests(&mut model).is_empty(), "no message, nothing asked");
}

#[tokio::test]
async fn delete_on_the_cursor_in_the_trash_asks_first() {
    let mail = Mail::new(sorted_mailbox()).await;
    let mut model = mail.model();
    select(&mut model, "trash");
    let id = model.messages[0].hit.id.clone();
    model.delete();
    assert_eq!(model.confirm.as_deref(), Some(id.as_str()));
    assert!(requests(&mut model).is_empty());
    model.confirm_delete();
    assert_eq!(requests(&mut model), [deleted(&id, true)]);
}

#[tokio::test]
async fn reply_and_forward_start_from_the_cursor_when_no_message_is_open() {
    let mail = Mail::new(sorted_mailbox()).await;
    let mut model = mail.model();
    let reply = model.compose(Start::Reply).expect("a reply to Plan 3");
    assert_eq!(reply.subject, "Re: Plan 3");
    assert_eq!(model.reader, Reader::Empty, "nothing was opened");
    model.move_cursor(1);
    let forward = model.compose(Start::Forward).expect("Plan 2 forwarded");
    assert_eq!(forward.subject, "Fwd: Plan 2");

    select(&mut model, "archive");
    assert!(model.compose(Start::Reply).is_none(), "nothing to answer");
}

#[tokio::test]
async fn a_picker_or_a_confirmation_closes_with_the_message_that_is_gone() {
    let mail = Mail::new(sorted_mailbox()).await;
    let mut model = mail.model();
    open(&mut model, "Plan 3");
    model.pick_folder();
    assert!(model.picker.is_some());

    mail.server.expunge("INBOX", 3);
    mail.sync().await;
    model.refresh();
    assert_eq!(model.reader, Reader::Gone);
    assert!(model.picker.is_none(), "no folder picker on a message gone");
    assert!(!model.modal());

    select(&mut model, "trash");
    open(&mut model, "Plan 6");
    model.delete();
    assert!(model.confirm.is_some());
    mail.server.expunge("Trash", 1);
    mail.sync().await;
    model.refresh();
    assert_eq!(model.reader, Reader::Gone);
    assert!(model.confirm.is_none(), "no confirmation on a message gone");
    assert!(!model.modal());
}

#[tokio::test]
async fn clicking_another_folder_closes_the_picker_and_the_confirmation() {
    let mail = Mail::new(sorted_mailbox()).await;
    let mut model = mail.model();
    let archive = model.accounts[0]
        .folders
        .iter()
        .find(|f| f.folder.role.as_deref() == Some("archive"))
        .unwrap()
        .folder
        .id;
    open(&mut model, "Plan 3");
    model.pick_folder();
    model.select_folder(ID, archive);
    assert!(model.picker.is_none());

    select(&mut model, "trash");
    open(&mut model, "Plan 6");
    model.delete();
    assert!(model.confirm.is_some());
    model.select_folder(ID, archive);
    assert!(model.confirm.is_none());
    assert!(requests(&mut model).is_empty(), "nothing was asked");
}

#[tokio::test]
async fn after_the_daemon_stopped_the_next_action_is_still_asked() {
    // The asking starts a daemon again when none answers (`missive_core::client`).
    let mail = Mail::new(sorted_mailbox()).await;
    let mut model = mail.model();
    model.apply(&Event::Gone);
    assert_eq!(model.daemon, missive_window::model::Daemon::Stopped);
    let id = open(&mut model, "Plan 3");
    model.toggle_flag();
    assert_eq!(requests(&mut model), [mark(&id, "flagged")]);
}
