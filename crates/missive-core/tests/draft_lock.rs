//! A draft's lock goes with the process that holds it, killed or not: a window that crashed does
//! not keep its draft shut.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

use missive_core::draft_lock::{self, Taking};

const FOLDER: &str = "MISSIVE_TEST_LOCK_FOLDER";

/// Not a test of its own: the process the one below starts, which takes a lock and waits to be
/// killed. Run on its own, it does nothing.
#[test]
fn holder() {
    let Some(folder) = std::env::var_os(FOLDER) else {
        return;
    };
    let Taking::Ours(_lock) = draft_lock::take_in(folder.as_ref(), "alice", "d@example.org") else {
        panic!("the holder takes the lock");
    };
    println!("locked");
    std::thread::sleep(std::time::Duration::from_secs(60));
}

#[test]
fn a_window_that_crashed_does_not_keep_its_draft_shut() {
    let dir = tempfile::tempdir().unwrap();
    let mut holder = Command::new(std::env::current_exe().unwrap())
        .args(["holder", "--exact", "--nocapture", "--test-threads=1"])
        .env(FOLDER, dir.path())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(holder.stdout.take().unwrap()).lines();
    // The harness writes `test holder ... ` before the test's own output, on the same line.
    while !lines.next().unwrap().unwrap().ends_with("locked") {}

    // Another process holds it: this one is refused, and told so when it only looks.
    assert!(draft_lock::is_held_in(dir.path(), "alice", "d@example.org"));
    assert!(matches!(
        draft_lock::take_in(dir.path(), "alice", "d@example.org"),
        Taking::Elsewhere
    ));

    // Killed without a chance to clean up: the file is still there, and the draft is free.
    holder.kill().unwrap();
    holder.wait().unwrap();
    assert!(!draft_lock::is_held_in(
        dir.path(),
        "alice",
        "d@example.org"
    ));
    assert!(matches!(
        draft_lock::take_in(dir.path(), "alice", "d@example.org"),
        Taking::Ours(_)
    ));
}
