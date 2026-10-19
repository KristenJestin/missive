//! What the window shows, without the window: the accounts and their folders, the messages of
//! the selected folder, and the message open in the reader. It reads the local copy, opened read
//! only as `missive list` and `show` open it, and follows the daemon's events to know what to read
//! again. It never writes the copy: what the user does to mail (mark, move, delete) becomes a
//! request to the daemon, which the window hands over (see `view.rs`), and the daemon changes the
//! copy and the server.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use missive_core::account::{AccountStore, Listing};
use missive_core::body;
use missive_core::copy::{Copy, FolderRef, List, Listed, MessageId, NotFound, Person};
use missive_core::events::{ActionEvent, Event};
use missive_core::protocol::{Answer, Command};
use missive_core::reply::{self, Mode};
use missive_core::store::Store;
use missive_render::message;
use missive_render::reduce::{Theme, reduce};

use crate::compose::Compose;

/// The most messages a folder lists, the newest first.
pub const LIMIT: usize = 500;

/// The width the reduction computes styles at: what the author's CSS hides does not depend on it
/// in practice, and the window lays the result out at its own width.
const REDUCE_WIDTH: u32 = 800;

/// How long a message may take to reduce before its text is shown instead.
const BUDGET: Duration = Duration::from_secs(2);

/// How long a message is shown before it is marked read: moving down the list does not mark every
/// message it passes.
pub const READ_AFTER: Duration = Duration::from_secs(2);

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
    /// The folder the message is in.
    pub folder: FolderRef,
    pub unread: bool,
    pub flagged: bool,
    pub subject: Option<String>,
    pub from: Vec<Person>,
    pub to: Vec<Person>,
    pub cc: Vec<Person>,
    pub date: Option<i64>,
    pub body: Body,
    /// The message's parts named by a Content-ID, for its `cid:` images.
    pub parts: HashMap<String, Vec<u8>>,
    /// The message as the server sent it, for "View original".
    pub raw: Vec<u8>,
}

/// The body of the open message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Body {
    /// Being reduced, off the window's thread.
    Preparing,
    /// The reduced form.
    Reduced(String),
    /// The message's text: its body could not be reduced (too large, too deep, too long, or the
    /// reduction failed).
    Text(String),
    /// The message has no readable body.
    None,
}

/// The wait before the open message is marked read, to start with [`Model::take_read_timer`]
/// and end with [`Model::read_timer_elapsed`].
#[derive(Debug)]
pub struct ReadTimer {
    /// Which showing of a message it is for: another message, or the reader closed, ends it.
    shown: u64,
    pub after: Duration,
}

/// The folders a message can be moved to, with the one the keyboard is on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Picker {
    /// The message to move.
    message: String,
    pub folders: Vec<FolderRef>,
    pub cursor: usize,
}

/// The message an action acts on.
struct Target {
    id: String,
    folder: FolderRef,
    unread: bool,
    flagged: bool,
}

/// A message to reduce off the window's thread: [`Reduction::run`] takes up to its budget.
pub struct Reduction {
    /// Which opening it is for: a later one makes it stale.
    opening: u64,
    raw: Vec<u8>,
    theme: Theme,
    pub budget: Duration,
}

/// What a reduction gives back to the model.
pub struct Reduced {
    opening: u64,
    body: Body,
    parts: HashMap<String, Vec<u8>>,
}

