//! What the window shows, without the window: the accounts and their folders, the messages of
//! the selected folder, and the message open in the reader. It reads the local copy, opened read
//! only as `missive list` and `show` open it, and follows the daemon's events to know what to read
//! again. It never writes anything: opening a message does not mark it read.

use std::collections::HashMap;
use std::path::PathBuf;

use missive_core::account::{AccountStore, Listing};
use missive_core::copy::{Copy, FolderRef, List, Listed, NotFound, Person};
use missive_core::events::Event;
use missive_core::store::Store;
use missive_render::message;
use missive_render::reduce::{Theme, reduce};

/// The most messages a folder lists, the newest first.
pub const LIMIT: usize = 500;

/// The width the reduction computes styles at: what the author's CSS hides does not depend on it
/// in practice, and the window lays the result out at its own width.
const REDUCE_WIDTH: u32 = 800;

/// Where the window reads from.
pub struct Sources {
    pub accounts: AccountStore,
    pub copy: PathBuf,
}

impl Sources {
    /// The user's own configuration and copy, as the command line finds them.
    pub fn from_env() -> std::io::Result<Sources> {
        Ok(Sources {
            accounts: AccountStore::from_env()?,
            copy: Store::default_path()?,
        })
    }
}

/// Whether a daemon answers on the socket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Daemon {
    /// Not known yet: the window has not tried the socket.
    Unknown,
    Running,
    Stopped,
}

/// An account in the first pane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountPane {
    pub id: String,
    /// Its connection state (`no-login`, `auth-refused`, `certificate-refused`, `unreachable`,
    /// `syncing`, `ready`); `None` when it is not known: the account has not reached the copy,
    /// or it was `syncing` when the daemon went away.
    pub state: Option<String>,
    pub folders: Vec<FolderPane>,
}

/// A folder in the first pane, with its unread messages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderPane {
    pub folder: FolderRef,
    pub unread: u64,
}

/// A folder, by account and number in the copy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderKey {
    pub account: String,
    pub folder: i64,
}

/// What the reader shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reader {
    /// No message is open.
    Empty,
    Open(Box<Opened>),
    /// The message that was open has left the copy (the server removed or moved it).
    Gone,
}

/// A message open in the reader.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Opened {
    pub id: String,
    pub subject: Option<String>,
    pub from: Vec<Person>,
    pub to: Vec<Person>,
    pub cc: Vec<Person>,
    pub date: Option<i64>,
    /// The body in the reduced form; `None` when the message has no readable body.
    pub html: Option<String>,
    /// The message's parts named by a Content-ID, for its `cid:` images.
    pub parts: HashMap<String, Vec<u8>>,
    /// The message as the server sent it, for "View original".
    pub raw: Vec<u8>,
}

pub struct Model {
    sources: Sources,
    copy: Option<Copy>,
    pub daemon: Daemon,
    /// The states the copy recorded, by account.
    recorded: HashMap<String, String>,
    /// The states the daemon told, by account: they are newer than the copy's.
    told: HashMap<String, String>,
    pub accounts: Vec<AccountPane>,
    pub folder: Option<FolderKey>,
    pub messages: Vec<Listed>,
    /// The message the keyboard is on, in `messages`.
    pub cursor: Option<usize>,
    pub reader: Reader,
    /// Why the copy or the configuration could not be read, for people.
    pub problem: Option<String>,
    dark: bool,
}

impl Model {
    /// Reads the copy and selects the first account's inbox.
    pub fn new(sources: Sources) -> Model {
        let mut model = Model {
            sources,
            copy: None,
            daemon: Daemon::Unknown,
            recorded: HashMap::new(),
            told: HashMap::new(),
            accounts: Vec::new(),
            folder: None,
            messages: Vec::new(),
            cursor: None,
            reader: Reader::Empty,
            problem: None,
            dark: false,
        };
        model.refresh();
        model
    }

