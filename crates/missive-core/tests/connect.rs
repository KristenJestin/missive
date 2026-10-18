//! The login checks against the fake server, over both securities of each protocol.

use std::net::SocketAddr;

use missive_core::account::{Password, Security, Server};
use missive_core::connect::{ConnectError, check_imap, check_smtp, client_config};
use missive_core::fake::FakeServer;

const USER: &str = "alice@example.org";
const PASSWORD: &str = "correct horse battery staple";

fn server(address: SocketAddr, security: Security) -> Server {
    Server {
        host: "localhost".to_owned(),
        port: address.port(),
        security,
    }
}

fn password(password: &str) -> Password {
    Password::new(password.to_owned())
}

#[tokio::test(flavor = "multi_thread")]
async fn both_protocols_log_in_over_starttls_and_implicit_tls_with_the_pinned_certificate() {
    let fake = FakeServer::builder().user(USER, PASSWORD).start().await;
    let tls = client_config(Some(fake.certificate_der()));
    let right = password(PASSWORD);

    for (address, security) in [
        (fake.imap_starttls_addr(), Security::Starttls),
        (fake.imap_tls_addr(), Security::Tls),
    ] {
        check_imap(&server(address, security), USER, &right, tls.clone())
            .await
            .unwrap();
    }
    for (address, security) in [
        (fake.smtp_starttls_addr(), Security::Starttls),
        (fake.smtp_tls_addr(), Security::Tls),
    ] {
        check_smtp(&server(address, security), USER, &right, tls.clone())
            .await
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_password_gives_auth_refused_on_smtp_too() {
    let fake = FakeServer::builder().user(USER, PASSWORD).start().await;
    let tls = client_config(Some(fake.certificate_der()));

    let refused = check_smtp(
        &server(fake.smtp_starttls_addr(), Security::Starttls),
        USER,
        &password("wrong"),
        tls,
    )
    .await;

    assert_eq!(refused, Err(ConnectError::AuthRefused));
}

#[tokio::test(flavor = "multi_thread")]
async fn without_the_import_smtp_refuses_the_certificate_too() {
    let fake = FakeServer::builder().user(USER, PASSWORD).start().await;

    let refused = check_smtp(
        &server(fake.smtp_tls_addr(), Security::Tls),
        USER,
        &password(PASSWORD),
        client_config(None),
    )
    .await;

    assert_eq!(refused, Err(ConnectError::CertificateRefused));
}

#[test]
fn each_error_names_its_state() {
    assert_eq!(
        ConnectError::Unreachable(String::new()).state(),
        "unreachable"
    );
    assert_eq!(ConnectError::AuthRefused.state(), "auth-refused");
    assert_eq!(
        ConnectError::CertificateRefused.state(),
        "certificate-refused"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn imap_no_unavailable_at_login_gives_unreachable_not_auth_refused() {
    let fake = FakeServer::builder()
        .user(USER, PASSWORD)
        .imap_login_unavailable()
        .start()
        .await;
    let tls = client_config(Some(fake.certificate_der()));

    let refused = check_imap(
        &server(fake.imap_tls_addr(), Security::Tls),
        USER,
        &password(PASSWORD),
        tls,
    )
    .await;

    assert!(
        matches!(refused, Err(ConnectError::Unreachable(_))),
        "{refused:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn imap_no_authenticationfailed_at_login_still_gives_auth_refused() {
    let fake = FakeServer::builder().user(USER, PASSWORD).start().await;
    let tls = client_config(Some(fake.certificate_der()));

    let refused = check_imap(
        &server(fake.imap_tls_addr(), Security::Tls),
        USER,
        &password("wrong"),
        tls,
    )
    .await;

    assert_eq!(refused, Err(ConnectError::AuthRefused));
}

#[tokio::test(flavor = "multi_thread")]
async fn smtp_auth_login_refused_at_the_user_name_step_gives_auth_refused() {
    let fake = FakeServer::builder()
        .user(USER, PASSWORD)
        .smtp_login_only()
        .start()
        .await;
    let tls = client_config(Some(fake.certificate_der()));

    let refused = check_smtp(
        &server(fake.smtp_tls_addr(), Security::Tls),
        "nobody@example.org",
        &password(PASSWORD),
        tls,
    )
    .await;

    assert_eq!(refused, Err(ConnectError::AuthRefused));
}

#[tokio::test(flavor = "multi_thread")]
async fn smtp_auth_login_refused_at_the_password_step_gives_auth_refused() {
    let fake = FakeServer::builder()
        .user(USER, PASSWORD)
        .smtp_login_only()
        .start()
        .await;
    let tls = client_config(Some(fake.certificate_der()));

    let refused = check_smtp(
        &server(fake.smtp_tls_addr(), Security::Tls),
        USER,
        &password("wrong"),
        tls,
    )
    .await;

    assert_eq!(refused, Err(ConnectError::AuthRefused));
}

#[tokio::test(flavor = "multi_thread")]
async fn smtp_auth_login_alone_logs_in() {
    let fake = FakeServer::builder()
        .user(USER, PASSWORD)
        .smtp_login_only()
        .start()
        .await;
    let tls = client_config(Some(fake.certificate_der()));

    check_smtp(
        &server(fake.smtp_tls_addr(), Security::Tls),
        USER,
        &password(PASSWORD),
        tls,
    )
    .await
    .unwrap();
}
