//! The compose window in GPUI's test window (no compositor, no GPU): what is typed is saved as a
//! draft once the typing stops, Ctrl+Enter sends once and closes the window, closing saves
//! first, attachments come from the file chooser, and the reader's keys open the window.

mod common;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{ID, Mail, mailbox};
use gpui_kit::{
    AnyWindowHandle, ExternalPaths, FileDropEvent, Modifiers, TestAppContext, VisualContext,
    VisualTestContext,
};
use missive_core::draft_lock;
use missive_core::protocol::{Answer, Command, PROTOCOL, SaveDraft};
use missive_window::compose::{Compose, Field, SAVE_AFTER, State};
use missive_window::compose_view::{Chooser, ComposeWindow, Locker};
use missive_window::view::{self, Asker, MailWindow, Opener};

type Asked = Arc<Mutex<Vec<SaveDraft>>>;

/// An asker that records each `draft` request and answers it as the daemon would.
fn asker() -> (Asker, Asked) {
    let asked: Asked = Arc::new(Mutex::new(Vec::new()));
    let recorded = asked.clone();
    let asker: Asker = Arc::new(move |command: Command| {
        let Command::Draft(draft) = command else {
            panic!("not a draft request: {command:?}");
        };
        let send = draft.send;
        recorded.lock().unwrap().push(draft);
        Ok(Answer {
            protocol: PROTOCOL,
            answer: Some("draft".to_owned()),
            ok: true,
            outbox: send.then_some(1),
            message_id: Some("d@example.org".to_owned()),
            ..Answer::default()
        })
    });
    (asker, asked)
}

fn no_chooser() -> Chooser {
    Arc::new(|| Err("none in this test".to_owned()))
}

fn window(
    cx: &mut TestAppContext,
    compose: Compose,
    asker: Asker,
    chooser: Chooser,
) -> &mut VisualTestContext {
    cx.update(|cx| {
        gpui_kit::base::init(cx);
        view::bind_keys(cx);
    });
    let (_, cx) = cx
        .add_window_view(move |window, cx| ComposeWindow::new(compose, asker, chooser, window, cx));
    cx.run_until_parked();
    cx
}

fn new_message() -> Compose {
    Compose::new(vec![ID.to_owned()], ID.to_owned())
}

fn advance(cx: &mut VisualTestContext, by: Duration) {
    cx.executor().advance_clock(by);
    cx.run_until_parked();
}

/// Whether the compose window is still open.
fn open(cx: &mut VisualTestContext) -> bool {
    let handle: AnyWindowHandle = cx.window_handle();
    cx.cx.update(|cx| cx.windows().contains(&handle))
}

fn click(cx: &mut VisualTestContext, selector: &'static str) {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("no {selector}"));
    cx.simulate_click(bounds.center(), Modifiers::default());
    cx.run_until_parked();
}

#[gpui_kit::test]
fn what_is_typed_is_saved_once_the_typing_stops(cx: &mut TestAppContext) {
    let (asker, asked) = asker();
    let cx = window(cx, new_message(), asker, no_chooser());

    // A new message starts in To.
    cx.simulate_input("zoe@example.com");
    advance(cx, SAVE_AFTER - Duration::from_millis(100));
    assert!(asked.lock().unwrap().is_empty(), "still typing");
    cx.simulate_keystrokes("tab");
    advance(cx, SAVE_AFTER);

    let asked = asked.lock().unwrap();
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert_eq!(asked[0].to, Some(vec!["zoe@example.com".to_owned()]));
    assert!(!asked[0].send);
}

#[gpui_kit::test]
fn ctrl_enter_sends_once_and_closes_the_window(cx: &mut TestAppContext) {
    let (asker, asked) = asker();
    let cx = window(cx, new_message(), asker, no_chooser());

    cx.simulate_input("zoe@example.com");
    cx.simulate_keystrokes("ctrl-enter");
    cx.run_until_parked();

    let asked = asked.lock().unwrap();
    // Saved first, then sent under the draft's identifier, once.
    assert_eq!(
        asked.iter().map(|d| d.send).collect::<Vec<_>>(),
        [false, true]
    );
    assert_eq!(asked[1].draft.as_deref(), Some("d@example.org"));
    assert_eq!(asked[1].to, Some(vec!["zoe@example.com".to_owned()]));
    drop(asked);
    assert!(
        !open(cx),
        "the window closes once the message is in the outbox"
    );
}

