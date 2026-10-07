//! When Ordo hides an app (Cmd+H style) that has nothing to show, so its
//! Dock icon dims. The user's choice: the dimming is a cue, and an un-hide
//! costs a switch real time (Chrome's was measured at about 0.7 s, holding
//! its other windows parked), so how much cue is worth how much speed is
//! theirs to say.

use std::time::Duration;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Hiding {
    pub when: HideWhen,
    pub idle: Idle,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HideWhen {
    /// Ordo hides nothing, and un-hides what it hid.
    Never,
    /// Once the user settles after a switch, so a quick round trip never
    /// hides and un-hides an app. Around a hide, a window was measured coming
    /// back under one that stayed shown (9 of 11 real hides), for the
    /// stacking worker to repair at the app's own pace; the cause is not yet
    /// understood (no isolated hide reproduced it).
    #[default]
    Settled,
    /// Long enough that toggling between two workspaces at a working pace
    /// never hides anything.
    Delayed,
}

impl HideWhen {
    pub fn delay(self) -> Option<Duration> {
        match self {
            HideWhen::Never => None,
            HideWhen::Settled => Some(Duration::from_millis(500)),
            HideWhen::Delayed => Some(Duration::from_secs(5)),
        }
    }
}

/// What "nothing to show" means.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Idle {
    /// None of the app's windows is on screen.
    #[default]
    OffScreen,
    /// None is on the current workspace, counting the virtual monitors not
    /// being viewed: with fewer displays than monitors, an app whose windows
    /// sit on the hidden monitor stays shown.
    OffWorkspace,
}
