//! `missive-window`: the reading window, or one compose window on a draft (see `launch.rs`). A
//! program of its own, so that the command line and the daemon never carry GPUI.

use std::sync::Arc;

use futures_util::StreamExt;
use gpui_kit::prelude::*;
use gpui_kit::{App, Bounds, WindowBounds, WindowOptions, px, size};
use missive_core::protocol::socket_path;
use missive_core::{chooser, draft_lock, opener};
use missive_window::compose_view::{self, Chooser, Locker};
use missive_window::launch::{self, Launch};
use missive_window::model::{Model, Sources};
use missive_window::view::{self, Asker, MailWindow, Opening};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let launch = launch::parse(&args).unwrap_or_else(|e| {
        eprintln!("missive-window: {e}");
        std::process::exit(2);
    });
    let sources = match Sources::from_env() {
        Ok(sources) => sources,
        Err(e) => {
            eprintln!("missive-window: {e}");
            std::process::exit(1);
        }
    };
    // Without a runtime folder there is no daemon to ask: each action says so in the window.
    let asker: Asker =
        missive_window::ask::from_env().unwrap_or_else(|e| Arc::new(move |_| Err(e.clone())));
    let chooser: Chooser = Arc::new(choose);
    let locker: Locker = Arc::new(draft_lock::take);
    gpui_kit::application().run(move |cx: &mut App| {
        gpui_kit::base::init(cx);
        view::bind_keys(cx);
        // The program ends with its last window: the reader, or the compose windows it opened.
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        match launch {
            Launch::Reader => reader(sources, asker, chooser, locker, cx),
            Launch::Compose { account, draft } => {
                let compose = launch::compose(&sources, account, draft);
                if let Err(e) = compose_view::open(compose, asker, chooser, locker, cx) {
                    eprintln!("missive-window: {e}");
                    std::process::exit(1);
                }
            }
        }
    });
}

fn reader(sources: Sources, asker: Asker, chooser: Chooser, locker: Locker, cx: &mut App) {
    let bounds = Bounds::centered(None, size(px(1280.), px(800.)), cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        app_id: Some("missive".into()),
        ..Default::default()
    };
    let window = cx
        .open_window(options, |window, cx| {
            cx.new(|cx| {
                MailWindow::new(Model::new(sources), Arc::new(open), asker, window, cx)
                    .with_chooser(chooser)
                    .with_locker(locker)
            })
        })
        .expect("a window");

    // The daemon's events, read on a thread of their own and applied here.
    let (events, mut received) = futures_channel::mpsc::unbounded();
    if let Ok(socket) = socket_path() {
        std::thread::spawn(move || {
            missive_window::watch::follow(socket, |event| events.unbounded_send(event).is_ok())
        });
    }
    cx.spawn(async move |cx| {
        while let Some(event) = received.next().await {
            let applied = window.update(cx, |this, _, cx| {
                this.model.apply(&event);
                this.changed(cx);
            });
            if applied.is_err() {
                break;
            }
        }
    })
    .detach();
}

/// Runs `future` on a runtime of this thread: the portal is spoken through zbus on tokio, as in
/// the rest of Missive.
fn block_on<T>(future: impl Future<Output = Result<T, String>>) -> Result<T, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?
        .block_on(future)
}

/// The system's file chooser, through the portal. Called off the window's thread.
fn choose() -> Result<Vec<std::path::PathBuf>, String> {
    block_on(chooser::choose("Attach files"))
}

/// Opens with the system's default handler, on a thread of its own: the portal may take
/// seconds to answer, and the window does not wait for it.
fn open(opening: Opening) {
    std::thread::spawn(move || {
        let opened = block_on(async {
            match opening {
                Opening::Link(uri) => opener::open_uri(&uri).await,
                Opening::Original(raw) => opener::open_original(&raw).await.map(drop),
            }
        });
        if let Err(e) = opened {
            eprintln!("missive-window: {e}");
        }
    });
}