#[gpui_kit::test]
fn ctrl_enter_in_the_text_adds_no_line(cx: &mut TestAppContext) {
    let (asker, asked) = asker();
    let mut compose = new_message();
    compose.to = "zoe@example.com".to_owned();
    // With a recipient, the window starts in the text.
    let cx = window(cx, compose, asker, no_chooser());

    cx.simulate_input("Hello");
    cx.simulate_keystrokes("ctrl-enter");
    cx.run_until_parked();

    let asked = asked.lock().unwrap();
    assert_eq!(asked.last().unwrap().body.as_deref(), Some("Hello"));
    assert!(asked.last().unwrap().send);
}

#[gpui_kit::test]
fn the_fields_take_no_text_while_the_message_is_being_sent(cx: &mut TestAppContext) {
    let (asker, asked) = asker();
    let mut compose = new_message();
    compose.to = "zoe@example.com".to_owned();
    compose.state = State::Sending;
    let cx = window(cx, compose, asker, no_chooser());

    // The text typed meanwhile would show in the widget and be lost to the model.
    cx.simulate_input("Hello");
    let shown = |cx: &mut VisualTestContext, field| {
        cx.update(|window, cx| {
            let root = window.root::<ComposeWindow>().flatten().unwrap();
            root.read(cx).shown(field, cx)
        })
    };
    assert_eq!(shown(cx, Field::Body), "", "the body is read-only");
    cx.simulate_keystrokes("shift-tab");
    cx.simulate_input("more");
    assert_eq!(shown(cx, Field::To), "zoe@example.com", "so is To");

    // The send failed: the fields are editable again, and show what the model holds.
    cx.update(|window, cx| {
        let root = window.root::<ComposeWindow>().flatten().unwrap();
        root.update(cx, |this, cx| {
            this.model.state = State::Editing;
            this.changed(window, cx);
        });
    });
    cx.simulate_input("!");
    cx.run_until_parked();
    let model = cx.update(|window, cx| {
        let root = window.root::<ComposeWindow>().flatten().unwrap();
        let model = &root.read(cx).model;
        (model.to.clone(), model.body.clone())
    });
    assert_eq!(shown(cx, Field::To), model.0);
    assert_eq!(shown(cx, Field::Body), model.1);
    cx.simulate_keystrokes("ctrl-w");
    cx.run_until_parked();
    assert_eq!(asked.lock().unwrap().len(), 1, "closing saves what shows");
}

#[gpui_kit::test]
fn closing_saves_the_draft_then_closes(cx: &mut TestAppContext) {
    let (asker, asked) = asker();
    let cx = window(cx, new_message(), asker, no_chooser());

    cx.simulate_input("carl@example.net");
    cx.simulate_keystrokes("ctrl-w");
    cx.run_until_parked();

    let asked = asked.lock().unwrap();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].to, Some(vec!["carl@example.net".to_owned()]));
    drop(asked);
    assert!(!open(cx));
}

#[gpui_kit::test]
fn closing_an_empty_new_message_saves_nothing(cx: &mut TestAppContext) {
    let (asker, asked) = asker();
    let cx = window(cx, new_message(), asker, no_chooser());
    cx.simulate_keystrokes("ctrl-w");
    cx.run_until_parked();
    assert!(asked.lock().unwrap().is_empty());
    assert!(!open(cx));
}

#[gpui_kit::test]
fn files_from_the_chooser_are_attached_and_removed(cx: &mut TestAppContext) {
    let folder = tempfile::tempdir().unwrap();
    let file = folder.path().join("notes.txt");
    std::fs::write(&file, "Notes").unwrap();
    let chosen: Vec<PathBuf> = vec![file];
    let chooser: Chooser = Arc::new(move || Ok(chosen.clone()));
    let (asker, _) = asker();
    let cx = window(cx, new_message(), asker, chooser);

    click(cx, "attach");
    let names = |cx: &mut VisualTestContext| {
        cx.update(|window, cx| {
            let root = window.root::<ComposeWindow>().flatten().unwrap();
            root.read(cx)
                .model
                .attachments
                .iter()
                .map(|a| a.name.clone())
                .collect::<Vec<_>>()
        })
    };
    assert_eq!(names(cx), ["notes.txt"]);
    click(cx, "remove-0");
    assert!(names(cx).is_empty());
}

