//! The window: three panes over the [`Model`]. Accounts and folders on the left, the messages of
//! the selected folder in the middle, the open message on the right. Colours follow the system's
//! light or dark appearance; the text is the system's own font.
//!
//! What the user does to mail, from the keys or the reader's buttons, the model turns into
//! requests: the window hands them to its [`Asker`] off its own thread, one at a time and in
//! order, and gives the answers back to the model.

use std::sync::Arc;

use gpui_kit::base::{TextView, TextViewStyle};
use gpui_kit::prelude::*;
use gpui_kit::{
    App, ClickEvent, Context, ElementId, FocusHandle, FontWeight, KeyBinding, MouseButton, Rgba,
    ScrollStrategy, SharedString, SharedUri, StyleRefinement, UniformListScrollHandle, Window,
    WindowAppearance, actions, div, px, rems, rgb, uniform_list,
};
use missive_core::copy::{Person, local_time};
use missive_core::opener::followable;
use missive_core::protocol::{Answer, Command};

use crate::compose_view::{self, Chooser};
use crate::images::MessageImages;
use crate::model::{Body, Daemon, Model, Opened, Reader, Start};

actions!(
    missive_window,
    [
        SelectPrevious,
        SelectNext,
        OpenSelected,
        Back,
        MarkUnread,
        ToggleFlag,
        Archive,
        MoveTo,
        Delete,
        NewMessage,
        Reply,
        ReplyAll,
        Forward,
        EditDraft
    ]
);

/// The key context of the window.
const CONTEXT: &str = "MissiveWindow";

/// Up and down in the list, Enter to open, Escape back; U, S, A, M and Delete act on the open
/// message. In the folder picker and the confirmation, up, down, Enter and Escape are theirs.
/// N writes a new message; R, Shift+R and F answer the open message; E picks a draft up again.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("up", SelectPrevious, Some(CONTEXT)),
        KeyBinding::new("down", SelectNext, Some(CONTEXT)),
        KeyBinding::new("enter", OpenSelected, Some(CONTEXT)),
        KeyBinding::new("escape", Back, Some(CONTEXT)),
        KeyBinding::new("u", MarkUnread, Some(CONTEXT)),
        KeyBinding::new("s", ToggleFlag, Some(CONTEXT)),
        KeyBinding::new("a", Archive, Some(CONTEXT)),
        KeyBinding::new("m", MoveTo, Some(CONTEXT)),
        KeyBinding::new("delete", Delete, Some(CONTEXT)),
        KeyBinding::new("n", NewMessage, Some(CONTEXT)),
        KeyBinding::new("r", Reply, Some(CONTEXT)),
        KeyBinding::new("shift-r", ReplyAll, Some(CONTEXT)),
        KeyBinding::new("f", Forward, Some(CONTEXT)),
        KeyBinding::new("e", EditDraft, Some(CONTEXT)),
    ]);
    crate::compose_view::bind_keys(cx);
}

/// What the window asks the system to open, only ever on the user's click.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Opening {
    /// A link of the message.
    Link(String),
    /// The message's cleaned original page, as `missive open` writes it.
    Original(Vec<u8>),
}

/// Hands an [`Opening`] to the system (see `main.rs`), or to a test.
pub type Opener = Arc<dyn Fn(Opening) + Send + Sync>;

/// Asks the daemon (see `ask.rs`), or a test, and gives its answer or why it could not be asked.
/// Called off the window's thread: it may wait for the daemon to start.
pub type Asker = Arc<dyn Fn(Command) -> Result<Answer, String> + Send + Sync>;

pub struct MailWindow {
    pub model: Model,
    focus: FocusHandle,
    list: UniformListScrollHandle,
    /// The images of the open message.
    images: Arc<MessageImages>,
    opener: Opener,
    asker: Asker,
    /// Whether a request waits for the daemon's answer: the next one waits for it, so that the
    /// daemon records them in the order they were asked.
    asking: bool,
    /// The system's sans-serif family, when it is installed.
    font: Option<SharedString>,
    /// The file chooser of the compose windows this window opens.
    chooser: Chooser,
}

