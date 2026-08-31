//! Ledger persistence across daemon restarts.
//!
//! The emulated backend's promises — which workspace each window belongs to,
//! and the on-screen frame a parked window restores to — exist nowhere in
//! observable reality: a parked sliver at the display corner is mute about
//! where it came from. Losing them on restart meant every restart stranded
//! the hidden workspaces' windows (issues.txt: F6b). This file is those
//! promises, written through on every mutation, reloaded on boot.
//!
//! Persisted state is a CLAIM, not truth. Belief still follows reality:
//! - The whole file is discarded if the machine rebooted since it was
//!   written (CGWindowIDs regenerate when WindowServer restarts, so every id
//!   in the file is garbage). Boot time is the tamper seal.
//! - Entries for windows that no longer exist are pruned only on
//!   CG-confirmed death, exactly like live ledger entries — never on mere
//!   absence from an AX scan (the first scan after a restart is the one
//!   MOST likely partial, with every app enumerated cold).
//! - `ordo run --fresh` ignores the file outright — the escape hatch when
//!   bad state must not come back online.
//!
//! Deliberately a separate file from the log DB: the log is append-only
//! history with a retention policy; this is a tiny mutable snapshot of
//! current promises. At reconciler R3 these become Desired's placement
//! slice — the schema is that data, not backend internals.

use std::path::Path;

use serde::{Deserialize, Serialize};

use ordo_core::{Pid, Rect, WindowId, WorkspaceId};

pub const VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PersistedState {
    pub version: u32,
    /// `kern.boottime` when written; a mismatch on load means the window ids
    /// are from a previous WindowServer life and the file is void.
    pub boot_time_sec: i64,
    pub current: WorkspaceId,
    pub windows: Vec<PersistedWindow>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PersistedWindow {
    pub id: WindowId,
    pub workspace: WorkspaceId,
    /// The owning app at the time of the claim — half of the window's
    /// identity (ids recycle within a boot; see the ledger's `Claim`).
    /// Defaults to unknown for files written before it existed; unknown
    /// matches any owner and upgrades on first sighting.
    #[serde(default = "unknown_owner")]
    pub owner: Pid,
    /// The frame a parked window restores to. Present for a parked window
    /// whose real frame was trustworthily seen; `None` means either "not
    /// parked" or "parked but its real frame is unknown" (the sliver guard
    /// refuses to record a park-corner frame as truth). Loading treats both
    /// `None` cases as unparked — a window without a promise re-parks and
    /// self-heals on its next trustworthy sighting, whereas a recorded lie
    /// would strand it at 1px forever.
    pub saved: Option<Rect>,
}

fn unknown_owner() -> Pid {
    Pid(0)
}

/// Seconds since epoch at which the machine booted, straight from the kernel.
pub fn boot_time_sec() -> i64 {
    let mut tv = libc::timeval {
        tv_sec: 0,
        tv_usec: 0,
    };
    let mut len = std::mem::size_of::<libc::timeval>();
    let name = c"kern.boottime";
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            &mut tv as *mut _ as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc == 0 {
        tv.tv_sec
    } else {
        0 // unknowable boot time: loads will never validate, which fails safe
    }
}

/// Atomic write: temp file + rename, so a crash mid-write leaves the previous
/// state intact rather than a truncated file.
pub fn save(path: &Path, state: &PersistedState) {
    let Ok(body) = serde_json::to_string_pretty(state) else {
        return;
    };
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, body).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// Load and validate. `None` means "start empty" — missing file, unparseable
/// file, wrong version, or a reboot since it was written.
pub fn load(path: &Path, boot_time: i64) -> Option<PersistedState> {
    let body = std::fs::read_to_string(path).ok()?;
    let state: PersistedState = serde_json::from_str(&body).ok()?;
    if state.version != VERSION || state.boot_time_sec != boot_time || boot_time == 0 {
        return None;
    }
    Some(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> PersistedState {
        PersistedState {
            version: VERSION,
            boot_time_sec: 1234,
            current: WorkspaceId(2),
            windows: vec![
                PersistedWindow {
                    id: WindowId(7),
                    workspace: WorkspaceId(2),
                    owner: Pid(100),
                    saved: None,
                },
                PersistedWindow {
                    id: WindowId(9),
                    workspace: WorkspaceId(3),
                    owner: Pid(200),
                    saved: Some(Rect {
                        x: 10.0,
                        y: 20.0,
                        w: 800.0,
                        h: 600.0,
                    }),
                },
            ],
        }
    }

    #[test]
    fn roundtrips_and_rejects_a_stale_boot() {
        let dir = std::env::temp_dir().join(format!("ordo-statefile-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");

        let state = sample();
        save(&path, &state);
        assert_eq!(load(&path, 1234), Some(state.clone()));
        // Different boot time = different WindowServer life = void.
        assert_eq!(load(&path, 1235), None);
        // Unknowable boot time must never validate anything.
        assert_eq!(load(&path, 0), None);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_pre_owner_v1_file_still_loads_with_unknown_owners() {
        // Files written before window identity carried an owner must keep
        // loading — their claims upgrade in place on first sighting instead
        // of being discarded wholesale on a schema bump.
        let body = r#"{
            "version": 1,
            "boot_time_sec": 1234,
            "current": 2,
            "windows": [{ "id": 7, "workspace": 3,
                          "saved": { "x": 1.0, "y": 2.0, "w": 3.0, "h": 4.0 } }]
        }"#;
        let state: PersistedState = serde_json::from_str(body).unwrap();
        assert_eq!(state.windows[0].owner, Pid(0));
        assert_eq!(state.windows[0].workspace, WorkspaceId(3));
    }
}
