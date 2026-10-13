//! One daemon per log: a second `ordo run` (a LaunchAgent's copy beside a
//! hand-started one) would fight the first over every window.
//!
//! The claim is an exclusive `flock` on `ordo.lock` beside the log, held for
//! the life of the process. The kernel drops it however the process ends,
//! crash and SIGKILL included, so there is no stale claim to judge, as there
//! would be with a pid that may since have been reused. The file itself is
//! never deleted: a copy starting while another deletes it could lock a new
//! file while the other still holds the old one.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

/// Held for as long as this process is the one daemon for its log.
#[derive(Debug)]
pub struct Claim {
    _file: File,
}

#[derive(Debug)]
pub enum Refused {
    /// Another live process holds the claim.
    Held(PathBuf),
    Io(PathBuf, std::io::Error),
}

pub fn claim(dir: &Path) -> Result<Claim, Refused> {
    let path = dir.join("ordo.lock");
    let io = |e| Refused::Io(path.clone(), e);
    std::fs::create_dir_all(dir).map_err(io)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(io)?;
    match file.try_lock() {
        Ok(()) => Ok(Claim { _file: file }),
        Err(TryLockError::WouldBlock) => Err(Refused::Held(path)),
        Err(TryLockError::Error(e)) => Err(io(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_daemon_is_refused_until_the_first_is_gone() {
        let dir = std::env::temp_dir().join(format!("ordo-instance-{}", std::process::id()));
        let first = claim(&dir).expect("the first daemon claims the log");
        assert!(matches!(claim(&dir), Err(Refused::Held(_))));
        drop(first);
        assert!(claim(&dir).is_ok(), "released with its holder");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
