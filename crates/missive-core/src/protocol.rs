//! How programs talk to the daemon: newline-delimited JSON on a Unix socket in
//! `$XDG_RUNTIME_DIR/missive/`. `docs/protocol.md` is the documentation of record.

use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The protocol version this build speaks. A request names the version it was written for; the
/// daemon refuses another one rather than misread it.
pub const PROTOCOL: u32 = 1;

/// `$XDG_RUNTIME_DIR/missive`: the daemon's socket and pid file. There is no fallback: a runtime
/// folder elsewhere would not be private to the user, nor emptied when they log out.
pub fn runtime_dir() -> io::Result<PathBuf> {
    match std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) {
        Some(path) if path.is_absolute() => Ok(path.join("missive")),
        _ => Err(io::Error::new(
            io::ErrorKind::NotFound,
            "XDG_RUNTIME_DIR is not set, so there is no private folder for the daemon's socket",
        )),
    }
}

/// The daemon's socket.
pub fn socket_path() -> io::Result<PathBuf> {
    Ok(runtime_dir()?.join("daemon.sock"))
}

/// The file the running daemon holds locked, with its pid in it; empty when it stopped cleanly.
pub fn pid_path() -> io::Result<PathBuf> {
    Ok(runtime_dir()?.join("daemon.pid"))
}

/// A request, one line of JSON.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub protocol: u32,
    #[serde(flatten)]
    pub command: Command,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "request", rename_all = "kebab-case")]
pub enum Command {
    /// Syncs every account, or one, now; answered when that sync ended.
    Sync {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<String>,
    },
    /// Answered at once; the daemon then writes `event` lines on the same connection, the present
    /// first (see `docs/events.md`).
    Subscribe,
    /// Marks messages `read`, `unread`, `flagged` or `unflagged`; answered once recorded.
    Mark {
        messages: Vec<String>,
        #[serde(rename = "as")]
        mark_as: String,
    },
    /// Moves messages to a folder, named by its role or its name; answered once recorded.
    Move { messages: Vec<String>, to: String },
    /// Moves messages to the trash, or with `permanently` deletes them for good from it;
    /// answered once recorded.
    Delete {
        messages: Vec<String>,
        #[serde(default)]
        permanently: bool,
    },
}

impl Command {
    pub fn name(&self) -> &'static str {
        match self {
            Command::Sync { .. } => "sync",
            Command::Subscribe => "subscribe",
            Command::Mark { .. } => "mark",
            Command::Move { .. } => "move",
            Command::Delete { .. } => "delete",
        }
    }
}

/// An answer, one line of JSON, in the order of the requests.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answer {
    pub protocol: u32,
    /// The request answered, or `None` when it could not be read.
    pub answer: Option<String>,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accounts: Option<Vec<AccountState>>,
    /// The messages an action was recorded for, by identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub messages: Option<Vec<String>>,
    /// When not `ok`: `bad-request`, `unsupported-protocol`, `unknown-request`,
    /// `no-such-account`, `failed`, or why an action was refused (`docs/protocol.md`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// When not `ok`: what went wrong, for people.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// An account and its connection state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountState {
    pub id: String,
    pub state: String,
}

impl Answer {
    pub fn error(answer: Option<&str>, error: &str, message: impl Into<String>) -> Answer {
        Answer {
            protocol: PROTOCOL,
            answer: answer.map(str::to_owned),
            ok: false,
            error: Some(error.to_owned()),
            message: Some(message.into()),
            ..Answer::default()
        }
    }
}

