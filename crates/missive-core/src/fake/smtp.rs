//! The SMTP side of the fake server.

use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_rustls::TlsAcceptor;

use super::{Io, Shared, SmtpMessage};

const HOST: &str = "fake.example.org";

pub(super) async fn serve(
    shared: Arc<Shared>,
    acceptor: TlsAcceptor,
    stream: Box<dyn Io>,
    mut tls: bool,
) {
    let mut io = BufReader::new(stream);
    if reply(&mut io, &format!("220 {HOST} ESMTP fake SMTP ready"))
        .await
        .is_err()
    {
        return;
    }
    let mut session = Session::default();
    loop {
        let Some(line) = read_line(&mut io).await else {
            return;
        };
        if let Some(delay) = shared.smtp_delay(&line) {
            tokio::time::sleep(delay).await;
        }
        let (verb, argument) = line.split_once(' ').unwrap_or((&line, ""));
        let verb = verb.to_ascii_uppercase();
        if matches!(verb.as_str(), "AUTH" | "MAIL" | "RCPT" | "DATA")
            && let Some(Some(answer)) = shared.smtp_rule(&line)
        {
            if reply(&mut io, &answer).await.is_err() {
                return;
            }
            continue;
        }
        let answer = match verb.as_str() {
            "EHLO" => {
                let mut lines = vec![HOST.to_owned()];
                if tls {
                    let mechanisms = if shared.smtp_login_only {
                        "LOGIN"
                    } else {
                        "PLAIN LOGIN"
                    };
                    lines.push(format!("AUTH {mechanisms}"));
                } else {
                    lines.push("STARTTLS".to_owned());
                }
                lines.push("8BITMIME".to_owned());
                let last = lines.len() - 1;
                lines
                    .iter()
                    .enumerate()
                    .map(|(i, l)| format!("250{}{l}", if i == last { ' ' } else { '-' }))
                    .collect::<Vec<_>>()
                    .join("\r\n")
            }
            "HELO" => format!("250 {HOST}"),
            "STARTTLS" if !tls => {
                if reply(&mut io, "220 2.0.0 ready to start TLS")
                    .await
                    .is_err()
                {
                    return;
                }
                let Some(stream) = super::start_tls(io.into_inner(), &acceptor).await else {
                    return;
                };
                io = BufReader::new(stream);
                tls = true;
                // RFC 3207: everything learnt before TLS is forgotten.
                session = Session::default();
                continue;
            }
            "AUTH" if !tls => "538 5.7.11 encryption required".to_owned(),
            "AUTH" if session.user.is_some() => "503 5.5.1 already authenticated".to_owned(),
            "AUTH" => match authenticate(&mut io, argument, &shared).await {
                None => return,
                Some(Some((user, password))) if shared.authenticate(&user, &password) => {
                    session.user = Some(user);
                    "235 2.7.0 authentication successful".to_owned()
                }
                Some(Some(_)) => "535 5.7.8 authentication credentials invalid".to_owned(),
                Some(None) => "501 5.5.2 cannot decode the credentials".to_owned(),
            },
            "MAIL" if session.user.is_none() => "530 5.7.0 authentication required".to_owned(),
            "MAIL" => match address(argument, "FROM:") {
                Some(from) => {
                    session.from = Some(from);
                    session.to.clear();
                    "250 2.1.0 OK".to_owned()
                }
                None => "501 5.5.4 MAIL FROM:<address>".to_owned(),
            },
            "RCPT" if session.from.is_none() => "503 5.5.1 MAIL first".to_owned(),
            "RCPT" => match address(argument, "TO:") {
                Some(to) => {
                    session.to.push(to);
                    "250 2.1.5 OK".to_owned()
                }
                None => "501 5.5.4 RCPT TO:<address>".to_owned(),
            },
            "DATA" if session.to.is_empty() => "503 5.5.1 RCPT first".to_owned(),
            "DATA" => {
                if reply(&mut io, "354 end data with <CR><LF>.<CR><LF>")
                    .await
                    .is_err()
                {
                    return;
                }
                let Some(data) = read_data(&mut io).await else {
                    return;
                };
                if let Some(delay) = shared.smtp_delay(".") {
                    tokio::time::sleep(delay).await;
                }
                let message = SmtpMessage {
                    from: session.from.take().unwrap_or_default(),
                    to: std::mem::take(&mut session.to),
                    data,
                };
                match shared.smtp_rule(".") {
                    Some(Some(answer)) => answer,
                    Some(None) => {
                        shared.state().smtp_messages.push(message);
                        let _ = io.shutdown().await;
                        return;
                    }
                    None => {
                        shared.state().smtp_messages.push(message);
                        "250 2.0.0 OK queued".to_owned()
                    }
                }
            }
            "RSET" => {
                session.from = None;
                session.to.clear();
                "250 2.0.0 OK".to_owned()
            }
            "NOOP" => "250 2.0.0 OK".to_owned(),
            "QUIT" => {
                let _ = reply(&mut io, "221 2.0.0 bye").await;
                return;
            }
            _ => "500 5.5.2 unknown command".to_owned(),
        };
        if reply(&mut io, &answer).await.is_err() {
            return;
        }
    }
}

