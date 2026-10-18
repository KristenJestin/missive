mod act;
mod daemon;
mod open;
mod read;
mod watch;

use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Args, Parser, Subcommand, ValueEnum};
use missive_core::account::{
    Account, AccountStore, Listing, Password, Security, Server, read_certificate, valid_id,
};
use missive_core::connect::{ConnectError, check_imap, check_smtp, client_config};
use missive_core::keyring::Keyring;
use missive_core::protocol::{Answer, Command as Request, PROTOCOL};
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

// A release build names its version in MISSIVE_VERSION (the tag it is published under, with its
// `v`); Cargo.toml stays at 0.0.0. A local build has no such name and says what it is.
const VERSION: &str = match option_env!("MISSIVE_VERSION") {
    Some(version) => version,
    None => "development build",
};

#[derive(Parser)]
#[command(name = "missive", version = VERSION, about = "A mail client for Linux")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Add, list and remove accounts.
    #[command(subcommand)]
    Account(AccountCommand),
    /// Sync every account now, or one, and say each one's connection state. Starts the daemon
    /// when none runs.
    Sync {
        /// The account's identifier, as `account list` shows it [default: every account].
        #[arg(long)]
        account: Option<String>,
    },
    /// Print the daemon's events, one JSON object per line, until stopped: new messages, unread
    /// counts, connection states, syncs (see docs/events.md). Never starts the daemon: without
    /// one it says `gone` and waits for it.
    Watch,
    /// Run the daemon in the foreground: it keeps the local copy of every account up to date.
    /// SIGTERM or SIGINT stops it.
    Daemon(DaemonArgs),
    /// Say each account's connection state, last sync and number of messages, and whether the
    /// daemon runs. Reads the local copy only.
    Status {
        /// One JSON object on standard output (see docs/cli.md).
        #[arg(long)]
        json: bool,
    },
    /// Count the unread messages, by account and folder. Reads the local copy only.
    Unread {
        /// One JSON object on standard output (see docs/cli.md).
        #[arg(long)]
        json: bool,
    },
    /// Search the sender, recipients, subject and text of the messages, newest first. Reads the
    /// local copy only.
    Search(read::SearchArgs),
    /// List the messages of a folder, newest first [default: the inbox of each account]. Reads
    /// the local copy only.
    List(read::ListArgs),
    /// Show one message: its headers, its text and its attachments, or with --raw exactly what the
    /// server sent. The HTML is never interpreted. Reads the local copy only; exits with 3 when
    /// the identifier names no message.
    Show(read::ShowArgs),
    /// Open a message in the system's default browser, cleaned of everything that runs or
    /// phones home (see docs/cli.md), from a private file; prints that file's path. Reads the
    /// local copy only; exits with 3 when the identifier names no message.
    Open(open::OpenArgs),
    /// Mark messages read or unread, flagged or not. Shown in the local copy at once, applied on
    /// the server when the account is connected; starts the daemon when none runs.
    Mark(act::MarkArgs),
    /// Move messages to another folder of their account. Shown in the local copy at once, applied
    /// on the server when the account is connected; starts the daemon when none runs.
    Move(act::MoveArgs),
    /// Move messages to the trash, or with --permanently delete them for good from it. Shown in
    /// the local copy at once, applied on the server when the account is connected; starts the
    /// daemon when none runs.
    Delete(act::DeleteArgs),
}

#[derive(Args)]
struct DaemonArgs {
    /// How often, in seconds, the folders other than the inbox are checked; new mail in the inbox
    /// comes at once, through IDLE.
    #[arg(long, value_name = "SECONDS", default_value_t = 300, value_parser = clap::value_parser!(u64).range(1..))]
    check_interval: u64,
    /// How many new messages are fetched, and written to the local copy, at a time, 100 at most
    /// (and never more than 32 MiB of them, but for a single larger message).
    #[arg(long, value_name = "N", default_value_t = 50, value_parser = clap::value_parser!(u64).range(1..=100))]
    batch_size: u64,
}

#[derive(Subcommand)]
enum AccountCommand {
    /// Add an IMAP/SMTP account, after checking that it can log in to both servers.
    ///
    /// What no option gives is asked for, when standard input is a terminal; the password is
    /// read without echo, or from standard input with --password-stdin.
    Add(AddArgs),
    /// List the accounts.
    List {
        /// One JSON array on standard output (see docs/cli.md).
        #[arg(long)]
        json: bool,
    },
    /// Remove an account: its settings, its certificate and its keyring entry.
    Remove {
        /// The account's identifier, as `account list` shows it.
        id: String,
    },
}

