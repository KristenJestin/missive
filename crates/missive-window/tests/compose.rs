//! The compose window's model, without a window: the fields a reply or a forward starts with,
//! the drafts saved as one types and on closing, and Send, which queues the message once.

mod common;

use common::{ID, Mail, mailbox};
use missive_core::protocol::{Answer, Attached, Command, PROTOCOL, SaveDraft};
use missive_window::compose::{Compose, Field, MAX_ATTACHMENT, State, addresses, load};
use missive_window::model::{Model, Start};

fn new_message() -> Compose {
    Compose::new(vec![ID.to_owned()], ID.to_owned())
}

fn saved(message_id: &str) -> Result<Answer, String> {
    Ok(Answer {
        protocol: PROTOCOL,
        answer: Some("draft".to_owned()),
        ok: true,
        message_id: Some(message_id.to_owned()),
        ..Answer::default()
    })
}

fn draft(command: Command) -> SaveDraft {
    match command {
        Command::Draft(draft) => draft,
        other => panic!("not a draft request: {other:?}"),
    }
}

/// Types `text` in `field`, waits until the save timer ends, and returns the request it asks.
fn type_and_wait(compose: &mut Compose, field: Field, text: &str) -> Option<Command> {
    compose.edit(field, text.to_owned());
    let timer = compose
        .take_save_timer()
        .expect("a change starts the save timer");
    compose.save_timer_elapsed(timer);
    compose.take_request()
}

fn open(model: &mut Model, subject: &str) {
    let id = model
        .messages
        .iter()
        .find(|m| m.hit.subject.as_deref() == Some(subject))
        .unwrap()
        .hit
        .id
        .clone();
    model.open(&id);
}

#[tokio::test]
async fn a_reply_has_the_senders_address_the_subject_the_thread_and_the_quote_below_the_cursor() {
    let mail = Mail::new(mailbox()).await;
    let mut model = mail.model();
    open(&mut model, "Plan 3");

    let mut compose = model.compose(Start::Reply).unwrap();

    assert_eq!(compose.account, ID);
    assert_eq!(compose.to, "\"Carol Sender\" <carol.sender@example.com>");
    assert_eq!(compose.cc, "");
    assert_eq!(compose.subject, "Re: Plan 3");
    assert_eq!(
        compose.body,
        "\n\nOn 2026-10-17 09:03 +0200, Carol Sender wrote:\n> The Zanzibar archive.\n"
    );
    assert!(compose.attachments.is_empty());
    compose.send();
    let send = draft(compose.take_request().unwrap());
    // A new message is saved before it is sent.
    assert!(!send.send);
    assert_eq!(send.in_reply_to.as_deref(), Some("3@example.com"));
    assert_eq!(send.references, ["3@example.com"]);
    compose.answered(saved("r@example.org"));
    let send = draft(compose.take_request().unwrap());
    assert!(send.send);
    assert_eq!(
        send.answers.as_deref(),
        Some(model_id(&model, "Plan 3").as_str())
    );
}

fn model_id(model: &Model, subject: &str) -> String {
    model
        .messages
        .iter()
        .find(|m| m.hit.subject.as_deref() == Some(subject))
        .unwrap()
        .hit
        .id
        .clone()
}

#[tokio::test]
async fn a_reply_all_and_a_forward_start_from_the_open_message() {
    let mail = Mail::new(mailbox()).await;
    let mut model = mail.model();
    assert!(
        model.compose(Start::Reply).is_some(),
        "no message open: Plan 3, under the cursor"
    );
    open(&mut model, "Plan 1");

    let all = model.compose(Start::ReplyAll).unwrap();
    assert_eq!(
        all.to,
        "\"Carol Sender\" <carol.sender@example.com>, \"Dave Recipient\" <dave@example.net>"
    );

    let forward = model.compose(Start::Forward).unwrap();
    assert_eq!(forward.subject, "Fwd: Plan 1");
    assert_eq!(forward.to, "");
    assert_eq!(forward.body, "");
    assert_eq!(forward.attachments.len(), 1);
    assert_eq!(forward.attachments[0].name, "Plan 1.eml");

    let new = model.compose(Start::New).unwrap();
    assert_eq!((new.account.as_str(), new.subject.as_str()), (ID, ""));
}