#[derive(Default)]
struct Session {
    user: Option<String>,
    from: Option<String>,
    to: Vec<String>,
}

async fn reply(io: &mut BufReader<Box<dyn Io>>, answer: &str) -> std::io::Result<()> {
    io.write_all(format!("{answer}\r\n").as_bytes()).await?;
    io.flush().await
}

async fn read_line(io: &mut BufReader<Box<dyn Io>>) -> Option<String> {
    let mut line = Vec::new();
    match io.read_until(b'\n', &mut line).await {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(
            String::from_utf8_lossy(&line)
                .trim_end_matches(['\r', '\n'])
                .to_owned(),
        ),
    }
}

/// Runs `AUTH PLAIN` or `AUTH LOGIN`. `None` when the connection is over, `Some(None)` when the
/// credentials cannot be decoded.
async fn authenticate(
    io: &mut BufReader<Box<dyn Io>>,
    argument: &str,
    shared: &Shared,
) -> Option<Option<(String, String)>> {
    let (mechanism, initial) = argument.split_once(' ').unwrap_or((argument, ""));
    match mechanism.to_ascii_uppercase().as_str() {
        "PLAIN" => {
            let response = if initial.is_empty() {
                reply(io, "334 ").await.ok()?;
                read_line(io).await?
            } else {
                initial.to_owned()
            };
            let decoded = decode(&response);
            // authorization identity, user, password
            Some(decoded.and_then(|text| {
                let mut parts = text.split('\0');
                let (_, user, password) = (parts.next()?, parts.next()?, parts.next()?);
                Some((user.to_owned(), password.to_owned()))
            }))
        }
        "LOGIN" => {
            let user = if initial.is_empty() {
                reply(io, &format!("334 {}", STANDARD.encode("Username:")))
                    .await
                    .ok()?;
                read_line(io).await?
            } else {
                initial.to_owned()
            };
            if shared.smtp_login_only && !decode(&user).is_some_and(|user| shared.knows(&user)) {
                // Refused at the user name step, before any password is asked for.
                return Some(Some((String::new(), String::new())));
            }
            reply(io, &format!("334 {}", STANDARD.encode("Password:")))
                .await
                .ok()?;
            let password = read_line(io).await?;
            Some(decode(&user).zip(decode(&password)))
        }
        _ => Some(None),
    }
}

fn decode(text: &str) -> Option<String> {
    String::from_utf8(STANDARD.decode(text.trim()).ok()?).ok()
}

/// The address of `FROM:<a@example.org>` or `TO:<…>`, parameters after it ignored.
fn address(argument: &str, prefix: &str) -> Option<String> {
    let rest = argument.get(..prefix.len())?;
    if !rest.eq_ignore_ascii_case(prefix) {
        return None;
    }
    let rest = argument[prefix.len()..].trim_start();
    let inner = rest.strip_prefix('<')?;
    let end = inner.find('>')?;
    Some(inner[..end].to_owned())
}

/// Reads the data of a message up to its end, `CRLF . CRLF` as RFC 5321 says (a real server
/// takes nothing less), removing the dot stuffing.
async fn read_data(io: &mut BufReader<Box<dyn Io>>) -> Option<Vec<u8>> {
    let mut data = Vec::new();
    // The CRLF that ended `DATA` counts as the one before a `.` on the first line.
    let mut after_crlf = true;
    loop {
        let mut line = Vec::new();
        if io.read_until(b'\n', &mut line).await.ok()? == 0 {
            return None;
        }
        if after_crlf && line == b".\r\n" {
            return Some(data);
        }
        after_crlf = line.ends_with(b"\r\n");
        let line = line.strip_prefix(b".").unwrap_or(&line);
        data.extend_from_slice(line);
    }
}