#[derive(Args)]
struct AddArgs {
    /// The account's identifier, used to name it on the command line [default: the address].
    #[arg(long)]
    id: Option<String>,
    /// The display name, as in "Alice Example <alice@example.org>".
    #[arg(long)]
    name: Option<String>,
    #[arg(long)]
    address: Option<String>,
    #[arg(long)]
    imap_host: Option<String>,
    /// [default: 993 with tls, 143 with starttls]
    #[arg(long)]
    imap_port: Option<u16>,
    /// [default: tls]
    #[arg(long, value_enum)]
    imap_security: Option<SecurityArg>,
    #[arg(long)]
    smtp_host: Option<String>,
    /// [default: 465 with tls, 587 with starttls]
    #[arg(long)]
    smtp_port: Option<u16>,
    /// [default: tls]
    #[arg(long, value_enum)]
    smtp_security: Option<SecurityArg>,
    /// The login on both servers [default: the address].
    #[arg(long)]
    user: Option<String>,
    /// Read the password from standard input (one line) instead of asking for it.
    #[arg(long)]
    password_stdin: bool,
    /// Trust exactly this server certificate (PEM) for this account, instead of the system's
    /// authorities: for a local bridge with its own self-signed certificate.
    #[arg(long, value_name = "FILE.PEM")]
    certificate: Option<PathBuf>,
}

#[derive(Clone, Copy, ValueEnum)]
enum SecurityArg {
    Starttls,
    Tls,
}

impl From<SecurityArg> for Security {
    fn from(security: SecurityArg) -> Self {
        match security {
            SecurityArg::Starttls => Security::Starttls,
            SecurityArg::Tls => Security::Tls,
        }
    }
}

/// A failure, said to the user as is.
struct Failure(String);

impl<E: std::fmt::Display> From<E> for Failure {
    fn from(error: E) -> Self {
        Failure(error.to_string())
    }
}

type Result<T> = std::result::Result<T, Failure>;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Account(AccountCommand::Add(args)) => add(args).await,
        Command::Account(AccountCommand::List { json }) => list(json),
        Command::Account(AccountCommand::Remove { id }) => remove(&id).await,
        Command::Sync { account } => sync(account).await,
        Command::Watch => watch::run().await.map_err(Failure),
        Command::Daemon(args) => daemon::run(daemon::Options {
            check_interval: Duration::from_secs(args.check_interval),
            batch_size: args.batch_size as usize,
        })
        .await
        .map_err(Failure),
        Command::Status { json } => read::status(json).map_err(Failure),
        Command::Unread { json } => read::unread(json).map_err(Failure),
        Command::Search(args) => read::search(args).map_err(Failure),
        Command::List(args) => read::list(args).map_err(Failure),
        Command::Show(args) => match read::show(args) {
            Ok(()) => Ok(()),
            Err(read::ShowError::NotFound(message)) => {
                eprintln!("missive: {message}");
                return ExitCode::from(read::NOT_FOUND);
            }
            Err(read::ShowError::Failed(message)) => Err(Failure(message)),
        },
        Command::Open(args) => match open::run(args).await {
            Ok(()) => Ok(()),
            Err(read::ShowError::NotFound(message)) => {
                eprintln!("missive: {message}");
                return ExitCode::from(read::NOT_FOUND);
            }
            Err(read::ShowError::Failed(message)) => Err(Failure(message)),
        },
        Command::Mark(args) => acted(act::mark(args).await),
        Command::Move(args) => acted(act::move_to(args).await),
        Command::Delete(args) => acted(act::delete(args).await),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure(message)) => {
            eprintln!("missive: {message}");
            ExitCode::FAILURE
        }
    }
}

/// An action's result, with the exit status of an identifier that names no message.
fn acted(result: std::result::Result<(), act::ActError>) -> Result<()> {
    match result {
        Ok(()) => Ok(()),
        Err(act::ActError::NotFound(message)) => {
            eprintln!("missive: {message}");
            std::process::exit(act::NOT_FOUND.into());
        }
        Err(act::ActError::Failed(message)) => Err(Failure(message)),
    }
}