#[test]
fn typing_then_waiting_saves_one_draft() {
    let mut compose = new_message();
    compose.edit(Field::To, "zoe@example.com".to_owned());
    let early = compose.take_save_timer().unwrap();
    compose.edit(Field::Subject, "Lunch".to_owned());
    let late = compose.take_save_timer().unwrap();

    // The first timer ends while the user still types: nothing is saved.
    compose.save_timer_elapsed(early);
    assert_eq!(compose.take_request(), None);
    compose.save_timer_elapsed(late);
    let first = draft(compose.take_request().unwrap());
    assert_eq!(first.draft, None);
    assert_eq!(first.to, Some(vec!["zoe@example.com".to_owned()]));
    assert_eq!(first.subject.as_deref(), Some("Lunch"));
    assert!(!first.send);
    assert_eq!(compose.take_request(), None, "one at a time");
    compose.answered(saved("d@example.org"));
    assert_eq!(compose.take_request(), None, "saved once");

    // The next save names the draft, and leaves its attachments as they are.
    let next = draft(type_and_wait(&mut compose, Field::Body, "Noon?").unwrap());
    assert_eq!(next.draft.as_deref(), Some("d@example.org"));
    assert_eq!(next.body.as_deref(), Some("Noon?"));
    assert_eq!(next.attachments, None);
}

#[test]
fn closing_saves_the_draft_then_closes() {
    let mut compose = new_message();
    compose.edit(Field::Body, "Unfinished".to_owned());

    assert!(!compose.close(), "the window waits for the save");
    let request = draft(compose.take_request().unwrap());
    assert_eq!(request.body.as_deref(), Some("Unfinished"));
    assert!(!compose.done());
    compose.answered(saved("d@example.org"));
    assert!(compose.done());
}

#[test]
fn closing_while_a_save_is_in_flight_saves_what_was_typed_since_before_closing() {
    let mut compose = new_message();
    let first = draft(type_and_wait(&mut compose, Field::Body, "Dear Zoe").unwrap());
    assert_eq!(first.body.as_deref(), Some("Dear Zoe"));

    // The save is on its way with a snapshot; the user types more, then closes.
    compose.edit(Field::Body, "Dear Zoe, lunch at noon?".to_owned());
    assert!(!compose.close());
    assert_eq!(compose.take_request(), None, "one at a time");

    // The first answer must not close the window: what was typed since is not saved.
    compose.answered(saved("d@example.org"));
    assert!(!compose.done());
    let second = draft(compose.take_request().expect("the rest is saved too"));
    assert_eq!(second.draft.as_deref(), Some("d@example.org"));
    assert_eq!(second.body.as_deref(), Some("Dear Zoe, lunch at noon?"));
    compose.answered(saved("d@example.org"));
    assert!(compose.done());
}

#[test]
fn closing_without_a_change_since_the_last_save_closes_at_once() {
    let mut compose = new_message();
    type_and_wait(&mut compose, Field::Body, "Done").unwrap();
    compose.answered(saved("d@example.org"));
    assert!(compose.close());
    assert_eq!(compose.take_request(), None);
}

#[test]
fn an_empty_new_message_saves_nothing() {
    let mut compose = new_message();
    assert!(compose.close());
    assert_eq!(compose.take_request(), None);

    let mut compose = new_message();
    compose.edit(Field::Body, "Oops".to_owned());
    compose.edit(Field::Body, "  ".to_owned());
    let timer = compose.take_save_timer().unwrap();
    compose.save_timer_elapsed(timer);
    assert_eq!(compose.take_request(), None);
    assert!(compose.close());
    assert_eq!(compose.take_request(), None);
}