impl MailWindow {
    pub fn new(
        model: Model,
        opener: Opener,
        asker: Asker,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        cx.observe_window_appearance(window, |this, window, cx| {
            this.model.set_dark(is_dark(window.appearance()));
            this.changed(cx);
        })
        .detach();
        let mut model = model;
        model.set_dark(is_dark(window.appearance()));
        let font = crate::font::sans_serif(&cx.text_system().all_font_names()).map(Into::into);
        MailWindow {
            font,
            images: images_of(&model),
            model,
            focus,
            list: UniformListScrollHandle::new(),
            opener,
            asker,
            asking: false,
            chooser: Arc::new(|| Err("the file chooser is not available".to_owned())),
        }
    }

    /// The file chooser the compose windows ask for files to attach.
    pub fn with_chooser(mut self, chooser: Chooser) -> Self {
        self.chooser = chooser;
        self
    }

    /// Opens a compose window for `start`, when the model has something to start from.
    pub fn compose(&mut self, start: Start, cx: &mut Context<Self>) {
        if let Some(compose) = self.model.compose(start)
            && let Err(e) =
                compose_view::open(compose, self.asker.clone(), self.chooser.clone(), cx)
        {
            self.model.notice = Some(format!("The compose window cannot open: {e}."));
        }
        self.changed(cx);
    }

    /// After the model changed: the open message is reduced off this thread, its read timer
    /// starts, the next request goes to the daemon, the images follow the message, the list
    /// follows the cursor.
    pub fn changed(&mut self, cx: &mut Context<Self>) {
        if let Some(timer) = self.model.take_read_timer() {
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(timer.after).await;
                this.update(cx, |this, cx| {
                    this.model.read_timer_elapsed(timer);
                    this.changed(cx);
                })
            })
            .detach();
        }
        if !self.asking
            && let Some(command) = self.model.take_request()
        {
            self.asking = true;
            let asker = self.asker.clone();
            cx.spawn(async move |this, cx| {
                let answer = cx.background_spawn(async move { asker(command) }).await;
                this.update(cx, |this, cx| {
                    this.asking = false;
                    this.model.answered(answer);
                    this.changed(cx);
                })
            })
            .detach();
        }
        if let Some(reduction) = self.model.take_reduction() {
            cx.spawn(async move |this, cx| {
                let reduced = cx.background_spawn(async move { reduction.run() }).await;
                this.update(cx, |this, cx| {
                    this.model.reduced(reduced);
                    this.changed(cx);
                })
            })
            .detach();
        }
        self.images = images_of(&self.model);
        if let Some(at) = self.model.cursor {
            self.list.scroll_to_item(at, ScrollStrategy::Nearest);
        }
        cx.notify();
    }

    fn select_previous(&mut self, _: &SelectPrevious, _: &mut Window, cx: &mut Context<Self>) {
        self.step(-1);
        self.changed(cx);
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.step(1);
        self.changed(cx);
    }

    /// Up and down: in the picker when it is open, else in the list.
    fn step(&mut self, by: isize) {
        if self.model.picker.is_some() {
            self.model.picker_move(by);
        } else if self.model.confirm.is_none() {
            self.model.move_cursor(by);
        }
    }

    fn open_selected(&mut self, _: &OpenSelected, _: &mut Window, cx: &mut Context<Self>) {
        if self.model.picker.is_some() {
            self.model.choose_folder();
        } else if self.model.confirm.is_some() {
            self.model.confirm_delete();
        } else {
            self.model.open_cursor();
        }
        self.changed(cx);
    }

    fn mark_unread(&mut self, _: &MarkUnread, _: &mut Window, cx: &mut Context<Self>) {
        self.model.mark_unread();
        self.changed(cx);
    }

    fn toggle_flag(&mut self, _: &ToggleFlag, _: &mut Window, cx: &mut Context<Self>) {
        self.model.toggle_flag();
        self.changed(cx);
    }

    fn archive(&mut self, _: &Archive, _: &mut Window, cx: &mut Context<Self>) {
        self.model.archive();
        self.changed(cx);
    }

    fn move_to(&mut self, _: &MoveTo, _: &mut Window, cx: &mut Context<Self>) {
        self.model.pick_folder();
        self.changed(cx);
    }

    fn delete(&mut self, _: &Delete, _: &mut Window, cx: &mut Context<Self>) {
        self.model.delete();
        self.changed(cx);
    }

    fn new_message(&mut self, _: &NewMessage, _: &mut Window, cx: &mut Context<Self>) {
        self.compose(Start::New, cx);
    }

    fn reply(&mut self, _: &Reply, _: &mut Window, cx: &mut Context<Self>) {
        self.compose(Start::Reply, cx);
    }

    fn reply_all(&mut self, _: &ReplyAll, _: &mut Window, cx: &mut Context<Self>) {
        self.compose(Start::ReplyAll, cx);
    }

    fn forward(&mut self, _: &Forward, _: &mut Window, cx: &mut Context<Self>) {
        self.compose(Start::Forward, cx);
    }

    fn edit_draft(&mut self, _: &EditDraft, _: &mut Window, cx: &mut Context<Self>) {
        self.compose(Start::Draft, cx);
    }

    fn back(&mut self, _: &Back, _: &mut Window, cx: &mut Context<Self>) {
        self.model.back();
        self.changed(cx);
    }
}

