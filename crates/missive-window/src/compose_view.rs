//! The compose window: a second window over a [`Compose`]. From, To, Cc, Bcc (shown when asked),
//! Subject, the attachments and the text, plain only. The fields hand what is typed to the
//! model; the window starts the save timer, hands the model's requests to the [`Asker`] off its
//! own thread, one at a time, and closes once the message is sent or its draft saved.

use std::path::PathBuf;
use std::sync::Arc;

use gpui_kit::base::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::base::{ColorTokens, Theme};
use gpui_kit::prelude::*;
use gpui_kit::{
    App, Bounds, Context, Entity, ExternalPaths, FocusHandle, Focusable, FontWeight, KeyBinding,
    SharedString, Window, WindowBounds, WindowHandle, WindowOptions, actions, div, px, size,
};

use missive_core::draft_lock::{DraftLock, Taking};

use crate::compose::{self, Compose, Field, State};
use crate::view::{Asker, Palette, button, is_dark};

actions!(missive_compose, [SendMessage, CloseCompose]);

/// The key context of the compose window.
const CONTEXT: &str = "ComposeWindow";

/// `Ctrl+Enter` sends, `Ctrl+W` closes. `Ctrl+Enter` is bound in the text fields' own context
/// too: theirs would otherwise add a line before the window sees it.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("ctrl-enter", SendMessage, Some(CONTEXT)),
        KeyBinding::new("ctrl-enter", SendMessage, Some("Input")),
        KeyBinding::new("ctrl-w", CloseCompose, Some(CONTEXT)),
    ]);
}

/// Asks the system's file chooser (see `portal.rs`), or a test, for the files to attach: none
/// when the user cancels. Called off the window's thread.
pub type Chooser = Arc<dyn Fn() -> Result<Vec<PathBuf>, String> + Send + Sync>;

/// Takes the lock of a draft (its account, its `Message-ID`): `missive_core::draft_lock::take`,
/// or a test's own, so that no test touches the real runtime folder.
pub type Locker = Arc<dyn Fn(&str, &str) -> Taking + Send + Sync>;

/// Opens a compose window, unless another process has one open on the same draft: the window
/// holds the draft's lock for as long as it is open.
pub fn open(
    compose: Compose,
    asker: Asker,
    chooser: Chooser,
    locker: Locker,
    cx: &mut App,
) -> Result<WindowHandle<ComposeWindow>, String> {
    let lock = match compose.draft_key().map(|key| locker(&compose.account, key)) {
        Some(Taking::Elsewhere) => {
            return Err("this draft is already open in another compose window".to_owned());
        }
        Some(Taking::Ours(lock)) => Some(lock),
        _ => None,
    };
    let bounds = Bounds::centered(None, size(px(800.), px(680.)), cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        app_id: Some("missive".into()),
        ..Default::default()
    };
    cx.open_window(options, |window, cx| {
        cx.new(|cx| ComposeWindow::new(compose, asker, chooser, window, cx).holding(lock, locker))
    })
    .map_err(|e| e.to_string())
}

pub struct ComposeWindow {
    pub model: Compose,
    asker: Asker,
    chooser: Chooser,
    focus: FocusHandle,
    fields: Vec<(Field, Entity<InputState>)>,
    body: Entity<TextareaState>,
    /// Whether a request waits for the daemon's answer.
    asking: bool,
    font: Option<SharedString>,
    /// The draft's lock, held while this window is open (see [`ComposeWindow::holding`]).
    lock: Option<DraftLock>,
    locker: Option<Locker>,
}