#[test]
fn send_queues_the_message_once_and_pressing_twice_does_not_send_twice() {
    let mut compose = new_message();
    compose.edit(Field::To, "zoe@example.com".to_owned());
    compose.edit(Field::Body, "Hello".to_owned());

    compose.send();
    compose.send();
    // Saved first, so that the send names its draft.
    let save = draft(compose.take_request().unwrap());
    assert!(!save.send);
    compose.send();
    compose.answered(saved("d@example.org"));
    let send = draft(compose.take_request().unwrap());
    assert!(send.send);
    assert_eq!(send.draft.as_deref(), Some("d@example.org"));
    compose.send();
    assert_eq!(compose.take_request(), None);
    assert_eq!(compose.state, State::Sending);
    compose.edit(Field::Body, "Changed while sending".to_owned());
    assert_eq!(compose.body, "Hello", "nothing changes while it is sent");

    compose.answered(Ok(Answer {
        protocol: PROTOCOL,
        answer: Some("draft".to_owned()),
        ok: true,
        outbox: Some(1),
        message_id: Some("d@example.org".to_owned()),
        ..Answer::default()
    }));
    assert!(compose.done());
    compose.send();
    assert_eq!(compose.take_request(), None);
}

#[test]
fn a_send_whose_answer_was_lost_asked_again_is_told_busy_and_counts_as_sent() {
    let mut compose = new_message();
    type_and_wait(&mut compose, Field::To, "zoe@example.com").unwrap();
    compose.answered(saved("d@example.org"));
    compose.send();
    compose.take_request().unwrap();
    compose.answered(Err("the connection was cut".to_owned()));
    assert_eq!(compose.state, State::Editing);
    assert!(compose.notice.as_deref().unwrap().contains("Not sent"));
    assert_eq!(compose.to, "zoe@example.com", "kept");

    compose.send();
    let again = draft(compose.take_request().unwrap());
    assert_eq!(again.draft.as_deref(), Some("d@example.org"));
    compose.answered(Ok(Answer::error(
        Some("draft"),
        "busy",
        "this draft is being sent",
    )));
    assert!(compose.done());
}

#[test]
fn send_without_a_recipient_asks_nothing_and_says_why() {
    let mut compose = new_message();
    compose.edit(Field::Body, "Hello".to_owned());
    compose.send();
    assert_eq!(compose.state, State::Editing);
    assert!(compose.notice.as_deref().unwrap().contains("no recipient"));
    assert_eq!(compose.take_request(), None);
}

#[test]
fn without_the_daemon_the_text_is_kept_and_the_window_says_so() {
    let mut compose = new_message();
    compose.edit(Field::Body, "Precious words".to_owned());
    assert!(!compose.close());
    compose.take_request().unwrap();
    compose.answered(Err("no daemon answers".to_owned()));

    assert!(!compose.done());
    assert_eq!(compose.body, "Precious words");
    let notice = compose.notice.clone().unwrap();
    assert!(notice.contains("no daemon answers"), "{notice}");
    assert!(notice.contains("Close again to discard"), "{notice}");
    // Asked to close again, the user chose to discard it.
    assert!(compose.close());
}

#[test]
fn attachments_are_added_and_removed() {
    let folder = tempfile::tempdir().unwrap();
    let report = folder.path().join("report.pdf");
    std::fs::write(&report, b"%PDF-1.7 report").unwrap();
    let photo = folder.path().join("photo.jpg");
    std::fs::write(&photo, [0xff, 0xd8, 0xff]).unwrap();
    let mut compose = new_message();

    compose.attach(load(vec![report, photo, folder.path().join("missing.txt")]));
    assert_eq!(
        compose
            .attachments
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>(),
        ["report.pdf", "photo.jpg"]
    );
    assert!(compose.notice.as_deref().unwrap().contains("missing.txt"));
    let timer = compose.take_save_timer().unwrap();
    compose.save_timer_elapsed(timer);
    let first = draft(compose.take_request().unwrap());
    assert_eq!(
        first.attachments,
        Some(vec![
            Attached {
                name: "report.pdf".to_owned(),
                data: b"%PDF-1.7 report".to_vec()
            },
            Attached {
                name: "photo.jpg".to_owned(),
                data: vec![0xff, 0xd8, 0xff]
            },
        ])
    );
    compose.answered(saved("d@example.org"));

    compose.remove_attachment(0);
    let timer = compose.take_save_timer().unwrap();
    compose.save_timer_elapsed(timer);
    let next = draft(compose.take_request().unwrap());
    assert_eq!(next.attachments.unwrap().len(), 1);
    assert_eq!(compose.attachments[0].name, "photo.jpg");
}

