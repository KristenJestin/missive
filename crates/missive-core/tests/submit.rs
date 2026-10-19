//! The SMTP submission client against the fake server: what arrives, and where each failure
//! leaves a message.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use missive_core::account::{Password, Security, Server};
use missive_core::connect::{ConnectError, client_config};
use missive_core::fake::{FakeServer, SmtpMessage};
use missive_core::submit::{Outgoing, Submitted, submit};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

const USER: &str = "alice@example.org";
const PASSWORD: &str = "correct horse battery staple";

/// A message with a line that is a lone dot and one that starts with one: they must arrive as
/// they are.
const RAW: &[u8] = b"From: alice@example.org\r\nTo: bob@example.com\r\nSubject: Dots\r\n\r\n\
    one\r\n.\r\n..two\r\nthree\r\n";

fn server(address: SocketAddr, security: Security) -> Server {
    Server {
        host: "localhost".to_owned(),
        port: address.port(),
        security,
    }
}

fn recipients() -> Vec<String> {
    vec![
        "bob@example.com".to_owned(),
        "hidden@example.net".to_owned(),
    ]
}

/// Submits [`RAW`] over implicit TLS; returns what came of it, and whether `sending` was called.
async fn send(fake: &FakeServer) -> (Result<Submitted, ConnectError>, bool) {
    let called = Arc::new(AtomicBool::new(false));
    let marked = called.clone();
    let recipients = recipients();
    let submitted = submit(
        &server(fake.smtp_tls_addr(), Security::Tls),
        USER,
        &Password::new(PASSWORD.to_owned()),
        client_config(Some(fake.certificate_der())),
        Outgoing {
            sender: USER,
            recipients: &recipients,
            raw: RAW,
        },
        move || {
            marked.store(true, Ordering::SeqCst);
            Ok(())
        },
    )
    .await
    .map(|(submitted, _)| submitted);
    (submitted, called.load(Ordering::SeqCst))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_arrives_as_built_with_its_envelope_over_tls_and_starttls() {
    let fake = FakeServer::builder().user(USER, PASSWORD).start().await;
    let tls = client_config(Some(fake.certificate_der()));
    let recipients = recipients();

    for (address, security) in [
        (fake.smtp_tls_addr(), Security::Tls),
        (fake.smtp_starttls_addr(), Security::Starttls),
    ] {
        let submitted = submit(
            &server(address, security),
            USER,
            &Password::new(PASSWORD.to_owned()),
            tls.clone(),
            Outgoing {
                sender: USER,
                recipients: &recipients,
                raw: RAW,
            },
            || Ok(()),
        )
        .await
        .map(|(submitted, _)| submitted);
        assert_eq!(submitted, Ok(Submitted::Sent));
    }

    let expected = SmtpMessage {
        from: USER.to_owned(),
        to: recipients,
        data: RAW.to_vec(),
    };
    assert_eq!(fake.smtp_messages(), [expected.clone(), expected]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_certificate_is_refused_before_anything_is_sent() {
    let fake = FakeServer::builder().user(USER, PASSWORD).start().await;
    let other = FakeServer::builder().start().await;
    let recipients = recipients();

    let submitted = submit(
        &server(fake.smtp_starttls_addr(), Security::Starttls),
        USER,
        &Password::new(PASSWORD.to_owned()),
        client_config(Some(other.certificate_der())),
        Outgoing {
            sender: USER,
            recipients: &recipients,
            raw: RAW,
        },
        || Ok(()),
    )
    .await
    .map(|(submitted, _)| submitted);

    assert_eq!(submitted, Err(ConnectError::CertificateRefused));
    assert!(fake.smtp_messages().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_temporary_refusal_before_the_end_of_data_is_to_try_again() {
    for command in ["MAIL", "RCPT", "DATA", "."] {
        let fake = FakeServer::builder()
            .user(USER, PASSWORD)
            .smtp_answer(command, "451 4.3.0 try again later")
            .start()
            .await;

        let (submitted, sending) = send(&fake).await;

        assert!(
            matches!(submitted, Ok(Submitted::Again { .. })),
            "{command}: {submitted:?}"
        );
        assert_eq!(sending, command == ".", "{command}");
        assert!(fake.smtp_messages().is_empty(), "{command}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_permanent_refusal_says_which_step_refused() {
    for (command, step) in [
        ("MAIL", "MAIL FROM"),
        ("RCPT", "RCPT TO"),
        ("DATA", "DATA"),
        (".", "end of data"),
    ] {
        let fake = FakeServer::builder()
            .user(USER, PASSWORD)
            .smtp_answer(command, "554 5.7.1 refused")
            .start()
            .await;

        let (submitted, _) = send(&fake).await;

        assert_eq!(submitted, Ok(Submitted::Refused { code: 554, step }));
        assert!(fake.smtp_messages().is_empty(), "{command}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_connection_cut_after_the_end_of_data_is_uncertain() {
    let fake = FakeServer::builder().user(USER, PASSWORD).start().await;
    fake.smtp_cut_after_data();

    let (submitted, sending) = send(&fake).await;

    assert!(
        matches!(submitted, Ok(Submitted::Uncertain { .. })),
        "{submitted:?}"
    );
    assert!(sending);
    // The server took it: sending it again would deliver it twice.
    assert_eq!(fake.smtp_messages().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn without_the_mark_of_sending_the_end_of_data_is_never_written() {
    let fake = FakeServer::builder().user(USER, PASSWORD).start().await;
    let recipients = recipients();

    let submitted = submit(
        &server(fake.smtp_tls_addr(), Security::Tls),
        USER,
        &Password::new(PASSWORD.to_owned()),
        client_config(Some(fake.certificate_der())),
        Outgoing {
            sender: USER,
            recipients: &recipients,
            raw: RAW,
        },
        || Err("the outbox could not be written".to_owned()),
    )
    .await
    .map(|(submitted, _)| submitted);

    assert_eq!(
        submitted,
        Ok(Submitted::Again {
            reason: "the outbox could not be written".to_owned()
        })
    );
    assert!(fake.smtp_messages().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_password_is_refused_before_anything_is_sent() {
    let fake = FakeServer::builder().user(USER, "another").start().await;

    let (submitted, sending) = send(&fake).await;

    assert_eq!(submitted, Err(ConnectError::AuthRefused));
    assert!(!sending);
}

/// A server that offers STARTTLS, answers `answer` to it, and returns every byte the client sent
/// after that answer.
async fn starttls_answered(answer: &'static str) -> (SocketAddr, tokio::task::JoinHandle<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let served = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut io = BufReader::new(stream);
        io.write_all(b"220 fake.example.org ESMTP\r\n")
            .await
            .unwrap();
        loop {
            let mut line = String::new();
            if io.read_line(&mut line).await.unwrap() == 0 {
                return Vec::new();
            }
            if line.to_ascii_uppercase().starts_with("EHLO") {
                io.write_all(b"250-fake.example.org\r\n250 STARTTLS\r\n")
                    .await
                    .unwrap();
            } else if line.to_ascii_uppercase().starts_with("STARTTLS") {
                io.write_all(format!("{answer}\r\n").as_bytes())
                    .await
                    .unwrap();
                break;
            }
        }
        let mut after = Vec::new();
        let _ = io.read_to_end(&mut after).await;
        after
    });
    (address, served)
}

#[tokio::test(flavor = "multi_thread")]
async fn starttls_answered_with_anything_but_220_aborts_before_auth() {
    for answer in [
        "454 4.7.0 TLS not available now",
        "502 5.5.1 not implemented",
        "250 2.0.0 OK",
    ] {
        let (address, served) = starttls_answered(answer).await;
        let recipients = recipients();

        let submitted = submit(
            &server(address, Security::Starttls),
            USER,
            &Password::new(PASSWORD.to_owned()),
            client_config(None),
            Outgoing {
                sender: USER,
                recipients: &recipients,
                raw: RAW,
            },
            || Ok(()),
        )
        .await
        .map(|(submitted, _)| submitted);

        assert!(
            matches!(submitted, Err(ConnectError::Unreachable(_))),
            "{answer}: {submitted:?}"
        );
        // Nothing went in clear after the refusal: no AUTH, so no password.
        assert_eq!(served.await.unwrap(), b"", "{answer}");
    }
}
