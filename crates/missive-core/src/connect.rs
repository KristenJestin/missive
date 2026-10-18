//! Connecting to an account's servers, and why it failed: the connection states of an account
//! that are errors (`unreachable`, `auth-refused`), plus a refused certificate.

use std::fmt;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, RootCertStore};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

use crate::account::{Password, Security, Server};
use crate::tls::pinned_client_config;

/// How long a login check may take, from the first connection to the server's last answer.
const CHECK_TIMEOUT: Duration = Duration::from_secs(30);

/// Why a connection to a server failed. Its [`state`](ConnectError::state) is the account's
/// connection state; nothing here ever holds a password.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectError {
    /// The server could not be reached or did not speak as expected; logging in again would not
    /// help.
    Unreachable(String),
    /// The server refused the user name or the password.
    AuthRefused,
    /// The server's certificate is trusted neither by the system store nor by the certificate
    /// imported for the account.
    CertificateRefused,
}

impl ConnectError {
    pub fn state(&self) -> &'static str {
        match self {
            ConnectError::Unreachable(_) => "unreachable",
            ConnectError::AuthRefused => "auth-refused",
            ConnectError::CertificateRefused => "certificate-refused",
        }
    }
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: ", self.state())?;
        match self {
            ConnectError::Unreachable(reason) => write!(f, "{reason}"),
            ConnectError::AuthRefused => {
                f.write_str("the server refused the user name or password")
            }
            ConnectError::CertificateRefused => f.write_str(
                "the server's certificate is not trusted: it is neither signed by an authority of \
                 the system nor the certificate imported for this account. If this server is \
                 yours (a local bridge, for example), import its certificate with \
                 --certificate <file.pem>",
            ),
        }
    }
}

impl std::error::Error for ConnectError {}

impl From<io::Error> for ConnectError {
    fn from(error: io::Error) -> Self {
        let tls = error
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<rustls::Error>());
        match tls {
            Some(rustls::Error::InvalidCertificate(_)) => ConnectError::CertificateRefused,
            Some(tls) => ConnectError::Unreachable(format!("TLS failed ({tls})")),
            None => ConnectError::Unreachable(error.to_string()),
        }
    }
}

type Result<T> = std::result::Result<T, ConnectError>;

