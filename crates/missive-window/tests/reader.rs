//! The reader drawn in GPUI's test window (no compositor, no GPU): nothing a message names is
//! ever fetched, a link opens only on the user's click, and "View original" hands the message to
//! the system's handler.

mod common;

use std::collections::HashMap;
use std::future::Future;
use std::io::ErrorKind;
use std::net::TcpListener;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use common::{Mail, mailbox};
use gpui_kit::http_client::{self, AsyncBody, HttpClient, Request, Response, Url, http};
use gpui_kit::prelude::*;
use gpui_kit::{
    Context, Modifiers, MouseButton, TestAppContext, VisualTestContext, Window, div, point, px,
};
use missive_render::reduce::{Theme, reduce};
use missive_window::images::MessageImages;
use missive_window::view::{self, MailWindow, Opener, Opening, Palette};

/// An HTTP client that really connects, so that a fetch would show on the listener. GPUI's own
/// has none (it fails every request): this one stands for any client a later version could
/// bring.
struct Connecting;

impl HttpClient for Connecting {
    fn user_agent(&self) -> Option<&http::HeaderValue> {
        None
    }

    fn proxy(&self) -> Option<&Url> {
        None
    }

    fn send(
        &self,
        request: Request<AsyncBody>,
    ) -> Pin<Box<dyn Future<Output = http_client::Result<Response<AsyncBody>>> + Send>> {
        let uri = request.uri().clone();
        Box::pin(async move {
            let host = uri.host().unwrap_or("127.0.0.1").to_owned();
            let _ = std::net::TcpStream::connect((host, uri.port_u16().unwrap_or(80)));
            Err(http_client::anyhow!("no answer in a test"))
        })
    }
}

/// A local listener, and whether anything connected to it.
struct Listener(TcpListener);

impl Listener {
    fn start() -> Listener {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        Listener(listener)
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}/{path}", self.0.local_addr().unwrap())
    }

    fn contacted(&self) -> bool {
        match self.0.accept() {
            Ok(_) => true,
            Err(e) if e.kind() == ErrorKind::WouldBlock => false,
            Err(e) => panic!("{e}"),
        }
    }
}

/// What the opener was handed.
fn recorder() -> (Opener, Arc<Mutex<Vec<Opening>>>) {
    let opened = Arc::new(Mutex::new(Vec::new()));
    let seen = opened.clone();
    (
        Arc::new(move |opening| seen.lock().unwrap().push(opening)),
        opened,
    )
}

/// One message body at the top left of a window.
struct Body {
    html: String,
    images: Arc<MessageImages>,
    opener: Opener,
}

impl Render for Body {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().w(px(480.)).child(view::body(
            "test",
            &self.html,
            self.images.clone(),
            self.opener.clone(),
            Palette::new(false),
        ))
    }
}

fn show<'a>(cx: &'a mut TestAppContext, html: &str, opener: Opener) -> &'a mut VisualTestContext {
    cx.update(|cx| {
        gpui_kit::base::init(cx);
        cx.set_http_client(Arc::new(Connecting));
    });
    let html = html.to_owned();
    let (_, cx) = cx.add_window_view(move |_, _| Body {
        html,
        images: Arc::new(MessageImages::new(&HashMap::new())),
        opener,
    });
    cx.run_until_parked();
    cx
}

/// Every way a body can name an image on the listener: its own `img`, and the forms a reduced
/// body could still carry.
fn remote_images(listener: &Listener) -> String {
    format!(
        "<p>Before</p><img src=\"{}\" alt=\"Logo\"><p><img src=\"{}\" width=\"1\" height=\"1\"></p>\
         <picture><source srcset=\"{}\"><img src=\"{}\"></picture><p>After</p>",
        listener.url("logo.png"),
        listener.url("pixel.gif"),
        listener.url("source.png"),
        listener.url("picture.png"),
    )
}

#[gpui_kit::test]
fn a_remote_image_in_a_message_never_reaches_the_network(cx: &mut TestAppContext) {
    let listener = Listener::start();
    let (opener, opened) = recorder();
    let cx = show(cx, &remote_images(&listener), opener);
    cx.run_until_parked();
    assert!(!listener.contacted(), "the listener was contacted");
    assert!(opened.lock().unwrap().is_empty());
}

#[gpui_kit::test]
fn a_remote_image_is_never_fetched_through_the_reduced_form_either(cx: &mut TestAppContext) {
    let listener = Listener::start();
    let html = reduce(&remote_images(&listener), &Theme::Light, 800);
    let (opener, _) = recorder();
    let cx = show(cx, &html, opener);
    cx.run_until_parked();
    assert!(!listener.contacted(), "the listener was contacted");
}

