//! The window's subscription to the daemon's events, against a socket that answers as the daemon
//! does, in a temporary folder.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::sync::mpsc;
use std::time::Duration;

use missive_core::events::Event;
use missive_window::watch::follow;

const WAIT: Duration = Duration::from_secs(10);

#[test]
fn without_a_daemon_gone_is_told_once() {
    let folder = tempfile::tempdir().unwrap();
    let socket = folder.path().join("daemon.sock");
    let (tell, told) = mpsc::channel();
    std::thread::spawn(move || follow(socket, move |event| tell.send(event).is_ok()));
    assert_eq!(told.recv_timeout(WAIT).unwrap(), Event::Gone);
    assert!(
        told.recv_timeout(Duration::from_millis(2500)).is_err(),
        "gone is told once while the daemon stays away"
    );
}

#[test]
fn the_window_subscribes_follows_the_events_and_says_when_the_daemon_stops() {
    let folder = tempfile::tempdir().unwrap();
    let socket = folder.path().join("daemon.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let daemon = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut request = String::new();
        BufReader::new(&stream).read_line(&mut request).unwrap();
        let mut write = &stream;
        write
            .write_all(b"{\"protocol\":1,\"answer\":\"subscribe\",\"ok\":true}\n")
            .unwrap();
        write
            .write_all(
                b"{\"protocol\":1,\"event\":\"state\",\"account\":\"alice\",\"state\":\"unreachable\"}\n\
                  {\"protocol\":1,\"event\":\"an-event-of-a-later-daemon\",\"x\":1}\n\
                  {\"protocol\":1,\"event\":\"lagged\",\"missed\":3}\n",
            )
            .unwrap();
        request
    });
    let (tell, told) = mpsc::channel();
    std::thread::spawn(move || follow(socket, move |event| tell.send(event).is_ok()));

    let request: serde_json::Value = serde_json::from_str(&daemon.join().unwrap()).unwrap();
    assert_eq!(request["request"], "subscribe");
    assert_eq!(
        told.recv_timeout(WAIT).unwrap(),
        Event::State {
            account: "alice".to_owned(),
            state: "unreachable".to_owned()
        }
    );
    assert_eq!(
        told.recv_timeout(WAIT).unwrap(),
        Event::Lagged { missed: 3 },
        "an event it does not know is skipped"
    );
    assert_eq!(told.recv_timeout(WAIT).unwrap(), Event::Gone);
}
