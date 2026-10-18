//! The actions on mail in GPUI's test window (no compositor, no GPU): the keys and the reader's
//! buttons hand their requests to the window's asker, the read timer runs on the window's clock,
//! a permanent delete asks in the window first, and a refusal shows a notice.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{Mail, sorted_mailbox};
use gpui_kit::{Modifiers, TestAppContext, VisualTestContext};
use missive_core::protocol::{Answer, Command, PROTOCOL};
use missive_window::model::{Model, Reader};
use missive_window::view::{self, Asker, MailWindow, Opener};

/// An asker that records each request and answers it as the daemon would, `ok` unless `refuse`
/// names the error to answer with.
fn asker(refuse: Option<&'static str>) -> (Asker, Arc<Mutex<Vec<Command>>>) {
    let asked = Arc::new(Mutex::new(Vec::new()));
    let recorded = asked.clone();
    let asker: Asker = Arc::new(move |command: Command| {
        let name = command.name().to_owned();
        recorded.lock().unwrap().push(command);
        Ok(match refuse {
            Some(error) => Answer::error(Some(&name), error, "the message waits for its move"),
            None => Answer {
                protocol: PROTOCOL,
                answer: Some(name),
                ok: true,
                ..Answer::default()
            },
        })
    });
    (asker, asked)
}

fn window(cx: &mut TestAppContext, model: Model, asker: Asker) -> &mut VisualTestContext {
    cx.update(|cx| {
        gpui_kit::base::init(cx);
        view::bind_keys(cx);
    });
    let opener: Opener = Arc::new(|_| {});
    let (_, cx) =
        cx.add_window_view(move |window, cx| MailWindow::new(model, opener, asker, window, cx));
    cx.run_until_parked();
    cx
}

fn shown(cx: &mut VisualTestContext) -> (Option<String>, Option<String>) {
    cx.update(|window, cx| {
        let root = window.root::<MailWindow>().flatten().unwrap();
        let model = &root.read(cx).model;
        let subject = match &model.reader {
            Reader::Open(opened) => opened.subject.clone(),
            _ => None,
        };
        (subject, model.notice.clone())
    })
}

