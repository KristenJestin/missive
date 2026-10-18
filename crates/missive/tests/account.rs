//! `missive account …` against the fake IMAP/SMTP server and a fake Secret Service on a private
//! bus. The binary runs with a cleared environment: its configuration, data and runtime folders
//! are temporary, and the only session bus it can see is the test's own.

use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

use missive_core::fake::{FakeKeyring, FakeServer, PromptAnswer};
use tempfile::TempDir;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::process::Command;

const USER: &str = "alice@example.org";
const PASSWORD: &str = "Recognisable-Pa55word-4c1d";
const WRONG_PASSWORD: &str = "Another-Recognisable-W7ong";

struct Setup {
    server: FakeServer,
    keyring: FakeKeyring,
    home: TempDir,
}

impl Setup {
    async fn new() -> Self {
        Self::with_keyring(FakeKeyring::start().await).await
    }

    async fn with_keyring(keyring: FakeKeyring) -> Self {
        let server = FakeServer::builder().user(USER, PASSWORD).start().await;
        let home = tempfile::tempdir().unwrap();
        for folder in ["config", "data", "runtime"] {
            std::fs::create_dir(home.path().join(folder)).unwrap();
        }
        Setup {
            server,
            keyring,
            home,
        }
    }

    fn config(&self) -> PathBuf {
        self.home.path().join("config")
    }

    /// The fake's certificate, as a user would have saved it.
    fn certificate_file(&self) -> PathBuf {
        let path = self.home.path().join("bridge.pem");
        std::fs::write(&path, self.server.certificate_pem()).unwrap();
        path
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_missive"));
        command
            .args(args)
            .env_clear()
            .env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.config())
            .env("XDG_DATA_HOME", self.home.path().join("data"))
            .env("XDG_RUNTIME_DIR", self.home.path().join("runtime"))
            .env("DBUS_SESSION_BUS_ADDRESS", self.keyring.address())
            .stdin(Stdio::null());
        command
    }

    async fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().await.unwrap()
    }

    /// Runs `missive` with `stdin` written to its standard input.
    async fn run_with_stdin(&self, mut command: Command, stdin: &str) -> Output {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        input.write_all(stdin.as_bytes()).await.unwrap();
        drop(input);
        child.wait_with_output().await.unwrap()
    }

    /// `account add` with every value as a flag, IMAP over STARTTLS and SMTP over implicit TLS,
    /// the password on standard input.
    async fn add(&self, extra: &[&str], password: &str) -> Output {
        let imap_port = self.server.imap_starttls_addr().port().to_string();
        let smtp_port = self.server.smtp_tls_addr().port().to_string();
        self.add_with_ports(&imap_port, &smtp_port, extra, password)
            .await
    }

    async fn add_with_ports(
        &self,
        imap_port: &str,
        smtp_port: &str,
        extra: &[&str],
        password: &str,
    ) -> Output {
        let mut args = vec![
            "account",
            "add",
            "--name",
            "Alice Example",
            "--address",
            USER,
            "--imap-host",
            "127.0.0.1",
            "--imap-port",
            imap_port,
            "--imap-security",
            "starttls",
            "--smtp-host",
            "localhost",
            "--smtp-port",
            smtp_port,
            "--smtp-security",
            "tls",
            "--password-stdin",
        ];
        args.extend_from_slice(extra);
        self.run_with_stdin(self.command(&args), &format!("{password}\n"))
            .await
    }

    async fn add_with_certificate(&self) -> Output {
        let certificate = self.certificate_file();
        self.add(&["--certificate", certificate.to_str().unwrap()], PASSWORD)
            .await
    }

    /// Every file under the configuration, data and runtime folders, as bytes.
    fn files(&self) -> Vec<(PathBuf, Vec<u8>)> {
        let mut files = Vec::new();
        for folder in ["config", "data", "runtime"] {
            collect(&self.home.path().join(folder), &mut files);
        }
        files
    }

    fn nothing_saved(&self) {
        assert_eq!(self.files(), Vec::new(), "files were saved");
        assert!(self.keyring.items().is_empty(), "a keyring entry was saved");
    }
}

