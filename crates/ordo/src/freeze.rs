//! Freeze diagnostics: what Ordo writes down about the input pipeline when
//! input stops reaching the apps (the README tells the story). Three kinds
//! of note, one table (`freeze_diag`):
//!   - `tap_disabled`: macOS switched Ordo's tap off and the tap switched
//!     itself back on.
//!   - `snapshot`: the freeze chord's full picture — every event tap on the
//!     machine with its owner, who holds secure input, what the window server
//!     and the apps call frontmost, the modifier and button state the HID
//!     and session layers each believe, and the processes by CPU.
//!   - `tap_sample`: the tap list alone, whenever it changes.
//!
//! The shapes live here, portable and serializable, so the record is
//! testable without an event tap; the gathering is
//! [`crate::platform::freeze`]. Latency fields are copied raw: what macOS
//! reports there has not matched anything observable, so nothing here reads
//! meaning into them.

use serde::{Deserialize, Serialize};

/// Why macOS disabled the tap, as the callback was told.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TapDisabledBy {
    Timeout,
    UserInput,
}

impl TapDisabledBy {
    pub fn as_str(&self) -> &'static str {
        match self {
            TapDisabledBy::Timeout => "timeout",
            TapDisabledBy::UserInput => "user_input",
        }
    }
}

/// Where in the event stream a tap sits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TapPoint {
    Hid,
    Session,
    AnnotatedSession,
    Other(u32),
}

/// One entry of `CGGetEventTapList`, with the tapping process named.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TapInfo {
    pub id: u32,
    pub point: TapPoint,
    /// A listen-only tap sees events and cannot hold them up; a filter tap
    /// can swallow, alter, or delay every event it is interested in.
    pub listen_only: bool,
    pub enabled: bool,
    /// `eventsOfInterest`, raw; `mask_names` decodes the common bits.
    pub mask: u64,
    pub mask_names: Vec<String>,
    pub tapping_pid: i32,
    /// The tapping process's executable path, when the pid could be resolved.
    pub tapping_process: Option<String>,
    /// 0 for a tap on the whole session.
    pub tapped_pid: i32,
    #[serde(with = "nan_as_null")]
    pub min_usec_latency: f32,
    #[serde(with = "nan_as_null")]
    pub avg_usec_latency: f32,
    #[serde(with = "nan_as_null")]
    pub max_usec_latency: f32,
}

/// JSON has no NaN. serde_json writes one as null on its own but refuses to
/// read null back into an f32, and the latency fields are exactly where
/// macOS hands out garbage.
mod nan_as_null {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &f32, s: S) -> Result<S::Ok, S::Error> {
        if v.is_finite() {
            s.serialize_f32(*v)
        } else {
            s.serialize_none()
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<f32, D::Error> {
        Ok(Option::<f32>::deserialize(d)?.unwrap_or(f32::NAN))
    }
}

/// The login session, as the window server describes it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionInfo {
    /// The process holding secure event input (a password field, or anything
    /// that forgot to release it), from the session dictionary.
    pub secure_input_pid: Option<i64>,
    pub secure_input_process: Option<String>,
    /// Carbon's own answer to the same question.
    pub secure_event_input_enabled: bool,
    pub on_console: Option<bool>,
    pub login_done: Option<bool>,
    pub user: Option<String>,
}

/// What one event-source state believes about the input devices. The HID
/// state is the hardware's; the combined session state is what the apps are
/// shown, after every tap and every synthesized event. A button or modifier
/// held in one and not the other points at the layer that stuck it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SourceState {
    pub flags: u64,
    pub left_button: bool,
    pub right_button: bool,
    pub secs_since_key_down: f64,
    pub secs_since_mouse_down: f64,
    pub secs_since_mouse_moved: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct InputState {
    pub hid: SourceState,
    pub session: SourceState,
}

/// Who is in front: by the window server's stack, and by the apps' own
/// `AXFrontmost` (the read Ordo's focus tracking stands on).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Front {
    pub server_window: Option<u32>,
    pub server_pid: Option<i32>,
    pub server_process: Option<String>,
    pub ax_pid: Option<i32>,
    pub ax_process: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FreezeSnapshot {
    pub wall_ms: i64,
    pub took_ms: u64,
    pub taps: Vec<TapInfo>,
    pub taps_error: Option<String>,
    pub session: SessionInfo,
    pub input: InputState,
    pub front: Front,
    /// `ps` output, header first, top processes by CPU.
    pub top_processes: Vec<String>,
    /// Every `ps` line naming a known injector or tap owner
    /// ([`is_suspect`]), wherever it ranks: whether each was alive, since
    /// when, and under what parent.
    pub suspects: Vec<String>,
    pub ps_error: Option<String>,
}

