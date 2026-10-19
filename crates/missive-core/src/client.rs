//! Asking the daemon: connecting to its socket, starting it when none answers, sending one
//! request and reading its answer (`docs/protocol.md`). The command line and the window share it.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::protocol::{Answer, Command, PROTOCOL, Request};

/// `$XDG_STATE_HOME/missive/daemon.log`, or `~/.local/state/missive/daemon.log`: where a daemon
/// started on demand writes its log.
pub fn log_path() -> Result<PathBuf, String> {
    let state = match std::env::var_os("XDG_STATE_HOME").map(PathBuf::from) {
        Some(path) if path.is_absolute() => path,
        _ => match std::env::var_os("HOME") {
            Some(home) if !home.is_empty() => PathBuf::from(home).join(".local/state"),
            _ => return Err("neither XDG_STATE_HOME nor HOME is set".to_owned()),
        },
    };
    Ok(state.join("missive").join("daemon.log"))
}

/// Connects to the daemon on `socket`, starting `program daemon` when none answers.
pub async fn connect(socket: &Path, program: &Path) -> Result<UnixStream, String> {
    match UnixStream::connect(socket).await {
        Ok(stream) => return Ok(stream),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) => {}
        Err(e) => {
            return Err(format!(
                "cannot reach the daemon at {}: {e}",
                socket.display()
            ));
        }
    }
    start_and_wait(socket, program, &log_path()?).await
}