    /// Reads everything again, keeping the folder, the message under the cursor and the open
    /// message when they are still there.
    pub fn refresh(&mut self) {
        self.problem = None;
        if let Err(problem) = self.load_accounts() {
            self.problem = Some(problem);
        }
        let folder_known = self
            .folder
            .as_ref()
            .is_some_and(|key| self.pane(key).is_some());
        if !folder_known {
            self.folder = self.first_inbox();
        }
        let under_cursor = self.under_cursor().map(str::to_owned);
        self.load_messages();
        // A message that left the list leaves the cursor where it was, on the next one.
        let found = under_cursor.and_then(|id| self.messages.iter().position(|m| m.hit.id == id));
        self.cursor = match found {
            Some(at) => Some(at),
            None if self.messages.is_empty() => None,
            None => Some(self.cursor.unwrap_or(0).min(self.messages.len() - 1)),
        };
        if let Reader::Open(opened) = &self.reader {
            let id = opened.id.clone();
            if !self.still_there(&id) {
                self.reader = Reader::Gone;
            }
        }
    }

    /// Follows one event of the daemon.
    pub fn apply(&mut self, event: &Event) {
        match event {
            Event::Gone => {
                self.daemon = Daemon::Stopped;
                self.told.clear();
                self.refresh_states();
            }
            Event::State { account, state } => {
                self.daemon = Daemon::Running;
                self.told.insert(account.clone(), state.clone());
                self.refresh_states();
            }
            Event::SyncStarted { .. } => self.daemon = Daemon::Running,
            Event::NewMessage(_)
            | Event::Unread(_)
            | Event::SyncEnded { .. }
            | Event::Action(_)
            | Event::Lagged { .. } => {
                self.daemon = Daemon::Running;
                self.refresh();
            }
        }
    }

    /// Shows the messages of a folder, the cursor on the newest.
    pub fn select_folder(&mut self, account: &str, folder: i64) {
        let key = FolderKey {
            account: account.to_owned(),
            folder,
        };
        if self.pane(&key).is_none() {
            return;
        }
        self.folder = Some(key);
        self.load_messages();
        self.cursor = (!self.messages.is_empty()).then_some(0);
    }

    /// Moves the cursor by `by` messages, staying in the list.
    pub fn move_cursor(&mut self, by: isize) {
        if self.messages.is_empty() {
            return;
        }
        let last = self.messages.len() - 1;
        self.cursor = Some(match self.cursor {
            Some(at) => at.saturating_add_signed(by).min(last),
            None if by < 0 => last,
            None => 0,
        });
    }

    /// Opens the message under the cursor.
    pub fn open_cursor(&mut self) {
        if let Some(id) = self.under_cursor().map(str::to_owned) {
            self.open(&id);
        }
    }

    /// Opens a message of the list, and puts the cursor on it.
    pub fn open(&mut self, id: &str) {
        if let Some(at) = self.messages.iter().position(|m| m.hit.id == id) {
            self.cursor = Some(at);
        }
        self.reader = self.read(id);
    }

    /// Closes the reader.
    pub fn back(&mut self) {
        self.reader = Reader::Empty;
    }

    /// Follows the system's colours: the open message is reduced again under the other theme.
    pub fn set_dark(&mut self, dark: bool) {
        if self.dark == dark {
            return;
        }
        self.dark = dark;
        if let Reader::Open(opened) = &self.reader {
            let id = opened.id.clone();
            self.reader = self.read(&id);
        }
    }

    /// The identifier of the message under the cursor.
    pub fn under_cursor(&self) -> Option<&str> {
        Some(self.messages.get(self.cursor?)?.hit.id.as_str())
    }

    /// The folder pane of a key.
    pub fn pane(&self, key: &FolderKey) -> Option<&FolderPane> {
        self.accounts
            .iter()
            .find(|a| a.id == key.account)?
            .folders
            .iter()
            .find(|f| f.folder.id == key.folder)
    }

    /// The copy, opened once it exists: the daemon may make it while the window is open.
    fn copy(&mut self) -> Result<Option<&Copy>, String> {
        if self.copy.is_none() {
            self.copy = Copy::open(&self.sources.copy).map_err(|e| e.to_string())?;
        }
        Ok(self.copy.as_ref())
    }