/// Remote-control tools and tap owners worth finding in the process list
/// by name. Substrings, case-sensitive, as `ps` prints them.
const SUSPECTS: &[&str] = &[
    "Splashtop",
    "SRStreamer",
    "ninja",
    "NinjaRMM",
    "LinearMouse",
    "Hammerspoon",
];

pub fn is_suspect(ps_line: &str) -> bool {
    SUSPECTS.iter().any(|s| ps_line.contains(s))
}

/// One row of `freeze_diag`. `kind` names the variant in the row and in the
/// JSON alike, so the payload reads on its own.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Diag {
    TapDisabled { wall_ms: i64, by: TapDisabledBy },
    TapSample { wall_ms: i64, taps: Vec<TapInfo> },
    /// Boxed: a snapshot is an order of magnitude larger than the other
    /// notes, and the enum rides in every `Msg`.
    Snapshot(Box<FreezeSnapshot>),
}

impl Diag {
    pub fn kind(&self) -> &'static str {
        match self {
            Diag::TapDisabled { .. } => "tap_disabled",
            Diag::TapSample { .. } => "tap_sample",
            Diag::Snapshot(_) => "snapshot",
        }
    }

    /// When it was observed, not when it was filed: the engine may be busy.
    pub fn wall_ms(&self) -> i64 {
        match self {
            Diag::TapDisabled { wall_ms, .. } | Diag::TapSample { wall_ms, .. } => *wall_ms,
            Diag::Snapshot(s) => s.wall_ms,
        }
    }
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn process_label(pid: i32, path: Option<&str>) -> String {
    match path {
        Some(p) => format!("{pid} {}", basename(p)),
        None => format!("{pid} ?"),
    }
}

impl FreezeSnapshot {
    /// The one line that goes to stderr: who taps, who holds secure input,
    /// who is in front, what is stuck, what is hot. The log has the rest.
    pub fn summary(&self) -> String {
        // One entry per tapping process: a system daemon holds several
        // taps, and naming it once with a count reads better than thrice.
        let mut pids: Vec<i32> = Vec::new();
        for t in &self.taps {
            if !pids.contains(&t.tapping_pid) {
                pids.push(t.tapping_pid);
            }
        }
        let taps: Vec<String> = pids
            .iter()
            .map(|pid| {
                let own: Vec<&TapInfo> = self.taps.iter().filter(|t| t.tapping_pid == *pid).collect();
                let mut s = process_label(*pid, own[0].tapping_process.as_deref());
                let mut kinds: Vec<String> = Vec::new();
                for t in &own {
                    let kind = format!(
                        "{} {}",
                        match t.point {
                            TapPoint::Hid => "hid",
                            TapPoint::Session => "session",
                            TapPoint::AnnotatedSession => "annotated",
                            TapPoint::Other(_) => "other",
                        },
                        if t.listen_only { "listen" } else { "filter" }
                    );
                    if !kinds.contains(&kind) {
                        kinds.push(kind);
                    }
                }
                s.push(' ');
                s.push_str(&kinds.join("+"));
                if own.len() > 1 {
                    s.push_str(&format!(" x{}", own.len()));
                }
                match own.iter().filter(|t| !t.enabled).count() {
                    0 => {}
                    d if d == own.len() => s.push_str(" DISABLED"),
                    d => s.push_str(&format!(" ({d} DISABLED)")),
                }
                s
            })
            .collect();
        let secure = match self.session.secure_input_pid {
            Some(pid) => process_label(pid as i32, self.session.secure_input_process.as_deref()),
            None if self.session.secure_event_input_enabled => "enabled, owner unknown".into(),
            None => "none".into(),
        };
        let front = match self.front.server_pid {
            Some(pid) => process_label(pid, self.front.server_process.as_deref()),
            None => "none".into(),
        };
        let mut stuck = Vec::new();
        for (name, st) in [("hid", &self.input.hid), ("session", &self.input.session)] {
            if st.left_button {
                stuck.push(format!("{name} left button held"));
            }
            if st.right_button {
                stuck.push(format!("{name} right button held"));
            }
            if st.flags & MODIFIER_FLAGS != 0 {
                stuck.push(format!("{name} modifiers {:#x}", st.flags));
            }
        }
        let stuck = if stuck.is_empty() {
            "nothing held".to_string()
        } else {
            stuck.join(", ")
        };
        let hot: Vec<String> = self.top_processes.iter().skip(1).take(3).map(|l| hot_label(l)).collect();
        format!(
            "freeze snapshot at {}: taps [{}]; secure input {}; front {}; {}; top cpu [{}]; took {} ms{}",
            self.wall_ms,
            taps.join(", "),
            secure,
            front,
            stuck,
            hot.join(" | "),
            self.took_ms,
            match (&self.taps_error, &self.ps_error) {
                (None, None) => String::new(),
                (t, p) => format!(
                    "; errors: {}",
                    [t.as_deref(), p.as_deref()].into_iter().flatten().collect::<Vec<_>>().join("; ")
                ),
            }
        )
    }
}