/// Starts `program daemon` and waits until it answers on `socket`.
async fn start_and_wait(socket: &Path, program: &Path, log: &Path) -> Result<UnixStream, String> {
    let mut child = start(program, log)?;
    let started = Instant::now();
    let connected = loop {
        if let Ok(stream) = UnixStream::connect(socket).await {
            break Ok(stream);
        }
        // Another `sync` may have started one first: this one then stops, and that one answers.
        let exited = child.try_wait().ok().flatten().is_some();
        if (exited && started.elapsed() > Duration::from_secs(2))
            || started.elapsed() > Duration::from_secs(10)
        {
            break Err(format!(
                "the daemon did not start (its log is {})",
                log.display()
            ));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    reap(child);
    connected
}

/// Starts `program daemon` in its own process group, so that the terminal's Ctrl-C does not reach
/// it, logging to `log`.
fn start(program: &Path, log: &Path) -> Result<std::process::Child, String> {
    use std::os::unix::process::CommandExt;
    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .map_err(|e| format!("cannot open {}: {e}", log.display()))?;
    std::process::Command::new(program)
        .arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(file)
        .process_group(0)
        .spawn()
        .map_err(|e| format!("cannot start the daemon: {e}"))
}

/// How long a client waits for the daemon's answer once its request is sent. Every request but
/// `sync` and `subscribe` is answered once it is recorded in the copy, never after the server:
/// a daemon that stays silent longer is stuck.
pub const ANSWER_TIMEOUT: Duration = Duration::from_secs(10);

/// The same for a request that carries a message to record (`send`, and `draft` with `send`): the
/// daemon may take long to write a large one while a sync holds the copy, and it has no guard
/// against the same message being sent twice, so a client must not give up on it early.
pub const QUEUING_TIMEOUT: Duration = Duration::from_secs(120);

/// How long to wait for the answer to `command`.
pub fn answer_timeout(command: &Command) -> Duration {
    if queues_a_message(command) {
        QUEUING_TIMEOUT
    } else {
        ANSWER_TIMEOUT
    }
}

fn queues_a_message(command: &Command) -> bool {
    match command {
        Command::Send(_) => true,
        Command::Draft(draft) => draft.send,
        _ => false,
    }
}

/// Sends one request to the daemon on `socket`, starting `program daemon` when none answers, and
/// reads its answer, for at most [`answer_timeout`].
pub async fn ask(socket: &Path, program: &Path, command: Command) -> Result<Answer, String> {
    let timeout = answer_timeout(&command);
    ask_within(socket, program, command, timeout).await
}

async fn ask_within(
    socket: &Path,
    program: &Path,
    command: Command,
    timeout: Duration,
) -> Result<Answer, String> {
    let queues = queues_a_message(&command);
    let stream = connect(socket, program).await?;
    let (read, mut write) = stream.into_split();
    let mut line = serde_json::to_string(&Request {
        protocol: PROTOCOL,
        command,
    })
    .map_err(|e| e.to_string())?;
    line.push('\n');
    write
        .write_all(line.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    let mut line = String::new();
    tokio::time::timeout(timeout, BufReader::new(read).read_line(&mut line))
        .await
        .map_err(|_| {
            let waited = timeout.as_secs().max(1);
            if queues {
                // The message may well be in the outbox: "not sent" would be a lie, and a retry
                // would send it twice.
                format!(
                    "the daemon did not answer within {waited} seconds: the message may have been \
                     queued; check `missive status` (or the outbox notice of the window) before \
                     sending it again"
                )
            } else {
                format!("the daemon did not answer within {waited} seconds")
            }
        })?
        .map_err(|e| e.to_string())?;
    serde_json::from_str(&line)
        .map_err(|_| "the daemon gave an answer this missive cannot read".to_owned())
}

/// Waits for `child` on a thread of its own: a daemon that exits while its client lives on (the
/// window) would stay a zombie otherwise. The thread ends with the daemon.
fn reap(mut child: std::process::Child) {
    let _ = std::thread::Builder::new()
        .name("daemon-reaper".to_owned())
        .spawn(move || {
            let _ = child.wait();
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixListener;

    fn answer_line() -> String {
        let answer = Answer {
            protocol: PROTOCOL,
            answer: Some("sync".to_owned()),
            ok: true,
            ..Answer::default()
        };
        format!("{}\n", serde_json::to_string(&answer).unwrap())
    }

    #[tokio::test]
    async fn a_daemon_that_never_answers_gives_an_error_after_the_timeout() {
        let folder = tempfile::tempdir().unwrap();
        let socket = folder.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        // It accepts, reads the request and holds the connection open, silent.
        let silent = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut lines = BufReader::new(stream);
            let mut request = String::new();
            lines.read_line(&mut request).await.unwrap();
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        let started = Instant::now();
        let asked = ask_within(
            &socket,
            Path::new("missive-is-not-started"),
            Command::Subscribe,
            Duration::from_millis(200),
        )
        .await;
        let error = asked.expect_err("no answer comes");
        assert!(error.contains("did not answer"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(5));
        silent.abort();
    }

    /// A daemon that accepts, reads the request and never answers.
    fn silent(socket: &Path) -> tokio::task::JoinHandle<()> {
        let listener = UnixListener::bind(socket).unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut lines = BufReader::new(stream);
            let mut request = String::new();
            lines.read_line(&mut request).await.unwrap();
            std::future::pending::<()>().await;
        })
    }

    fn send() -> Command {
        Command::Send(crate::protocol::Send::default())
    }

    #[tokio::test(start_paused = true)]
    async fn a_request_that_queues_a_message_waits_much_longer_than_an_action() {
        for (command, waited) in [
            (send(), QUEUING_TIMEOUT),
            (
                Command::Draft(crate::protocol::SaveDraft {
                    send: true,
                    ..Default::default()
                }),
                QUEUING_TIMEOUT,
            ),
            (
                Command::Draft(crate::protocol::SaveDraft::default()),
                ANSWER_TIMEOUT,
            ),
            (
                Command::Mark {
                    messages: Vec::new(),
                    mark_as: "read".to_owned(),
                },
                ANSWER_TIMEOUT,
            ),
        ] {
            let folder = tempfile::tempdir().unwrap();
            let socket = folder.path().join("daemon.sock");
            let daemon = silent(&socket);
            let started = tokio::time::Instant::now();
            let asked = ask(&socket, Path::new("missive-is-not-started"), command).await;
            assert!(asked.is_err());
            let elapsed = started.elapsed();
            assert!(
                elapsed >= waited && elapsed < waited + Duration::from_secs(1),
                "{elapsed:?} for {waited:?}"
            );
            daemon.abort();
        }
        assert_eq!(QUEUING_TIMEOUT.as_secs(), 120);
    }

    #[tokio::test(start_paused = true)]
    async fn the_error_on_a_message_that_may_be_queued_says_so_and_never_says_not_sent() {
        let folder = tempfile::tempdir().unwrap();
        let socket = folder.path().join("daemon.sock");
        let daemon = silent(&socket);
        let error = ask(&socket, Path::new("missive-is-not-started"), send())
            .await
            .unwrap_err();
        assert!(error.contains("may have been queued"), "{error}");
        assert!(error.contains("missive status"), "{error}");
        assert!(error.contains("before sending it again"), "{error}");
        assert!(!error.contains("not sent"), "{error}");
        daemon.abort();

        let socket = folder.path().join("other.sock");
        let daemon = silent(&socket);
        let error = ask(
            &socket,
            Path::new("missive-is-not-started"),
            Command::Subscribe,
        )
        .await
        .unwrap_err();
        assert!(!error.contains("queued"), "{error}");
        daemon.abort();
    }

    #[tokio::test]
    async fn an_answer_in_time_is_read() {
        let folder = tempfile::tempdir().unwrap();
        let socket = folder.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut lines = BufReader::new(stream);
            let mut request = String::new();
            lines.read_line(&mut request).await.unwrap();
            lines
                .get_mut()
                .write_all(answer_line().as_bytes())
                .await
                .unwrap();
        });
        let answer = ask_within(
            &socket,
            Path::new("missive-is-not-started"),
            Command::Subscribe,
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert!(answer.ok);
    }

    /// The pids of this process's children that have exited and were not waited for.
    fn zombies() -> Vec<u32> {
        let mut children = String::new();
        for task in std::fs::read_dir("/proc/self/task").unwrap() {
            let file = task.unwrap().path().join("children");
            children.push_str(&std::fs::read_to_string(file).unwrap_or_default());
            children.push(' ');
        }
        children
            .split_whitespace()
            .filter_map(|pid| pid.parse::<u32>().ok())
            .filter(|pid| {
                std::fs::read_to_string(format!("/proc/{pid}/stat"))
                    .is_ok_and(|stat| stat.rsplit(") ").next().is_some_and(|r| r.starts_with('Z')))
            })
            .collect()
    }

    async fn until_none_is_left_over() {
        let started = Instant::now();
        while !zombies().is_empty() {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "a daemon the client started stays a zombie"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn a_daemon_the_client_started_is_reaped_when_it_exits() {
        // `sleep daemon` is refused at once, as a daemon that dies right after its start: this
        // process keeps no zombie of it, whether it came up (a socket answers) or not.
        let folder = tempfile::tempdir().unwrap();
        let log = folder.path().join("daemon.log");
        let socket = folder.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let connected = start_and_wait(&socket, Path::new("sleep"), &log).await;
        assert!(connected.is_ok());
        until_none_is_left_over().await;

        drop(listener);
        std::fs::remove_file(&socket).unwrap();
        let never = start_and_wait(&socket, Path::new("sleep"), &log).await;
        assert!(never.is_err());
        until_none_is_left_over().await;
    }
}
