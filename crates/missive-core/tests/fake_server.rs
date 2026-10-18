//! The fake IMAP and SMTP server, driven by real clients: `async-imap` over `tokio-rustls` for
//! IMAP, a few raw lines over `tokio-rustls` for SMTP.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_imap::extensions::idle::IdleResponse;
use async_imap::imap_proto::{RequestId, Response, Status};
use async_imap::types::{Flag, NameAttribute, UnsolicitedResponse};
use async_imap::{Client, Session};
use futures_util::TryStreamExt;
use missive_core::fake::{FakeServer, Folder, Message, SmtpMessage, SpecialUse};
use missive_core::tls::pinned_client_config;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, RootCertStore};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

const USER: &str = "alice@example.org";
const PASSWORD: &str = "correct horse battery staple";

const HELLO: &str = "From: Bob <bob@example.com>\r\n\
To: Alice <alice@example.org>\r\n\
Subject: Hello\r\n\
Message-ID: <hello@example.com>\r\n\
\r\n\
Hello Alice.\r\n";

const AGAIN: &str = "From: Bob <bob@example.com>\r\n\
To: Alice <alice@example.org>\r\n\
Subject: Again\r\n\
Message-ID: <again@example.com>\r\n\
\r\n\
Hello again.\r\n";

type Imap = Session<TlsStream<TcpStream>>;

async fn server() -> FakeServer {
    FakeServer::builder()
        .user(USER, PASSWORD)
        .folder(Folder::new("INBOX").message(Message::new(HELLO)))
        .folder(Folder::new("Sent").special_use(SpecialUse::Sent))
        .folder(Folder::new("Trash").special_use(SpecialUse::Trash))
        .start()
        .await
}