fn collect(path: &Path, files: &mut Vec<(PathBuf, Vec<u8>)>) {
    for entry in std::fs::read_dir(path).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect(&path, files);
        } else {
            files.push((path.clone(), std::fs::read(&path).unwrap()));
        }
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

fn succeeded(output: &Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(output),
        stderr(output)
    );
}

fn failed(output: &Output) {
    assert!(
        !output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(output),
        stderr(output)
    );
}

/// A localhost port nothing listens on.
async fn closed_port() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port.to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_account_with_the_fakes_ca_marked_certificate_imported_is_added() {
    let setup = Setup::new().await;

    let output = setup.add_with_certificate().await;

    succeeded(&output);
    let items = setup.keyring.items();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].secret, PASSWORD.as_bytes());
    assert_eq!(items[0].attributes["application"], "missive");
    assert_eq!(items[0].attributes["account"], USER);
    let account = setup.config().join("missive/accounts").join(USER);
    assert!(account.join("account.toml").is_file());
    let pinned = std::fs::read_to_string(account.join("certificate.pem")).unwrap();
    assert_eq!(pinned.trim(), setup.server.certificate_pem().trim());
    // The login check went through both protocols.
    assert!(
        setup
            .server
            .imap_log()
            .iter()
            .any(|c| c.starts_with("LOGIN"))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_same_without_the_import_is_refused_with_a_message_about_the_certificate() {
    let setup = Setup::new().await;

    let output = setup.add(&[], PASSWORD).await;

    failed(&output);
    let stderr = stderr(&output);
    assert!(stderr.contains("certificate-refused"), "{stderr}");
    assert!(stderr.contains("certificate is not trusted"), "{stderr}");
    assert!(stderr.contains("--certificate <file.pem>"), "{stderr}");
    assert!(!stderr.contains("auth-refused"), "{stderr}");
    assert!(!stderr.contains("unreachable"), "{stderr}");
    setup.nothing_saved();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_password_gives_auth_refused() {
    let setup = Setup::new().await;
    let certificate = setup.certificate_file();

    let output = setup
        .add(
            &["--certificate", certificate.to_str().unwrap()],
            WRONG_PASSWORD,
        )
        .await;

    failed(&output);
    let stderr = stderr(&output);
    assert!(stderr.contains("auth-refused"), "{stderr}");
    assert!(stderr.contains("Nothing was saved"), "{stderr}");
    setup.nothing_saved();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_closed_port_gives_unreachable() {
    let setup = Setup::new().await;
    let certificate = setup.certificate_file();
    let smtp_port = setup.server.smtp_tls_addr().port().to_string();

    let output = setup
        .add_with_ports(
            &closed_port().await,
            &smtp_port,
            &["--certificate", certificate.to_str().unwrap()],
            PASSWORD,
        )
        .await;

    failed(&output);
    let stderr = stderr(&output);
    assert!(stderr.contains("unreachable"), "{stderr}");
    assert!(!stderr.contains("auth-refused"), "{stderr}");
    assert!(!stderr.to_lowercase().contains("log in"), "{stderr}");
    setup.nothing_saved();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_closed_smtp_port_gives_unreachable_too() {
    let setup = Setup::new().await;
    let certificate = setup.certificate_file();
    let imap_port = setup.server.imap_starttls_addr().port().to_string();

    let output = setup
        .add_with_ports(
            &imap_port,
            &closed_port().await,
            &["--certificate", certificate.to_str().unwrap()],
            PASSWORD,
        )
        .await;

    failed(&output);
    let stderr = stderr(&output);
    assert!(stderr.contains("SMTP"), "{stderr}");
    assert!(stderr.contains("unreachable"), "{stderr}");
    setup.nothing_saved();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_password_never_appears_in_the_config_folder_a_log_an_error_or_an_output() {
    let setup = Setup::new().await;
    let certificate = setup.certificate_file();
    let certificate = certificate.to_str().unwrap();

    let mut outputs = vec![
        // An error that carries the typed password back to the user would show here.
        setup
            .add(&["--certificate", certificate], WRONG_PASSWORD)
            .await,
        setup.add(&[], PASSWORD).await,
        setup.add(&["--certificate", certificate], PASSWORD).await,
        // A second add of the same account fails before it reads the password; the scan below
        // still covers its output.
        setup.add(&["--certificate", certificate], PASSWORD).await,
    ];
    outputs.push(setup.run(&["account", "list"]).await);
    outputs.push(setup.run(&["account", "list", "--json"]).await);
    succeeded(&outputs[2]);

    for password in [PASSWORD, WRONG_PASSWORD] {
        for output in &outputs {
            assert!(!stdout(output).contains(password), "{}", stdout(output));
            assert!(!stderr(output).contains(password), "{}", stderr(output));
        }
        for (path, bytes) in setup.files() {
            let text = String::from_utf8_lossy(&bytes);
            assert!(
                !text.contains(password),
                "{} holds the password",
                path.display()
            );
        }
    }
    // Where it does belong.
    assert_eq!(setup.keyring.items()[0].secret, PASSWORD.as_bytes());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_locked_keyring_whose_prompt_is_accepted_still_saves_the_account_and_the_password() {
    let setup = Setup::with_keyring(FakeKeyring::start_locked(PromptAnswer::Accept).await).await;

    let output = setup.add_with_certificate().await;

    succeeded(&output);
    let items = setup.keyring.items();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].secret, PASSWORD.as_bytes());
    assert!(
        setup
            .config()
            .join("missive/accounts")
            .join(USER)
            .join("account.toml")
            .is_file()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_locked_keyring_whose_prompt_is_dismissed_saves_nothing_and_says_it_stayed_locked() {
    let setup = Setup::with_keyring(FakeKeyring::start_locked(PromptAnswer::Dismiss).await).await;

    let output = setup.add_with_certificate().await;

    failed(&output);
    let stderr = stderr(&output);
    assert!(stderr.contains("keyring stayed locked"), "{stderr}");
    assert!(!stderr.contains("unreachable"), "{stderr}");
    assert!(!stderr.contains("auth-refused"), "{stderr}");
    setup.nothing_saved();
}

#[tokio::test(flavor = "multi_thread")]
async fn remove_leaves_no_keyring_entry_and_no_config_entry() {
    let setup = Setup::new().await;
    succeeded(&setup.add_with_certificate().await);

    let output = setup.run(&["account", "remove", USER]).await;

    succeeded(&output);
    assert!(setup.keyring.items().is_empty());
    assert_eq!(setup.files(), Vec::new());
    let list = setup.run(&["account", "list", "--json"]).await;
    assert_eq!(stdout(&list).trim(), "[]");
}

#[tokio::test(flavor = "multi_thread")]
async fn remove_of_an_unknown_account_fails_and_keeps_the_others() {
    let setup = Setup::new().await;
    succeeded(&setup.add_with_certificate().await);

    let output = setup.run(&["account", "remove", "bob@example.com"]).await;

    failed(&output);
    assert!(
        stderr(&output).contains("no account"),
        "{}",
        stderr(&output)
    );
    assert_eq!(setup.keyring.items().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn tests_use_a_private_keyring_never_the_real_one() {
    let setup = Setup::new().await;
    let certificate = setup.certificate_file();

    // Without the private bus, `missive` finds no session bus at all (its runtime folder is
    // empty) and says so, rather than reaching another keyring.
    let mut command = setup.command(&[
        "account",
        "add",
        "--name",
        "Alice Example",
        "--address",
        USER,
        "--imap-host",
        "127.0.0.1",
        "--smtp-host",
        "127.0.0.1",
        "--certificate",
        certificate.to_str().unwrap(),
        "--password-stdin",
    ]);
    command.env_remove("DBUS_SESSION_BUS_ADDRESS");
    let output = setup
        .run_with_stdin(command, &format!("{PASSWORD}\n"))
        .await;

    failed(&output);
    assert!(stderr(&output).contains("keyring"), "{}", stderr(&output));
    setup.nothing_saved();

    // With it, the password lands in the fake.
    succeeded(&setup.add_with_certificate().await);
    assert_eq!(setup.keyring.items().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn account_list_json_follows_the_documented_format() {
    let setup = Setup::new().await;
    assert_eq!(
        stdout(&setup.run(&["account", "list", "--json"]).await).trim(),
        "[]"
    );
    succeeded(&setup.add_with_certificate().await);

    let output = setup.run(&["account", "list", "--json"]).await;

    succeeded(&output);
    let listed: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    let imap_port = setup.server.imap_starttls_addr().port();
    let smtp_port = setup.server.smtp_tls_addr().port();
    assert_eq!(
        listed,
        serde_json::json!([{
            "id": USER,
            "name": "Alice Example",
            "address": USER,
            "user": USER,
            "imap": { "host": "127.0.0.1", "port": imap_port, "security": "starttls" },
            "smtp": { "host": "localhost", "port": smtp_port, "security": "tls" },
            "certificate": true,
        }])
    );
    let text = stdout(&setup.run(&["account", "list"]).await);
    assert!(text.contains(USER), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn one_broken_account_is_skipped_by_list_and_the_others_are_listed() {
    let setup = Setup::new().await;
    let certificate = setup.certificate_file();
    succeeded(&setup.add_with_certificate().await);
    succeeded(
        &setup
            .add(
                &[
                    "--certificate",
                    certificate.to_str().unwrap(),
                    "--id",
                    "broken",
                ],
                PASSWORD,
            )
            .await,
    );
    std::fs::write(
        setup.config().join("missive/accounts/broken/account.toml"),
        "name = \"Content-That-Must-Not-Be-Echoed\n",
    )
    .unwrap();

    let text = setup.run(&["account", "list"]).await;
    let json = setup.run(&["account", "list", "--json"]).await;

    for output in [&text, &json] {
        succeeded(output);
        let stderr = stderr(output);
        assert!(stderr.contains("broken"), "{stderr}");
        assert!(stderr.contains("invalid"), "{stderr}");
        assert!(
            !stderr.contains("Content-That-Must-Not-Be-Echoed"),
            "{stderr}"
        );
        assert!(!stdout(output).contains("Content-That-Must-Not-Be-Echoed"));
    }
    let text = stdout(&text);
    assert!(text.contains(USER), "{text}");
    assert!(!text.contains("broken"), "{text}");
    let listed: serde_json::Value = serde_json::from_str(&stdout(&json)).unwrap();
    let ids: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|account| account["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [USER]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_chosen_id_names_the_account() {
    let setup = Setup::new().await;
    let certificate = setup.certificate_file();

    let output = setup
        .add(
            &[
                "--certificate",
                certificate.to_str().unwrap(),
                "--id",
                "bridge",
            ],
            PASSWORD,
        )
        .await;

    succeeded(&output);
    assert_eq!(setup.keyring.items()[0].attributes["account"], "bridge");
    assert!(setup.config().join("missive/accounts/bridge").is_dir());
    succeeded(&setup.run(&["account", "remove", "bridge"]).await);
    assert!(setup.keyring.items().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_existing_id_is_refused() {
    let setup = Setup::new().await;
    succeeded(&setup.add_with_certificate().await);

    let output = setup.add_with_certificate().await;

    failed(&output);
    assert!(
        stderr(&output).contains("already exists"),
        "{}",
        stderr(&output)
    );
    assert_eq!(setup.keyring.items().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_terminal_a_missing_value_fails_clearly_instead_of_prompting() {
    let setup = Setup::new().await;

    let output = setup
        .run(&[
            "account",
            "add",
            "--name",
            "Alice Example",
            "--address",
            USER,
        ])
        .await;

    failed(&output);
    let stderr = stderr(&output);
    assert!(stderr.contains("--imap-host"), "{stderr}");
    assert!(stderr.contains("not a terminal"), "{stderr}");
    setup.nothing_saved();
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_terminal_the_password_comes_only_from_password_stdin() {
    let setup = Setup::new().await;

    let output = setup
        .run(&[
            "account",
            "add",
            "--name",
            "Alice Example",
            "--address",
            USER,
            "--imap-host",
            "127.0.0.1",
            "--smtp-host",
            "127.0.0.1",
        ])
        .await;

    failed(&output);
    assert!(
        stderr(&output).contains("--password-stdin"),
        "{}",
        stderr(&output)
    );
    setup.nothing_saved();
}
