//! Opening the compose window from `missive compose` and `missive mailto`: when a display is
//! there, `missive-window` is started on the draft they saved first, so that nothing is lost if
//! the window cannot start. It runs on its own: `missive` returns at once.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Whether a display is there to open a window on: Wayland's, or X11's.
pub fn display() -> bool {
    ["WAYLAND_DISPLAY", "DISPLAY"]
        .iter()
        .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
}

/// Starts the compose window on the draft `draft` of `account`, when there is a display. Its
/// failure to start is said on standard error: the draft is saved either way.
pub fn compose(account: &str, draft: &str) {
    if !display() {
        return;
    }
    let started = std::env::current_exe().and_then(|exe| {
        Command::new(program_beside(&exe))
            .args(["compose", "--account", account, "--draft", draft])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            // A group of its own: closing the terminal `missive` ran in leaves the window open.
            .process_group(0)
            .spawn()
    });
    if let Err(e) = started {
        eprintln!("The compose window could not start ({e}); the draft is saved.");
    }
}

/// `missive-window` in the folder of `exe` when there is one, as an archive or a package
/// installs them, else `missive-window`, looked for on the `PATH`.
fn program_beside(exe: &Path) -> PathBuf {
    exe.parent()
        .map(|folder| folder.join("missive-window"))
        .filter(|program| program.is_file())
        .unwrap_or_else(|| PathBuf::from("missive-window"))
}
