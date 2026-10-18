//! `missive-window`: the reading window. A program of its own, so that the command line and the
//! daemon never carry GPUI.

use std::sync::Arc;

use futures_util::StreamExt;
use gpui_kit::prelude::*;
use gpui_kit::{App, Bounds, WindowBounds, WindowOptions, px, size};
use missive_core::opener;
use missive_core::protocol::socket_path;
use missive_window::model::{Model, Sources};
use missive_window::view::{self, Asker, MailWindow, Opening};

fn main() {
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
    gpui_kit::application().run(move |cx: &mut App| {
        gpui_kit::base::init(cx);
        view::bind_keys(cx);
        let bounds = Bounds::centered(None, size(px(1280.), px(800.)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            app_id: Some("missive".into()),
            ..Default::default()
        };
        let window = cx
            .open_window(options, |window, cx| {
                cx.new(|cx| MailWindow::new(Model::new(sources), Arc::new(open), asker, window, cx))
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
    });
}

/// Opens with the system's default handler, on a thread of its own: the portal may take
/// seconds to answer, and the window does not wait for it.
fn open(opening: Opening) {
    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(e) => return eprintln!("missive-window: {e}"),
        };
        let opened = runtime.block_on(async {
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