impl ComposeWindow {
    pub fn new(
        model: Compose,
        asker: Asker,
        chooser: Chooser,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        follow_theme(is_dark(window.appearance()), cx);
        cx.observe_window_appearance(window, |_, window, cx| {
            follow_theme(is_dark(window.appearance()), cx);
            cx.notify();
        })
        .detach();
        let mut fields = Vec::new();
        for (field, value) in [
            (Field::To, &model.to),
            (Field::Cc, &model.cc),
            (Field::Bcc, &model.bcc),
            (Field::Subject, &model.subject),
        ] {
            let value = value.clone();
            let state = cx.new(|cx| {
                let mut state = InputState::new(window, cx);
                state.set_value(value, window, cx);
                state
            });
            cx.subscribe_in(&state, window, move |this, state, event, window, cx| {
                if matches!(event, InputEvent::Change) {
                    let value = state.read(cx).value().to_string();
                    this.model.edit(field, value);
                    this.changed(window, cx);
                }
            })
            .detach();
            fields.push((field, state));
        }
        let text = model.body.clone();
        let body = cx.new(|cx| {
            let mut state = TextareaState::new(window, cx);
            state.set_value(text, window, cx);
            state
        });
        cx.subscribe_in(&body, window, |this, state, event, window, cx| {
            if matches!(event, InputEvent::Change) {
                let value = state.read(cx).value().to_string();
                this.model.edit(Field::Body, value);
                this.changed(window, cx);
            }
        })
        .detach();
        // A reply is written above its quote; a new message starts with its recipient.
        let first = if model.to.is_empty() {
            fields[0].1.read(cx).focus_handle(cx)
        } else {
            body.read(cx).focus_handle(cx)
        };
        window.focus(&first, cx);
        let entity = cx.entity().downgrade();
        window.on_window_should_close(cx, move |window, cx| {
            entity
                .update(cx, |this, cx| {
                    let close = this.model.close();
                    if !close {
                        this.changed(window, cx);
                    }
                    close
                })
                .unwrap_or(true)
        });
        let font = crate::font::sans_serif(&cx.text_system().all_font_names()).map(Into::into);
        let this = ComposeWindow {
            model,
            asker,
            chooser,
            focus: cx.focus_handle(),
            fields,
            body,
            asking: false,
            font,
            lock: None,
            locker: None,
        };
        this.title(window);
        this.lock_fields(cx);
        this
    }

    /// Holds `lock`, the lock of the draft this window was opened on, until the window closes;
    /// a new message takes `locker`'s once the daemon has saved it and given it a `Message-ID`.
    pub fn holding(mut self, lock: Option<DraftLock>, locker: Locker) -> Self {
        self.lock = lock;
        self.locker = Some(locker);
        self
    }

    /// Takes the lock of the draft once it has a `Message-ID` and the window holds none. Nobody
    /// else knows the draft before its first save, so it is free; if it is not, the window goes
    /// on without: the text is safe in the draft.
    fn lock_draft(&mut self) {
        if self.lock.is_some() {
            return;
        }
        if let (Some(locker), Some(key)) = (&self.locker, self.model.draft_key())
            && let Taking::Ours(lock) = locker(&self.model.account, key)
        {
            self.lock = Some(lock);
        }
    }

