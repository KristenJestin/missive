//! The window: three panes over the [`Model`]. Accounts and folders on the left, the messages of
//! the selected folder in the middle, the open message on the right. Colours follow the system's
//! light or dark appearance; the text is the system's own font.

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

use crate::images::MessageImages;
use crate::model::{Body, Daemon, Model, Opened, Reader};

actions!(
    missive_window,
    [SelectPrevious, SelectNext, OpenSelected, Back]
);

/// The key context of the window.
const CONTEXT: &str = "MissiveWindow";

/// Up and down in the list, Enter to open, Escape back.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("up", SelectPrevious, Some(CONTEXT)),
        KeyBinding::new("down", SelectNext, Some(CONTEXT)),
        KeyBinding::new("enter", OpenSelected, Some(CONTEXT)),
        KeyBinding::new("escape", Back, Some(CONTEXT)),
    ]);
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

pub struct MailWindow {
    pub model: Model,
    focus: FocusHandle,
    list: UniformListScrollHandle,
    /// The images of the open message.
    images: Arc<MessageImages>,
    opener: Opener,
    /// The system's sans-serif family, when it is installed.
    font: Option<SharedString>,
}

impl MailWindow {
    pub fn new(model: Model, opener: Opener, window: &mut Window, cx: &mut Context<Self>) -> Self {
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
        }
    }

    /// After the model changed: the open message is reduced off this thread, the images follow
    /// it, the list follows the cursor.
    pub fn changed(&mut self, cx: &mut Context<Self>) {
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
        self.model.move_cursor(-1);
        self.changed(cx);
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.model.move_cursor(1);
        self.changed(cx);
    }

    fn open_selected(&mut self, _: &OpenSelected, _: &mut Window, cx: &mut Context<Self>) {
        self.model.open_cursor();
        self.changed(cx);
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

fn is_dark(appearance: WindowAppearance) -> bool {
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
            .py_2();
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
            Reader::Empty => pane.child(
                div()
                    .p_4()
                    .text_color(palette.muted)
                    .child("Enter or a click opens a message."),
            ),
            Reader::Gone => pane.child(div().p_4().text_color(palette.muted).child(
                "This message is no longer in the local copy: the server removed or moved it.",
            )),
            Reader::Open(opened) => {
                let raw = opened.raw.clone();
                let opener = self.opener.clone();
                pane.child(headers(opened, palette).child(
                    div()
                        .id("view-original")
                        .debug_selector(|| "view-original".to_owned())
                        .mt_2()
                        .px_3()
                        .py_1()
                        .w_auto()
                        .flex_none()
                        .self_start()
                        .border_1()
                        .border_color(palette.line)
                        .rounded(px(6.))
                        .cursor_pointer()
                        .on_click(cx.listener(move |_, _, _, _| {
                            opener(Opening::Original(raw.clone()));
                        }))
                        .child("View original"),
                ))
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