fn images_of(model: &Model) -> Arc<MessageImages> {
    Arc::new(match &model.reader {
        Reader::Open(opened) => MessageImages::new(&opened.parts),
        _ => MessageImages::new(&Default::default()),
    })
}

pub(crate) fn is_dark(appearance: WindowAppearance) -> bool {
    matches!(
        appearance,
        WindowAppearance::Dark | WindowAppearance::VibrantDark
    )
}

/// The colours of the window, in the system's light or dark appearance.
#[derive(Clone, Copy)]
pub struct Palette {
    pub dark: bool,
    pub background: Rgba,
    pub pane: Rgba,
    pub text: Rgba,
    pub muted: Rgba,
    pub line: Rgba,
    pub link: Rgba,
    pub selected: Rgba,
    pub selection: Rgba,
    pub warning: Rgba,
}

impl Palette {
    pub fn of(window: &Window) -> Palette {
        Palette::new(is_dark(window.appearance()))
    }

    pub fn new(dark: bool) -> Palette {
        if dark {
            Palette {
                dark,
                background: rgb(0x1e1e1e),
                pane: rgb(0x262626),
                text: rgb(0xf6f5f4),
                muted: rgb(0xc0bfbc),
                line: rgb(0x3d3d3d),
                link: rgb(0x78aeed),
                selected: rgb(0x29466a),
                selection: rgb(0x35577f),
                warning: rgb(0xf8e45c),
            }
        } else {
            Palette {
                dark,
                background: rgb(0xffffff),
                pane: rgb(0xf6f5f4),
                text: rgb(0x1e1e1e),
                muted: rgb(0x5e5c64),
                line: rgb(0xdeddda),
                link: rgb(0x1c71d8),
                selected: rgb(0xd5e3f7),
                selection: rgb(0xb4d5ff),
                warning: rgb(0x9c6e03),
            }
        }
    }
}

impl Render for MailWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::of(window);
        let banner = match (&self.model.problem, self.model.daemon) {
            (Some(problem), _) => Some(problem.clone()),
            (None, Daemon::Stopped) => Some(
                "The daemon is not running: this is the local copy as it was last synced \
                 (missive sync starts it)."
                    .to_owned(),
            ),
            _ => None,
        };
        div()
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::open_selected))
            .on_action(cx.listener(Self::back))
            .on_action(cx.listener(Self::mark_unread))
            .on_action(cx.listener(Self::toggle_flag))
            .on_action(cx.listener(Self::archive))
            .on_action(cx.listener(Self::move_to))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::new_message))
            .on_action(cx.listener(Self::reply))
            .on_action(cx.listener(Self::reply_all))
            .on_action(cx.listener(Self::forward))
            .on_action(cx.listener(Self::edit_draft))
            .size_full()
            .flex()
            .flex_col()
            .bg(palette.background)
            .text_color(palette.text)
            .when_some(self.font.clone(), |root, font| root.font_family(font))
            .text_size(px(14.))
            .children(banner.map(|text| {
                div()
                    .id("banner")
                    .px_4()
                    .py_1()
                    .border_b_1()
                    .border_color(palette.line)
                    .bg(palette.pane)
                    .text_color(palette.warning)
                    .child(text)
            }))
            .children(self.model.notice.clone().map(|text| {
                div()
                    .id("notice")
                    .debug_selector(|| "notice".to_owned())
                    .px_4()
                    .py_1()
                    .border_b_1()
                    .border_color(palette.line)
                    .bg(palette.pane)
                    .text_color(palette.warning)
                    .child(text)
            }))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(self.folders(palette, cx))
                    .child(self.messages(palette, cx))
                    .child(self.reader(palette, cx)),
            )
    }
}