#[test]
fn a_file_over_the_cap_and_a_file_that_is_no_regular_file_are_not_attached() {
    let folder = tempfile::tempdir().unwrap();
    let small = folder.path().join("small.txt");
    std::fs::write(&small, "Small").unwrap();
    // A sparse file: it costs no disk, and reading it whole would cost the memory.
    let huge = folder.path().join("huge.bin");
    std::fs::File::create(&huge)
        .unwrap()
        .set_len(MAX_ATTACHMENT + 1)
        .unwrap();
    // Reading a FIFO waits for a writer that never comes: the window would freeze.
    let fifo = folder.path().join("pipe");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    let mut compose = new_message();

    compose.attach(load(vec![huge, fifo, small]));

    assert_eq!(
        compose
            .attachments
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>(),
        ["small.txt"]
    );
    let notice = compose.notice.clone().unwrap();
    assert!(notice.contains("huge.bin"), "{notice}");
    assert!(notice.contains("25 MiB"), "{notice}");
    assert!(notice.contains("pipe"), "{notice}");
    assert!(notice.contains("not a regular file"), "{notice}");
}

#[test]
fn a_file_exactly_at_the_cap_is_attached() {
    let folder = tempfile::tempdir().unwrap();
    let exact = folder.path().join("exact.bin");
    std::fs::File::create(&exact)
        .unwrap()
        .set_len(MAX_ATTACHMENT)
        .unwrap();
    let mut compose = new_message();
    compose.attach(load(vec![exact]));
    assert_eq!(compose.attachments.len(), 1);
    assert_eq!(compose.notice, None);
}

#[test]
fn a_draft_picked_up_again_shows_its_fields_and_saves_under_its_identifier() {
    let raw = b"X-Missive-Draft: 1\r\nBcc: hidden@example.net\r\nFrom: alice@example.org\r\n\
        To: \"Zoe, Z.\" <zoe@example.com>, carl@example.net\r\nSubject: Plans\r\n\
        Message-ID: <d@example.org>\r\n\r\nLine one\r\nLine two\r\n";
    let mut compose = Compose::draft(
        vec![ID.to_owned()],
        ID.to_owned(),
        "d@example.org".to_owned(),
        raw,
    )
    .unwrap();
    assert_eq!(
        compose.to,
        "\"Zoe, Z.\" <zoe@example.com>, carl@example.net"
    );
    assert_eq!(compose.bcc, "hidden@example.net");
    assert!(compose.show_bcc);
    assert_eq!(compose.subject, "Plans");
    assert_eq!(compose.body, "Line one\nLine two\n");
    // Nothing changed: closing saves nothing.
    assert!(compose.close());

    let mut compose = Compose::draft(
        vec![ID.to_owned()],
        ID.to_owned(),
        "d@example.org".to_owned(),
        raw,
    )
    .unwrap();
    let request = draft(type_and_wait(&mut compose, Field::Subject, "Plans, v2").unwrap());
    assert_eq!(request.draft.as_deref(), Some("d@example.org"));
    assert_eq!(
        request.to,
        Some(vec![
            "\"Zoe, Z.\" <zoe@example.com>".to_owned(),
            "carl@example.net".to_owned()
        ])
    );
}

