//! Choosing files to attach with the system's file chooser, through a fake desktop portal on a
//! private bus: the real session bus is never touched.

use std::path::PathBuf;

use missive_core::chooser::choose_files;
use missive_core::fake::{Chosen, FakeFileChooser};

async fn connect(portal: &FakeFileChooser) -> zbus::Connection {
    zbus::connection::Builder::address(portal.address())
        .unwrap()
        .build()
        .await
        .unwrap()
}

#[tokio::test]
async fn the_files_chosen_are_given_as_paths_several_at_once() {
    let portal = FakeFileChooser::start(Chosen::Files(vec![
        "file:///tmp/notes.txt".to_owned(),
        "file:///tmp/R%C3%A9sum%C3%A9%20final.pdf".to_owned(),
    ]))
    .await;

    let chosen = choose_files(&connect(&portal).await, "Attach files")
        .await
        .unwrap();

    assert_eq!(
        chosen,
        [
            PathBuf::from("/tmp/notes.txt"),
            PathBuf::from("/tmp/Résumé final.pdf")
        ]
    );
    let calls = portal.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].title, "Attach files");
    assert!(calls[0].multiple);
}

#[tokio::test]
async fn cancelling_chooses_nothing() {
    let portal = FakeFileChooser::start(Chosen::Cancelled).await;
    let chosen = choose_files(&connect(&portal).await, "Attach files")
        .await
        .unwrap();
    assert!(chosen.is_empty());
}

#[tokio::test]
async fn a_chooser_that_fails_says_so() {
    let portal = FakeFileChooser::start(Chosen::Failed).await;
    assert!(
        choose_files(&connect(&portal).await, "Attach files")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn only_local_files_are_attached() {
    let portal = FakeFileChooser::start(Chosen::Files(vec![
        "https://www.example.org/a.pdf".to_owned(),
        "file:///tmp/b.txt".to_owned(),
    ]))
    .await;
    let chosen = choose_files(&connect(&portal).await, "Attach files")
        .await
        .unwrap();
    assert_eq!(chosen, [PathBuf::from("/tmp/b.txt")]);
}