impl MailWindow {
    fn folders(&self, palette: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let mut pane = div()
            .id("folders")
            .w(px(220.))
            .flex_none()
            .h_full()
            .overflow_y_scroll()
            .bg(palette.pane)
            .border_r_1()
            .border_color(palette.line)
            .py_2()
            .child(
                div().px_3().pb_1().flex().child(
                    button("new-message", "New message", Some("N"), palette)
                        .on_click(cx.listener(|this, _, _, cx| this.compose(Start::New, cx))),
                ),
            );
        if self.model.accounts.is_empty() {
            pane = pane.child(
                div()
                    .px_3()
                    .text_color(palette.muted)
                    .child("No account (missive account add makes one)."),
            );
        }
        for account in &self.model.accounts {
            pane = pane.child(
                div()
                    .px_3()
                    .pt_2()
                    .flex()
                    .justify_between()
                    .child(
                        div()
                            .font_weight(FontWeight::BOLD)
                            .child(account.id.clone()),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(palette.muted)
                            .child(state_label(account.state.as_deref())),
                    ),
            );
            for folder in &account.folders {
                let selected =
                    self.model.folder.as_ref().is_some_and(|key| {
                        key.account == account.id && key.folder == folder.folder.id
                    });
                let (account_id, folder_id) = (account.id.clone(), folder.folder.id);
                let label = match &folder.folder.role {
                    Some(role) => role_label(role).to_owned(),
                    None => folder.folder.name.clone(),
                };
                pane = pane.child(
                    div()
                        .id(SharedString::from(format!(
                            "folder-{}-{}",
                            account.id, folder.folder.id
                        )))
                        .px_3()
                        .py_1()
                        .flex()
                        .justify_between()
                        .when(selected, |row| row.bg(palette.selected))
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.model.select_folder(&account_id, folder_id);
                            this.changed(cx);
                        }))
                        .child(label)
                        .when(folder.unread > 0, |row| {
                            row.child(
                                div()
                                    .font_weight(FontWeight::BOLD)
                                    .child(folder.unread.to_string()),
                            )
                        }),
                );
            }
        }
        pane
    }

    fn messages(&self, palette: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let rows: Vec<Row> = self
            .model
            .messages
            .iter()
            .enumerate()
            .map(|(at, listed)| Row {
                id: listed.hit.id.clone(),
                sender: sender(&listed.hit.from),
                subject: listed
                    .hit
                    .subject
                    .clone()
                    .unwrap_or_else(|| "(no subject)".to_owned()),
                date: listed.hit.date.map(local_time).unwrap_or_default(),
                unread: listed.hit.unread,
                attachment: listed.has_attachments,
                cursor: self.model.cursor == Some(at),
            })
            .collect();
        let view = cx.entity().downgrade();
        let pane = div()
            .w(px(380.))
            .flex_none()
            .h_full()
            .border_r_1()
            .border_color(palette.line);
        if rows.is_empty() {
            return pane.child(
                div()
                    .p_3()
                    .text_color(palette.muted)
                    .child("No message in this folder."),
            );
        }
        pane.child(
            uniform_list("messages", rows.len(), move |range, _, _| {
                rows[range]
                    .iter()
                    .map(|row| {
                        let id = row.id.clone();
                        let view = view.clone();
                        div()
                            .id(ElementId::Name(row.id.clone().into()))
                            .w_full()
                            .h(px(56.))
                            .px_3()
                            .py_1()
                            .flex()
                            .flex_col()
                            .justify_center()
                            .border_b_1()
                            .border_color(palette.line)
                            .when(row.cursor, |div| div.bg(palette.selected))
                            .when(row.unread, |div| div.font_weight(FontWeight::BOLD))
                            .cursor_pointer()
                            .on_click(move |_, _, cx| {
                                view.update(cx, |this, cx| {
                                    this.model.open(&id);
                                    this.changed(cx);
                                })
                                .ok();
                            })
                            .child(
                                div()
                                    .flex()
                                    .justify_between()
                                    .gap_2()
                                    .child(div().truncate().child(row.sender.clone()))
                                    .child(
                                        div()
                                            .flex_none()
                                            .text_size(px(12.))
                                            .text_color(palette.muted)
                                            .child(row.date.clone()),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .gap_2()
                                    .child(div().flex_1().truncate().child(row.subject.clone()))
                                    .when(row.attachment, |div| {
                                        // A word, not a paper clip: an emoji font is not on
                                        // every system.
                                        div.child(
                                            gpui_kit::div()
                                                .flex_none()
                                                .text_size(px(12.))
                                                .text_color(palette.muted)
                                                .child("attachment"),
                                        )
                                    }),
                            )
                    })
                    .collect()
            })
            .track_scroll(&self.list)
            .h_full(),
        )
    }

    fn reader(&self, palette: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let pane = div().flex_1().min_w_0().h_full().flex().flex_col();
        match &self.model.reader {
            // The keys act on the list's cursor when no message is open: their questions show
            // here too.
            Reader::Empty => pane
                .children(self.picker(palette, cx))
                .children(self.confirmation(palette, cx))
                .child(
                    div()
                        .p_4()
                        .text_color(palette.muted)
                        .child("Enter or a click opens a message."),
                ),
            Reader::Gone => pane
                .children(self.picker(palette, cx))
                .children(self.confirmation(palette, cx))
                .child(div().p_4().text_color(palette.muted).child(
                    "This message is no longer in the local copy: the server removed or moved it.",
                )),
            Reader::Open(opened) => {
                let raw = opened.raw.clone();
                let opener = self.opener.clone();
                pane.child(headers(opened, palette).child(self.buttons(opened, palette, cx).child(
                    button("view-original", "View original", None, palette).on_click(
                        cx.listener(move |_, _, _, _| {
                            opener(Opening::Original(raw.clone()));
                        }),
                    ),
                )))
                .children(self.picker(palette, cx))
                .children(self.confirmation(palette, cx))
                .child(
                    div()
                        .id("body")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .px_6()
                        .py_4()
                        .child(match &opened.body {
                            Body::Preparing => div()
                                .text_color(palette.muted)
                                .child("Preparing…")
                                .into_any_element(),
                            Body::Reduced(html) => body(
                                &opened.id,
                                html,
                                self.images.clone(),
                                self.opener.clone(),
                                palette,
                            )
                            .into_any_element(),
                            Body::Text(text) => {
                                let raw = opened.raw.clone();
                                let opener = self.opener.clone();
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_3()
                                    .child(
                                        div()
                                            .id("text-only")
                                            .debug_selector(|| "text-only".to_owned())
                                            .text_color(palette.muted)
                                            .cursor_pointer()
                                            .on_click(cx.listener(move |_, _, _, _| {
                                                opener(Opening::Original(raw.clone()));
                                            }))
                                            .child("Shown as text: this message is too large or too deep to lay out. View original shows it as it was sent."),
                                    )
                                    .child(text.clone())
                                    .into_any_element()
                            }
                            Body::None => div()
                                .text_color(palette.muted)
                                .child("This message has no readable body: View original shows it as it was sent.")
                                .into_any_element(),
                        }),
                )
            }
        }
    }
}

