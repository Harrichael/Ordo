use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A window's identity everywhere in the core and in the log: the CGWindowID.
///
/// Chosen because it is a plain integer that CoreGraphics, SkyLight, and (via
/// the private `_AXUIElementGetWindow`) the Accessibility API all agree on.
/// AXUIElement handles never cross into the core — they are process-local
/// CFTypeRefs with no useful Send story; the shell resolves
/// `WindowId -> AXUIElement` at effect-execution time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct WindowId(pub u32);

/// A monitor's identity: the display *hardware UUID*, not CGDirectDisplayID.
/// Display IDs can change across hot-plug and sleep; the UUID is also how the
/// SkyLight managed-display dictionaries key displays, so using it avoids a
/// translation layer in the backend.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MonitorId(pub u128);

impl fmt::Display for MonitorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = format!("{:032X}", self.0);
        write!(
            f,
            "{}-{}-{}-{}-{}",
            &s[0..8],
            &s[8..12],
            &s[12..16],
            &s[16..20],
            &s[20..32]
        )
    }
}

impl fmt::Debug for MonitorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "MonitorId({self})")
    }
}

impl std::str::FromStr for MonitorId {
    type Err = std::num::ParseIntError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let hex: String = s.chars().filter(|c| *c != '-').collect();
        Ok(MonitorId(u128::from_str_radix(&hex, 16)?))
    }
}

// Serialized as the canonical UUID string so the SQLite log stays legible and
// so MonitorId works as a JSON map key.
impl Serialize for MonitorId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for MonitorId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// 1-based workspace ordinal. Nothing more: the core never sees a CGSSpaceID.
/// The ordinal <-> space translation is the native backend's private business,
/// which is what lets an emulated backend implement the same vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct WorkspaceId(pub u8);

/// A virtual monitor: a 1-based ordinal, left to right. This is the monitor
/// the CONTROL plane speaks of — a window is declared onto one, MRU chords are
/// scoped by one, J/K step between them — and it names a position, never a
/// piece of hardware: the display that stands at position 2 today may be a
/// different panel tomorrow, and the same declarations must hold. How virtual
/// monitors land on the displays actually present is the projection
/// (`crate::project`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct VirtualMonitorId(pub u8);

/// App identity at runtime. The bundle id is carried as log metadata only —
/// pids are what AX and CG events actually speak.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Pid(pub i32);

/// Correlates an issued [`crate::Effect`] with its later observation or
/// executor result. Minted from a counter in [`crate::State`] so replay stays
/// deterministic.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct OpId(pub u64);

/// Global CoreGraphics coordinates: origin at the top-left of the main
/// display, y grows downward. This is the AX coordinate space — deliberately
/// NOT the flipped NSScreen space, so the shell converts at its own edge.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// Apps round and clamp requested frames (title-bar snapping, integral pixel
/// alignment), so observed frames rarely match requested ones exactly. Two
/// points is enough slack to call a placement "as requested" without masking
/// real moves.
pub const FRAME_EPSILON: f64 = 2.0;

impl Rect {
    pub fn center(&self) -> Point {
        Point {
            x: self.x + self.w / 2.0,
            y: self.y + self.h / 2.0,
        }
    }

    pub fn contains(&self, p: Point) -> bool {
        p.x >= self.x && p.x < self.x + self.w && p.y >= self.y && p.y < self.y + self.h
    }

    pub fn approx_eq(&self, other: &Rect, eps: f64) -> bool {
        (self.x - other.x).abs() <= eps
            && (self.y - other.y).abs() <= eps
            && (self.w - other.w).abs() <= eps
            && (self.h - other.h).abs() <= eps
    }

    /// Re-home a window frame from one monitor to another, preserving the
    /// window's *relative* position (proportional center mapping) so monitors
    /// of different resolutions still feel symmetric. Size is kept, clamped to
    /// fit; the result always lies fully inside `to`.
    pub fn translate_between(&self, from: &Rect, to: &Rect) -> Rect {
        let w = self.w.min(to.w);
        let h = self.h.min(to.h);
        let rel_x = if from.w > 0.0 {
            (self.center().x - from.x) / from.w
        } else {
            0.5
        };
        let rel_y = if from.h > 0.0 {
            (self.center().y - from.y) / from.h
        } else {
            0.5
        };
        let x = (to.x + rel_x * to.w - w / 2.0).clamp(to.x, to.x + to.w - w);
        let y = (to.y + rel_y * to.h - h / 2.0).clamp(to.y, to.y + to.h - h);
        Rect { x, y, w, h }
    }
}