    /// After the model changed: the save timer starts, the next request goes to the daemon, and
    /// the window closes once the message is sent or its draft saved on closing.
    pub fn changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(timer) = self.model.take_save_timer() {
            cx.spawn_in(window, async move |this, cx| {
                cx.background_executor().timer(timer.after).await;
                this.update_in(cx, |this, window, cx| {
                    this.model.save_timer_elapsed(timer);
                    this.changed(window, cx);
                })
            })
            .detach();
        }
        if !self.asking
            && let Some(command) = self.model.take_request()
        {
            self.asking = true;
            let asker = self.asker.clone();
            cx.spawn_in(window, async move |this, cx| {
                let answer = cx.background_spawn(async move { asker(command) }).await;
                this.update_in(cx, |this, window, cx| {
                    this.asking = false;
                    this.model.answered(answer);
                    this.lock_draft();
                    this.changed(window, cx);
                })
            })
            .detach();
        }
        if self.model.done() {
            window.remove_window();
        }
        self.lock_fields(cx);
        self.title(window);
        cx.notify();
    }

    fn title(&self, window: &mut Window) {
        let subject = self.model.subject.trim();
        window.set_window_title(if subject.is_empty() {
            "New message"
        } else {
            subject
        });
    }

    fn send(&mut self, _: &SendMessage, window: &mut Window, cx: &mut Context<Self>) {
        self.model.send();
        self.changed(window, cx);
    }

    fn close(&mut self, _: &CloseCompose, window: &mut Window, cx: &mut Context<Self>) {
        if self.model.close() {
            window.remove_window();
        } else {
            self.changed(window, cx);
        }
    }

    /// Asks the file chooser off this thread, and attaches what the user chose.
    fn choose_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let chooser = self.chooser.clone();
        cx.spawn_in(window, async move |this, cx| {
            let chosen = cx
                .background_spawn(async move { chooser().map(compose::load) })
                .await;
            this.update_in(cx, |this, window, cx| {
                match chosen {
                    Ok(loaded) => this.model.attach(loaded),
                    Err(e) => this.model.notice = Some(format!("No file chooser: {e}.")),
                }
                this.changed(window, cx);
            })
        })
        .detach();
    }

    /// Reads the dropped files off this thread, and attaches them.
    fn attach_dropped(&mut self, paths: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| {
            let loaded = cx
                .background_spawn(async move { compose::load(paths) })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.model.attach(loaded);
                this.changed(window, cx);
            })
        })
        .detach();
    }

    /// What the widget of `field` shows.
    pub fn shown(&self, field: Field, cx: &App) -> String {
        match field {
            Field::Body => self.body.read(cx).value().to_string(),
            _ => self.input(field).read(cx).value().to_string(),
        }
    }

    /// The fields take no text while the message is being sent: what was typed then would show
    /// in the window and be missing from the model, and be lost if the send failed.
    fn lock_fields(&self, cx: &mut Context<Self>) {
        let locked = self.model.state == State::Sending;
        for (_, state) in &self.fields {
            state.update(cx, |state, cx| state.set_readonly(locked, cx));
        }
        self.body
            .update(cx, |state, cx| state.set_readonly(locked, cx));
    }

    fn input(&self, field: Field) -> Entity<InputState> {
        self.fields
            .iter()
            .find(|(f, _)| *f == field)
            .map(|(_, state)| state.clone())
            .expect("every field has an input")
    }
}

/// The text fields' colours, which gpui-base takes from its theme, follow the window's.
fn follow_theme(dark: bool, cx: &mut App) {
    let palette = Palette::new(dark);
    let mut colors = if dark {
        ColorTokens::dark()
    } else {
        ColorTokens::light()
    };
    colors.foreground = palette.text.into();
    colors.muted_foreground = palette.muted.into();
    colors.selection = palette.selection.into();
    Theme::global_mut(cx).tokens.colors = colors;
}

impl Render for ComposeWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::of(window);
        let sending = self.model.state == State::Sending;
        let status = if sending {
            "Sending…"
        } else if self.model.draft.is_some() && !self.model.needs_save() {
            "Draft saved"
        } else {
            ""
        };
        div()
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::send))
            .on_action(cx.listener(Self::close))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                this.attach_dropped(paths.paths().to_vec(), window, cx);
            }))
            .size_full()
            .flex()
            .flex_col()
            .bg(palette.background)
            .text_color(palette.text)
            .when_some(self.font.clone(), |root, font| root.font_family(font))
            .text_size(px(14.))
            .children(self.model.notice.clone().map(|text| {
                div()
                    .id("compose-notice")
                    .debug_selector(|| "compose-notice".to_owned())
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
                    .px_4()
                    .py_2()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .border_b_1()
                    .border_color(palette.line)
                    .child(self.from(palette, cx))
                    .child(line("To", self.input(Field::To), palette))
                    .child(line("Cc", self.input(Field::Cc), palette))
                    .when(self.model.show_bcc, |header| {
                        header.child(line("Bcc", self.input(Field::Bcc), palette))
                    })
                    .child(line("Subject", self.input(Field::Subject), palette))
                    .child(self.attachments(palette, cx)),
            )
            .child(
                div()
                    .id("compose-body")
                    .flex_1()
                    .min_h_0()
                    .px_4()
                    .py_2()
                    .overflow_y_scroll()
                    .child(Textarea::new(&self.body)),
            )
            .child(
                div()
                    .px_4()
                    .py_2()
                    .flex()
                    .items_center()
                    .gap_3()
                    .border_t_1()
                    .border_color(palette.line)
                    .child(
                        button("send", "Send", Some("Ctrl+Enter"), palette)
                            .when(!sending, |b| b.bg(palette.selected))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.model.send();
                                this.changed(window, cx);
                            })),
                    )
                    .child(div().text_color(palette.muted).child(status)),
            )
    }
}

