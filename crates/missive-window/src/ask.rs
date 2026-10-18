//! Asking the daemon from the window: the same request, the same socket and the same start of the
//! daemon when none runs as the command line (`missive_core::client`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use missive_core::client;
use missive_core::protocol::{Command, socket_path};

use crate::view::Asker;

/// Asks the daemon on `socket`, starting `program daemon` when none answers. Each request blocks
/// until it is answered: the window calls it off its own thread, one request at a time.
pub fn asker(socket: PathBuf, program: PathBuf) -> Asker {
    Arc::new(move |command: Command| {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        runtime.block_on(client::ask(&socket, &program, command))
    })
}

/// The user's own daemon, started from the `missive` beside this program, as an archive or a
/// package installs them, else from the one on the `PATH`.
pub fn from_env() -> Result<Asker, String> {
    let socket = socket_path().map_err(|e| e.to_string())?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    Ok(asker(socket, program_beside(&exe)))
}

/// `missive` in the folder of `exe` when there is one, else `missive`, looked for on the `PATH`.
pub fn program_beside(exe: &Path) -> PathBuf {
    exe.parent()
        .map(|folder| folder.join("missive"))
        .filter(|program| program.is_file())
        .unwrap_or_else(|| PathBuf::from("missive"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_daemon_is_started_from_the_missive_beside_the_window_else_from_the_path() {
        let folder = tempfile::tempdir().unwrap();
        let exe = folder.path().join("missive-window");
        assert_eq!(program_beside(&exe), PathBuf::from("missive"));
        std::fs::write(folder.path().join("missive"), "").unwrap();
        assert_eq!(program_beside(&exe), folder.path().join("missive"));
    }
}
