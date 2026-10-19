//! Choosing files with the system's file chooser: the desktop portal's
//! `org.freedesktop.portal.FileChooser`, which shows the desktop's own dialog, outside the
//! program. Only local files are given back.

use std::collections::HashMap;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use futures_util::StreamExt;
use percent_encoding::percent_decode_str;
use zbus::Connection;
use zbus::zvariant::{OwnedValue, Value};

use crate::opener::responses;

/// Asks the user for files on `connection`'s portal, several at once, under `title`. No file
/// when the user cancels. The answer is waited for as long as the user takes.
pub async fn choose_files(connection: &Connection, title: &str) -> Result<Vec<PathBuf>, String> {
    let failed = |e: zbus::Error| format!("the desktop portal's file chooser failed ({e})");
    // One per request of this process: two windows may ask at once.
    static ASKED: AtomicU64 = AtomicU64::new(0);
    let token = format!(
        "missive{}_{}",
        std::process::id(),
        ASKED.fetch_add(1, Ordering::Relaxed)
    );
    let mut answers = responses(connection, &token).await.map_err(failed)?;
    let options = HashMap::from([
        ("handle_token", Value::from(token.as_str())),
        ("multiple", Value::from(true)),
    ]);
    connection
        .call_method(
            Some("org.freedesktop.portal.Desktop"),
            "/org/freedesktop/portal/desktop",
            Some("org.freedesktop.portal.FileChooser"),
            "OpenFile",
            &("", title, options),
        )
        .await
        .map_err(failed)?;
    let answer = answers
        .next()
        .await
        .ok_or("the desktop portal's file chooser did not answer")?
        .map_err(failed)?;
    let (code, results): (u32, HashMap<String, OwnedValue>) =
        answer.body().deserialize().map_err(failed)?;
    match code {
        0 => {}
        1 => return Ok(Vec::new()),
        _ => return Err("the desktop portal's file chooser could not open".to_owned()),
    }
    let uris: Vec<String> = results
        .get("uris")
        .and_then(|uris| Vec::<String>::try_from(uris.try_clone().ok()?).ok())
        .unwrap_or_default();
    Ok(uris.iter().filter_map(|uri| local_path(uri)).collect())
}

/// The path of a `file://` URI on this machine, its escapes decoded, bytes as they are.
fn local_path(uri: &str) -> Option<PathBuf> {
    let path = uri.strip_prefix("file://")?;
    if !path.starts_with('/') {
        return None;
    }
    let bytes: Vec<u8> = percent_decode_str(path).collect();
    Some(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
}