/// A client that trusts exactly the fake's certificate, read back from its exported PEM.
fn pinned(server: &FakeServer) -> Arc<ClientConfig> {
    let certificate = CertificateDer::from_pem_slice(server.certificate_pem().as_bytes())
        .expect("the fake exports one PEM certificate");
    Arc::new(pinned_client_config(certificate))
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// A standard webpki client with the given trust anchors.
fn webpki(roots: RootCertStore) -> Arc<ClientConfig> {
    Arc::new(
        ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

async fn tls(config: Arc<ClientConfig>, tcp: TcpStream) -> std::io::Result<TlsStream<TcpStream>> {
    let name = ServerName::try_from("localhost").unwrap();
    TlsConnector::from(config).connect(name, tcp).await
}

async fn imap_tls(address: SocketAddr, config: Arc<ClientConfig>) -> Client<TlsStream<TcpStream>> {
    let tcp = TcpStream::connect(address).await.unwrap();
    let stream = tls(config, tcp).await.expect("TLS handshake");
    let mut client = Client::new(stream);
    client.read_response().await.unwrap().expect("greeting");
    client
}

async fn imap_starttls(
    address: SocketAddr,
    config: Arc<ClientConfig>,
) -> Client<TlsStream<TcpStream>> {
    let tcp = TcpStream::connect(address).await.unwrap();
    let mut client = Client::new(tcp);
    client.read_response().await.unwrap().expect("greeting");
    client
        .run_command_and_check_ok("STARTTLS", None)
        .await
        .unwrap();
    let stream = tls(config, client.into_inner())
        .await
        .expect("TLS handshake");
    Client::new(stream)
}

async fn login(client: Client<TlsStream<TcpStream>>) -> Imap {
    client
        .login(USER, PASSWORD)
        .await
        .map_err(|(e, _)| e)
        .unwrap()
}

/// Lists the folders and fetches the inbox's message: what both login scenarios check.
async fn list_and_fetch(session: &mut Imap) {
    let names: Vec<_> = session
        .list(Some(""), Some("*"))
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    let folders: Vec<_> = names.iter().map(|n| n.name().to_owned()).collect();
    assert_eq!(folders, ["INBOX", "Sent", "Trash"]);
    let sent = names.iter().find(|n| n.name() == "Sent").unwrap();
    assert!(sent.attributes().contains(&NameAttribute::Sent));

    let mailbox = session.examine("INBOX").await.unwrap();
    assert_eq!(mailbox.exists, 1);
    let fetches: Vec<_> = session
        .uid_fetch("1:*", "(UID FLAGS BODY.PEEK[])")
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    assert_eq!(fetches.len(), 1);
    assert_eq!(fetches[0].uid, Some(1));
    assert_eq!(fetches[0].body(), Some(HELLO.as_bytes()));
}

#[tokio::test]
async fn logs_in_over_starttls_lists_folders_and_fetches_a_message() {
    let server = server().await;
    let client = imap_starttls(server.imap_starttls_addr(), pinned(&server)).await;
    let mut session = login(client).await;
    list_and_fetch(&mut session).await;
    session.logout().await.unwrap();
}

#[tokio::test]
async fn logs_in_over_implicit_tls_lists_folders_and_fetches_a_message() {
    let server = server().await;
    let client = imap_tls(server.imap_tls_addr(), pinned(&server)).await;
    let mut session = login(client).await;
    list_and_fetch(&mut session).await;
    session.logout().await.unwrap();
}

#[tokio::test]
async fn a_client_trusting_only_the_exported_certificate_connects() {
    let server = server().await;
    let tcp = TcpStream::connect(server.imap_tls_addr()).await.unwrap();
    assert!(tls(pinned(&server), tcp).await.is_ok());
}

#[tokio::test]
async fn a_client_trusting_nothing_is_refused() {
    let server = server().await;
    let tcp = TcpStream::connect(server.imap_tls_addr()).await.unwrap();
    let refused = tls(webpki(RootCertStore::empty()), tcp).await;
    assert!(refused.is_err());
}

#[tokio::test]
async fn a_pinned_client_refuses_another_certificate() {
    let server = server().await;
    let other = FakeServer::builder().user(USER, PASSWORD).start().await;
    let tcp = TcpStream::connect(other.imap_tls_addr()).await.unwrap();
    let refused = tls(pinned(&server), tcp).await;
    assert!(refused.is_err());
}

/// Why a pinned certificate is needed at all: with the certificate as its trust anchor, a standard
/// webpki client still refuses a certificate marked as a CA, like Proton Mail Bridge's.
#[tokio::test]
async fn the_certificate_is_marked_as_a_ca_and_refused_by_plain_webpki() {
    let server = server().await;
    let mut roots = RootCertStore::empty();
    roots.add(server.certificate_der()).unwrap();
    let tcp = TcpStream::connect(server.imap_tls_addr()).await.unwrap();
    let error = tls(webpki(roots), tcp).await.unwrap_err();
    assert!(
        error.to_string().contains("CaUsedAsEndEntity"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn login_before_starttls_is_refused() {
    let server = server().await;
    let tcp = TcpStream::connect(server.imap_starttls_addr())
        .await
        .unwrap();
    let mut client = Client::new(tcp);
    client.read_response().await.unwrap();
    assert!(client.login(USER, PASSWORD).await.is_err());
}

#[tokio::test]
async fn a_wrong_password_is_refused() {
    let server = server().await;
    let client = imap_tls(server.imap_tls_addr(), pinned(&server)).await;
    assert!(client.login(USER, "wrong").await.is_err());
}

#[tokio::test]
async fn the_log_records_what_the_client_sent_without_the_password() {
    let server = server().await;
    let mut session = login(imap_tls(server.imap_tls_addr(), pinned(&server)).await).await;
    session.examine("INBOX").await.unwrap();
    let _: Vec<_> = session
        .uid_fetch("1", "BODY.PEEK[]")
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    session.logout().await.unwrap();

    let log = server.imap_log();
    assert!(log.iter().all(|line| !line.contains(PASSWORD)), "{log:?}");
    assert!(log.iter().any(|line| line.starts_with("LOGIN ")), "{log:?}");
    assert!(log.contains(&"EXAMINE \"INBOX\"".to_owned()), "{log:?}");
    assert!(
        log.contains(&"UID FETCH 1 BODY.PEEK[]".to_owned()),
        "{log:?}"
    );
}

#[tokio::test]
async fn fetching_a_body_marks_it_seen_only_in_a_selected_folder() {
    let server = server().await;
    let mut session = login(imap_tls(server.imap_tls_addr(), pinned(&server)).await).await;

    session.examine("INBOX").await.unwrap();
    let _: Vec<_> = session
        .uid_fetch("1", "BODY[]")
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    assert_eq!(server.flags("INBOX", 1), Some(vec![]));

    session.select("INBOX").await.unwrap();
    let _: Vec<_> = session
        .uid_fetch("1", "BODY.PEEK[]")
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    assert_eq!(server.flags("INBOX", 1), Some(vec![]));
    let _: Vec<_> = session
        .uid_fetch("1", "BODY[]")
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    assert_eq!(server.flags("INBOX", 1), Some(vec!["\\Seen".to_owned()]));
}

#[tokio::test]
async fn uid_search_finds_unseen_messages() {
    let server = server().await;
    let seen = server.deliver("INBOX", Message::new(AGAIN).flag("\\Seen"));
    let mut session = login(imap_tls(server.imap_tls_addr(), pinned(&server)).await).await;
    session.examine("INBOX").await.unwrap();
    let unseen = session.uid_search("UNSEEN").await.unwrap();
    assert_eq!(unseen.into_iter().collect::<Vec<_>>(), [1]);
    let all = session.uid_search("ALL").await.unwrap();
    assert!(all.contains(&seen) && all.len() == 2);
}

#[tokio::test]
async fn a_new_message_reaches_a_client_idling_on_the_folder() {
    let server = server().await;
    let mut session = login(imap_tls(server.imap_tls_addr(), pinned(&server)).await).await;
    session.examine("INBOX").await.unwrap();

    let mut idle = session.idle();
    idle.init().await.unwrap();
    let (wait, _stop) = idle.wait_with_timeout(Duration::from_secs(10));
    let delivered = async {
        // The client is idling once its IDLE is logged.
        while !server.imap_log().contains(&"IDLE".to_owned()) {
            tokio::task::yield_now().await;
        }
        server.deliver("INBOX", Message::new(AGAIN))
    };
    let (response, uid) = tokio::join!(wait, delivered);
    match response.unwrap() {
        IdleResponse::NewData(data) => {
            assert!(format!("{:?}", data.parsed()).contains("Exists(2)"));
        }
        other => panic!("expected new data, got {other:?}"),
    }
    assert_eq!(uid, 2);

    let mut session = idle.done().await.unwrap();
    let fetches: Vec<_> = session
        .uid_fetch("2", "BODY.PEEK[]")
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    assert_eq!(fetches[0].body(), Some(AGAIN.as_bytes()));
}

#[tokio::test]
async fn a_flag_change_and_an_expunge_reach_a_client_on_noop() {
    let server = server().await;
    server.deliver("INBOX", Message::new(AGAIN));
    let mut session = login(imap_tls(server.imap_tls_addr(), pinned(&server)).await).await;
    session.examine("INBOX").await.unwrap();

    server.set_flags("INBOX", 2, &["\\Seen", "\\Flagged"]);
    server.expunge("INBOX", 1);
    session.noop().await.unwrap();

    let mut updates = Vec::new();
    while let Ok(update) = session.unsolicited_responses.try_recv() {
        updates.push(update);
    }
    assert!(
        updates
            .iter()
            .any(|u| matches!(u, UnsolicitedResponse::Expunge(1))),
        "{updates:?}"
    );
    assert!(
        updates
            .iter()
            .any(|u| matches!(u, UnsolicitedResponse::Other(data)
            if format!("{:?}", data.parsed()).contains("Flagged"))),
        "{updates:?}"
    );

    // The second message is now the first.
    let fetches: Vec<_> = session
        .fetch("1", "(UID FLAGS)")
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    assert_eq!(fetches[0].uid, Some(2));
    let flags: Vec<_> = fetches[0].flags().collect();
    assert_eq!(flags, [Flag::Seen, Flag::Flagged]);
}

#[tokio::test]
async fn a_uidvalidity_bump_renumbers_the_folder() {
    let server = server().await;
    server.deliver("INBOX", Message::new(AGAIN));
    server.expunge("INBOX", 1);
    let mut session = login(imap_tls(server.imap_tls_addr(), pinned(&server)).await).await;
    let before = session.examine("INBOX").await.unwrap();

    let validity = server.bump_uidvalidity("INBOX");
    // The selected folder changed under the client: the server closes the connection.
    assert!(session.noop().await.is_err());

    let mut session = login(imap_tls(server.imap_tls_addr(), pinned(&server)).await).await;
    let after = session.examine("INBOX").await.unwrap();
    assert_eq!(after.uid_validity, Some(validity));
    assert_ne!(after.uid_validity, before.uid_validity);
    let uids = session.uid_search("ALL").await.unwrap();
    assert_eq!(uids.into_iter().collect::<Vec<_>>(), [1]);
}

#[tokio::test]
async fn condstore_reports_the_changes_since_a_modseq() {
    let server = server().await;
    server.deliver("INBOX", Message::new(AGAIN));
    let mut session = login(imap_tls(server.imap_tls_addr(), pinned(&server)).await).await;
    let mailbox = session.select_condstore("INBOX").await.unwrap();
    let since = mailbox.highest_modseq.expect("HIGHESTMODSEQ");

    server.set_flags("INBOX", 2, &["\\Flagged"]);
    let changed: Vec<_> = session
        .uid_fetch("1:*", format!("(FLAGS) (CHANGEDSINCE {since})"))
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    assert_eq!(changed.len(), 1);
    assert_eq!(changed[0].uid, Some(2));
    assert!(changed[0].modseq.unwrap() > since);
}

#[tokio::test]
async fn a_folder_without_special_use_has_no_role() {
    let server = FakeServer::builder()
        .user(USER, PASSWORD)
        .folder(Folder::new("Projects/Missive"))
        .start()
        .await;
    let mut session = login(imap_tls(server.imap_tls_addr(), pinned(&server)).await).await;
    let names: Vec<_> = session
        .list(Some(""), Some("*"))
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    // INBOX always exists, like on a real server.
    let folders: Vec<_> = names.iter().map(|n| n.name().to_owned()).collect();
    assert_eq!(folders, ["INBOX", "Projects/Missive"]);
    assert_eq!(names[1].delimiter(), Some("/"));
    assert!(names[1].attributes().is_empty());
}

#[tokio::test]
async fn a_fetch_delay_holds_each_fetch_answer_back() {
    let server = FakeServer::builder()
        .user(USER, PASSWORD)
        .folder(Folder::new("INBOX").message(Message::new(HELLO)))
        .fetch_delay(Duration::from_millis(300))
        .start()
        .await;
    let mut session = login(imap_tls(server.imap_tls_addr(), pinned(&server)).await).await;
    session.examine("INBOX").await.unwrap();

    let started = std::time::Instant::now();
    let fetches: Vec<_> = session
        .uid_fetch("1:*", "(FLAGS)")
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();

    assert_eq!(fetches.len(), 1);
    assert!(started.elapsed() >= Duration::from_millis(300));
    // Other commands are not held back.
    let started = std::time::Instant::now();
    session.noop().await.unwrap();
    assert!(started.elapsed() < Duration::from_millis(300));
}

/// Messages for the misbehaving server's tests.
async fn three_messages() -> FakeServer {
    FakeServer::builder()
        .user(USER, PASSWORD)
        .folder(
            Folder::new("INBOX")
                .message(Message::new(HELLO))
                .message(Message::new(AGAIN))
                .message(Message::new(HELLO)),
        )
        .start()
        .await
}

#[tokio::test]
async fn a_refused_command_is_answered_no_and_nothing_else_until_healed() {
    let server = three_messages().await;
    let mut session = login(imap_tls(server.imap_tls_addr(), pinned(&server)).await).await;
    session.examine("INBOX").await.unwrap();
    server.refuse("uid search");

    let id = session.run_command("UID SEARCH ALL").await.unwrap();
    let answer = session.read_response().await.unwrap().unwrap();

    match answer.parsed() {
        Response::Done { tag, status, .. } => {
            assert_eq!(tag, &id);
            assert_eq!(*status, Status::No);
        }
        other => panic!("not a tagged NO: {other:?}"),
    }
    assert_eq!(server.imap_log().last().unwrap(), "UID SEARCH ALL");
    // Other commands are answered as usual.
    session.noop().await.unwrap();
    server.heal();
    session
        .run_command_and_check_ok("UID SEARCH ALL")
        .await
        .unwrap();
}

#[tokio::test]
async fn a_cut_answer_stops_after_its_first_lines_with_bye_and_a_clean_close() {
    let server = three_messages().await;
    let mut session = login(imap_tls(server.imap_tls_addr(), pinned(&server)).await).await;
    session.examine("INBOX").await.unwrap();
    server.cut("UID FETCH 1:*", 2);

    session.run_command("UID FETCH 1:* (FLAGS)").await.unwrap();
    let mut answers = Vec::new();
    while let Some(answer) = session.read_response().await.unwrap() {
        answers.push(answer);
    }

    assert_eq!(answers.len(), 3, "{answers:?}");
    assert!(matches!(answers[0].parsed(), Response::Fetch(1, _)));
    assert!(matches!(answers[1].parsed(), Response::Fetch(2, _)));
    assert!(matches!(
        answers[2].parsed(),
        Response::Data {
            status: Status::Bye,
            ..
        }
    ));
}

#[tokio::test]
async fn a_cut_set_by_the_builder_applies_to_every_connection() {
    let server = FakeServer::builder()
        .user(USER, PASSWORD)
        .cut("EXAMINE", 0)
        .start()
        .await;
    for _ in 0..2 {
        let mut session = login(imap_tls(server.imap_tls_addr(), pinned(&server)).await).await;
        session.run_command("EXAMINE INBOX").await.unwrap();
        let bye = session.read_response().await.unwrap().unwrap();
        assert!(matches!(
            bye.parsed(),
            Response::Data {
                status: Status::Bye,
                ..
            }
        ));
        assert!(session.read_response().await.unwrap().is_none());
    }
}

/// Reads answers up to the tagged one: the untagged ones, and whether the command ended `OK`
/// (else `NO`).
async fn answers_until_done(session: &mut Imap, id: &RequestId) -> (Vec<String>, bool) {
    let mut untagged = Vec::new();
    loop {
        let answer = session.read_response().await.unwrap().unwrap();
        match answer.parsed() {
            Response::Done { tag, status, .. } if tag == id => {
                return (untagged, *status == Status::Ok);
            }
            other => untagged.push(format!("{other:?}")),
        }
    }
}

#[tokio::test]
async fn a_message_refused_by_uid_is_left_out_of_every_fetch_that_holds_it_and_the_fetch_ends_no() {
    let server = three_messages().await;
    let mut session = login(imap_tls(server.imap_tls_addr(), pinned(&server)).await).await;
    session.examine("INBOX").await.unwrap();
    server.refuse_uid(2);

    // The others come, as with Dovecot's imap_fetch_failure = no-after; then the NO.
    let id = session.run_command("UID FETCH 1:3 (FLAGS)").await.unwrap();
    let (untagged, status) = answers_until_done(&mut session, &id).await;
    assert!(!status);
    assert_eq!(untagged.len(), 2, "{untagged:?}");
    assert!(untagged[0].contains("Uid(1)") && untagged[1].contains("Uid(3)"));
    // A set that does not hold it is answered as usual, and so is a sequence number.
    let id = session.run_command("UID FETCH 1,3 (FLAGS)").await.unwrap();
    assert!(answers_until_done(&mut session, &id).await.1);
    let id = session.run_command("UID FETCH 2 (FLAGS)").await.unwrap();
    let (untagged, status) = answers_until_done(&mut session, &id).await;
    assert_eq!((untagged.len(), status), (0, false));

    server.heal();
    let id = session.run_command("UID FETCH 1:3 (FLAGS)").await.unwrap();
    let (untagged, status) = answers_until_done(&mut session, &id).await;
    assert_eq!((untagged.len(), status), (3, true));
}

#[tokio::test]
async fn a_truncated_answer_keeps_its_first_lines_and_still_ends_with_its_tagged_ok() {
    let server = three_messages().await;
    let mut session = login(imap_tls(server.imap_tls_addr(), pinned(&server)).await).await;
    server.truncate("examine", 3);

    let id = session.run_command("EXAMINE \"INBOX\"").await.unwrap();
    let (untagged, status) = answers_until_done(&mut session, &id).await;

    // FLAGS, PERMANENTFLAGS, UIDVALIDITY, and no EXISTS: yet the command is said done.
    assert!(status);
    assert_eq!(untagged.len(), 3, "{untagged:?}");
    assert!(!untagged.iter().any(|a| a.contains("Exists")));
    server.heal();
    let id = session.run_command("EXAMINE \"INBOX\"").await.unwrap();
    assert!(answers_until_done(&mut session, &id).await.0.len() > 3);
}

#[tokio::test]
async fn idle_keepalives_come_at_their_interval() {
    let server = FakeServer::builder()
        .user(USER, PASSWORD)
        .idle_keepalive(Duration::from_millis(100))
        .start()
        .await;
    let mut session = login(imap_tls(server.imap_tls_addr(), pinned(&server)).await).await;
    session.examine("INBOX").await.unwrap();

    session.run_command("IDLE").await.unwrap();
    let started = std::time::Instant::now();
    let mut answers = Vec::new();
    for _ in 0..3 {
        answers.push(session.read_response().await.unwrap().unwrap());
    }

    assert!(matches!(answers[0].parsed(), Response::Continue { .. }));
    for keepalive in &answers[1..] {
        match keepalive.parsed() {
            Response::Data {
                status: Status::Ok,
                outcome,
            } => assert_eq!(outcome.information.as_deref(), Some("Still here")),
            other => panic!("not a keepalive: {other:?}"),
        }
    }
    assert!(started.elapsed() >= Duration::from_millis(200));
}

/// A minimal SMTP client: SMTP is line based, and lettre cannot take a pinned rustls config.
struct Smtp {
    stream: BufReader<TlsStream<TcpStream>>,
}

impl Smtp {
    async fn reply(stream: &mut (impl AsyncBufReadExt + Unpin)) -> String {
        let mut reply = String::new();
        loop {
            let mut line = String::new();
            stream.read_line(&mut line).await.unwrap();
            reply.push_str(&line);
            if line.as_bytes().get(3) != Some(&b'-') {
                return reply;
            }
        }
    }

    async fn implicit(server: &FakeServer) -> Smtp {
        let tcp = TcpStream::connect(server.smtp_tls_addr()).await.unwrap();
        let stream = tls(pinned(server), tcp).await.unwrap();
        let mut smtp = Smtp {
            stream: BufReader::new(stream),
        };
        assert!(Self::reply(&mut smtp.stream).await.starts_with("220"));
        smtp
    }

    async fn starttls(server: &FakeServer) -> Smtp {
        let tcp = TcpStream::connect(server.smtp_starttls_addr())
            .await
            .unwrap();
        let mut plain = BufReader::new(tcp);
        assert!(Self::reply(&mut plain).await.starts_with("220"));
        plain
            .write_all(b"EHLO client.example.org\r\n")
            .await
            .unwrap();
        let ehlo = Self::reply(&mut plain).await;
        assert!(
            ehlo.contains("STARTTLS") && !ehlo.contains("AUTH"),
            "{ehlo}"
        );
        plain.write_all(b"STARTTLS\r\n").await.unwrap();
        assert!(Self::reply(&mut plain).await.starts_with("220"));
        let stream = tls(pinned(server), plain.into_inner()).await.unwrap();
        Smtp {
            stream: BufReader::new(stream),
        }
    }

    async fn send(&mut self, line: &str) -> String {
        self.stream
            .write_all(format!("{line}\r\n").as_bytes())
            .await
            .unwrap();
        Self::reply(&mut self.stream).await
    }

    async fn deliver(&mut self, data: &str) {
        assert!(
            self.send("MAIL FROM:<alice@example.org>")
                .await
                .starts_with("250")
        );
        assert!(
            self.send("RCPT TO:<bob@example.com>")
                .await
                .starts_with("250")
        );
        assert!(
            self.send("RCPT TO:<carol@example.com>")
                .await
                .starts_with("250")
        );
        assert!(self.send("DATA").await.starts_with("354"));
        let reply = self.send(&format!("{data}.")).await;
        assert!(reply.starts_with("250"), "{reply}");
    }
}

fn base64(text: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(text)
}

const OUTGOING: &str = "From: Alice <alice@example.org>\r\n\
To: Bob <bob@example.com>\r\n\
Subject: Lunch\r\n\
\r\n\
..a line that starts with a dot\r\n";

fn accepted() -> SmtpMessage {
    SmtpMessage {
        from: "alice@example.org".to_owned(),
        to: vec!["bob@example.com".to_owned(), "carol@example.com".to_owned()],
        data: OUTGOING.replace("\r\n..", "\r\n.").into_bytes(),
    }
}

#[tokio::test]
async fn smtp_accepts_a_message_over_starttls_with_auth_plain() {
    let server = server().await;
    let mut smtp = Smtp::starttls(&server).await;
    let ehlo = smtp.send("EHLO client.example.org").await;
    assert!(ehlo.contains("AUTH PLAIN LOGIN"), "{ehlo}");
    let auth = smtp
        .send(&format!(
            "AUTH PLAIN {}",
            base64(&format!("\0{USER}\0{PASSWORD}"))
        ))
        .await;
    assert!(auth.starts_with("235"), "{auth}");
    smtp.deliver(OUTGOING).await;
    assert!(smtp.send("QUIT").await.starts_with("221"));
    assert_eq!(server.smtp_messages(), [accepted()]);
}

#[tokio::test]
async fn smtp_accepts_a_message_over_implicit_tls_with_auth_login() {
    let server = server().await;
    let mut smtp = Smtp::implicit(&server).await;
    smtp.send("EHLO client.example.org").await;
    assert!(smtp.send("AUTH LOGIN").await.starts_with("334"));
    assert!(smtp.send(&base64(USER)).await.starts_with("334"));
    assert!(smtp.send(&base64(PASSWORD)).await.starts_with("235"));
    smtp.deliver(OUTGOING).await;
    assert_eq!(server.smtp_messages(), [accepted()]);
}

#[tokio::test]
async fn smtp_refuses_a_wrong_password_and_mail_without_auth() {
    let server = server().await;
    let mut smtp = Smtp::implicit(&server).await;
    smtp.send("EHLO client.example.org").await;
    let auth = smtp
        .send(&format!(
            "AUTH PLAIN {}",
            base64(&format!("\0{USER}\0wrong"))
        ))
        .await;
    assert!(auth.starts_with("535"), "{auth}");
    assert!(
        smtp.send("MAIL FROM:<alice@example.org>")
            .await
            .starts_with("530")
    );
    assert!(server.smtp_messages().is_empty());
}