#[gpui_kit::test]
fn the_reader_keys_open_a_compose_window(cx: &mut TestAppContext) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let mail = runtime.block_on(Mail::new(mailbox()));
    let (asker, _) = asker();
    cx.update(|cx| {
        gpui_kit::base::init(cx);
        view::bind_keys(cx);
    });
    let opener: Opener = Arc::new(|_| {});
    let model = mail.model();
    let (_, cx) =
        cx.add_window_view(move |window, cx| MailWindow::new(model, opener, asker, window, cx));
    cx.run_until_parked();

    let subjects = |cx: &mut VisualTestContext| -> Vec<String> {
        cx.update(|_, cx| {
            cx.windows()
                .into_iter()
                .filter_map(|w| w.downcast::<ComposeWindow>())
                .map(|w| w.read(cx).unwrap().model.subject.clone())
                .collect()
        })
    };
    cx.simulate_keystrokes("r");
    cx.run_until_parked();
    assert_eq!(
        subjects(cx),
        ["Re: Plan 3"],
        "nothing is open: the list's cursor is answered"
    );
    cx.simulate_keystrokes("enter f");
    cx.run_until_parked();
    assert_eq!(subjects(cx).len(), 2);
    assert!(subjects(cx).contains(&"Fwd: Plan 3".to_owned()));
}

/// The compose windows open, by subject.
fn compose_windows(cx: &mut VisualTestContext) -> Vec<String> {
    cx.update(|_, cx| {
        cx.windows()
            .into_iter()
            .filter_map(|w| w.downcast::<ComposeWindow>())
            .map(|w| w.read(cx).unwrap().model.subject.clone())
            .collect()
    })
}

#[gpui_kit::test]
fn a_draft_is_opened_in_one_window_however_many_times_it_is_asked_for(cx: &mut TestAppContext) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let drafts = missive_core::fake::Folder::new("Drafts")
        .special_use(missive_core::fake::SpecialUse::Drafts)
        .message(missive_core::fake::Message::new(
            "X-Missive-Draft: 1\r\nFrom: alice@example.org\r\nTo: zoe@example.com\r\n\
             Subject: One draft\r\nMessage-ID: <one@example.org>\r\n\r\nHello\r\n",
        ))
        .message(missive_core::fake::Message::new(
            "X-Missive-Draft: 1\r\nFrom: alice@example.org\r\nTo: zoe@example.com\r\n\
             Subject: Another draft\r\nMessage-ID: <two@example.org>\r\n\r\nHello\r\n",
        ));
    let mail = runtime.block_on(Mail::new(mailbox().folder(drafts)));
    let (asker, _) = asker();
    cx.update(|cx| {
        gpui_kit::base::init(cx);
        view::bind_keys(cx);
    });
    let opener: Opener = Arc::new(|_| {});
    let mut model = mail.model();
    let folder = model.accounts[0]
        .folders
        .iter()
        .find(|f| f.folder.role.as_deref() == Some("drafts"))
        .unwrap()
        .folder
        .id;
    model.select_folder(ID, folder);
    let (_, cx) =
        cx.add_window_view(move |window, cx| MailWindow::new(model, opener, asker, window, cx));
    cx.run_until_parked();

    cx.simulate_keystrokes("e");
    cx.run_until_parked();
    assert_eq!(compose_windows(cx).len(), 1);
    // The same draft again, with the first window still open: it is not opened twice.
    cx.simulate_keystrokes("e");
    cx.run_until_parked();
    assert_eq!(compose_windows(cx).len(), 1);

    // Another draft has a window of its own.
    cx.simulate_keystrokes("down e");
    cx.run_until_parked();
    let subjects = compose_windows(cx);
    assert_eq!(subjects.len(), 2, "{subjects:?}");
    assert_ne!(subjects[0], subjects[1]);

    // Once the window is closed, the draft opens again.
    let first = cx.update(|_, cx| {
        cx.windows()
            .into_iter()
            .find_map(|w| w.downcast::<ComposeWindow>())
            .unwrap()
    });
    cx.update(|_, cx| {
        first
            .update(cx, |_, window, _| window.remove_window())
            .unwrap()
    });
    cx.run_until_parked();
    assert_eq!(compose_windows(cx).len(), 1);
    cx.simulate_keystrokes("up e");
    cx.run_until_parked();
    assert_eq!(compose_windows(cx).len(), 2);
}