async fn add(args: AddArgs) -> Result<()> {
    let mut ask = Asker::new(args.password_stdin);
    let name = ask.value("Display name", "--name", args.name, None)?;
    let address = ask.value("Address", "--address", args.address, None)?;
    let imap = ask.server(
        "IMAP",
        args.imap_host,
        args.imap_security,
        args.imap_port,
        (993, 143),
    )?;
    let smtp = ask.server(
        "SMTP",
        args.smtp_host,
        args.smtp_security,
        args.smtp_port,
        (465, 587),
    )?;
    let user = ask.value("User name", "--user", args.user, Some(address.clone()))?;
    ask.done()?;
    let id = args.id.unwrap_or_else(|| address.clone());
    if !valid_id(&id) {
        return Err(Failure(format!(
            "{id:?} cannot name an account: use letters, digits and . _ @ + - (--id)"
        )));
    }
    let store = AccountStore::from_env()?;
    if store.exists(&id) {
        return Err(Failure(format!(
            "the account {id} already exists (missive account remove {id} first, or choose \
             another --id)"
        )));
    }
    let certificate = match &args.certificate {
        Some(path) => {
            let pem = std::fs::read(path)
                .map_err(|e| Failure(format!("cannot read {}: {e}", path.display())))?;
            let certificate = read_certificate(&pem)
                .map_err(|e| Failure(format!("cannot import {}: {e}", path.display())))?;
            Some(certificate)
        }
        None => None,
    };
    let password = read_password(args.password_stdin)?;
    // Opened first: a missing keyring is found before anything is sent to the servers.
    let keyring = Keyring::open().await?;

    let account = Account {
        name,
        address,
        user,
        imap,
        smtp,
    };
    let tls = client_config(certificate.clone());
    check_imap(&account.imap, &account.user, &password, tls.clone())
        .await
        .map_err(|error| refused("IMAP", &account.imap, error))?;
    check_smtp(&account.smtp, &account.user, &password, tls)
        .await
        .map_err(|error| refused("SMTP", &account.smtp, error))?;

    keyring.store(&id, &password).await?;
    if let Err(error) = store.save(&id, &account, certificate.as_ref()) {
        let _ = keyring.delete(&id).await;
        return Err(Failure(format!(
            "cannot save the account: {error}. Nothing was saved."
        )));
    }
    println!("Added the account {id}.");
    Ok(())
}

fn refused(protocol: &str, server: &Server, error: ConnectError) -> Failure {
    Failure(format!(
        "{protocol} {}:{}: {error}. Nothing was saved.",
        server.host, server.port
    ))
}

#[derive(Serialize)]
struct Listed<'a> {
    id: &'a str,
    #[serde(flatten)]
    account: &'a Account,
    certificate: bool,
}

fn list(json: bool) -> Result<()> {
    let store = AccountStore::from_env()?;
    let Listing { accounts, skipped } = store.list()?;
    // On stderr, so that the JSON on stdout stays valid.
    for (id, kind) in &skipped {
        eprintln!(
            "missive: skipped the account folder {id}: its account.toml is unusable ({kind})"
        );
    }
    // Only this command reads the certificate, so a damaged one is skipped here: the daemon and
    // `status`, `unread` and `search` need the account whether or not its certificate reads.
    let mut listed = Vec::new();
    for (id, account) in &accounts {
        match store.certificate(id) {
            Ok(certificate) => listed.push(Listed {
                id,
                account,
                certificate: certificate.is_some(),
            }),
            Err(e) => eprintln!(
                "missive: skipped the account folder {id}: its certificate.pem is unusable ({})",
                e.kind()
            ),
        }
    }
    if json {
        println!("{}", serde_json::to_string(&listed)?);
        return Ok(());
    }
    for Listed { id, account, .. } in &listed {
        println!(
            "{id}\t{} <{}>\timap {}\tsmtp {}",
            account.name,
            account.address,
            describe(&account.imap),
            describe(&account.smtp)
        );
    }
    Ok(())
}

fn describe(server: &Server) -> String {
    format!(
        "{}:{} {}",
        server.host,
        server.port,
        server.security.as_str()
    )
}

async fn remove(id: &str) -> Result<()> {
    let store = AccountStore::from_env()?;
    if !valid_id(id) || !store.exists(id) {
        return Err(Failure(format!(
            "no account {id} (missive account list shows them)"
        )));
    }
    // The keyring first: if it is unavailable, the account stays whole and can be removed later.
    Keyring::open().await?.delete(id).await?;
    store.remove(id)?;
    println!("Removed the account {id}.");
    Ok(())
}