fn id_of(mail_model: &Model, subject: &str) -> String {
    mail_model
        .messages
        .iter()
        .find(|m| m.hit.subject.as_deref() == Some(subject))
        .unwrap()
        .hit
        .id
        .clone()
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

fn advance(cx: &mut VisualTestContext, by: Duration) {
    cx.executor().advance_clock(by);
    cx.run_until_parked();
}

fn click(cx: &mut VisualTestContext, selector: &'static str) {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("no {selector}"));
    cx.simulate_click(bounds.center(), Modifiers::default());
    cx.run_until_parked();
}

fn mail() -> (tokio::runtime::Runtime, Mail) {
    // The fake server lives on a tokio runtime of its own, beside GPUI's test executor.
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let mail = runtime.block_on(Mail::new(sorted_mailbox()));
    (runtime, mail)
}

#[gpui_kit::test]
fn a_message_shown_for_two_seconds_is_marked_read_and_one_moved_past_is_not(
    cx: &mut TestAppContext,
) {
    let (_runtime, mail) = mail();
    let model = mail.model();
    let plan2 = id_of(&model, "Plan 2");
    let (asker, asked) = asker(None);
    let cx = window(cx, model, asker);

    cx.simulate_keystrokes("enter");
    assert_eq!(shown(cx).0.as_deref(), Some("Plan 3"));
    advance(cx, Duration::from_millis(1_900));
    cx.simulate_keystrokes("down enter");
    assert_eq!(shown(cx).0.as_deref(), Some("Plan 2"));
    advance(cx, Duration::from_millis(1_900));
    assert!(
        asked.lock().unwrap().is_empty(),
        "Plan 3 was left before 2 s"
    );
    advance(cx, Duration::from_millis(100));
    assert_eq!(*asked.lock().unwrap(), [mark(&plan2, "read")]);
    advance(cx, Duration::from_secs(10));
    assert_eq!(asked.lock().unwrap().len(), 1, "once");
}

#[gpui_kit::test]
fn each_key_sends_its_request(cx: &mut TestAppContext) {
    let (_runtime, mail) = mail();
    let model = mail.model();
    let plan1 = id_of(&model, "Plan 1");
    let plan3 = id_of(&model, "Plan 3");
    let (asker, asked) = asker(None);
    let cx = window(cx, model, asker);

    cx.simulate_keystrokes("down down enter u s");
    cx.run_until_parked();
    cx.simulate_keystrokes("up up enter a");
    cx.run_until_parked();
    assert_eq!(shown(cx).0, None, "an archived message leaves the reader");
    cx.simulate_keystrokes("enter m down down enter");
    cx.run_until_parked();
    cx.simulate_keystrokes("enter delete");
    cx.run_until_parked();
    assert_eq!(
        *asked.lock().unwrap(),
        [
            mark(&plan1, "unread"),
            mark(&plan1, "flagged"),
            moved(&plan3, "archive"),
            moved(&plan3, "Projects"),
            Command::Delete {
                messages: vec![plan3.clone()],
                permanently: false
            },
        ]
    );
}

#[gpui_kit::test]
fn the_reader_header_buttons_send_the_same_requests(cx: &mut TestAppContext) {
    let (_runtime, mail) = mail();
    let model = mail.model();
    let plan1 = id_of(&model, "Plan 1");
    let plan3 = id_of(&model, "Plan 3");
    let (asker, asked) = asker(None);
    let cx = window(cx, model, asker);

    cx.simulate_keystrokes("down down enter");
    click(cx, "mark-unread");
    click(cx, "flag");
    cx.simulate_keystrokes("up up enter");
    click(cx, "archive");
    cx.simulate_keystrokes("enter");
    click(cx, "move-to");
    click(cx, "pick-2");
    cx.simulate_keystrokes("enter");
    click(cx, "delete");
    assert_eq!(
        *asked.lock().unwrap(),
        [
            mark(&plan1, "unread"),
            mark(&plan1, "flagged"),
            moved(&plan3, "archive"),
            moved(&plan3, "Projects"),
            Command::Delete {
                messages: vec![plan3.clone()],
                permanently: false
            },
        ]
    );
}

#[gpui_kit::test]
fn a_permanent_delete_asks_in_the_window_first(cx: &mut TestAppContext) {
    let (_runtime, mail) = mail();
    let mut model = mail.model();
    let trash = model.accounts[0]
        .folders
        .iter()
        .find(|f| f.folder.role.as_deref() == Some("trash"))
        .unwrap()
        .folder
        .id;
    model.select_folder(common::ID, trash);
    let plan6 = id_of(&model, "Plan 6");
    let (asker, asked) = asker(None);
    let cx = window(cx, model, asker);

    cx.simulate_keystrokes("enter delete");
    assert!(cx.debug_bounds("confirm-delete").is_some(), "it asks");
    cx.simulate_keystrokes("escape");
    assert!(cx.debug_bounds("confirm-delete").is_none());
    assert_eq!(shown(cx).0.as_deref(), Some("Plan 6"), "still open");
    cx.run_until_parked();
    assert!(asked.lock().unwrap().is_empty(), "cancelled: nothing asked");

    cx.simulate_keystrokes("delete enter");
    cx.run_until_parked();
    assert_eq!(
        *asked.lock().unwrap(),
        [Command::Delete {
            messages: vec![plan6.clone()],
            permanently: true
        }]
    );

    cx.simulate_keystrokes("enter");
    click(cx, "delete");
    click(cx, "confirm-delete");
    assert_eq!(asked.lock().unwrap().len(), 2, "the button asks too");
}

#[gpui_kit::test]
fn a_refused_request_shows_a_notice_in_the_window(cx: &mut TestAppContext) {
    let (_runtime, mail) = mail();
    let model = mail.model();
    let (asker, _) = asker(Some("busy"));
    let cx = window(cx, model, asker);

    cx.simulate_keystrokes("enter a");
    cx.run_until_parked();
    let notice = shown(cx).1.expect("a notice");
    assert!(notice.contains("busy"), "{notice}");
    assert!(cx.debug_bounds("notice").is_some());
}