#[test]
fn a_field_splits_on_commas_outside_quotes_and_angle_brackets() {
    assert_eq!(
        addresses(" a@example.org,\"Doe, Jo\" <jo@example.org>; ,b@example.org "),
        [
            "a@example.org",
            "\"Doe, Jo\" <jo@example.org>",
            "b@example.org"
        ]
    );
    assert!(addresses("  ").is_empty());
}

#[test]
fn the_account_is_chosen_only_until_the_draft_is_saved() {
    let mut compose = Compose::new(
        vec!["alice".to_owned(), "bob".to_owned()],
        "alice".to_owned(),
    );
    assert!(compose.can_pick_account());
    compose.pick_account("bob");
    assert_eq!(compose.account, "bob");
    compose.pick_account("nobody");
    assert_eq!(compose.account, "bob");
    type_and_wait(&mut compose, Field::Body, "x").unwrap();
    compose.answered(saved("d@example.org"));
    assert!(!compose.can_pick_account());
    compose.pick_account("alice");
    assert_eq!(compose.account, "bob");
}

#[tokio::test]
async fn the_window_started_on_a_draft_shows_it_and_one_not_in_the_copy_says_so() {
    let mail = Mail::new(mailbox()).await;
    let store = missive_core::store::Store::open(&mail.copy_path());
    let mut store = store.unwrap();
    let key = store.account(ID).unwrap();
    store
        .save_draft(
            key,
            "w@example.org",
            b"X-Missive-Draft: 1\r\nTo: zoe@example.com\r\nSubject: From a link\r\n\
              Message-ID: <w@example.org>\r\n\r\nHello\r\n",
        )
        .unwrap();

    let shown =
        missive_window::launch::compose(&mail.sources(), ID.to_owned(), "w@example.org".to_owned());
    assert_eq!(shown.subject, "From a link");
    assert_eq!(shown.to, "zoe@example.com");
    assert_eq!(shown.draft.as_deref(), Some("w@example.org"));
    assert!(shown.notice.is_none());

    let missing = missive_window::launch::compose(
        &mail.sources(),
        ID.to_owned(),
        "gone@example.org".to_owned(),
    );
    assert!(missing.notice.unwrap().contains("gone@example.org"));
    assert_eq!(missing.draft, None);
}

fn own_draft_of(header: &str) -> Vec<u8> {
    format!(
        "X-Missive-Draft: 1\r\n{header}\r\nTo: zoe@example.com\r\nSubject: Re: Plans\r\n\
         Message-ID: <d@example.org>\r\n\r\nHello\r\n"
    )
    .into_bytes()
}

fn picked_up(raw: &[u8]) -> Compose {
    Compose::draft(
        vec![ID.to_owned()],
        ID.to_owned(),
        "alice/3/1/5".to_owned(),
        raw,
    )
    .unwrap()
}

#[test]
fn a_reply_names_the_message_it_answers_in_every_save_not_only_in_the_send() {
    // What a reply from the reader starts with, as `answering` gives it.
    let mut reply = Compose::answering(
        vec![ID.to_owned()],
        ID.to_owned(),
        missive_core::reply::Answering {
            answers: Some("alice/3/1/5".to_owned()),
            ..Default::default()
        },
    );
    reply.edit(Field::Body, "Noon.".to_owned());
    let timer = reply.take_save_timer().unwrap();
    reply.save_timer_elapsed(timer);
    let save = draft(reply.take_request().unwrap());
    assert!(!save.send);
    assert_eq!(save.answers.as_deref(), Some("alice/3/1/5"));
    assert_eq!(save.forwards, None);
}

#[test]
fn a_draft_picked_up_again_remembers_the_message_it_answers_or_forwards() {
    let mut answering = picked_up(&own_draft_of("X-Missive-Answers: alice/3/1/5"));
    let save = draft(type_and_wait(&mut answering, Field::Body, "Noon.").unwrap());
    assert_eq!(save.answers.as_deref(), Some("alice/3/1/5"));
    assert_eq!(save.forwards, None);
    answering.answered(saved("d@example.org"));
    answering.send();
    let send = draft(answering.take_request().unwrap());
    assert!(send.send);
    assert_eq!(send.answers.as_deref(), Some("alice/3/1/5"));

    let mut forwarding = picked_up(&own_draft_of("X-Missive-Forwards: alice/3/1/6"));
    let save = draft(type_and_wait(&mut forwarding, Field::Body, "FYI").unwrap());
    assert_eq!(save.forwards.as_deref(), Some("alice/3/1/6"));
    assert_eq!(save.answers, None);
}

