//! The daemon's events, on a thread of their own: the window subscribes on the socket as
//! `missive watch` does (`docs/events.md`). It never starts the daemon. Without one it says
//! [`Event::Gone`] once and tries the socket again every second, then subscribes when a daemon
//! answers.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use missive_core::events::Event;
use missive_core::protocol::{Answer, Command, PROTOCOL, Request};

/// How long to wait before looking again for a daemon.
const POLL: Duration = Duration::from_secs(1);

/// Follows the daemon on `socket` and hands each event to `tell`, until `tell` says the window is
/// gone (`false`).
pub fn follow(socket: PathBuf, tell: impl Fn(Event) -> bool) {
    // Whether `gone` was said and no daemon has answered since.
    let mut gone = false;
    loop {
        let streamed = match UnixStream::connect(&socket) {
            Ok(stream) => match subscribe(stream, &tell) {
                Followed::WindowGone => return,
                Followed::Closed { streamed } => streamed,
            },
            // No socket, nobody behind it, or one the window cannot reach: no daemon to show.
            Err(_) => false,
        };
        if (streamed || !gone) && !tell(Event::Gone) {
            return;
        }
        gone = true;
        std::thread::sleep(POLL);
    }
}

enum Followed {
    /// The daemon closed the connection, refused the subscription or said something unreadable.
    Closed {
        streamed: bool,
    },
    WindowGone,
}

fn subscribe(stream: UnixStream, tell: &impl Fn(Event) -> bool) -> Followed {
    let closed = Followed::Closed { streamed: false };
    let request = Request {
        protocol: PROTOCOL,
        command: Command::Subscribe,
    };
    let mut line = serde_json::to_string(&request).expect("a request serializes");
    line.push('\n');
    // Held to the end: closing the writing half would end the subscription.
    let mut write = &stream;
    if write.write_all(line.as_bytes()).is_err() {
        return closed;
    }
    let mut read = BufReader::new(&stream);
    line.clear();
    if !matches!(read.read_line(&mut line), Ok(n) if n > 0) || !line.ends_with('\n') {
        return closed;
    }
    if !serde_json::from_str::<Answer>(&line).is_ok_and(|answer| answer.ok) {
        return closed;
    }
    loop {
        line.clear();
        // A line cut short by the daemon stopping is not read: it would not be JSON.
        if !matches!(read.read_line(&mut line), Ok(n) if n > 0) || !line.ends_with('\n') {
            return Followed::Closed { streamed: true };
        }
        // An event this window does not know is skipped, as the stream's contract asks.
        if let Ok(event) = serde_json::from_str::<Event>(&line)
            && !tell(event)
        {
            return Followed::WindowGone;
        }
    }
}
