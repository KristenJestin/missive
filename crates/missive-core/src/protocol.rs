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
    /// Writes a message into the account's outbox, to be sent as soon as its server can be
    /// reached; answered once it is in the outbox.
    Send(Send),
    /// Saves a draft, a new one or a new version of one, and with `send` sends it; answered once
    /// it is in the local copy (and in the outbox).
    Draft(SaveDraft),
    /// Puts a `failed` message of the outbox back in the queue, or with `yes` an `uncertain` one
    /// (it may have arrived already); answered once queued.
    Resend {
        outbox: i64,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        yes: bool,
    },
    /// Removes a `failed` or `uncertain` message from the outbox, for good.
    Discard { outbox: i64 },
}

/// A draft to save: the fields given replace those of the draft named, the others are kept.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaveDraft {
    pub account: String,
    /// The draft to pick up again: its Message-ID, or the identifier of a message of the Drafts
    /// folder. `None` for a new draft.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cc: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bcc: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachments: Option<Vec<Attached>>,
    /// The Message-ID, without angle brackets, of the message the draft answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<String>,
    /// The `References` of the draft, Message-IDs without angle brackets; none given keeps its
    /// own.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub references: Vec<String>,
    /// Sends the draft (through the outbox); it is removed from the Drafts folder once the
    /// server took the message.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub send: bool,
    /// With `send`: the identifier of the message the draft is a reply to, marked answered once
    /// the server took the reply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answers: Option<String>,
    /// With `send`: the identifier of the message the draft forwards, marked forwarded
    /// (`$Forwarded`) once the server took the forward.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forwards: Option<String>,
}

/// A message to send: addresses as `name@example.org` or `Name <name@example.org>`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Send {
    pub account: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub to: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cc: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bcc: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(default)]
    pub body: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attached>,
    /// The `Message-ID`, without angle brackets, of the message this one answers, and the
    /// `References` of its thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub references: Vec<String>,
    /// The identifier of the message this one is a reply to: once the server took the reply,
    /// that message is marked answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answers: Option<String>,
    /// The identifier of the message this one forwards: once the server took the forward, that
    /// message is marked forwarded (`$Forwarded`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forwards: Option<String>,
}

/// A file attached to a message: its name, and its content in base64.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attached {
    pub name: String,
    #[serde(with = "base64_bytes")]
    pub data: Vec<u8>,
}

mod base64_bytes {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(deserializer)?;
        STANDARD.decode(text).map_err(serde::de::Error::custom)
    }
}

