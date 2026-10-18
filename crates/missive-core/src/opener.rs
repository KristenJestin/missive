//! Opening with the system's default handler: the desktop portal, else `xdg-open`, with the
//! environment Missive received. Nothing names a browser. `missive open` and the window's "View
//! original" open a message's cleaned page ([`crate::original`]) this way, and the window a link
//! the user clicked.

use std::collections::HashMap;
use std::io::Write;
use std::os::fd::AsFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use futures_util::StreamExt;
use percent_encoding::percent_decode_str;
use serde::Serialize;
use zbus::message::Type;
use zbus::zvariant::{DynamicType, Fd, OwnedValue, Value};
use zbus::{Connection, MatchRule, MessageStream};

use crate::original;
use crate::protocol::runtime_dir;

/// How long a page is kept: the browser has long read it by then.
const KEPT: Duration = Duration::from_secs(60 * 60);

/// How long the portal is given to take the file, and then to say whether it could open it. A
/// portal that has not said is taken to be asking the user.
const PORTAL_CALL: Duration = Duration::from_secs(5);
const PORTAL_ANSWER: Duration = Duration::from_secs(2);

/// Writes the cleaned page of a raw message to a private file and opens it. The page's path.
pub async fn open_original(raw: &[u8]) -> Result<PathBuf, String> {
    let missive = runtime_dir().map_err(|_| {
        "XDG_RUNTIME_DIR is not set, so there is no private folder for the page".to_owned()
    })?;
    let folder = private_folder(&missive)?;
    remove_old(&folder);

    // Created anew (O_EXCL) under a random name, readable by the user only.
    let mut file = tempfile::Builder::new()
        .prefix("")
        .suffix(".html")
        .permissions(std::fs::Permissions::from_mode(0o600))
        .tempfile_in(&folder)
        .map_err(|e| format!("cannot create a page in {}: {e}", folder.display()))?;
    file.write_all(original::page(raw).as_bytes())
        .map_err(|e| e.to_string())?;
    let (_, path) = file.keep().map_err(|e| e.to_string())?;

    let fall_back = settle(async {
        let file = std::fs::File::open(&path)?;
        let options = HashMap::from([("handle_token", Value::from(token()))]);
        portal(
            &Connection::session().await?,
            "OpenFile",
            &("", Fd::from(file.as_fd()), options),
        )
        .await
    })
    .await;
    if fall_back {
        xdg_open(path.as_os_str()).map_err(|e| {
            format!(
                "no desktop portal, and cannot run xdg-open ({e}) to open {}",
                path.display()
            )
        })?;
    }
    Ok(path)
}

/// Opens a link with the system's default handler: only `http:`, `https:` and `mailto:` ones,
/// those a message's reduced form keeps.
pub async fn open_uri(uri: &str) -> Result<(), String> {
    if !followable(uri) {
        return Err("only http, https and mailto links are opened".to_owned());
    }
    let uri = &mail_link(uri);
    let fall_back =
        settle(async { open_uri_with_portal(&Connection::session().await?, uri).await }).await;
    if fall_back {
        // Not the link itself: what a message points at stays out of messages and logs.
        xdg_open(uri.as_ref())
            .map_err(|e| format!("no desktop portal, and cannot run xdg-open ({e})"))?;
    }
    Ok(())
}

/// Hands a link to `org.freedesktop.portal.OpenURI.OpenURI` on `connection`. `Ok(true)` when the
/// portal took it and did not say it failed.
pub async fn open_uri_with_portal(connection: &Connection, uri: &str) -> zbus::Result<bool> {
    let options = HashMap::from([("handle_token", Value::from(token()))]);
    portal(connection, "OpenURI", &("", uri, options)).await
}

/// The fields a `mailto:` link may fill in the user's mail client. The others are left out: a
/// message must not be able to attach a file (`attach`, `attachment`) or add a hidden
/// recipient (`bcc`) behind the user's back.
const MAIL_FIELDS: &[&str] = &["subject", "body", "cc"];

/// A `mailto:` link with only its address and [`MAIL_FIELDS`]; any other link as it is.
pub fn mail_link(uri: &str) -> String {
    let Some(scheme) = uri.get(..7).filter(|s| s.eq_ignore_ascii_case("mailto:")) else {
        return uri.to_owned();
    };
    let rest = &uri[scheme.len()..];
    let rest = rest.split_once('#').map_or(rest, |(rest, _)| rest);
    let (address, query) = rest.split_once('?').unwrap_or((rest, ""));
    let kept: Vec<&str> = query
        .split('&')
        .filter(|field| {
            let name = field.split_once('=').map_or(*field, |(name, _)| name);
            let name = percent_decode_str(name).decode_utf8_lossy();
            MAIL_FIELDS
                .iter()
                .any(|kept| name.eq_ignore_ascii_case(kept))
        })
        .collect();
    let mut link = format!("{scheme}{address}");
    if !kept.is_empty() {
        link.push('?');
        link.push_str(&kept.join("&"));
    }
    link
}