/// A `ps -o pid,ppid,pcpu,pmem,etime,user,comm` line as "pid cpu% name"; a
/// line that is not one (a different ps, a header) goes through trimmed.
fn hot_label(line: &str) -> String {
    let fields: Vec<&str> = line.split_whitespace().collect();
    match fields.as_slice() {
        [pid, _, cpu, _, _, _, comm @ ..] if !comm.is_empty() => {
            format!("{pid} {cpu}% {}", basename(&comm.join(" ")))
        }
        _ => line.trim().to_string(),
    }
}

/// Shift, Control, Alternate, Command: the bits a stuck key would show in
/// `CGEventSourceFlagsState` (kCGEventFlagMask*). The rest — caps lock, the
/// numeric pad, the secondary function key — are nobody's chord.
const MODIFIER_FLAGS: u64 = 0x0002_0000 | 0x0004_0000 | 0x0008_0000 | 0x0010_0000;

#[cfg(test)]
mod tests {
    use super::*;

    fn tap(pid: i32, process: &str, point: TapPoint, listen_only: bool, enabled: bool) -> TapInfo {
        TapInfo {
            id: pid as u32,
            point,
            listen_only,
            enabled,
            mask: 0x1C00,
            mask_names: vec!["key_down".into(), "key_up".into(), "flags_changed".into()],
            tapping_pid: pid,
            tapping_process: Some(format!("/Applications/{process}.app/Contents/MacOS/{process}")),
            tapped_pid: 0,
            min_usec_latency: 1.0,
            avg_usec_latency: 2.0,
            max_usec_latency: 3.0e7,
        }
    }

    fn snapshot() -> FreezeSnapshot {
        let mut hammerspoon = tap(500, "Hammerspoon", TapPoint::Session, false, false);
        hammerspoon.avg_usec_latency = f32::NAN;
        FreezeSnapshot {
            wall_ms: 1_700_000_000_000,
            took_ms: 42,
            taps: vec![
                tap(300, "LinearMouse", TapPoint::Hid, false, true),
                tap(400, "ordo", TapPoint::Session, false, true),
                hammerspoon,
                tap(600, "NotificationCenter", TapPoint::Session, true, true),
                tap(650, "universalaccessd", TapPoint::Session, true, false),
                tap(650, "universalaccessd", TapPoint::Session, true, true),
                tap(650, "universalaccessd", TapPoint::Session, true, true),
            ],
            taps_error: None,
            session: SessionInfo {
                secure_input_pid: Some(700),
                secure_input_process: Some("/System/Library/CoreServices/loginwindow.app/Contents/MacOS/loginwindow".into()),
                secure_event_input_enabled: true,
                on_console: Some(true),
                login_done: Some(true),
                user: Some("mh".into()),
            },
            input: InputState {
                hid: SourceState::default(),
                session: SourceState {
                    flags: 0x0010_0000,
                    left_button: true,
                    ..SourceState::default()
                },
            },
            front: Front {
                server_window: Some(9),
                server_pid: Some(800),
                server_process: Some("/Applications/kitty.app/Contents/MacOS/kitty".into()),
                ax_pid: Some(800),
                ax_process: Some("/Applications/kitty.app/Contents/MacOS/kitty".into()),
            },
            top_processes: vec![
                "  PID  PPID  %CPU %MEM  ELAPSED USER COMM".into(),
                "  900     1  87.0  0.3 03:06:50 mh   /Applications/Splashtop Streamer.app/Contents/MacOS/Splashtop Streamer".into(),
                "  400     1   2.0  0.1 03:06:50 mh   ordo".into(),
            ],
            suspects: vec![
                "  900     1  87.0  0.3 03:06:50 mh   /Applications/Splashtop Streamer.app/Contents/MacOS/Splashtop Streamer".into(),
            ],
            ps_error: None,
        }
    }