impl Command {
    pub fn name(&self) -> &'static str {
        match self {
            Command::Sync { .. } => "sync",
            Command::Subscribe => "subscribe",
            Command::Mark { .. } => "mark",
            Command::Move { .. } => "move",
            Command::Delete { .. } => "delete",
            Command::Send(_) => "send",
            Command::Draft(_) => "draft",
            Command::Resend { .. } => "resend",
            Command::Discard { .. } => "discard",
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
    /// The numbers of the actions recorded, as their `action` events give them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actions: Option<Vec<i64>>,
    /// The number of a message put in the outbox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outbox: Option<i64>,
    /// The `Message-ID` of a message put in the outbox, or of a draft saved, without angle
    /// brackets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    /// A `draft` that picked up another client's draft: the draft named, which stays on the
    /// server as it is, beside the new draft `message_id` names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kept: Option<String>,
    /// When not `ok`: `bad-request`, `unsupported-protocol`, `unknown-request`,
    /// `no-such-account`, `no-such-message`, `failed`, or why an action was refused
    /// (`docs/protocol.md`).
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
        Some(
            "sync"
                | "subscribe"
                | "mark"
                | "move"
                | "delete"
                | "send"
                | "draft"
                | "resend"
                | "discard"
        )
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
    fn outbox_requests_read_as_documented() {
        assert_eq!(
            read_request(r#"{"protocol":1,"request":"resend","outbox":3}"#),
            Ok(Request {
                protocol: 1,
                command: Command::Resend {
                    outbox: 3,
                    yes: false
                }
            })
        );
        assert_eq!(
            read_request(r#"{"protocol":1,"request":"resend","outbox":3,"yes":true}"#),
            Ok(Request {
                protocol: 1,
                command: Command::Resend {
                    outbox: 3,
                    yes: true
                }
            })
        );
        assert_eq!(
            read_request(r#"{"protocol":1,"request":"discard","outbox":3}"#),
            Ok(Request {
                protocol: 1,
                command: Command::Discard { outbox: 3 }
            })
        );
        assert_eq!(
            read_request(r#"{"protocol":1,"request":"discard"}"#)
                .unwrap_err()
                .error
                .unwrap(),
            "bad-request"
        );
    }

    #[test]
    fn a_send_request_reads_with_its_attachments_in_base64() {
        let line = r#"{"protocol":1,"request":"send","account":"alice","to":["Zoé <zoe@example.com>"],"subject":"Hi","body":"Hello","attachments":[{"name":"a.bin","data":"AAH/"}]}"#;
        let request = Request {
            protocol: 1,
            command: Command::Send(Send {
                account: "alice".to_owned(),
                to: vec!["Zoé <zoe@example.com>".to_owned()],
                subject: Some("Hi".to_owned()),
                body: "Hello".to_owned(),
                attachments: vec![Attached {
                    name: "a.bin".to_owned(),
                    data: vec![0, 1, 255],
                }],
                ..Send::default()
            }),
        };
        assert_eq!(read_request(line), Ok(request.clone()));
        assert_eq!(serde_json::to_string(&request).unwrap(), line);
        assert_eq!(
            read_request(r#"{"protocol":1,"request":"send","account":"a","attachments":[{"name":"x","data":"!"}]}"#)
                .unwrap_err()
                .error
                .unwrap(),
            "bad-request"
        );
    }

    #[test]
    fn a_forward_request_names_the_message_it_forwards() {
        let line = r#"{"protocol":1,"request":"send","account":"alice","body":"","forwards":"alice/1/2/3"}"#;
        let request = Request {
            protocol: 1,
            command: Command::Send(Send {
                account: "alice".to_owned(),
                forwards: Some("alice/1/2/3".to_owned()),
                ..Send::default()
            }),
        };
        assert_eq!(read_request(line), Ok(request.clone()));
        assert_eq!(serde_json::to_string(&request).unwrap(), line);

        let line = r#"{"protocol":1,"request":"draft","account":"alice","draft":"1@example.org","send":true,"forwards":"alice/1/2/3"}"#;
        let Ok(Request {
            command: Command::Draft(draft),
            ..
        }) = read_request(line)
        else {
            panic!("a draft request")
        };
        assert_eq!(draft.forwards.as_deref(), Some("alice/1/2/3"));
    }

    #[test]
    fn a_reply_request_names_its_thread_and_the_message_it_answers() {
        let line = r#"{"protocol":1,"request":"send","account":"alice","body":"","in_reply_to":"a@example.com","references":["r@example.com","a@example.com"],"answers":"alice/1/2/3"}"#;
        let request = Request {
            protocol: 1,
            command: Command::Send(Send {
                account: "alice".to_owned(),
                in_reply_to: Some("a@example.com".to_owned()),
                references: vec!["r@example.com".to_owned(), "a@example.com".to_owned()],
                answers: Some("alice/1/2/3".to_owned()),
                ..Send::default()
            }),
        };
        assert_eq!(read_request(line), Ok(request.clone()));
        assert_eq!(serde_json::to_string(&request).unwrap(), line);
    }

    #[test]
    fn a_draft_request_reads_with_only_the_fields_given() {
        let line = r#"{"protocol":1,"request":"draft","account":"alice","draft":"1@example.org","subject":"Hi","send":true}"#;
        let request = Request {
            protocol: 1,
            command: Command::Draft(SaveDraft {
                account: "alice".to_owned(),
                draft: Some("1@example.org".to_owned()),
                subject: Some("Hi".to_owned()),
                send: true,
                ..SaveDraft::default()
            }),
        };
        assert_eq!(read_request(line), Ok(request.clone()));
        assert_eq!(serde_json::to_string(&request).unwrap(), line);
        assert_eq!(
            read_request(r#"{"protocol":1,"request":"draft","account":"a","to":[]}"#),
            Ok(Request {
                protocol: 1,
                command: Command::Draft(SaveDraft {
                    account: "a".to_owned(),
                    to: Some(Vec::new()),
                    ..SaveDraft::default()
                })
            })
        );
    }

    #[test]
    fn a_draft_request_may_name_the_message_it_answers() {
        let line =
            r#"{"protocol":1,"request":"draft","account":"alice","in_reply_to":"1@example.org"}"#;
        let request = Request {
            protocol: 1,
            command: Command::Draft(SaveDraft {
                account: "alice".to_owned(),
                in_reply_to: Some("1@example.org".to_owned()),
                ..SaveDraft::default()
            }),
        };
        assert_eq!(read_request(line), Ok(request.clone()));
        assert_eq!(serde_json::to_string(&request).unwrap(), line);
    }

    #[test]
    fn a_draft_sent_may_name_the_message_it_is_a_reply_to() {
        let line = r#"{"protocol":1,"request":"draft","account":"alice","draft":"1@example.org","send":true,"answers":"alice/1/2/3"}"#;
        let request = Request {
            protocol: 1,
            command: Command::Draft(SaveDraft {
                account: "alice".to_owned(),
                draft: Some("1@example.org".to_owned()),
                send: true,
                answers: Some("alice/1/2/3".to_owned()),
                ..SaveDraft::default()
            }),
        };
        assert_eq!(read_request(line), Ok(request.clone()));
        assert_eq!(serde_json::to_string(&request).unwrap(), line);
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