impl MailWindow {
    /// The reader's buttons, each with its key.
    fn buttons(&self, opened: &Opened, palette: Palette, cx: &mut Context<Self>) -> gpui_kit::Div {
        let act = |f: fn(&mut Model)| {
            cx.listener(move |this: &mut Self, _: &ClickEvent, _, cx| {
                f(&mut this.model);
                this.changed(cx);
            })
        };
        let (flag, flag_label) = if opened.flagged {
            ("flag", "Unflag")
        } else {
            ("flag", "Flag")
        };
        let forever = self.model.deletes_for_good();
        let start = |start: Start| {
            cx.listener(move |this: &mut Self, _: &ClickEvent, _, cx| this.compose(start, cx))
        };
        let draft = opened.folder.role.as_deref() == Some("drafts");
        div()
            .mt_2()
            .flex()
            .flex_wrap()
            .gap_2()
            .when(draft, |row| {
                row.child(
                    button("edit-draft", "Edit draft", Some("E"), palette)
                        .on_click(start(Start::Draft)),
                )
            })
            .child(button("reply", "Reply", Some("R"), palette).on_click(start(Start::Reply)))
            .child(
                button("reply-all", "Reply all", Some("Shift+R"), palette)
                    .on_click(start(Start::ReplyAll)),
            )
            .child(button("forward", "Forward", Some("F"), palette).on_click(start(Start::Forward)))
            .when(!opened.unread, |row| {
                row.child(
                    button("mark-unread", "Mark unread", Some("U"), palette)
                        .on_click(act(Model::mark_unread)),
                )
            })
            .child(button(flag, flag_label, Some("S"), palette).on_click(act(Model::toggle_flag)))
            .when(self.model.can_archive(), |row| {
                row.child(
                    button("archive", "Archive", Some("A"), palette).on_click(act(Model::archive)),
                )
            })
            .child(
                button("move-to", "Move to…", Some("M"), palette).on_click(act(Model::pick_folder)),
            )
            .child(
                button(
                    "delete",
                    if forever {
                        "Delete for good…"
                    } else {
                        "Delete"
                    },
                    Some("Delete"),
                    palette,
                )
                .on_click(act(Model::delete)),
            )
    }