#[test]
fn a_draft_of_another_client_names_nothing_it_answers() {
    let raw = b"X-Missive-Answers: alice/3/1/5\r\nTo: zoe@example.com\r\nSubject: Re: Plans\r\n\
        Message-ID: <d@example.org>\r\n\r\nHello\r\n";
    let mut compose = picked_up(raw);
    let save = draft(type_and_wait(&mut compose, Field::Body, "Noon.").unwrap());
    assert_eq!((save.answers, save.forwards), (None, None));
}

#[test]
fn the_message_a_reply_answers_being_gone_the_link_is_dropped_and_the_text_still_goes() {
    let mut compose = picked_up(&own_draft_of("X-Missive-Answers: alice/3/1/5"));
    compose.edit(Field::Body, "Noon.".to_owned());
    compose.send();
    let first = draft(compose.take_request().unwrap());
    assert_eq!(first.answers.as_deref(), Some("alice/3/1/5"));
    assert!(first.send);

    // Moved or deleted since: its identifier names nothing now.
    compose.answered(Ok(Answer::error(
        Some("draft"),
        "original-gone",
        "alice/3/1/5: no such message in the local copy",
    )));
    assert_eq!(compose.state, State::Sending, "the send goes on");
    let notice = compose.notice.clone().unwrap();
    assert!(notice.contains("not be marked"), "{notice}");
    let again = draft(compose.take_request().expect("asked again at once"));
    assert!(again.send);
    assert_eq!(again.answers, None);
    assert_eq!(again.body.as_deref(), Some("Noon."));

    compose.answered(saved("d@example.org"));
    assert!(compose.done());
}

#[test]
fn a_save_refused_for_the_message_it_answers_is_asked_again_without_the_link() {
    let mut compose = picked_up(&own_draft_of("X-Missive-Answers: alice/3/1/5"));
    let first = draft(type_and_wait(&mut compose, Field::Body, "Noon.").unwrap());
    assert_eq!(first.answers.as_deref(), Some("alice/3/1/5"));
    compose.answered(Ok(Answer::error(
        Some("draft"),
        "original-gone",
        "alice/3/1/5: no such message in the local copy",
    )));
    let again = draft(compose.take_request().expect("saved again at once"));
    assert_eq!(again.answers, None);
    compose.answered(saved("d@example.org"));
    // The notice stays until the next change.
    assert!(compose.notice.as_deref().unwrap().contains("not be marked"));
    assert_eq!(compose.take_request(), None);
}

#[test]
fn a_draft_that_names_no_message_of_its_own_stays_refused() {
    // The error is about the draft, not about the message it answers: no link is dropped.
    let mut compose = picked_up(&own_draft_of("X-Missive-Answers: alice/3/1/5"));
    type_and_wait(&mut compose, Field::Body, "Noon.").unwrap();
    compose.answered(Ok(Answer::error(
        Some("draft"),
        "no-such-message",
        "d@example.org: no such draft in the local copy",
    )));
    assert!(compose.notice.as_deref().unwrap().contains("Not saved"));
    assert_eq!(compose.take_request(), None);
}

