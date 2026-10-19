//! One compose window per draft, whichever process opened it.
//!
//! A window on a draft holds a lock on a file of Missive's runtime folder (`flock`, through
//! `File::try_lock`) for as long as it is open. The lock is on an open file, not on a pid
//! written in it and read back: it goes with the process when it ends, however it ends, so a
//! window that crashed leaves nothing behind that would keep the draft shut. A second window on
//! the same draft, started by `missive compose --draft` from a shell or from another reader,
//! finds the lock taken and is not opened.
//!
//! The file is named after the account and the draft's `Message-ID`, which every version of the
//! draft keeps. It stays in the runtime folder, which the system empties when the user logs out.

use std::fs::{DirBuilder, File, OpenOptions, TryLockError};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use crate::protocol::runtime_dir;

/// The lock of a draft, held until it is dropped.
#[derive(Debug)]
pub struct DraftLock {
    _file: File,
}

/// What asking for a draft's lock came to.
#[derive(Debug)]
pub enum Taking {
    /// This process holds it now.
    Ours(DraftLock),
    /// A window of another process, or of this one, holds it.
    Elsewhere,
    /// There is no folder to keep the lock in (no runtime folder, or it cannot be written):
    /// nothing is protected, and the window opens all the same.
    Unavailable,
}

/// Takes the lock of the draft `message_id` (without angle brackets) of `account`, in Missive's
/// runtime folder.
pub fn take(account: &str, message_id: &str) -> Taking {
    match runtime_dir() {
        Ok(dir) => take_in(&dir, account, message_id),
        Err(_) => Taking::Unavailable,
    }
}

/// Whether a window holds the lock of the draft, in Missive's runtime folder. Only looked at: the
/// file is not created, and the lock is taken shared and let go at once.
pub fn is_held(account: &str, message_id: &str) -> bool {
    runtime_dir().is_ok_and(|dir| is_held_in(&dir, account, message_id))
}

/// [`take`], in the folder `dir`.
pub fn take_in(dir: &Path, account: &str, message_id: &str) -> Taking {
    let opened = DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .and_then(|()| {
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(path_in(dir, account, message_id))
        });
    let Ok(file) = opened else {
        return Taking::Unavailable;
    };
    match file.try_lock() {
        Ok(()) => Taking::Ours(DraftLock { _file: file }),
        Err(TryLockError::WouldBlock) => Taking::Elsewhere,
        Err(TryLockError::Error(_)) => Taking::Unavailable,
    }
}

/// [`is_held`], in the folder `dir`.
pub fn is_held_in(dir: &Path, account: &str, message_id: &str) -> bool {
    let Ok(file) = File::open(path_in(dir, account, message_id)) else {
        return false;
    };
    match file.try_lock_shared() {
        Ok(()) => {
            let _ = file.unlock();
            false
        }
        Err(TryLockError::WouldBlock) => true,
        Err(TryLockError::Error(_)) => false,
    }
}

/// The file of a draft's lock. A Message-ID can hold any character and be long, so the name is
/// a hash of the account and the Message-ID (FNV-1a, written here because the name must stay
/// the same for every program and build that looks at it: the standard hasher promises
/// nothing of the kind). Two drafts whose names collide would only refuse each other's window.
fn path_in(dir: &Path, account: &str, message_id: &str) -> PathBuf {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in account.bytes().chain([0]).chain(message_id.bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    dir.join(format!("compose-{hash:016x}.lock"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_draft_is_locked_once_until_the_lock_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let held = |account, id| is_held_in(dir.path(), account, id);
        assert!(
            !held("alice", "d@example.org"),
            "nothing is created by looking"
        );

        let Taking::Ours(lock) = take_in(dir.path(), "alice", "d@example.org") else {
            panic!("the first window takes the lock");
        };
        assert!(held("alice", "d@example.org"));
        // Another window, another process or not: its descriptor is its own.
        assert!(matches!(
            take_in(dir.path(), "alice", "d@example.org"),
            Taking::Elsewhere
        ));
        // Another draft, or the same Message-ID of another account, is free.
        assert!(matches!(
            take_in(dir.path(), "alice", "other@example.org"),
            Taking::Ours(_)
        ));
        assert!(matches!(
            take_in(dir.path(), "bob", "d@example.org"),
            Taking::Ours(_)
        ));

        drop(lock);
        assert!(!held("alice", "d@example.org"));
        assert!(matches!(
            take_in(dir.path(), "alice", "d@example.org"),
            Taking::Ours(_)
        ));
    }

    #[test]
    fn a_folder_that_cannot_be_written_protects_nothing_and_blocks_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, b"not a folder").unwrap();
        assert!(matches!(
            take_in(&file.join("missive"), "alice", "d@example.org"),
            Taking::Unavailable
        ));
        assert!(!is_held_in(&file, "alice", "d@example.org"));
    }

    #[test]
    fn the_name_of_the_file_tells_drafts_apart_and_is_stable() {
        let dir = Path::new("/run/user/1000/missive");
        let name = |account, id| path_in(dir, account, id);
        assert_eq!(
            name("alice", "d@example.org"),
            name("alice", "d@example.org")
        );
        assert_ne!(
            name("alice", "d@example.org"),
            name("alice", "e@example.org")
        );
        // The separator keeps `ab` + `c` from being `a` + `bc`.
        assert_ne!(name("ab", "c"), name("a", "bc"));
        // Fixed for good: another build must find the same file (FNV-1a 64, computed apart).
        assert_eq!(
            name("alice", "d@example.org").file_name().unwrap(),
            "compose-28f55fcf3b801525.lock"
        );
    }
}