    fn load_accounts(&mut self) -> Result<(), String> {
        let Listing { accounts, .. } = self
            .sources
            .accounts
            .list()
            .map_err(|e| format!("the accounts could not be read ({e})"))?;
        let configured: Vec<String> = accounts.into_iter().map(|(id, _)| id).collect();
        let (rows, folders) = match self.copy()? {
            Some(copy) => (
                copy.accounts().map_err(|e| e.to_string())?,
                copy.unread().map_err(|e| e.to_string())?,
            ),
            None => (Vec::new(), Vec::new()),
        };
        self.recorded = rows.into_iter().map(|row| (row.id, row.state)).collect();
        self.accounts = configured
            .into_iter()
            .map(|id| AccountPane {
                state: None,
                folders: folders
                    .iter()
                    .filter(|f| f.account == id)
                    .map(|f| FolderPane {
                        folder: f.folder.clone(),
                        unread: f.unread,
                    })
                    .collect(),
                id,
            })
            .collect();
        self.refresh_states();
        Ok(())
    }

    /// The states to show: the daemon's when it told one, else the copy's, where `syncing` means
    /// nothing without a daemon (one killed mid-sync leaves it behind).
    fn refresh_states(&mut self) {
        for account in &mut self.accounts {
            account.state = match self.told.get(&account.id) {
                Some(state) => Some(state.clone()),
                None => self
                    .recorded
                    .get(&account.id)
                    .filter(|state| *state != "syncing" || self.daemon == Daemon::Running)
                    .cloned(),
            };
        }
    }

    fn first_inbox(&self) -> Option<FolderKey> {
        self.accounts.iter().find_map(|account| {
            let folder = account
                .folders
                .iter()
                .find(|f| f.folder.role.as_deref() == Some("inbox"))
                .or(account.folders.first())?;
            Some(FolderKey {
                account: account.id.clone(),
                folder: folder.folder.id,
            })
        })
    }

    fn load_messages(&mut self) {
        self.messages = Vec::new();
        let Some(key) = self.folder.clone() else {
            return;
        };
        let Some(name) = self.pane(&key).map(|pane| pane.folder.name.clone()) else {
            return;
        };
        let list = List {
            accounts: vec![key.account],
            role: None,
            name: Some(name),
            unread: false,
            limit: LIMIT,
        };
        match self.copy() {
            Ok(Some(copy)) => match copy.list(&list) {
                Ok(messages) => self.messages = messages,
                Err(e) => self.problem = Some(e.to_string()),
            },
            Ok(None) => {}
            Err(e) => self.problem = Some(e),
        }
    }

    fn still_there(&mut self, id: &str) -> bool {
        matches!(self.copy(), Ok(Some(copy)) if matches!(copy.message(id), Ok(Ok(_))))
    }

    fn read(&mut self, id: &str) -> Reader {
        let theme = if self.dark { Theme::Dark } else { Theme::Light };
        let stored = match self.copy() {
            Ok(Some(copy)) => copy.message(id),
            _ => return Reader::Gone,
        };
        let stored = match stored {
            Ok(Ok(stored)) => stored,
            Ok(Err(NotFound::Invalid | NotFound::Unknown | NotFound::Renumbered(_))) => {
                return Reader::Gone;
            }
            Err(e) => {
                self.problem = Some(e.to_string());
                return Reader::Gone;
            }
        };
        let (html, parts) = match message::read(&stored.raw) {
            Ok(body) => (Some(reduce(&body.html, &theme, REDUCE_WIDTH)), body.parts),
            Err(_) => (None, HashMap::new()),
        };
        Reader::Open(Box::new(Opened {
            id: stored.hit.id,
            subject: stored.hit.subject,
            from: stored.hit.from,
            to: stored.hit.to,
            cc: stored.cc,
            date: stored.hit.date,
            html,
            parts,
            raw: stored.raw,
        }))
    }
}
