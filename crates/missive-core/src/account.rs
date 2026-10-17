//! Accounts: their settings in Missive's config folder, and their password, which lives only in
//! the keyring (see [`crate::keyring`]).
//!
//! Each account is a folder `$XDG_CONFIG_HOME/missive/accounts/<id>/`, holding `account.toml` and,
//! when the user imported one, the server certificate it pins, `certificate.pem` (a certificate is
//! public, not a secret). Reading accounts touches neither the network nor the keyring.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;
use serde::{Deserialize, Serialize};

/// A password, kept out of every `Debug` (it has no `Display`): only [`Password::expose`] reads
/// it, where it is sent to a server or to the keyring.
#[derive(Clone)]
pub struct Password(String);

impl Password {
    pub fn new(password: String) -> Self {
        Password(password)
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Password {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Password(hidden)")
    }
}

/// How a connection is secured. There is no clear-text mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Security {
    /// A clear connection upgraded with `STARTTLS` before anything else is said.
    Starttls,
    /// TLS from the first byte.
    Tls,
}

impl Security {
    pub fn as_str(self) -> &'static str {
        match self {
            Security::Starttls => "starttls",
            Security::Tls => "tls",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Server {
    pub host: String,
    pub port: u16,
    pub security: Security,
}

/// An account's settings, without its identifier (the name of its folder) and without its
/// password.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    /// The display name, as in `From: Alice Example <alice@example.org>`.
    pub name: String,
    pub address: String,
    /// The login on both servers.
    pub user: String,
    pub imap: Server,
    pub smtp: Server,
}

/// Whether `id` can name an account: it becomes a folder name and a keyring attribute, and is
/// typed on the command line, so it is kept to letters, digits and `. _ @ + -`.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && !id.starts_with('.')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._@+-".contains(c))
}

/// The accounts in a config folder.
pub struct AccountStore {
    root: PathBuf,
}

impl AccountStore {
    /// The accounts of `$XDG_CONFIG_HOME/missive`, or `$HOME/.config/missive`.
    pub fn from_env() -> io::Result<Self> {
        let config = match std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from) {
            Some(path) if path.is_absolute() => path,
            _ => match std::env::var_os("HOME") {
                Some(home) if !home.is_empty() => PathBuf::from(home).join(".config"),
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "neither XDG_CONFIG_HOME nor HOME is set",
                    ));
                }
            },
        };
        Ok(Self::new(config.join("missive")))
    }

    /// The accounts of the config folder `missive` (which holds `accounts/`).
    pub fn new(missive: impl Into<PathBuf>) -> Self {
        AccountStore {
            root: missive.into().join("accounts"),
        }
    }

    fn folder(&self, id: &str) -> PathBuf {
        debug_assert!(valid_id(id));
        self.root.join(id)
    }

    pub fn exists(&self, id: &str) -> bool {
        self.folder(id).is_dir()
    }

    pub fn get(&self, id: &str) -> io::Result<Option<Account>> {
        let path = self.folder(id).join("account.toml");
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        toml::from_str(&text)
            .map(Some)
            .map_err(|e| invalid(&path, e.message()))
    }

    /// Every account, by identifier.
    pub fn list(&self) -> io::Result<Vec<(String, Account)>> {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut accounts = Vec::new();
        for entry in entries {
            let entry = entry?;
            let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            // Skips what `save` left behind if it was interrupted.
            if !valid_id(&id) || !entry.file_type()?.is_dir() {
                continue;
            }
            if let Some(account) = self.get(&id)? {
                accounts.push((id, account));
            }
        }
        accounts.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(accounts)
    }

    /// The certificate pinned for an account, if the user imported one.
    pub fn certificate(&self, id: &str) -> io::Result<Option<CertificateDer<'static>>> {
        let path = self.folder(id).join("certificate.pem");
        match std::fs::read(&path) {
            Ok(pem) => read_certificate(&pem)
                .map(Some)
                .map_err(|e| invalid(&path, &e)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Saves a new account, whole or not at all: its folder is written aside, then renamed into
    /// place.
    pub fn save(
        &self,
        id: &str,
        account: &Account,
        certificate: Option<&CertificateDer<'_>>,
    ) -> io::Result<()> {
        let folder = self.folder(id);
        if folder.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("the account {id} already exists"),
            ));
        }
        std::fs::create_dir_all(&self.root)?;
        let staging = self.root.join(format!(".{id}.new"));
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::create_dir(&staging)?;
        let written = (|| {
            let text = toml::to_string(account).map_err(|e| invalid(&staging, &e.to_string()))?;
            std::fs::write(staging.join("account.toml"), text)?;
            if let Some(certificate) = certificate {
                std::fs::write(staging.join("certificate.pem"), pem(certificate))?;
            }
            std::fs::rename(&staging, &folder)
        })();
        if written.is_err() {
            let _ = std::fs::remove_dir_all(&staging);
        }
        written
    }

    /// Removes an account's settings and certificate. `false` when there was no such account.
    pub fn remove(&self, id: &str) -> io::Result<bool> {
        match std::fs::remove_dir_all(self.folder(id)) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }
}

/// The first certificate of a PEM file. Only that certificate is kept, so that a file which also
/// holds a private key never has the key copied anywhere.
pub fn read_certificate(pem: &[u8]) -> Result<CertificateDer<'static>, String> {
    CertificateDer::pem_slice_iter(pem)
        .next()
        .ok_or_else(|| "it holds no PEM certificate".to_owned())?
        .map_err(|e| format!("it is not a valid PEM certificate ({e})"))
}

fn pem(certificate: &CertificateDer<'_>) -> String {
    let base64 = STANDARD.encode(certificate.as_ref());
    let mut pem = String::from("-----BEGIN CERTIFICATE-----\n");
    for line in base64.as_bytes().chunks(64) {
        pem.push_str(std::str::from_utf8(line).expect("base64 is ASCII"));
        pem.push('\n');
    }
    pem.push_str("-----END CERTIFICATE-----\n");
    pem
}

fn invalid(path: &Path, reason: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{}: {reason}", path.display()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_password_never_shows_in_debug() {
        let password = Password::new("Recognisable-Pa55word".to_owned());
        assert_eq!(format!("{password:?}"), "Password(hidden)");
    }

    #[test]
    fn an_id_is_a_plain_name() {
        assert!(valid_id("alice@example.org"));
        assert!(valid_id("work+bridge_2"));
        for id in ["", ".", "..", ".hidden", "a/b", "a b", "../up"] {
            assert!(!valid_id(id), "{id:?}");
        }
    }

    #[test]
    fn an_account_is_saved_listed_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let store = AccountStore::new(dir.path());
        let account = Account {
            name: "Alice Example".to_owned(),
            address: "alice@example.org".to_owned(),
            user: "alice@example.org".to_owned(),
            imap: Server {
                host: "imap.example.org".to_owned(),
                port: 993,
                security: Security::Tls,
            },
            smtp: Server {
                host: "smtp.example.org".to_owned(),
                port: 587,
                security: Security::Starttls,
            },
        };
        let certificate = CertificateDer::from(vec![1, 2, 3, 4, 5]);

        store.save("alice", &account, Some(&certificate)).unwrap();

        assert_eq!(
            store.list().unwrap(),
            vec![("alice".to_owned(), account.clone())]
        );
        assert_eq!(store.certificate("alice").unwrap(), Some(certificate));
        assert!(store.save("alice", &account, None).is_err());
        assert!(store.remove("alice").unwrap());
        assert_eq!(store.list().unwrap(), Vec::new());
        assert!(!store.remove("alice").unwrap());
    }
}