    /// The folder picker of "Move to…", under the reader's header.
    fn picker(&self, palette: Palette, cx: &mut Context<Self>) -> Option<gpui_kit::Div> {
        let picker = self.model.picker.as_ref()?;
        let mut list = div()
            .px_6()
            .py_2()
            .flex()
            .flex_col()
            .border_b_1()
            .border_color(palette.line)
            .bg(palette.pane)
            .child(
                div()
                    .text_color(palette.muted)
                    .child("Move to (Enter to move, Escape to cancel):"),
            );
        for (at, folder) in picker.folders.iter().enumerate() {
            let label = match &folder.role {
                Some(role) => role_label(role).to_owned(),
                None => folder.name.clone(),
            };
            list = list.child(
                div()
                    .id(ElementId::Name(format!("pick-{at}").into()))
                    .debug_selector(move || format!("pick-{at}"))
                    .px_2()
                    .py_1()
                    .rounded(px(4.))
                    .when(picker.cursor == at, |row| row.bg(palette.selected))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.model.choose_folder_at(at);
                        this.changed(cx);
                    }))
                    .child(label),
            );
        }
        Some(list)
    }

    /// The question before a message is deleted for good: nothing else asks first.
    fn confirmation(&self, palette: Palette, cx: &mut Context<Self>) -> Option<gpui_kit::Div> {
        self.model.confirm.as_ref()?;
        Some(
            div()
                .px_6()
                .py_2()
                .flex()
                .flex_col()
                .gap_2()
                .border_b_1()
                .border_color(palette.line)
                .bg(palette.pane)
                .child(
                    div()
                        .text_color(palette.warning)
                        .child("Delete this message for good? It cannot be undone."),
                )
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(
                            button("confirm-delete", "Delete for good", Some("Enter"), palette)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.model.confirm_delete();
                                    this.changed(cx);
                                })),
                        )
                        .child(
                            button("cancel-delete", "Cancel", Some("Escape"), palette).on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.model.dismiss();
                                    this.changed(cx);
                                }),
                            ),
                        ),
                ),
        )
    }
}

/// A button of the reader, with the key that does the same.
pub(crate) fn button(
    id: &'static str,
    label: &'static str,
    key: Option<&'static str>,
    palette: Palette,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    div()
        .id(id)
        .debug_selector(move || id.to_owned())
        .px_3()
        .py_1()
        .flex()
        .flex_none()
        .gap_2()
        .border_1()
        .border_color(palette.line)
        .rounded(px(6.))
        .cursor_pointer()
        .child(label)
        .children(key.map(|key| div().text_color(palette.muted).child(key)))
}

/// A line of the message list.
struct Row {
    id: String,
    sender: String,
    subject: String,
    date: String,
    unread: bool,
    attachment: bool,
    cursor: bool,
}