impl ComposeWindow {
    /// The account written from: a choice while the draft is not saved and there are several.
    fn from(&self, palette: Palette, cx: &mut Context<Self>) -> gpui_kit::Div {
        let mut row = div()
            .flex()
            .items_center()
            .gap_2()
            .child(label("From", palette));
        if !self.model.can_pick_account() {
            return row.child(self.model.account.clone());
        }
        for account in &self.model.accounts {
            let chosen = *account == self.model.account;
            let id = account.clone();
            row = row.child(
                div()
                    .id(SharedString::from(format!("from-{account}")))
                    .px_2()
                    .rounded(px(4.))
                    .border_1()
                    .border_color(palette.line)
                    .when(chosen, |b| b.bg(palette.selected))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.model.pick_account(&id);
                        this.changed(window, cx);
                    }))
                    .child(account.clone()),
            );
        }
        row
    }

    /// The files attached, each with its button to take it off, and the buttons to attach one
    /// and to show Bcc.
    fn attachments(&self, palette: Palette, cx: &mut Context<Self>) -> gpui_kit::Div {
        let mut row = div()
            .pt_1()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .child(
                button("attach", "Attach…", None, palette)
                    .on_click(cx.listener(|this, _, window, cx| this.choose_files(window, cx))),
            )
            .when(!self.model.show_bcc, |row| {
                row.child(
                    button("show-bcc", "Bcc", None, palette).on_click(cx.listener(
                        |this, _, window, cx| {
                            this.model.ask_bcc();
                            this.changed(window, cx);
                        },
                    )),
                )
            });
        for (at, attachment) in self.model.attachments.iter().enumerate() {
            row = row.child(
                div()
                    .px_2()
                    .flex()
                    .gap_2()
                    .rounded(px(4.))
                    .bg(palette.pane)
                    .child(format!(
                        "{} ({})",
                        attachment.name,
                        size_of(attachment.data.len())
                    ))
                    .child(
                        div()
                            .id(SharedString::from(format!("remove-{at}")))
                            .debug_selector(move || format!("remove-{at}"))
                            .text_color(palette.muted)
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.model.remove_attachment(at);
                                this.changed(window, cx);
                            }))
                            .child("Remove"),
                    ),
            );
        }
        row
    }
}

fn label(text: &'static str, palette: Palette) -> gpui_kit::Div {
    div()
        .w(px(64.))
        .flex_none()
        .text_color(palette.muted)
        .font_weight(FontWeight::NORMAL)
        .child(text)
}

/// A header line: its label, and its field.
fn line(text: &'static str, input: Entity<InputState>, palette: Palette) -> gpui_kit::Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .child(label(text, palette))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .px_2()
                .py_1()
                .border_b_1()
                .border_color(palette.line)
                .child(Input::new(&input)),
        )
}

/// A size for people: bytes, kB or MB.
fn size_of(bytes: usize) -> String {
    match bytes {
        0..1_000 => format!("{bytes} B"),
        1_000..1_000_000 => format!("{} kB", bytes / 1_000),
        _ => format!("{:.1} MB", bytes as f64 / 1_000_000.),
    }
}