#[test]
fn edits_made_after_a_lost_send_are_kept_as_a_new_draft_when_the_daemon_says_busy() {
    let mut compose = new_message();
    type_and_wait(&mut compose, Field::To, "zoe@example.com").unwrap();
    compose.answered(saved("d@example.org"));
    compose.edit(Field::Body, "First words".to_owned());
    compose.send();
    draft(compose.take_request().unwrap());
    // The answer was lost: the message may be in the outbox as it was.
    compose.answered(Err("the connection was cut".to_owned()));
    compose.edit(Field::Body, "First words, and more".to_owned());

    compose.send();
    let again = draft(compose.take_request().unwrap());
    assert_eq!(again.draft.as_deref(), Some("d@example.org"));
    compose.answered(Ok(Answer::error(
        Some("draft"),
        "busy",
        "this draft is being sent",
    )));

    assert!(
        !compose.done(),
        "what was typed since is not in the message queued"
    );
    assert_eq!(compose.state, State::Editing);
    assert_eq!(compose.body, "First words, and more");
    let notice = compose.notice.clone().unwrap();
    assert!(notice.contains("new draft"), "{notice}");
    let save = draft(compose.take_request().expect("saved as a new draft"));
    assert_eq!(save.draft, None);
    assert!(!save.send);
    assert_eq!(save.body.as_deref(), Some("First words, and more"));
    assert_eq!(save.answers, None);
    compose.answered(saved("n@example.org"));
    assert_eq!(compose.draft.as_deref(), Some("n@example.org"));
    assert!(
        compose.notice.as_deref().unwrap().contains("new draft"),
        "kept"
    );
}

#[test]
fn a_draft_already_sent_says_where_its_changes_were_saved() {
    let mut compose = picked_up(&own_draft_of("X-Missive-Answers: alice/3/1/5"));
    compose.edit(Field::Body, "Late words".to_owned());
    compose.send();
    draft(compose.take_request().unwrap());

    let mut answer = Answer::error(
        Some("draft"),
        "already-sent",
        "already sent; your changes are saved as a new draft n@example.org",
    );
    answer.message_id = Some("n@example.org".to_owned());
    compose.answered(Ok(answer));

    assert_eq!(compose.state, State::Editing, "nothing was sent");
    assert_eq!(compose.draft.as_deref(), Some("n@example.org"));
    assert!(!compose.needs_save(), "saved as that draft");
    let notice = compose.notice.clone().unwrap();
    assert!(notice.contains("n@example.org"), "{notice}");
    assert_eq!(compose.take_request(), None);
    // Sent again, it names the new draft.
    compose.send();
    let send = draft(compose.take_request().unwrap());
    assert_eq!(send.draft.as_deref(), Some("n@example.org"));
}

#[test]
fn a_draft_already_sent_with_no_change_to_save_stays_refused_with_its_text() {
    let mut compose = picked_up(&own_draft_of("X-Missive-Answers: alice/3/1/5"));
    compose.send();
    draft(compose.take_request().unwrap());
    compose.answered(Ok(Answer::error(
        Some("draft"),
        "already-sent",
        "this draft was already sent",
    )));
    assert_eq!(compose.state, State::Editing);
    assert_eq!(compose.draft.as_deref(), Some("alice/3/1/5"));
    assert!(compose.notice.as_deref().unwrap().contains("already"));
}

#[test]
fn a_text_that_drops_the_drafts_html_part_says_so_in_the_window() {
    let mut compose = picked_up(&own_draft_of("X-Missive-Foo: 1"));
    type_and_wait(&mut compose, Field::Body, "Another text").unwrap();
    let mut answer = saved("d@example.org").unwrap();
    answer.html_dropped = true;
    compose.answered(Ok(answer));
    let notice = compose.notice.clone().unwrap();
    assert!(notice.contains("HTML"), "{notice}");

    // The next save, which drops nothing, clears it.
    type_and_wait(&mut compose, Field::Body, "Another text, again").unwrap();
    compose.answered(saved("d@example.org"));
    assert_eq!(compose.notice, None);
}

const ORIGINAL: &[u8] = b"From: Zoe <zoe@example.com>\r\n\
    To: Alice <alice@example.org>, Bob <bob@example.net>\r\n\
    Subject: Lunch?\r\nMessage-ID: <lunch@example.com>\r\n\
    Date: Sat, 17 Oct 2026 07:30:00 +0200\r\n\r\nAre you free?\r\n";