async fn sync(account: Option<String>) -> Result<()> {
    let stream = daemon::connect().await.map_err(Failure)?;
    let (read, mut write) = stream.into_split();
    let request = missive_core::protocol::Request {
        protocol: PROTOCOL,
        command: Request::Sync { account },
    };
    let mut line = serde_json::to_string(&request)?;
    line.push('\n');
    write.write_all(line.as_bytes()).await?;
    let mut line = String::new();
    BufReader::new(read).read_line(&mut line).await?;
    let answer: Answer = serde_json::from_str(&line)
        .map_err(|_| Failure("the daemon gave an answer this missive cannot read".to_owned()))?;
    if !answer.ok {
        return Err(Failure(answer.message.unwrap_or_else(|| {
            format!("the daemon refused: {}", answer.error.unwrap_or_default())
        })));
    }
    let accounts = answer.accounts.unwrap_or_default();
    if accounts.is_empty() {
        return Err(Failure(
            "there is no account to sync (missive account add adds one)".to_owned(),
        ));
    }
    let mut failed = Vec::new();
    for account in &accounts {
        println!("{}: {}", account.id, account.state);
        let why = match account.state.as_str() {
            "ready" => continue,
            "no-login" => "the keyring holds no password for it",
            "auth-refused" => "the server refused the user name or password",
            "certificate-refused" => "the server's certificate is not trusted",
            "unreachable" => "the server cannot be reached now; the daemon tries again by itself",
            _ => "it is not synced",
        };
        failed.push(format!("{}: {}: {why}", account.id, account.state));
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(Failure(failed.join("\nmissive: ")))
    }
}

/// Asks for what the options did not give, on a terminal; elsewhere, collects what is missing to
/// say it at once.
struct Asker {
    interactive: bool,
    missing: Vec<&'static str>,
}

impl Asker {
    fn new(password_stdin: bool) -> Self {
        Asker {
            interactive: !password_stdin && std::io::stdin().is_terminal(),
            missing: Vec::new(),
        }
    }

    fn value(
        &mut self,
        label: &str,
        flag: &'static str,
        given: Option<String>,
        default: Option<String>,
    ) -> Result<String> {
        if let Some(value) = given {
            return Ok(value);
        }
        if !self.interactive {
            return Ok(default.unwrap_or_else(|| {
                self.missing.push(flag);
                String::new()
            }));
        }
        loop {
            match &default {
                Some(default) => eprint!("{label} [{default}]: "),
                None => eprint!("{label}: "),
            }
            std::io::stderr().flush()?;
            let mut line = String::new();
            if std::io::stdin().lock().read_line(&mut line)? == 0 {
                return Err(Failure(format!("no {flag} given")));
            }
            let line = line.trim();
            match (&default, line.is_empty()) {
                (Some(default), true) => return Ok(default.clone()),
                (None, true) => continue,
                (_, false) => return Ok(line.to_owned()),
            }
        }
    }

    fn server(
        &mut self,
        protocol: &'static str,
        host: Option<String>,
        security: Option<SecurityArg>,
        port: Option<u16>,
        (tls_port, starttls_port): (u16, u16),
    ) -> Result<Server> {
        let (host_flag, security_flag, port_flag) = match protocol {
            "IMAP" => ("--imap-host", "--imap-security", "--imap-port"),
            _ => ("--smtp-host", "--smtp-security", "--smtp-port"),
        };
        let host = self.value(&format!("{protocol} host"), host_flag, host, None)?;
        let security = match security {
            Some(security) => security.into(),
            None => loop {
                let answer = self.value(
                    &format!("{protocol} security (tls or starttls)"),
                    security_flag,
                    None,
                    Some("tls".to_owned()),
                )?;
                match answer.as_str() {
                    "tls" => break Security::Tls,
                    "starttls" => break Security::Starttls,
                    _ => eprintln!("Answer tls or starttls."),
                }
            },
        };
        let default_port = match security {
            Security::Tls => tls_port,
            Security::Starttls => starttls_port,
        };
        let port = match port {
            Some(port) => port,
            None => loop {
                let answer = self.value(
                    &format!("{protocol} port"),
                    port_flag,
                    None,
                    Some(default_port.to_string()),
                )?;
                match answer.parse() {
                    Ok(port) => break port,
                    Err(_) => eprintln!("A port is a number from 1 to 65535."),
                }
            },
        };
        Ok(Server {
            host,
            port,
            security,
        })
    }

    fn done(self) -> Result<()> {
        if self.missing.is_empty() {
            return Ok(());
        }
        Err(Failure(format!(
            "standard input is not a terminal, so nothing can be asked: give {}",
            self.missing.join(", ")
        )))
    }
}

fn read_password(from_stdin: bool) -> Result<Password> {
    let password = if from_stdin {
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        line.trim_end_matches(['\r', '\n']).to_owned()
    } else if std::io::stdin().is_terminal() {
        rpassword::prompt_password("Password: ")?
    } else {
        return Err(Failure(
            "standard input is not a terminal, so the password cannot be asked: pass it on \
             standard input with --password-stdin"
                .to_owned(),
        ));
    };
    if password.is_empty() {
        return Err(Failure("the password is empty".to_owned()));
    }
    Ok(Password::new(password))
}