#[gpui_kit::test]
fn files_dropped_on_the_window_are_attached_and_the_unreadable_ones_are_named(
    cx: &mut TestAppContext,
) {
    let folder = tempfile::tempdir().unwrap();
    let notes = folder.path().join("notes.txt");
    std::fs::write(&notes, "Notes").unwrap();
    let report = folder.path().join("report é.pdf");
    std::fs::write(&report, "%PDF-1.7").unwrap();
    let missing = folder.path().join("gone.txt");
    let (asker, _) = asker();
    let cx = window(cx, new_message(), asker, no_chooser());
    let at = cx.debug_bounds("attach").unwrap().center();

    // What a compositor sends when files are dragged over the window and let go.
    let paths = ExternalPaths([notes, report, missing].into_iter().collect());
    cx.simulate_event(FileDropEvent::Entered {
        position: at,
        paths,
    });
    cx.simulate_event(FileDropEvent::Pending { position: at });
    cx.simulate_event(FileDropEvent::Submit { position: at });
    cx.executor().run_until_parked();
    cx.run_until_parked();

    let (names, notice, saves) = cx.update(|window, cx| {
        let root = window.root::<ComposeWindow>().flatten().unwrap();
        let root = root.read(cx);
        (
            root.model
                .attachments
                .iter()
                .map(|a| a.name.clone())
                .collect::<Vec<_>>(),
            root.model.notice.clone().unwrap_or_default(),
            root.model.needs_save(),
        )
    });
    assert_eq!(names, ["notes.txt", "report é.pdf"]);
    assert!(notice.contains("gone.txt"), "{notice}");
    assert!(saves, "the draft is saved with them once the typing stops");
    // They are shown, each with its Remove button.
    assert!(cx.debug_bounds("remove-1").is_some());
}

/// A locker over a private folder, as the window's own is over the runtime folder.
fn locker(folder: &tempfile::TempDir) -> Locker {
    let folder = folder.path().to_owned();
    Arc::new(move |account, id| draft_lock::take_in(&folder, account, id))
}

#[gpui_kit::test]
fn a_new_message_takes_the_lock_of_its_draft_once_saved_and_gives_it_back_on_closing(
    cx: &mut TestAppContext,
) {
    let folder = tempfile::tempdir().unwrap();
    let (asker, _) = asker();
    cx.update(|cx| {
        gpui_kit::base::init(cx);
        view::bind_keys(cx);
    });
    let locks = locker(&folder);
    let (_, cx) = cx.add_window_view(move |window, cx| {
        ComposeWindow::new(new_message(), asker, no_chooser(), window, cx).holding(None, locks)
    });
    cx.run_until_parked();
    let held = || draft_lock::is_held_in(folder.path(), ID, "d@example.org");
    assert!(!held(), "nobody can know a draft that is not saved");

    cx.simulate_input("zoe@example.com");
    advance(cx, SAVE_AFTER);
    // The daemon named it `d@example.org`: a second window on it, anywhere, is refused.
    assert!(held());

    cx.simulate_keystrokes("ctrl-w");
    cx.run_until_parked();
    assert!(!open(cx));
    assert!(!held(), "closed windows hold nothing");
}

#[gpui_kit::test]
fn a_draft_open_in_another_process_is_not_opened_a_second_time(cx: &mut TestAppContext) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let drafts = missive_core::fake::Folder::new("Drafts")
        .special_use(missive_core::fake::SpecialUse::Drafts)
        .message(missive_core::fake::Message::new(
            "X-Missive-Draft: 1\r\nFrom: alice@example.org\r\nTo: zoe@example.com\r\n\
             Subject: One draft\r\nMessage-ID: <one@example.org>\r\n\r\nHello\r\n",
        ));
    let mail = runtime.block_on(Mail::new(mailbox().folder(drafts)));
    let (asker, _) = asker();
    cx.update(|cx| {
        gpui_kit::base::init(cx);
        view::bind_keys(cx);
    });
    let folder = tempfile::tempdir().unwrap();
    // The window of another process, which holds the draft's lock.
    let draft_lock::Taking::Ours(elsewhere) =
        draft_lock::take_in(folder.path(), ID, "one@example.org")
    else {
        panic!("free to take");
    };
    let opener: Opener = Arc::new(|_| {});
    let mut model = mail.model();
    let id = model.accounts[0]
        .folders
        .iter()
        .find(|f| f.folder.role.as_deref() == Some("drafts"))
        .unwrap()
        .folder
        .id;
    model.select_folder(ID, id);
    let locks = locker(&folder);
    let (_, cx) = cx.add_window_view(move |window, cx| {
        MailWindow::new(model, opener, asker, window, cx).with_locker(locks)
    });
    cx.run_until_parked();

    cx.simulate_keystrokes("e");
    cx.run_until_parked();
    assert_eq!(compose_windows(cx).len(), 0, "the draft is open elsewhere");

    // That window closes (or crashes): the draft opens here.
    drop(elsewhere);
    cx.simulate_keystrokes("e");
    cx.run_until_parked();
    assert_eq!(compose_windows(cx).len(), 1);
    assert!(draft_lock::is_held_in(folder.path(), ID, "one@example.org"));
}

