//! Submitting a message to the account's SMTP server, over TLS or STARTTLS with the account's
//! pinned certificate.
//!
//! The client is Missive's own, beside the login check of [`crate::connect`]: `lettre` (0.11.23,
//! its latest) keeps its rustls configuration to itself, so a pinned certificate marked as a CA
//! cannot be used through it; and what the outbox needs to know is where an exchange stopped,
//! which only a client that sends each step itself can tell. Before the end of data a failure
//! means the server took nothing; after it, the server may have taken the message, and only its
//! answer says whether it did.
//!
//! What is sent: `EHLO`, `STARTTLS` when asked, `AUTH PLAIN` or `AUTH LOGIN` (see
//! [`crate::connect::check_smtp`]), `MAIL FROM`, one `RCPT TO` per recipient (Bcc included),
//! `DATA` with the message's lines ended by CRLF and dot-stuffed, then `QUIT`. Nothing of it is
//! ever logged.

use std::sync::Arc;
use std::time::Duration;

use rustls::ClientConfig;
use tokio::io::AsyncWriteExt;

use crate::account::{Password, Server};
use crate::connect::{ConnectError, SmtpSession, open_smtp, reply, send};

/// How long the server is given to answer `MAIL`, `RCPT` and `DATA`: RFC 5321 asks clients to
/// wait 5 minutes.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// How long the message may take to be written, and the server to answer its end: RFC 5321 asks
/// clients to wait 10 minutes for that answer.
const DATA_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// What became of a submission, once logged in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Submitted {
    /// The server took the message: it answered `250` to its end of data.
    Sent,
    /// The server took nothing, and may take it later: a `4xx`, or the connection lost before the
    /// end of data. `reason` is for people.
    Again { reason: String },
    /// The server refused the message for good (`5xx`): sending it again would be refused again.
    Refused {
        code: u16,
        /// `MAIL FROM`, `RCPT TO`, `DATA` or `end of data`.
        step: &'static str,
    },
    /// The end of data was sent, and no answer came: the server may have taken the message.
    Uncertain { reason: String },
}

/// A message and its envelope.
#[derive(Clone, Copy, Debug)]
pub struct Outgoing<'a> {
    /// `MAIL FROM`.
    pub sender: &'a str,
    /// `RCPT TO`, one each.
    pub recipients: &'a [String],
    pub raw: &'a [u8],
}

/// Submits a message. `sending` is called just before the end of data
/// is written, and the end is written only if it returns `Ok`: the caller records there that what
/// follows may have reached the server. A login that fails is the error; whatever comes after it
/// is a [`Submitted`].
pub async fn submit(
    server: &Server,
    user: &str,
    password: &Password,
    tls: Arc<ClientConfig>,
    message: Outgoing<'_>,
    sending: impl FnOnce() -> Result<(), String>,
) -> Result<Submitted, ConnectError> {
    let mut smtp = open_smtp(server, user, password, tls).await?;
    let submitted = transaction(&mut smtp, message, sending).await;
    if !matches!(submitted, Submitted::Uncertain { .. }) {
        // Polite, not needed: what the server took is already said.
        let _ = tokio::time::timeout(Duration::from_secs(2), send(&mut smtp, "QUIT")).await;
    }
    Ok(submitted)
}