/// The proof that the two tests above can fail: the same text view without the message's image
/// source does reach the listener.
#[gpui_kit::test]
fn without_the_message_image_source_the_listener_would_be_contacted(cx: &mut TestAppContext) {
    struct Unguarded(String);
    impl Render for Unguarded {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .w(px(480.))
                .child(gpui_kit::base::TextView::html("unguarded", self.0.clone()))
        }
    }
    let listener = Listener::start();
    cx.update(|cx| {
        gpui_kit::base::init(cx);
        cx.set_http_client(Arc::new(Connecting));
    });
    let html = remote_images(&listener);
    let (_, cx) = cx.add_window_view(move |_, _| Unguarded(html));
    cx.run_until_parked();
    assert!(listener.contacted(), "the harness would not see a fetch");
}

const LINK: &str = "<a href=\"https://www.example.org/news\">The news of the week</a>";

#[gpui_kit::test]
fn a_link_opens_with_the_system_handler_on_a_click(cx: &mut TestAppContext) {
    let (opener, opened) = recorder();
    let cx = show(cx, LINK, opener);
    cx.simulate_click(point(px(10.), px(10.)), Modifiers::default());
    assert_eq!(
        *opened.lock().unwrap(),
        [Opening::Link("https://www.example.org/news".to_owned())]
    );
    assert_eq!(cx.opened_url(), None, "never through GPUI's own opener");
}

#[gpui_kit::test]
fn a_link_does_not_open_on_another_button(cx: &mut TestAppContext) {
    let (opener, opened) = recorder();
    let cx = show(cx, LINK, opener);
    for button in [MouseButton::Right, MouseButton::Middle] {
        cx.simulate_mouse_down(point(px(10.), px(10.)), button, Modifiers::default());
        cx.simulate_mouse_up(point(px(10.), px(10.)), button, Modifiers::default());
    }
    assert!(opened.lock().unwrap().is_empty());
    assert_eq!(cx.opened_url(), None);
}

#[gpui_kit::test]
fn a_link_that_is_not_web_or_mail_is_never_opened(cx: &mut TestAppContext) {
    let (opener, opened) = recorder();
    let cx = show(
        cx,
        "<a href=\"file:///etc/hostname\">A file of the machine</a>",
        opener,
    );
    cx.simulate_click(point(px(10.), px(10.)), Modifiers::default());
    assert!(opened.lock().unwrap().is_empty());
    assert_eq!(cx.opened_url(), None);
}

#[gpui_kit::test]
fn nothing_opens_without_a_click(cx: &mut TestAppContext) {
    let (opener, opened) = recorder();
    let cx = show(
        cx,
        &format!("{LINK}<p><a href=\"https://www.example.org/b\">Another</a></p>"),
        opener,
    );
    cx.run_until_parked();
    assert!(opened.lock().unwrap().is_empty());
    assert_eq!(cx.opened_url(), None);
}

/// The whole window, on a copy filled through the fake server.
fn window<'a>(
    cx: &'a mut TestAppContext,
    mail: &Mail,
    opener: Opener,
) -> &'a mut VisualTestContext {
    cx.update(|cx| {
        gpui_kit::base::init(cx);
        view::bind_keys(cx);
        cx.set_http_client(Arc::new(Connecting));
    });
    let model = mail.model();
    let (_, cx) = cx.add_window_view(move |window, cx| MailWindow::new(model, opener, window, cx));
    cx.run_until_parked();
    cx
}

#[gpui_kit::test]
fn the_keyboard_moves_opens_and_closes_and_view_original_opens_the_message(
    cx: &mut TestAppContext,
) {
    // The fake server lives on a tokio runtime of its own, beside GPUI's test executor.
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let mail = runtime.block_on(Mail::new(mailbox()));
    let (opener, opened) = recorder();
    let cx = window(cx, &mail, opener);
    cx.simulate_keystrokes("down enter");
    let subject = |cx: &mut VisualTestContext| {
        cx.update(|window, cx| {
            let root = window.root::<MailWindow>().flatten().unwrap();
            match &root.read(cx).model.reader {
                missive_window::model::Reader::Open(opened) => opened.subject.clone(),
                _ => None,
            }
        })
    };
    assert_eq!(subject(cx).as_deref(), Some("Plan 2"));

    let bounds = cx
        .debug_bounds("view-original")
        .expect("a View original button");
    cx.simulate_click(bounds.center(), Modifiers::default());
    let opened = opened.lock().unwrap().clone();
    assert_eq!(opened.len(), 1);
    let Opening::Original(raw) = &opened[0] else {
        panic!("not the original");
    };
    assert!(String::from_utf8_lossy(raw).contains("Subject: Plan 2"));

    cx.simulate_keystrokes("escape");
    assert_eq!(subject(cx), None);
}
