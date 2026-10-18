//! A fake desktop portal on a private session bus: see [`FakePortal`].

use std::collections::HashMap;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use zbus::zvariant::{OwnedFd, OwnedObjectPath, OwnedValue};
use zbus::{Connection, fdo, interface};

use super::bus::Daemon;

/// A private `dbus-daemon` with a fake `org.freedesktop.portal.Desktop` on it, serving
/// `org.freedesktop.portal.OpenURI.OpenFile` only: it opens nothing, it records each call
/// ([`FakePortal::calls`]) and answers on its request object as the test chose
/// ([`PortalAnswer`]).
///
/// Run the program under test with `DBUS_SESSION_BUS_ADDRESS` set to [`FakePortal::address`].
/// The bus and the fake stop when it is dropped; it needs a running tokio runtime.
pub struct FakePortal {
    // Dropped first: the connection goes before the bus it is on.
    _connection: Connection,
    _daemon: Daemon,
    address: String,
    calls: Arc<Mutex<Vec<PortalCall>>>,
}

/// What the portal answers to `OpenFile`, in the `Response` signal of its request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortalAnswer {
    /// `0`: the file was handed to an application.
    Opened,
    /// `2`: the portal could not open it (no application for it, say).
    Failed,
    /// No `Response` at all, as a portal still waiting for the user.
    Silent,
    /// `0`, but the `Response` comes only after this long, as a user choosing an application.
    Late(Duration),
    /// `0`, but the method itself returns only after this long, as a portal busy before it
    /// hands out the request.
    Slow(Duration),
}

/// One `OpenFile` call, as the portal received it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortalCall {
    pub parent_window: String,
    /// The file the descriptor names.
    pub path: PathBuf,
    /// What reading the descriptor gives.
    pub content: Vec<u8>,
    /// Whether the descriptor was open for reading only.
    pub read_only: bool,
    pub handle_token: Option<String>,
}

impl FakePortal {
    pub async fn start(answer: PortalAnswer) -> Self {
        let (daemon, address) = Daemon::start();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let connection = zbus::connection::Builder::address(address.as_str())
            .expect("a bus address")
            .name("org.freedesktop.portal.Desktop")
            .expect("a bus name")
            .serve_at(
                "/org/freedesktop/portal/desktop",
                OpenUri {
                    answer,
                    calls: calls.clone(),
                },
            )
            .expect("the portal object")
            .build()
            .await
            .expect("a connection to the private bus");
        FakePortal {
            _connection: connection,
            _daemon: daemon,
            address,
            calls,
        }
    }

    /// The address of the private bus, for `DBUS_SESSION_BUS_ADDRESS`.
    pub fn address(&self) -> &str {
        &self.address
    }

    /// Every `OpenFile` call, in order.
    pub fn calls(&self) -> Vec<PortalCall> {
        lock(&self.calls).clone()
    }
}

fn lock(calls: &Mutex<Vec<PortalCall>>) -> MutexGuard<'_, Vec<PortalCall>> {
    calls
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct OpenUri {
    answer: PortalAnswer,
    calls: Arc<Mutex<Vec<PortalCall>>>,
}

#[interface(name = "org.freedesktop.portal.OpenURI")]
impl OpenUri {
    async fn open_file(
        &self,
        parent_window: String,
        fd: OwnedFd,
        options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> fdo::Result<OwnedObjectPath> {
        let failed = |e: std::io::Error| fdo::Error::Failed(e.to_string());
        let raw = fd.as_raw_fd();
        let path = std::fs::read_link(format!("/proc/self/fd/{raw}")).map_err(failed)?;
        // `flags:` is in octal; the two low bits are the access mode, 0 for reading only.
        let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{raw}")).map_err(failed)?;
        let read_only = info
            .lines()
            .find_map(|line| line.strip_prefix("flags:"))
            .and_then(|flags| u32::from_str_radix(flags.trim(), 8).ok())
            .is_some_and(|flags| flags & 0o3 == 0);
        let mut content = Vec::new();
        std::fs::File::from(std::os::fd::OwnedFd::from(fd))
            .read_to_end(&mut content)
            .map_err(failed)?;
        let handle_token = options
            .get("handle_token")
            .and_then(|token| String::try_from(token.try_clone().ok()?).ok());
        lock(&self.calls).push(PortalCall {
            parent_window,
            path,
            content,
            read_only,
            handle_token: handle_token.clone(),
        });

        // The request object is named after the caller and its token, so that the caller can
        // listen to it before calling.
        let sender = header
            .sender()
            .map(|name| name.trim_start_matches(':').replace('.', "_"))
            .unwrap_or_default();
        let request = OwnedObjectPath::try_from(format!(
            "/org/freedesktop/portal/desktop/request/{sender}/{}",
            handle_token.as_deref().unwrap_or("t")
        ))
        .map_err(|e| fdo::Error::InvalidArgs(e.to_string()))?;
        let (code, delay) = match self.answer {
            PortalAnswer::Opened => (0u32, Duration::from_millis(20)),
            PortalAnswer::Failed => (2, Duration::from_millis(20)),
            PortalAnswer::Silent => return Ok(request),
            PortalAnswer::Late(after) => (0, after),
            PortalAnswer::Slow(before) => {
                tokio::time::sleep(before).await;
                (0, Duration::from_millis(20))
            }
        };
        let connection = connection.clone();
        let destination = header.sender().map(|name| name.to_owned());
        let path = request.clone();
        // After the method returned, as a real portal answers.
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let results: HashMap<String, OwnedValue> = HashMap::new();
            let _ = connection
                .emit_signal(
                    destination,
                    path,
                    "org.freedesktop.portal.Request",
                    "Response",
                    &(code, results),
                )
                .await;
        });
        Ok(request)
    }
}