/// Reads a request line. An unreadable request gets the answer to send back.
// Once per request line: the size of an answer on the error path costs nothing worth boxing.
#[allow(clippy::result_large_err)]
pub fn read_request(line: &str) -> Result<Request, Answer> {
    let value: serde_json::Value = serde_json::from_str(line)
        .map_err(|_| Answer::error(None, "bad-request", "the request is not JSON"))?;
    let name = value
        .get("request")
        .and_then(|r| r.as_str())
        .map(str::to_owned);
    let name = name.as_deref();
    match value.get("protocol").and_then(|p| p.as_u64()) {
        Some(version) if version == u64::from(PROTOCOL) => {}
        Some(version) => {
            return Err(Answer::error(
                name,
                "unsupported-protocol",
                format!("this daemon speaks protocol {PROTOCOL}, not {version}"),
            ));
        }
        None => {
            return Err(Answer::error(
                name,
                "bad-request",
                "the request has no protocol version",
            ));
        }
    }
    let known = matches!(
        name,
        Some("sync" | "subscribe" | "mark" | "move" | "delete")
    );
    serde_json::from_value(value).map_err(|_| {
        if known || name.is_none() {
            Answer::error(name, "bad-request", "the request is not well formed")
        } else {
            Answer::error(
                name,
                "unknown-request",
                "this daemon does not know that request",
            )
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sync_request_reads_with_and_without_an_account() {
        assert_eq!(
            read_request(r#"{"protocol":1,"request":"sync"}"#),
            Ok(Request {
                protocol: 1,
                command: Command::Sync { account: None }
            })
        );
        assert_eq!(
            read_request(r#"{"protocol":1,"request":"sync","account":"alice"}"#),
            Ok(Request {
                protocol: 1,
                command: Command::Sync {
                    account: Some("alice".to_owned())
                }
            })
        );
    }

    #[test]
    fn a_subscribe_request_reads_and_writes_as_one_line() {
        let line = r#"{"protocol":1,"request":"subscribe"}"#;
        let request = Request {
            protocol: 1,
            command: Command::Subscribe,
        };
        assert_eq!(read_request(line), Ok(request.clone()));
        assert_eq!(serde_json::to_string(&request).unwrap(), line);
        assert_eq!(
            read_request(r#"{"protocol":1,"request":"subscribe","account":3}"#),
            Ok(request)
        );
        assert_eq!(
            read_request(r#"{"protocol":2,"request":"subscribe"}"#)
                .unwrap_err()
                .error
                .unwrap(),
            "unsupported-protocol"
        );
    }

    #[test]
    fn action_requests_read_as_documented() {
        assert_eq!(
            read_request(r#"{"protocol":1,"request":"mark","messages":["a/1/2/3"],"as":"read"}"#),
            Ok(Request {
                protocol: 1,
                command: Command::Mark {
                    messages: vec!["a/1/2/3".to_owned()],
                    mark_as: "read".to_owned()
                }
            })
        );
        assert_eq!(
            read_request(r#"{"protocol":1,"request":"move","messages":[],"to":"archive"}"#),
            Ok(Request {
                protocol: 1,
                command: Command::Move {
                    messages: Vec::new(),
                    to: "archive".to_owned()
                }
            })
        );
        assert_eq!(
            read_request(r#"{"protocol":1,"request":"delete","messages":["a/1/2/3"]}"#),
            Ok(Request {
                protocol: 1,
                command: Command::Delete {
                    messages: vec!["a/1/2/3".to_owned()],
                    permanently: false
                }
            })
        );
        assert_eq!(
            read_request(r#"{"protocol":1,"request":"move","messages":["a/1/2/3"]}"#)
                .unwrap_err()
                .error
                .unwrap(),
            "bad-request"
        );
    }

    #[test]
    fn a_request_writes_as_one_line() {
        let request = Request {
            protocol: PROTOCOL,
            command: Command::Sync { account: None },
        };
        assert_eq!(
            serde_json::to_string(&request).unwrap(),
            r#"{"protocol":1,"request":"sync"}"#
        );
    }

    #[test]
    fn unreadable_requests_are_told_apart() {
        let error = |line: &str| read_request(line).unwrap_err().error.unwrap();
        assert_eq!(error("{"), "bad-request");
        assert_eq!(error(r#"{"request":"sync"}"#), "bad-request");
        assert_eq!(
            error(r#"{"protocol":9,"request":"sync"}"#),
            "unsupported-protocol"
        );
        assert_eq!(
            error(r#"{"protocol":1,"request":"dance"}"#),
            "unknown-request"
        );
        assert_eq!(
            error(r#"{"protocol":1,"request":"sync","account":3}"#),
            "bad-request"
        );
    }
}