/// Whether a link is one Missive opens on a click.
pub fn followable(uri: &str) -> bool {
    let lower = uri.trim().to_ascii_lowercase();
    lower.starts_with("https:") || lower.starts_with("http:") || lower.starts_with("mailto:")
}

/// Whether the portal's answer leaves the opening to `xdg-open`. Only a portal that is absent,
/// errors or says it failed does. One that is slow is asking the user (an application chooser):
/// opening a second time beside it would be worse than waiting.
async fn settle(call: impl Future<Output = zbus::Result<bool>>) -> bool {
    match tokio::time::timeout(PORTAL_CALL, call).await {
        Ok(Ok(opened)) => !opened,
        Ok(Err(_)) => true,
        Err(_) => false,
    }
}

/// `original` in Missive's runtime folder, created if need be, and both checked: a folder
/// that is not Missive's own (another user's, writable by others, or a symlink that sends the
/// page somewhere else) would let someone else read or swap a page that is about to be opened in
/// the user's browser.
fn private_folder(missive: &Path) -> Result<PathBuf, String> {
    let original = missive.join("original");
    for folder in [missive, original.as_path()] {
        match std::fs::DirBuilder::new().mode(0o700).create(folder) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(format!("cannot create {}: {e}", folder.display())),
        }
        // Never through a symlink.
        let meta = std::fs::symlink_metadata(folder)
            .map_err(|e| format!("cannot inspect {}: {e}", folder.display()))?;
        if !meta.is_dir()
            || meta.uid() != rustix::process::geteuid().as_raw()
            || meta.mode() & 0o022 != 0
        {
            return Err(format!(
                "{} is not private to you (it must be a folder of yours that others cannot \
                 write in): refusing to put a page there",
                folder.display()
            ));
        }
    }
    Ok(original)
}

/// Removes the pages older than [`KEPT`]: there is no daemon in the way, each opening cleans up
/// after the previous ones.
fn remove_old(folder: &Path) {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return;
    };
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|modified| SystemTime::now().duration_since(modified).ok())
            .is_some_and(|age| age > KEPT);
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The token a request object is named after.
fn token() -> String {
    format!("missive{}", std::process::id())
}

/// Calls `method` of `org.freedesktop.portal.OpenURI`, whose options carry [`token`], and waits
/// for its answer. `Ok(true)` when the portal took the call and did not say it failed.
async fn portal<B>(connection: &Connection, method: &str, body: &B) -> zbus::Result<bool>
where
    B: Serialize + DynamicType,
{
    let sender = connection
        .unique_name()
        .map(|name| name.trim_start_matches(':').replace('.', "_"))
        .unwrap_or_default();
    // Listened to before the call, as the portal documents, so that no answer is missed.
    let request = format!(
        "/org/freedesktop/portal/desktop/request/{sender}/{}",
        token()
    );
    let rule = MatchRule::builder()
        .msg_type(Type::Signal)
        .interface("org.freedesktop.portal.Request")?
        .member("Response")?
        .path(request.as_str())?
        .build();
    let mut answers = MessageStream::for_match_rule(rule, connection, Some(1)).await?;
    connection
        .call_method(
            Some("org.freedesktop.portal.Desktop"),
            "/org/freedesktop/portal/desktop",
            Some("org.freedesktop.portal.OpenURI"),
            method,
            body,
        )
        .await?;
    let Ok(Some(answer)) = tokio::time::timeout(PORTAL_ANSWER, answers.next()).await else {
        return Ok(true);
    };
    // 0: opened; 1: the user cancelled, which is their answer; 2: the portal could not.
    let (code, _): (u32, HashMap<String, OwnedValue>) = answer?.body().deserialize()?;
    Ok(code != 2)
}

/// Starts `xdg-open` and returns at once: the browser may live long. It gets exactly the
/// environment Missive received, nothing added.
fn xdg_open(target: &std::ffi::OsStr) -> std::io::Result<()> {
    Command::new("xdg-open")
        .arg(target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(drop)
}
