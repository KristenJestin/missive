//! The outbox's workers against the fake SMTP server: a message is never delivered twice.

use missive_core::account::{Account, Password, Security, Server};
use missive_core::compose::Built;
use missive_core::connect::client_config;
use missive_core::fake::FakeServer;
use missive_core::outbox::{Tried, send_next};
use missive_core::store::Store;

const USER: &str = "alice@example.org";
const PASSWORD: &str = "correct horse battery staple";

fn account(fake: &FakeServer) -> Account {
    let server = |port| Server {
        host: "localhost".to_owned(),
        port,
        security: Security::Tls,
    };
    Account {
        name: "Alice Example".to_owned(),
        address: USER.to_owned(),
        user: USER.to_owned(),
        imap: server(fake.imap_tls_addr().port()),
        smtp: server(fake.smtp_tls_addr().port()),
        server_saves_sent: true,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn two_workers_on_the_same_account_deliver_its_message_once() {
    let fake = FakeServer::builder().user(USER, PASSWORD).start().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("copy.sqlite");
    let mut first = Store::open(&path).unwrap();
    let mut second = Store::open(&path).unwrap();
    let key = first.account("alice").unwrap();
    first
        .enqueue(
            key,
            &Built {
                sender: USER.to_owned(),
                recipients: vec!["bob@example.com".to_owned()],
                message_id: "1@example.org".to_owned(),
                raw: b"Subject: Once\r\n\r\nHello\r\n".to_vec(),
                bcc_header: None,
            },
            false,
            None,
        )
        .unwrap();
    let account = account(&fake);
    let password = Password::new(PASSWORD.to_owned());
    let tls = client_config(Some(fake.certificate_der()));

    let (one, two) = tokio::join!(
        send_next(&mut first, key, &account, &password, tls.clone()),
        send_next(&mut second, key, &account, &password, tls.clone()),
    );

    let tried: Vec<Tried> = [one.unwrap(), two.unwrap()].into_iter().flatten().collect();
    assert!(
        matches!(tried.as_slice(), [Tried::Sent { .. }]),
        "{tried:?}"
    );
    assert_eq!(fake.smtp_messages().len(), 1);
}