async fn transaction(
    smtp: &mut SmtpSession,
    message: Outgoing<'_>,
    sending: impl FnOnce() -> Result<(), String>,
) -> Submitted {
    let mut steps = vec![("MAIL FROM", format!("MAIL FROM:<{}>", message.sender), 250)];
    for recipient in message.recipients {
        steps.push(("RCPT TO", format!("RCPT TO:<{recipient}>"), 250));
    }
    steps.push(("DATA", "DATA".to_owned(), 354));
    for (step, line, wanted) in steps {
        let answered = tokio::time::timeout(COMMAND_TIMEOUT, async {
            send(smtp, &line).await?;
            reply(smtp).await
        })
        .await;
        let code = match answered {
            Ok(Ok((code, _))) => code,
            Ok(Err(error)) => return lost(error),
            Err(_) => return no_answer(step),
        };
        // RFC 5321 lets a server accept a recipient it forwards with 251.
        if code == wanted || (step == "RCPT TO" && code == 251) {
            continue;
        }
        return refusal(code, step);
    }

    let data = dot_stuffed(message.raw);
    let written = tokio::time::timeout(DATA_TIMEOUT, async {
        smtp.get_mut().write_all(&data).await?;
        smtp.get_mut().flush().await
    })
    .await;
    match written {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return lost(error.into()),
        Err(_) => return no_answer("DATA"),
    }
    if let Err(reason) = sending() {
        return Submitted::Again { reason };
    }
    let ended = tokio::time::timeout(DATA_TIMEOUT, async {
        smtp.get_mut().write_all(b".\r\n").await?;
        smtp.get_mut().flush().await?;
        reply(smtp).await
    })
    .await;
    match ended {
        Ok(Ok((code, _))) if (200..300).contains(&code) => Submitted::Sent,
        Ok(Ok((code, _))) if (400..600).contains(&code) => refusal(code, "end of data"),
        Ok(Ok((code, _))) => Submitted::Uncertain {
            reason: format!("the server answered {code} to the end of the message"),
        },
        Ok(Err(error)) => Submitted::Uncertain {
            reason: format!("the connection was lost before the server answered ({error})"),
        },
        Err(_) => Submitted::Uncertain {
            reason: format!(
                "the server did not answer the end of the message within {} minutes",
                DATA_TIMEOUT.as_secs() / 60
            ),
        },
    }
}

/// A `4xx` is for now, a `5xx` for good; any other code where it is not expected says the server
/// is not following the exchange, which another try may mend.
fn refusal(code: u16, step: &'static str) -> Submitted {
    if (500..600).contains(&code) {
        Submitted::Refused { code, step }
    } else {
        Submitted::Again {
            reason: format!("the server answered {code} to {step}"),
        }
    }
}

fn lost(error: ConnectError) -> Submitted {
    Submitted::Again {
        reason: error.to_string(),
    }
}

fn no_answer(step: &str) -> Submitted {
    Submitted::Again {
        reason: format!("the server did not answer {step} in time"),
    }
}

/// The message as `DATA` sends it, without its final `.` line: every line ended by CRLF, whatever
/// ended it (a bare CR or LF would be refused or misread by a server), and a `.` doubled at the
/// start of a line, so that no line of the message reads as the end of data.
pub fn dot_stuffed(raw: &[u8]) -> Vec<u8> {
    let mut data = Vec::with_capacity(raw.len() + raw.len() / 64 + 2);
    let mut line_start = true;
    let mut bytes = raw.iter().peekable();
    while let Some(&byte) = bytes.next() {
        if line_start && byte == b'.' {
            data.push(b'.');
        }
        match byte {
            b'\r' => {
                bytes.next_if_eq(&&b'\n');
                data.extend_from_slice(b"\r\n");
                line_start = true;
            }
            b'\n' => {
                data.extend_from_slice(b"\r\n");
                line_start = true;
            }
            byte => {
                data.push(byte);
                line_start = false;
            }
        }
    }
    if !line_start {
        data.extend_from_slice(b"\r\n");
    }
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_is_dot_stuffed_and_every_line_ends_with_crlf() {
        assert_eq!(dot_stuffed(b""), b"");
        assert_eq!(dot_stuffed(b"a\r\nb\r\n"), b"a\r\nb\r\n");
        assert_eq!(dot_stuffed(b"a\nb\rc\r\n"), b"a\r\nb\r\nc\r\n");
        assert_eq!(dot_stuffed(b"no end"), b"no end\r\n");
        assert_eq!(
            dot_stuffed(b".\r\n..two\r\nmid.dle\r\n.\n"),
            b"..\r\n...two\r\nmid.dle\r\n..\r\n"
        );
        assert_eq!(dot_stuffed(b".first"), b"..first\r\n");
    }
}
