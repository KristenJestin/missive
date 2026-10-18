//! `missive watch`: the daemon's events, one JSON object per line on standard output (see
//! `docs/events.md`).
//!
//! It never starts the daemon. Without one it says `gone` and polls the socket, once a second (a
//! file watch would need a dependency for a wait that costs one failed `connect` a second), then
//! subscribes when a daemon answers; and when the daemon stops it says `gone` again and goes back
//! to polling. Lines from the daemon are passed on as they are.

use std::io::ErrorKind;
use std::time::Duration;

use missive_core::events::Event;
use missive_core::protocol::{Answer, Command, PROTOCOL, Request, socket_path};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Stdout};
use tokio::net::UnixStream;
use tokio::signal::unix::{SignalKind, signal};

/// How long to wait before looking again for a daemon.
const POLL: Duration = Duration::from_secs(1);

/// What ended a subscription.
enum Followed {
    /// The daemon closed the connection: it stopped, or never answered.
    Closed { streamed: bool },
    /// Standard output was closed: whoever read the stream is gone.
    StdoutClosed,
}

/// Watches until SIGTERM or SIGINT, or until standard output is closed.
pub async fn run() -> Result<(), String> {
    let mut terminate = signal(SignalKind::terminate()).map_err(|e| e.to_string())?;
    let mut interrupt = signal(SignalKind::interrupt()).map_err(|e| e.to_string())?;
    let mut stdout = tokio::io::stdout();
    tokio::select! {
        result = stream(&mut stdout) => result,
        _ = terminate.recv() => Ok(()),
        _ = interrupt.recv() => Ok(()),
    }
}

async fn stream(out: &mut Stdout) -> Result<(), String> {
    let socket = socket_path().map_err(|e| e.to_string())?;
    // Whether `gone` was said and no daemon has answered since.
    let mut gone = false;
    loop {
        match UnixStream::connect(&socket).await {
            Ok(stream) => match follow(stream, out).await? {
                Followed::StdoutClosed => return Ok(()),
                Followed::Closed { streamed } => {
                    if (streamed || !gone) && say(out, &(Event::Gone.line() + "\n")).await.is_err()
                    {
                        return Ok(());
                    }
                    gone = true;
                }
            },
            Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused) => {
                if !gone && say(out, &(Event::Gone.line() + "\n")).await.is_err() {
                    return Ok(());
                }
                gone = true;
            }
            Err(e) => {
                return Err(format!(
                    "cannot reach the daemon at {}: {e}",
                    socket.display()
                ));
            }
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Subscribes on `stream` and writes what the daemon says to `out`, until one of them ends.
async fn follow(stream: UnixStream, out: &mut Stdout) -> Result<Followed, String> {
    // Held to the end: closing the writing half would end the subscription.
    let (read, mut write) = stream.into_split();
    let request = Request {
        protocol: PROTOCOL,
        command: Command::Subscribe,
    };
    let mut line = serde_json::to_string(&request).expect("a request serializes");
    line.push('\n');
    if write.write_all(line.as_bytes()).await.is_err() {
        return Ok(Followed::Closed { streamed: false });
    }
    let mut read = BufReader::new(read);
    line.clear();
    if !matches!(read.read_line(&mut line).await, Ok(n) if n > 0) || !line.ends_with('\n') {
        return Ok(Followed::Closed { streamed: false });
    }
    let answer: Answer = serde_json::from_str(&line)
        .map_err(|_| "the daemon gave an answer this missive cannot read".to_owned())?;
    if !answer.ok {
        return Err(answer.message.unwrap_or_else(|| {
            format!("the daemon refused: {}", answer.error.unwrap_or_default())
        }));
    }
    loop {
        line.clear();
        // A line cut short by the daemon stopping is not passed on: it would not be JSON.
        if !matches!(read.read_line(&mut line).await, Ok(n) if n > 0) || !line.ends_with('\n') {
            return Ok(Followed::Closed { streamed: true });
        }
        if say(out, &line).await.is_err() {
            return Ok(Followed::StdoutClosed);
        }
    }
}

/// Writes one line and flushes it, so that a reader sees it at once.
async fn say(out: &mut Stdout, line: &str) -> std::io::Result<()> {
    out.write_all(line.as_bytes()).await?;
    out.flush().await
}