/// An asker whose `n`th request is answered by `answers[n]` (the last one answers the rest).
fn asker_answering(answers: Vec<Answer>) -> Asker {
    let next = Arc::new(Mutex::new(0usize));
    Arc::new(move |_| {
        let mut next = next.lock().unwrap();
        let answer = answers[(*next).min(answers.len() - 1)].clone();
        *next += 1;
        Ok(answer)
    })
}

fn saved_as(message_id: &str) -> Answer {
    Answer {
        protocol: PROTOCOL,
        answer: Some("draft".to_owned()),
        ok: true,
        message_id: Some(message_id.to_owned()),
        ..Answer::default()
    }
}

#[gpui_kit::test]
fn the_lock_follows_the_new_draft_made_from_another_clients_draft(cx: &mut TestAppContext) {
    let folder = tempfile::tempdir().unwrap();
    cx.update(|cx| {
        gpui_kit::base::init(cx);
        view::bind_keys(cx);
    });
    let raw = b"From: alice@example.org\r\nTo: zoe@example.com\r\nSubject: Plans\r\n\
        Message-ID: <theirs@example.org>\r\n\r\nHello\r\n";
    let compose = Compose::draft(
        vec![ID.to_owned()],
        ID.to_owned(),
        "alice/3/1/5".to_owned(),
        raw,
        Default::default(),
    )
    .unwrap();
    let draft_lock::Taking::Ours(lock) =
        draft_lock::take_in(folder.path(), ID, "theirs@example.org")
    else {
        panic!("free to take");
    };
    // The daemon keeps the other client's draft as it is and saves a new one beside it.
    let asker = asker_answering(vec![saved_as("ours@example.org")]);
    let locks = locker(&folder);
    let (_, cx) = cx.add_window_view(move |window, cx| {
        ComposeWindow::new(compose, asker, no_chooser(), window, cx).holding(Some(lock), locks)
    });
    cx.run_until_parked();
    let held = |id| draft_lock::is_held_in(folder.path(), ID, id);
    assert!(held("theirs@example.org"));

    cx.simulate_input(" more");
    advance(cx, SAVE_AFTER);
    // The draft the window works on now is the new one: a second window on it is refused.
    assert!(held("ours@example.org"), "the new draft is locked");
    assert!(!held("theirs@example.org"), "the lock moved");

    cx.simulate_keystrokes("ctrl-w");
    cx.run_until_parked();
    assert!(!held("ours@example.org"));
}

#[gpui_kit::test]
fn the_lock_follows_the_new_draft_made_when_the_message_was_already_sent(cx: &mut TestAppContext) {
    let folder = tempfile::tempdir().unwrap();
    cx.update(|cx| {
        gpui_kit::base::init(cx);
        view::bind_keys(cx);
    });
    let mut already_sent = Answer::error(Some("draft"), "already-sent", "already sent");
    already_sent.message_id = Some("new@example.org".to_owned());
    let asker = asker_answering(vec![saved_as("d@example.org"), already_sent]);
    let locks = locker(&folder);
    let (_, cx) = cx.add_window_view(move |window, cx| {
        ComposeWindow::new(new_message(), asker, no_chooser(), window, cx).holding(None, locks)
    });
    cx.run_until_parked();
    let held = |id| draft_lock::is_held_in(folder.path(), ID, id);

    cx.simulate_input("zoe@example.com");
    advance(cx, SAVE_AFTER);
    assert!(held("d@example.org"));

    cx.simulate_keystrokes("ctrl-enter");
    cx.run_until_parked();
    assert!(open(cx), "nothing was sent: the window stays");
    assert!(held("new@example.org"), "the new draft is locked");
    assert!(!held("d@example.org"), "the lock moved");
}
