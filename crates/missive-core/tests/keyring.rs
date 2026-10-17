//! The keyring client against the fake Secret Service on its private bus.

use missive_core::account::Password;
use missive_core::fake::FakeKeyring;
use missive_core::keyring::Keyring;

async fn open(fake: &FakeKeyring) -> Keyring {
    let connection = zbus::connection::Builder::address(fake.address())
        .unwrap()
        .build()
        .await
        .unwrap();
    Keyring::open_on(connection).await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_password_is_stored_read_back_replaced_and_deleted() {
    let fake = FakeKeyring::start().await;
    let keyring = open(&fake).await;

    keyring
        .store("alice", &Password::new("first".to_owned()))
        .await
        .unwrap();
    keyring
        .store("bob", &Password::new("other".to_owned()))
        .await
        .unwrap();
    keyring
        .store("alice", &Password::new("second".to_owned()))
        .await
        .unwrap();

    let items = fake.items();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].label, "Missive: alice");
    assert_eq!(items[0].attributes["application"], "missive");
    assert_eq!(items[0].attributes["account"], "alice");
    let alice = keyring.password("alice").await.unwrap().unwrap();
    assert_eq!(alice.expose(), "second");

    assert!(keyring.delete("alice").await.unwrap());
    assert!(keyring.password("alice").await.unwrap().is_none());
    assert!(!keyring.delete("alice").await.unwrap());
    assert_eq!(fake.items().len(), 1);
    assert_eq!(
        keyring.password("bob").await.unwrap().unwrap().expose(),
        "other"
    );
}
