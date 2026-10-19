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
    let log = log_path()?;
    let mut child = start(program, &log)?;
    let started = Instant::now();
    loop {
        if let Ok(stream) = UnixStream::connect(socket).await {
            return Ok(stream);
        }
        // Another `sync` may have started one first: this one then stops, and that one answers.
        let exited = child.try_wait().ok().flatten().is_some();
        if (exited && started.elapsed() > Duration::from_secs(2))
            || started.elapsed() > Duration::from_secs(10)
        {
            return Err(format!(
                "the daemon did not start (its log is {})",
                log.display()
            ));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
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

/// Sends one request to the daemon on `socket`, starting `program daemon` when none answers, and
/// reads its answer, for at most [`ANSWER_TIMEOUT`].
pub async fn ask(socket: &Path, program: &Path, command: Command) -> Result<Answer, String> {
    ask_within(socket, program, command, ANSWER_TIMEOUT).await
}

async fn ask_within(
    socket: &Path,
    program: &Path,
    command: Command,
    timeout: Duration,
) -> Result<Answer, String> {
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
            format!(
                "the daemon did not answer within {} seconds",
                timeout.as_secs().max(1)
            )
        })?
        .map_err(|e| e.to_string())?;
    serde_json::from_str(&line)
        .map_err(|_| "the daemon gave an answer this missive cannot read".to_owned())
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
}