impl Reduction {
    pub fn run(self) -> Reduced {
        let (body, parts) = match message::read(&self.raw) {
            Ok(message) => {
                let body = match reduce(&message.html, self.theme, REDUCE_WIDTH, self.budget) {
                    Ok(html) => Body::Reduced(html),
                    Err(_) => Body::Text(body::read(&self.raw).readable),
                };
                (body, message.parts)
            }
            Err(_) => (Body::None, HashMap::new()),
        };
        Reduced {
            opening: self.opening,
            body,
            parts,
        }
    }
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
    /// How many times a message was read for the reader: the last one is the one shown.
    openings: u64,
    reduction: Option<Reduction>,
    /// How many times a message was opened: the last one is the one a read timer is for.
    shown: u64,
    /// The showing whose read timer runs, until it ends or the user marks the message unread.
    reading: Option<u64>,
    /// A read timer for the window to start.
    read_timer: Option<ReadTimer>,
    /// What to ask the daemon, in order.
    requests: std::collections::VecDeque<Command>,
    /// The folder picker of "Move to…", when it is open.
    pub picker: Option<Picker>,
    /// The message to delete for good once the user confirms it.
    pub confirm: Option<String>,
    /// What went wrong with an action, for people, until the next action.
    pub notice: Option<String>,
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
            openings: 0,
            reduction: None,
            shown: 0,
            reading: None,
            read_timer: None,
            requests: Default::default(),
            picker: None,
            confirm: None,
            notice: None,
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
            match self.flags_of(&id) {
                Some((unread, flagged)) => {
                    if let Reader::Open(opened) = &mut self.reader {
                        opened.unread = unread;
                        opened.flagged = flagged;
                    }
                }
                None => self.reader = Reader::Gone,
            }
        }
        self.drop_stale_dialogs();
    }

    /// Closes a picker or a confirmation whose message is neither open nor listed any more: it
    /// would stay on a screen that no longer shows it, and keep the keys from acting.
    fn drop_stale_dialogs(&mut self) {
        let message = match (&self.picker, &self.confirm) {
            (Some(picker), _) => &picker.message,
            (None, Some(id)) => id,
            (None, None) => return,
        };
        let here = matches!(&self.reader, Reader::Open(opened) if opened.id == *message)
            || self.messages.iter().any(|m| m.hit.id == *message);
        if !here {
            self.dismiss();
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
            // The outbox is not shown here: a sending only says that a daemon runs.
            Event::SyncStarted { .. }
            | Event::Sent(_)
            | Event::SendFailed(_)
            | Event::SendUncertain(_)
            | Event::SentCopyFailed(_) => self.daemon = Daemon::Running,
            Event::Action(action) => {
                if action.outcome == "refused" {
                    self.notice = Some(refused(action));
                }
                self.daemon = Daemon::Running;
                self.refresh();
            }
            Event::NewMessage(_)
            | Event::Unread(_)
            | Event::SyncEnded { .. }
            // A draft saved or removed changes the Drafts folder.
            | Event::Draft(_)
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
        self.dismiss();
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

    /// Opens a message of the list, and puts the cursor on it. An unread message asks for a
    /// read timer: it is marked read once shown for [`READ_AFTER`].
    pub fn open(&mut self, id: &str) {
        if let Some(at) = self.messages.iter().position(|m| m.hit.id == id) {
            self.cursor = Some(at);
        }
        self.dismiss();
        self.reader = self.read(id);
        self.shown += 1;
        self.reading = None;
        self.read_timer = None;
        if matches!(&self.reader, Reader::Open(opened) if opened.unread) {
            self.reading = Some(self.shown);
            self.read_timer = Some(ReadTimer {
                shown: self.shown,
                after: READ_AFTER,
            });
        }
    }

    /// Closes the reader, a picker or a confirmation open on it first.
    pub fn back(&mut self) {
        if self.picker.is_some() || self.confirm.is_some() {
            return self.dismiss();
        }
        self.reader = Reader::Empty;
        self.reduction = None;
        self.reading = None;
    }

    /// The read timer the open message asks for, to start once.
    pub fn take_read_timer(&mut self) -> Option<ReadTimer> {
        self.read_timer.take()
    }

    /// The message was shown for [`READ_AFTER`]: it is marked read, unless the user moved on or
    /// marked it unread in the meantime. Offline too: the daemon queues it.
    pub fn read_timer_elapsed(&mut self, timer: ReadTimer) {
        if self.reading != Some(timer.shown) {
            return;
        }
        self.reading = None;
        if let Reader::Open(opened) = &self.reader
            && opened.unread
        {
            let id = opened.id.clone();
            self.ask(Command::Mark {
                messages: vec![id],
                mark_as: "read".to_owned(),
            });
        }
    }

    /// The next request for the daemon, in the order they were asked.
    pub fn take_request(&mut self) -> Option<Command> {
        self.requests.pop_front()
    }

    /// The daemon's answer to a request, or why it could not be asked: the copy shows what the
    /// daemon recorded.
    pub fn answered(&mut self, answer: Result<Answer, String>) {
        match answer {
            Ok(answer) if answer.ok => {}
            Ok(answer) => {
                let error = answer.error.unwrap_or_default();
                self.notice = Some(match answer.message {
                    Some(message) => format!("Refused ({error}): {message}"),
                    None => format!("Refused ({error})."),
                });
            }
            Err(e) => self.notice = Some(format!("The daemon could not be asked: {e}")),
        }
        self.refresh();
    }

    /// The message an action acts on: the open one, else the one under the list's cursor.
    fn target(&self) -> Option<Target> {
        if let Reader::Open(opened) = &self.reader {
            return Some(Target {
                id: opened.id.clone(),
                folder: opened.folder.clone(),
                unread: opened.unread,
                flagged: opened.flagged,
            });
        }
        let listed = self.messages.get(self.cursor?)?;
        Some(Target {
            id: listed.hit.id.clone(),
            folder: listed.hit.folder.clone(),
            unread: listed.hit.unread,
            flagged: listed.flagged,
        })
    }

    /// `U`: marks the message unread; its read timer stops either way.
    pub fn mark_unread(&mut self) {
        if self.modal() {
            return;
        }
        self.reading = None;
        if let Some(target) = self.target()
            && !target.unread
        {
            self.ask(Command::Mark {
                messages: vec![target.id],
                mark_as: "unread".to_owned(),
            });
        }
    }

    /// `S`: flags the message, or takes its flag off.
    pub fn toggle_flag(&mut self) {
        if self.modal() {
            return;
        }
        if let Some(target) = self.target() {
            let mark_as = if target.flagged {
                "unflagged"
            } else {
                "flagged"
            };
            self.ask(Command::Mark {
                messages: vec![target.id],
                mark_as: mark_as.to_owned(),
            });
        }
    }

    /// Whether the message can be archived: its account has a folder of the role `archive`, and
    /// the message is not in it.
    pub fn can_archive(&self) -> bool {
        let Some(target) = self.target() else {
            return false;
        };
        self.folders_of(&target.id)
            .iter()
            .any(|f| f.role.as_deref() == Some("archive") && f.id != target.folder.id)
    }

    /// `A`: moves the message to the account's archive.
    pub fn archive(&mut self) {
        if !self.modal()
            && self.can_archive()
            && let Some(target) = self.target()
        {
            self.leave(
                target.id.clone(),
                Command::Move {
                    messages: vec![target.id],
                    to: "archive".to_owned(),
                },
            );
        }
    }

    /// `M`: opens the folder picker for the message: every folder of its account but its own.
    pub fn pick_folder(&mut self) {
        if self.modal() {
            return;
        }
        if let Some(target) = self.target() {
            let folders: Vec<FolderRef> = self
                .folders_of(&target.id)
                .into_iter()
                .filter(|f| f.id != target.folder.id)
                .collect();
            if !folders.is_empty() {
                self.picker = Some(Picker {
                    message: target.id,
                    folders,
                    cursor: 0,
                });
            }
        }
    }

    /// Moves the picker's cursor by `by` folders, staying in the list.
    pub fn picker_move(&mut self, by: isize) {
        if let Some(picker) = &mut self.picker {
            picker.cursor = picker
                .cursor
                .saturating_add_signed(by)
                .min(picker.folders.len() - 1);
        }
    }

    /// Moves the message to the folder under the picker's cursor.
    pub fn choose_folder(&mut self) {
        if let Some(picker) = self.picker.take()
            && let Some(folder) = picker.folders.get(picker.cursor)
        {
            // A role is looked for first by the daemon: a folder that has one is named by it.
            let to = folder.role.clone().unwrap_or_else(|| folder.name.clone());
            self.leave(
                picker.message.clone(),
                Command::Move {
                    messages: vec![picker.message],
                    to,
                },
            );
        }
    }

    /// Moves the message to the folder at `at` of the picker.
    pub fn choose_folder_at(&mut self, at: usize) {
        if let Some(picker) = &mut self.picker {
            picker.cursor = at.min(picker.folders.len() - 1);
        }
        self.choose_folder();
    }

    /// `Delete`: moves the message to the trash. In the trash, it is deleted for good, and only
    /// once the user confirms it.
    pub fn delete(&mut self) {
        if self.modal() {
            return;
        }
        let Some(target) = self.target() else {
            return;
        };
        if target.folder.role.as_deref() == Some("trash") {
            self.confirm = Some(target.id);
            return;
        }
        self.leave(
            target.id.clone(),
            Command::Delete {
                messages: vec![target.id],
                permanently: false,
            },
        );
    }

    /// Whether `Delete` deletes the message for good: only in the trash. In an account that has
    /// none, the daemon refuses the move (`no-trash`), and the notice says so.
    pub fn deletes_for_good(&self) -> bool {
        self.target()
            .is_some_and(|target| target.folder.role.as_deref() == Some("trash"))
    }

    /// The user confirmed deleting a message for good.
    pub fn confirm_delete(&mut self) {
        if let Some(id) = self.confirm.take() {
            self.leave(
                id.clone(),
                Command::Delete {
                    messages: vec![id],
                    permanently: true,
                },
            );
        }
    }

    /// Closes the folder picker or the confirmation, doing nothing.
    pub fn dismiss(&mut self) {
        self.picker = None;
        self.confirm = None;
    }

    /// Whether a picker or a confirmation waits for the user.
    pub fn modal(&self) -> bool {
        self.picker.is_some() || self.confirm.is_some()
    }

    fn ask(&mut self, command: Command) {
        self.notice = None;
        self.requests.push_back(command);
    }

    /// Asks for a request that takes message `id` away, and moves on as most mail clients do: if
    /// it is open, the message below it opens, else the one above, else the reader closes; if it
    /// is only under the cursor, the cursor goes to that message, so that a second key does not
    /// ask for the same one again.
    fn leave(&mut self, id: String, command: Command) {
        self.ask(command);
        let next = self
            .messages
            .iter()
            .position(|m| m.hit.id == id)
            .and_then(|at| {
                if at + 1 < self.messages.len() {
                    Some(at + 1)
                } else {
                    at.checked_sub(1)
                }
            });
        let open = matches!(&self.reader, Reader::Open(opened) if opened.id == id);
        match (open, next) {
            (true, Some(next)) => {
                let next = self.messages[next].hit.id.clone();
                self.open(&next);
            }
            (true, None) => self.back(),
            (false, next) => self.cursor = next.or(self.cursor),
        }
    }

    /// The folders of the account a message is of.
    fn folders_of(&self, id: &str) -> Vec<FolderRef> {
        let account = id.split('/').next().unwrap_or_default();
        self.accounts
            .iter()
            .find(|a| a.id == account)
            .map(|a| a.folders.iter().map(|f| f.folder.clone()).collect())
            .unwrap_or_default()
    }

    /// The reduction the open message waits for, to run off the window's thread.
    pub fn take_reduction(&mut self) -> Option<Reduction> {
        self.reduction.take()
    }

    /// Shows a reduction's result, unless another message (or the same one again) was opened
    /// since.
    pub fn reduced(&mut self, reduced: Reduced) {
        if reduced.opening != self.openings {
            return;
        }
        if let Reader::Open(opened) = &mut self.reader {
            opened.body = reduced.body;
            opened.parts = reduced.parts;
        }
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
            self.drop_stale_dialogs();
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

    /// Whether a message is unread and flagged, `None` when it is no longer in the copy.
    fn flags_of(&mut self, id: &str) -> Option<(bool, bool)> {
        match self.copy() {
            Ok(Some(copy)) => match copy.message(id) {
                Ok(Ok(stored)) => Some((stored.hit.unread, stored.flagged)),
                _ => None,
            },
            _ => None,
        }
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
        self.openings += 1;
        self.reduction = Some(Reduction {
            opening: self.openings,
            raw: stored.raw.clone(),
            theme,
            budget: BUDGET,
        });
        Reader::Open(Box::new(Opened {
            id: stored.hit.id,
            folder: stored.hit.folder,
            unread: stored.hit.unread,
            flagged: stored.flagged,
            subject: stored.hit.subject,
            from: stored.hit.from,
            to: stored.hit.to,
            cc: stored.cc,
            date: stored.hit.date,
            body: Body::Preparing,
            parts: HashMap::new(),
            raw: stored.raw,
        }))
    }
}

/// What a compose window starts from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Start {
    /// `N`: a new message, from the account of the folder shown.
    New,
    /// `R`, `Shift+R`, `F`: an answer to the open message.
    Reply,
    ReplyAll,
    Forward,
    /// `E`: the draft open, or under the cursor in a Drafts folder, picked up again.
    Draft,
}

impl Model {
    /// The compose window to open for `start`, or `None` when there is nothing to start from;
    /// what prevents it is said in the notice.
    pub fn compose(&mut self, start: Start) -> Option<Compose> {
        if self.modal() {
            return None;
        }
        let accounts: Vec<String> = self.accounts.iter().map(|a| a.id.clone()).collect();
        let mode = match start {
            Start::New => {
                let account = self
                    .folder
                    .as_ref()
                    .map(|key| key.account.clone())
                    .or_else(|| accounts.first().cloned());
                let Some(account) = account else {
                    self.notice = Some(
                        "No account to write from (missive account add makes one).".to_owned(),
                    );
                    return None;
                };
                return Some(Compose::new(accounts, account));
            }
            Start::Draft => return self.draft_to_edit(accounts),
            Start::Reply => Some(Mode::Reply),
            Start::ReplyAll => Some(Mode::ReplyAll),
            Start::Forward => None,
        };
        let (id, raw, subject) = self.answered_message()?;
        let account = MessageId::parse(&id)?.account;
        let answering = self
            .sources
            .accounts
            .get(&account)
            .map_err(|e| format!("the account cannot be read ({e})"))
            .and_then(|found| found.ok_or_else(|| format!("no account {account}")))
            .and_then(|found| {
                reply::answering(&id, &raw, subject.as_deref(), mode, &found.address)
            });
        match (answering, mode) {
            (Ok(answering), Some(mode)) => {
                // Each account's address: choosing another sending account works the recipients
                // out again.
                let addresses = self
                    .sources
                    .accounts
                    .list()
                    .map(|listing| {
                        listing
                            .accounts
                            .into_iter()
                            .map(|(id, found)| (id, found.address))
                            .collect()
                    })
                    .unwrap_or_default();
                Some(
                    Compose::answering(accounts, account, answering)
                        .with_recipients_of(&raw, mode, addresses),
                )
            }
            (Ok(answering), None) => Some(Compose::answering(accounts, account, answering)),
            (Err(e), _) => {
                self.notice = Some(format!("Cannot answer: {e}."));
                None
            }
        }
    }

    /// The message a reply or a forward starts from, with its source and subject: the open one,
    /// else the one under the list's cursor.
    fn answered_message(&mut self) -> Option<(String, Vec<u8>, Option<String>)> {
        if let Reader::Open(opened) = &self.reader {
            return Some((
                opened.id.clone(),
                opened.raw.clone(),
                opened.subject.clone(),
            ));
        }
        let id = self.under_cursor()?.to_owned();
        match self.copy() {
            Ok(Some(copy)) => match copy.message(&id) {
                Ok(Ok(stored)) => Some((stored.hit.id, stored.raw, stored.hit.subject)),
                _ => None,
            },
            _ => None,
        }
    }

    /// The draft open in the reader, else the one under the cursor, when it is in a Drafts
    /// folder: a draft waiting for the server is listed by its Message-ID, and only the copy's
    /// draft table has it.
    fn draft_to_edit(&mut self, accounts: Vec<String>) -> Option<Compose> {
        let (id, account) = match &self.reader {
            Reader::Open(opened) if opened.folder.role.as_deref() == Some("drafts") => {
                (opened.id.clone(), MessageId::parse(&opened.id)?.account)
            }
            _ => {
                let listed = self.messages.get(self.cursor?)?;
                if listed.hit.folder.role.as_deref() != Some("drafts") {
                    return None;
                }
                (listed.hit.id.clone(), listed.hit.account.clone())
            }
        };
        let raw = match self.copy() {
            Ok(Some(copy)) => copy
                .draft(&account, &id)
                .and_then(|raw| {
                    raw.map(|raw| {
                        copy.followup(&account, &raw)
                            .map(|followup| (raw, followup))
                    })
                    .transpose()
                })
                .map_err(|e| e.to_string()),
            Ok(None) => Ok(None),
            Err(e) => Err(e),
        };
        let found = match raw {
            Ok(Some((raw, followup))) => Compose::draft(accounts, account, id, &raw, followup),
            Ok(None) => Err("it is no longer in the local copy".to_owned()),
            Err(e) => Err(e),
        };
        match found {
            Ok(compose) => Some(compose),
            Err(e) => {
                self.notice = Some(format!("Cannot pick the draft up again: {e}."));
                None
            }
        }
    }
}

/// A notice for an action the server refused, with the reason's kind.
fn refused(action: &ActionEvent) -> String {
    let what = match action.action.as_str() {
        "read" => "Marking read",
        "unread" => "Marking unread",
        "flagged" => "Flagging",
        "unflagged" => "Taking the flag off",
        "answered" => "Marking answered",
        "forwarded" => "Marking forwarded",
        "move" => "Moving",
        "delete" => "Moving to the trash",
        "delete-permanently" => "Deleting for good",
        _ => "An action",
    };
    let reason = action.reason.as_deref().unwrap_or("refused");
    format!("{what} was refused by the server ({reason}); the message is back as it was.")
}