fn headers(opened: &Opened, palette: Palette) -> gpui_kit::Div {
    let line = |label: &'static str, value: String| {
        div()
            .flex()
            .gap_2()
            .child(
                div()
                    .w(px(48.))
                    .flex_none()
                    .text_color(palette.muted)
                    .child(label),
            )
            .child(div().flex_1().min_w_0().child(value))
    };
    div()
        .px_6()
        .py_3()
        .flex()
        .flex_col()
        .gap_1()
        .border_b_1()
        .border_color(palette.line)
        .child(
            div()
                .text_size(px(18.))
                .font_weight(FontWeight::BOLD)
                .child(
                    opened
                        .subject
                        .clone()
                        .unwrap_or_else(|| "(no subject)".to_owned()),
                ),
        )
        .child(line("From", people(&opened.from, "")))
        .child(line("To", people(&opened.to, "")))
        .when(!opened.cc.is_empty(), |div| {
            div.child(line("Cc", people(&opened.cc, "")))
        })
        .child(line(
            "Date",
            opened.date.map(local_time).unwrap_or_default(),
        ))
}

/// The body of a message in its reduced form, drawn by gpui-base's text view. Both of its hooks
/// are always set: its image source is the message's own, and a link opens only on a click.
pub fn body(
    id: &str,
    html: &str,
    images: Arc<MessageImages>,
    opener: Opener,
    palette: Palette,
) -> TextView {
    let style = TextViewStyle::default()
        .with_foreground(palette.text.into())
        .with_muted_foreground(palette.muted.into())
        .with_link(palette.link.into())
        .with_border(palette.line.into())
        .with_selection(palette.selection.into())
        // Tighter than the text view's own: the lines of a plain-text message are blocks.
        .with_paragraph_gap(rems(0.4))
        // The text view's tables are otherwise drawn on gpui-base's light surface.
        .with_table(StyleRefinement::default().bg(palette.background))
        .with_table_head(StyleRefinement::default().bg(palette.pane))
        .with_table_cell(StyleRefinement::default().text_color(palette.text))
        .with_dark(palette.dark);
    TextView::html(SharedString::from(format!("message-{id}")), html.to_owned())
        .style(style)
        .selectable(true)
        // Authoritative: nothing outside the message is ever asked for.
        .image_source(move |uri: &SharedUri| images.source(uri.as_ref()))
        .on_link_click(move |url, event, _, _| {
            if clicked(event) && followable(url) {
                opener(Opening::Link(url.to_string()));
            }
        })
}

/// Whether a click is the user's choice to follow a link: the main button, a key, or a tap.
fn clicked(event: &ClickEvent) -> bool {
    match event {
        ClickEvent::Mouse(click) => click.up.button == MouseButton::Left,
        ClickEvent::Keyboard(_) => true,
        ClickEvent::Touch(tap) => !tap.long_press,
    }
}

/// Who sent a message, as the list shows it: the first sender's name, else their address.
fn sender(from: &[Person]) -> String {
    from.first()
        .and_then(|p| {
            p.name
                .clone()
                .filter(|n| !n.is_empty())
                .or(p.address.clone())
        })
        .unwrap_or_else(|| "(no sender)".to_owned())
}

/// Names and addresses as a header line: the name when there is one, else the address.
fn people(people: &[Person], none: &str) -> String {
    let names: Vec<String> = people
        .iter()
        .filter_map(|p| match (&p.name, &p.address) {
            (Some(name), Some(address)) if !name.is_empty() => Some(format!("{name} <{address}>")),
            (Some(name), None) => Some(name.clone()),
            (_, Some(address)) => Some(address.clone()),
            _ => None,
        })
        .collect();
    if names.is_empty() {
        none.to_owned()
    } else {
        names.join(", ")
    }
}

fn role_label(role: &str) -> &str {
    match role {
        "inbox" => "Inbox",
        "sent" => "Sent",
        "drafts" => "Drafts",
        "trash" => "Trash",
        "junk" => "Junk",
        "archive" => "Archive",
        other => other,
    }
}

/// An account's connection state, for people.
pub fn state_label(state: Option<&str>) -> &'static str {
    match state {
        Some("ready") => "ready",
        Some("syncing") => "syncing",
        Some("unreachable") => "unreachable",
        Some("no-login") => "no login",
        Some("auth-refused") => "login refused",
        Some("certificate-refused") => "certificate refused",
        Some(_) | None => "unknown",
    }
}
