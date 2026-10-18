//! A local copy filled through the fake IMAP server, in temporary folders: the sync's own
//! functions write it, as the daemon does, and the window reads it.

#![allow(dead_code)]

use std::path::PathBuf;

use missive_core::account::{Account, AccountStore, Password, Security, Server};
use missive_core::connect::{client_config, open_imap};
use missive_core::fake::{FakeServer, Folder, Message, SpecialUse};
use missive_core::store::Store;
use missive_core::sync;
use missive_window::model::{Model, Sources};
use tempfile::TempDir;

pub const ID: &str = "alice";
pub const USER: &str = "alice@example.org";
pub const PASSWORD: &str = "Recognisable-Pa55word-4c1d";

/// A message from Carol whose subject is `Plan <n>`, dated `n` minutes after 09:00 (+0200) on 17
/// October 2026.
pub fn plan(n: u32, headers: &str, body: &str) -> Message {
    Message::new(format!(
        "From: Carol Sender <carol.sender@example.com>\r\nTo: Dave Recipient <dave@example.net>\r\n\
         Subject: Plan {n}\r\nMessage-ID: <{n}@example.com>\r\n\
         Date: Sat, 17 Oct 2026 09:{n:02}:00 +0200\r\n{headers}\r\n{body}\r\n"
    ))
}

/// Plan 2, in HTML: a remote image, a tracking pixel, a link and a hidden preheader.
pub fn newsletter() -> Message {
    plan(
        2,
        "Content-Type: text/html; charset=utf-8\r\n",
        "<html><body><div style=\"display:none\">Preheader</div>\
         <p><img src=\"https://images.example.org/logo.png\" alt=\"Our logo\"></p>\
         <p>Read <a href=\"https://www.example.org/news\">the news</a>.</p>\
         <img src=\"https://track.example.net/pixel.gif\" width=\"1\" height=\"1\"></body></html>",
    )
}

/// Plan 5, nested deeper than the reduction follows (in SVG, nothing closes what came before).
pub fn hostile() -> Message {
    plan(
        5,
        "Content-Type: text/html; charset=utf-8\r\n",
        &format!(
            "<p>The hostile words.</p><svg>{}</svg>",
            "<tr>".repeat(5_000)
        ),
    )
}

/// INBOX: Plan 1, seen; Plan 2, the newsletter; Plan 3, plain text. Sent: Plan 4, seen.
pub fn mailbox() -> missive_core::fake::Builder {
    FakeServer::builder()
        .user(USER, PASSWORD)
        .folder(
            Folder::new("INBOX")
                .message(plan(1, "", "Nothing special.").flag("\\Seen"))
                .message(newsletter())
                .message(plan(3, "", "The Zanzibar archive.")),
        )
        .folder(
            Folder::new("Sent")
                .special_use(SpecialUse::Sent)
                .message(plan(4, "", "Sent.").flag("\\Seen")),
        )
}

pub struct Mail {
    pub home: TempDir,
    pub server: FakeServer,
}

impl Mail {
    /// The account configured, its copy filled by a first sync.
    pub async fn new(builder: missive_core::fake::Builder) -> Mail {
        let mail = Mail {
            home: tempfile::tempdir().unwrap(),
            server: builder.start().await,
        };
        std::fs::create_dir_all(mail.copy_path().parent().unwrap()).unwrap();
        mail.store()
            .save(ID, &mail.account(), Some(&mail.server.certificate_der()))
            .unwrap();
        mail.sync().await;
        mail
    }

    fn account(&self) -> Account {
        let server = |port| Server {
            host: "127.0.0.1".to_owned(),
            port,
            security: Security::Tls,
        };
        Account {
            name: "Alice Example".to_owned(),
            address: USER.to_owned(),
            user: USER.to_owned(),
            imap: server(self.server.imap_tls_addr().port()),
            smtp: server(self.server.smtp_tls_addr().port()),
            server_saves_sent: false,
        }
    }

    /// Where a daemon run with `XDG_CONFIG_HOME` in the folder `config` reads the accounts.
    fn store(&self) -> AccountStore {
        AccountStore::new(self.home.path().join("config/missive"))
    }

    /// Where a daemon run with `XDG_DATA_HOME` in the folder `data` keeps the copy.
    pub fn copy_path(&self) -> PathBuf {
        self.home.path().join("data/missive/copy.sqlite")
    }

    pub fn sources(&self) -> Sources {
        Sources {
            accounts: self.store(),
            copy: self.copy_path(),
        }
    }

    pub fn model(&self) -> Model {
        Model::new(self.sources())
    }

    /// One pass over every folder, as the daemon makes it, ending `ready`.
    pub async fn sync(&self) {
        let mut store = Store::open(&self.copy_path()).unwrap();
        let key = store.account(ID).unwrap();
        let tls = client_config(Some(self.server.certificate_der()));
        let password = Password::new(PASSWORD.to_owned());
        let mut session = open_imap(&self.account().imap, USER, &password, tls)
            .await
            .unwrap();
        sync::prepare(&mut session).await.unwrap();
        let mut folders = sync::folders(&mut session, &mut store, key).await.unwrap();
        for folder in &mut folders {
            sync::folder(&mut session, &mut store, folder, 50)
                .await
                .unwrap()
                .unwrap();
        }
        store.set_state(key, "ready", true).unwrap();
    }

    /// Records a state in the copy, as the daemon does.
    pub fn set_state(&self, state: &str) {
        let store = Store::open(&self.copy_path()).unwrap();
        let key = store.account(ID).unwrap();
        store.set_state(key, state, false).unwrap();
    }

    /// The flags the server has for a message of INBOX.
    pub fn server_flags(&self, uid: u32) -> Vec<String> {
        self.server.flags("INBOX", uid).unwrap()
    }
}

/// INBOX: Plan 1, seen; Plan 2, flagged; Plan 3. An archive, a trash holding Plan 6, and a
/// folder of the user's own, `Projects`.
pub fn sorted_mailbox() -> missive_core::fake::Builder {
    FakeServer::builder()
        .user(USER, PASSWORD)
        .folder(
            Folder::new("INBOX")
                .message(plan(1, "", "Nothing special.").flag("\\Seen"))
                .message(plan(2, "", "Flagged.").flag("\\Flagged"))
                .message(plan(3, "", "The Zanzibar archive.")),
        )
        .folder(Folder::new("Archive").special_use(SpecialUse::Archive))
        .folder(
            Folder::new("Trash")
                .special_use(SpecialUse::Trash)
                .message(plan(6, "", "Thrown away.").flag("\\Seen")),
        )
        .folder(Folder::new("Projects"))
}