/// What a connection trusts: the certificate imported for the account, pinned, or else the
/// system's certificate store and nothing more.
pub fn client_config(pinned: Option<CertificateDer<'static>>) -> Arc<ClientConfig> {
    if let Some(certificate) = pinned {
        return Arc::new(pinned_client_config(certificate));
    }
    let mut roots = RootCertStore::empty();
    // A certificate the store cannot read is skipped, as every TLS client does.
    roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
    if roots.is_empty() {
        // Said apart from the refused certificate that will follow: importing one is then the
        // only way in, but the system's store is what is wrong.
        eprintln!(
            "missive: no system certificates were found, so no server certificate can be \
             verified (check the ca-certificates of this system, or import the server's own \
             certificate)"
        );
    }
    Arc::new(
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .expect("ring supports the default protocol versions")
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

/// Logs in to the IMAP server and out again.
pub async fn check_imap(
    server: &Server,
    user: &str,
    password: &Password,
    tls: Arc<ClientConfig>,
) -> Result<()> {
    within(CHECK_TIMEOUT, async {
        let tcp = connect(server).await?;
        let stream = match server.security {
            Security::Tls => {
                let stream = handshake(server, tls, tcp).await?;
                let mut client = async_imap::Client::new(stream);
                greeting(&mut client).await?;
                client.into_inner()
            }
            Security::Starttls => {
                let mut client = async_imap::Client::new(tcp);
                greeting(&mut client).await?;
                client
                    .run_command_and_check_ok("STARTTLS", None)
                    .await
                    .map_err(imap_error)?;
                handshake(server, tls, client.into_inner()).await?
            }
        };
        let mut session = async_imap::Client::new(stream)
            .login(user, password.expose())
            .await
            .map_err(|(error, _)| match error {
                async_imap::error::Error::No(text) => login_refusal(&text),
                error => imap_error(error),
            })?;
        session.logout().await.map_err(imap_error)
    })
    .await
}

/// A `NO` to `LOGIN` is a refusal of the credentials when it has no response code, or the code
/// says so (RFC 5530: `AUTHENTICATIONFAILED`, `AUTHORIZATIONFAILED`). Any other code
/// (`UNAVAILABLE`, which a bridge answers while it starts, for one) says the server cannot log
/// anyone in right now, which asking for the password again would not mend.
fn login_refusal(text: &str) -> ConnectError {
    // async-imap puts the answer in a string: `code:` holds a code it knows, and a code it does
    // not know stays at the start of `info:`.
    let code = match text.strip_prefix("code: None, info: Some(\"[") {
        Some(rest) => rest
            .split(']')
            .next()
            .and_then(|c| c.split_whitespace().next()),
        None if text.starts_with("code: None") => None,
        None => Some("a code"),
    };
    match code.map(str::to_ascii_uppercase).as_deref() {
        None | Some("AUTHENTICATIONFAILED" | "AUTHORIZATIONFAILED") => ConnectError::AuthRefused,
        Some(_) => ConnectError::Unreachable(
            "the IMAP server cannot log in now (it refused with a code other than a wrong \
             user name or password)"
                .to_owned(),
        ),
    }
}

async fn greeting<S>(client: &mut async_imap::Client<S>) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + fmt::Debug + Send,
{
    match client.read_response().await {
        Ok(Some(_)) => Ok(()),
        Err(error) => Err(error.into()),
        Ok(None) => Err(ConnectError::Unreachable(
            "the IMAP server closed the connection".to_owned(),
        )),
    }
}

/// Server texts are left out: they are not needed to tell the states apart.
fn imap_error(error: async_imap::error::Error) -> ConnectError {
    match error {
        async_imap::error::Error::Io(error) => error.into(),
        async_imap::error::Error::ConnectionLost => {
            ConnectError::Unreachable("the IMAP server closed the connection".to_owned())
        }
        _ => ConnectError::Unreachable("the server did not answer as an IMAP server".to_owned()),
    }
}

/// Logs in to the SMTP server and quits. Written here rather than with `lettre`, which cannot
/// take the pinned rustls configuration of [`client_config`].
pub async fn check_smtp(
    server: &Server,
    user: &str,
    password: &Password,
    tls: Arc<ClientConfig>,
) -> Result<()> {
    within(CHECK_TIMEOUT, async {
        let tcp = connect(server).await?;
        let mut smtp = match server.security {
            Security::Tls => BufReader::new(handshake(server, tls, tcp).await?),
            Security::Starttls => {
                let mut clear = BufReader::new(tcp);
                expect(&mut clear, 220).await?;
                command(&mut clear, "EHLO localhost", 250).await?;
                command(&mut clear, "STARTTLS", 220).await?;
                BufReader::new(handshake(server, tls, clear.into_inner()).await?)
            }
        };
        if server.security == Security::Tls {
            expect(&mut smtp, 220).await?;
        }
        let extensions = command(&mut smtp, "EHLO localhost", 250).await?;
        let mechanisms: Vec<&str> = extensions
            .iter()
            .filter_map(|line| line.strip_prefix("AUTH "))
            .flat_map(str::split_whitespace)
            .collect();
        let (user, password) = (user.as_bytes(), password.expose().as_bytes());
        let code = if mechanisms.contains(&"PLAIN") {
            let token = STANDARD.encode([b"\0", user, b"\0", password].concat());
            send(&mut smtp, &format!("AUTH PLAIN {token}")).await?;
            reply(&mut smtp).await?.0
        } else if mechanisms.contains(&"LOGIN") {
            command(&mut smtp, "AUTH LOGIN", 334).await?;
            send(&mut smtp, &STANDARD.encode(user)).await?;
            // Some servers refuse an unknown user name here, before asking for a password.
            match reply(&mut smtp).await?.0 {
                334 => {
                    send(&mut smtp, &STANDARD.encode(password)).await?;
                    reply(&mut smtp).await?.0
                }
                code => code,
            }
        } else {
            return Err(ConnectError::Unreachable(
                "the SMTP server offers no login Missive knows (PLAIN or LOGIN)".to_owned(),
            ));
        };
        match code {
            235 => {}
            535 => return Err(ConnectError::AuthRefused),
            code => return Err(unexpected(code)),
        }
        // The login is what was checked; a server that drops the line on QUIT changes nothing.
        let _ = send(&mut smtp, "QUIT").await;
        Ok(())
    })
    .await
}

async fn send<S: AsyncRead + AsyncWrite + Unpin>(
    smtp: &mut BufReader<S>,
    line: &str,
) -> Result<()> {
    let stream = smtp.get_mut();
    stream.write_all(format!("{line}\r\n").as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

/// Sends a command and expects `code`; returns the reply's text lines.
async fn command<S: AsyncRead + AsyncWrite + Unpin>(
    smtp: &mut BufReader<S>,
    line: &str,
    code: u16,
) -> Result<Vec<String>> {
    send(smtp, line).await?;
    expect(smtp, code).await
}

async fn expect<S: AsyncRead + Unpin>(smtp: &mut BufReader<S>, code: u16) -> Result<Vec<String>> {
    let (answered, lines) = reply(smtp).await?;
    if answered != code {
        return Err(unexpected(answered));
    }
    Ok(lines)
}

/// Reads one reply, multi-line or not: its code and the text of each line.
async fn reply<S: AsyncRead + Unpin>(smtp: &mut BufReader<S>) -> Result<(u16, Vec<String>)> {
    let mut lines = Vec::new();
    loop {
        let mut line = String::new();
        if smtp.read_line(&mut line).await? == 0 {
            return Err(ConnectError::Unreachable(
                "the SMTP server closed the connection".to_owned(),
            ));
        }
        let line = line.trim_end_matches(['\r', '\n']);
        let code = line.get(..3).and_then(|code| code.parse::<u16>().ok());
        let (Some(code), separator) = (code, line.as_bytes().get(3).copied()) else {
            return Err(ConnectError::Unreachable(
                "the server did not answer as an SMTP server".to_owned(),
            ));
        };
        lines.push(line.get(4..).unwrap_or_default().to_owned());
        if separator != Some(b'-') {
            return Ok((code, lines));
        }
    }
}

fn unexpected(code: u16) -> ConnectError {
    ConnectError::Unreachable(format!("the SMTP server answered {code}"))
}

async fn connect(server: &Server) -> Result<TcpStream> {
    TcpStream::connect((server.host.as_str(), server.port))
        .await
        .map_err(|e| ConnectError::Unreachable(format!("cannot connect ({e})")))
}

async fn handshake(
    server: &Server,
    tls: Arc<ClientConfig>,
    tcp: TcpStream,
) -> Result<TlsStream<TcpStream>> {
    let name = ServerName::try_from(server.host.clone())
        .map_err(|_| ConnectError::Unreachable("the host name is not valid".to_owned()))?;
    Ok(TlsConnector::from(tls).connect(name, tcp).await?)
}

async fn within<T>(timeout: Duration, check: impl Future<Output = Result<T>>) -> Result<T> {
    tokio::time::timeout(timeout, check)
        .await
        .unwrap_or_else(|_| {
            Err(ConnectError::Unreachable(format!(
                "no answer within {} seconds",
                timeout.as_secs()
            )))
        })
}
