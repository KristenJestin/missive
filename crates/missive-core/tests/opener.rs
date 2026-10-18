//! Opening a link with the system's default handler, through a fake desktop portal on a private
//! bus: the real session bus is never touched.

use missive_core::fake::{FakePortal, PortalAnswer};
use missive_core::opener::{followable, mail_link, open_uri_with_portal};

async fn connect(portal: &FakePortal) -> zbus::Connection {
    zbus::connection::Builder::address(portal.address())
        .unwrap()
        .build()
        .await
        .unwrap()
}

#[tokio::test]
async fn a_link_is_handed_to_the_portal_as_it_is() {
    let portal = FakePortal::start(PortalAnswer::Opened).await;
    let opened = open_uri_with_portal(&connect(&portal).await, "https://www.example.org/a?b=c")
        .await
        .unwrap();
    assert!(opened);
    assert_eq!(portal.uris(), ["https://www.example.org/a?b=c"]);
    assert!(portal.calls().is_empty(), "no file was opened");
}

#[tokio::test]
async fn a_portal_that_could_not_open_a_link_says_so() {
    let portal = FakePortal::start(PortalAnswer::Failed).await;
    let opened = open_uri_with_portal(&connect(&portal).await, "mailto:carol@example.com")
        .await
        .unwrap();
    assert!(!opened, "xdg-open is tried next");
}

#[test]
fn only_web_and_mail_links_are_followed() {
    for uri in [
        "https://www.example.org/",
        "http://www.example.org/",
        "MAILTO:carol@example.com",
    ] {
        assert!(followable(uri), "{uri}");
    }
    for uri in [
        "javascript:alert(1)",
        "file:///etc/passwd",
        "data:text/html,x",
        "cid:logo@example.org",
        "smb://example.org/share",
        "www.example.org",
    ] {
        assert!(!followable(uri), "{uri}");
    }
}

#[test]
fn a_mail_link_keeps_only_its_subject_body_and_cc() {
    assert_eq!(
        mail_link(
            "mailto:carol@example.com?subject=Hi&attach=/home/a/.ssh/id_ed25519&body=Yes\
             &%61ttachment=x&BCC=eve@example.net&cc=dave@example.net&Subject=Two#frag"
        ),
        "mailto:carol@example.com?subject=Hi&body=Yes&cc=dave@example.net&Subject=Two"
    );
    assert_eq!(
        mail_link("MAILTO:carol@example.com?attach=x"),
        "MAILTO:carol@example.com"
    );
}

#[test]
fn a_web_link_is_left_as_it_is() {
    let uri = "https://www.example.org/a?attach=x&bcc=y#z";
    assert_eq!(mail_link(uri), uri);
}