    /// The record survives the log and back, latency nonsense included (a
    /// NaN becomes null, which is the honest value), and the kind is in the
    /// JSON so a row reads without its column.
    #[test]
    fn every_diag_round_trips_through_json_with_its_kind_inside() {
        let diags = vec![
            Diag::TapDisabled {
                wall_ms: 5,
                by: TapDisabledBy::UserInput,
            },
            Diag::TapSample {
                wall_ms: 6,
                taps: vec![tap(400, "ordo", TapPoint::Session, false, true)],
            },
            Diag::Snapshot(Box::new(snapshot())),
        ];
        for d in diags {
            let json = serde_json::to_string(&d).unwrap();
            let v: serde_json::Value = serde_json::from_str(&json).unwrap();
            assert_eq!(v["kind"], d.kind(), "{json}");
            let back: Diag = serde_json::from_str(&json).unwrap();
            assert_eq!(back.wall_ms(), d.wall_ms());
            match (d, back) {
                (Diag::Snapshot(mut a), Diag::Snapshot(mut b)) => {
                    assert!(b.taps[2].avg_usec_latency.is_nan(), "NaN came back as null");
                    a.taps[2].avg_usec_latency = 0.0;
                    b.taps[2].avg_usec_latency = 0.0;
                    assert_eq!(a, b);
                }
                (d, back) => assert_eq!(d, back),
            }
        }
    }

    /// The stderr line names the suspects a reader would go looking for in
    /// the log: each tap's owner and whether it filters, a disabled tap,
    /// the secure-input holder, the front app, a held button, and what is
    /// burning CPU.
    #[test]
    fn the_summary_names_the_suspects() {
        let s = snapshot().summary();
        assert!(s.contains("300 LinearMouse hid filter"), "{s}");
        assert!(s.contains("400 ordo session filter"), "{s}");
        assert!(s.contains("500 Hammerspoon session filter DISABLED"), "{s}");
        assert!(s.contains("600 NotificationCenter session listen,"), "{s}");
        assert!(s.contains("650 universalaccessd session listen x3 (1 DISABLED)"), "{s}");
        assert_eq!(s.matches("universalaccessd").count(), 1, "{s}");
        assert!(s.contains("secure input 700 loginwindow"), "{s}");
        assert!(s.contains("front 800 kitty"), "{s}");
        assert!(s.contains("session left button held"), "{s}");
        assert!(s.contains("session modifiers 0x100000"), "{s}");
        assert!(!s.contains("hid left button"), "{s}");
        assert!(s.contains("top cpu [900 87.0% Splashtop Streamer | 400 2.0% ordo]"), "{s}");
        assert!(!s.contains("errors"), "{s}");

        let quiet = FreezeSnapshot {
            taps: vec![],
            session: SessionInfo::default(),
            input: InputState::default(),
            front: Front::default(),
            top_processes: vec![],
            ps_error: Some("ps timed out".into()),
            ..snapshot()
        }
        .summary();
        assert!(quiet.contains("secure input none"), "{quiet}");
        assert!(quiet.contains("nothing held"), "{quiet}");
        assert!(quiet.contains("errors: ps timed out"), "{quiet}");
    }

    /// The suspects are the remote-control pieces and the tap owners, by
    /// name, wherever `ps` ranks them; the rest of the list is not.
    #[test]
    fn suspects_are_the_known_injectors_and_tap_owners_by_name() {
        for line in [
            "335 1 0.0 0.1 03:00:00 root /Applications/Splashtop Streamer.app/Contents/MacOS/SRStreamerDaemon",
            "24641 1 0.0 0.2 03:00:00 mh /Applications/Splashtop Streamer.app/Contents/MacOS/Splashtop Streamer",
            "332 1 0.1 0.1 03:00:00 root /Applications/NinjaRMMAgent/programfiles/ninjarmm-macagent",
            "1243 1 0.0 0.1 03:00:00 mh /Applications/LinearMouse.app/Contents/MacOS/LinearMouse",
            "1206 1 0.3 0.4 03:00:00 mh /Applications/Hammerspoon.app/Contents/MacOS/Hammerspoon",
        ] {
            assert!(is_suspect(line), "{line}");
        }
        for line in [
            "  PID  PPID  %CPU %MEM  ELAPSED USER COMM",
            "166 1 26.4 0.5 03:00:00 _windowserver /System/Library/PrivateFrameworks/SkyLight.framework/Resources/WindowServer",
            "856 1 1.0 0.5 03:00:00 mh /Applications/kitty.app/Contents/MacOS/kitty",
        ] {
            assert!(!is_suspect(line), "{line}");
        }
    }
}