fn reply_all_from(account: &str) -> Compose {
    let accounts = vec!["alice".to_owned(), "bob".to_owned()];
    let own = if account == "alice" {
        "alice@example.org"
    } else {
        "bob@example.net"
    };
    let answering = missive_core::reply::answering(
        "alice/1/1/1",
        ORIGINAL,
        Some("Lunch?"),
        Some(missive_core::reply::Mode::ReplyAll),
        own,
    )
    .unwrap();
    Compose::answering(accounts, account.to_owned(), answering).with_recipients_of(
        ORIGINAL,
        missive_core::reply::Mode::ReplyAll,
        vec![
            ("alice".to_owned(), "alice@example.org".to_owned()),
            ("bob".to_owned(), "bob@example.net".to_owned()),
        ],
    )
}

#[test]
fn switching_the_sending_account_of_a_reply_all_recomputes_the_recipients() {
    let mut compose = reply_all_from("alice");
    assert_eq!(
        compose.to,
        "\"Zoe\" <zoe@example.com>, \"Bob\" <bob@example.net>"
    );
    assert_eq!(compose.cc, "");

    compose.pick_account("bob");

    // Bob's own address leaves, and Alice's, which the reply left out, comes in.
    assert_eq!(
        compose.to,
        "\"Zoe\" <zoe@example.com>, \"Alice\" <alice@example.org>"
    );
    let timer = compose.take_save_timer().unwrap();
    compose.save_timer_elapsed(timer);
    let save = draft(compose.take_request().unwrap());
    assert_eq!(save.account, "bob");
    assert_eq!(
        save.to,
        Some(vec![
            "\"Zoe\" <zoe@example.com>".to_owned(),
            "\"Alice\" <alice@example.org>".to_owned()
        ])
    );
}

#[test]
fn recipients_the_user_changed_are_left_as_written_when_the_account_changes() {
    let mut compose = reply_all_from("alice");
    compose.edit(Field::Cc, "carl@example.net".to_owned());

    compose.pick_account("bob");

    assert_eq!(
        compose.to,
        "\"Zoe\" <zoe@example.com>, \"Bob\" <bob@example.net>"
    );
    assert_eq!(compose.cc, "carl@example.net");
    let notice = compose.notice.clone().unwrap();
    assert!(notice.contains("bob@example.net"), "{notice}");
}

#[test]
fn a_message_that_answers_nothing_keeps_its_recipients_when_the_account_changes() {
    let mut compose = Compose::new(
        vec!["alice".to_owned(), "bob".to_owned()],
        "alice".to_owned(),
    );
    compose.edit(Field::To, "bob@example.net".to_owned());
    compose.pick_account("bob");
    assert_eq!(compose.to, "bob@example.net");
    assert_eq!(compose.notice, None);
}

#[tokio::test]
async fn a_reply_all_from_the_reader_works_the_recipients_out_again_for_the_account_chosen() {
    let copied = "Cc: Alice <alice@example.org>, Bob <bob@example.net>\r\n";
    let mail = Mail::new(
        missive_core::fake::FakeServer::builder()
            .user(common::USER, common::PASSWORD)
            .folder(
                missive_core::fake::Folder::new("INBOX").message(common::plan(6, copied, "Lunch?")),
            ),
    )
    .await;
    // A second account, which reaches no server: the window only reads its address.
    let store = missive_core::account::AccountStore::new(mail.home.path().join("config/missive"));
    let mut bob = store.get(ID).unwrap().unwrap();
    bob.address = "bob@example.net".to_owned();
    bob.user = bob.address.clone();
    store.save("bob", &bob, None).unwrap();
    let mut model = mail.model();
    open(&mut model, "Plan 6");

    let mut compose = model.compose(Start::ReplyAll).unwrap();
    assert!(compose.accounts.contains(&"bob".to_owned()));
    assert_eq!(compose.cc, "\"Bob\" <bob@example.net>");

    compose.pick_account("bob");

    assert_eq!(compose.cc, "\"Alice\" <alice@example.org>");
}
