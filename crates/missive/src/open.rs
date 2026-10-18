//! `missive open`: a message's cleaned page ([`missive_core::original`]), written to a private
//! file and opened with the system's default handler: the desktop portal, else `xdg-open`.
//! Nothing names a browser.

use std::collections::HashMap;
use std::io::Write;
use std::os::fd::AsFd;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use clap::Args;
use futures_util::StreamExt;
use missive_core::original;
use missive_core::protocol::runtime_dir;
use zbus::message::Type;
use zbus::zvariant::{Fd, OwnedValue, Value};
use zbus::{Connection, MatchRule, MessageStream};

use crate::read::{self, ShowError};

#[derive(Args)]
pub struct OpenArgs {
    /// The message's identifier, as `search` and `list` print it.
    #[arg(value_name = "MESSAGE ID")]
    id: String,
    /// The original message, cleaned, in the default browser. It is the only view until the
    /// window exists; scripts that want the browser pass it already.
    #[arg(long)]
    original: bool,
}

/// How long a page is kept: the browser has long read it by then.
const KEPT: Duration = Duration::from_secs(60 * 60);

/// How long the portal is given to take the file, and then to say whether it could open it. A
/// portal that has not said is taken to be asking the user.
const PORTAL_CALL: Duration = Duration::from_secs(5);
const PORTAL_ANSWER: Duration = Duration::from_secs(2);

pub async fn run(args: OpenArgs) -> Result<(), ShowError> {
    // Without the window, the original is the only view: `--original` changes nothing yet.
    let OpenArgs { id, original: _ } = args;
    let stored = read::message(&id)?;
    let folder = runtime_dir()
        .map_err(|_| {
            ShowError::Failed(
                "XDG_RUNTIME_DIR is not set, so there is no private folder for the page".to_owned(),
            )
        })?
        .join("original");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&folder)
        .map_err(|e| format!("cannot create {}: {e}", folder.display()))?;
    std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("cannot make {} private: {e}", folder.display()))?;
    remove_old(&folder);

    // Created anew (O_EXCL) under a random name, readable by the user only.
    let mut file = tempfile::Builder::new()
        .prefix("")
        .suffix(".html")
        .permissions(std::fs::Permissions::from_mode(0o600))
        .tempfile_in(&folder)
        .map_err(|e| format!("cannot create a page in {}: {e}", folder.display()))?;
    file.write_all(original::page(&stored.raw).as_bytes())?;
    let (_, path) = file.keep()?;

    let opened = matches!(
        tokio::time::timeout(PORTAL_CALL, portal(&path)).await,
        Ok(Ok(true))
    );
    if !opened {
        xdg_open(&path)?;
    }
    println!("{}", path.display());
    Ok(())
}

/// Removes the pages older than [`KEPT`]: there is no daemon in the way, each `open` cleans up
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

/// Hands the page to `org.freedesktop.portal.OpenURI.OpenFile`, as a descriptor open for
/// reading: a sandboxed browser can read it, where it could not a path. `Ok(true)` when the
/// portal took it and did not say it failed.
async fn portal(path: &Path) -> zbus::Result<bool> {
    let connection = Connection::session().await?;
    let token = format!("missive{}", std::process::id());
    let sender = connection
        .unique_name()
        .map(|name| name.trim_start_matches(':').replace('.', "_"))
        .unwrap_or_default();
    // Listened to before the call, as the portal documents, so that no answer is missed.
    let request = format!("/org/freedesktop/portal/desktop/request/{sender}/{token}");
    let rule = MatchRule::builder()
        .msg_type(Type::Signal)
        .interface("org.freedesktop.portal.Request")?
        .member("Response")?
        .path(request.as_str())?
        .build();
    let mut answers = MessageStream::for_match_rule(rule, &connection, Some(1)).await?;
    let file = std::fs::File::open(path)?;
    let options = HashMap::from([("handle_token", Value::from(token.as_str()))]);
    connection
        .call_method(
            Some("org.freedesktop.portal.Desktop"),
            "/org/freedesktop/portal/desktop",
            Some("org.freedesktop.portal.OpenURI"),
            "OpenFile",
            &("", Fd::from(file.as_fd()), options),
        )
        .await?;
    let Ok(Some(answer)) = tokio::time::timeout(PORTAL_ANSWER, answers.next()).await else {
        return Ok(true);
    };
    // 0: opened; 1: the user cancelled, which is their answer; 2: the portal could not.
    let (code, _): (u32, HashMap<String, OwnedValue>) = answer?.body().deserialize()?;
    Ok(code != 2)
}

/// Starts `xdg-open` on the page and returns at once: the browser may live long. It gets
/// exactly the environment Missive received, nothing added.
fn xdg_open(path: &PathBuf) -> Result<(), String> {
    Command::new("xdg-open")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(drop)
        .map_err(|e| {
            format!(
                "no desktop portal, and cannot run xdg-open ({e}) to open {}",
                path.display()
            )
        })
}
