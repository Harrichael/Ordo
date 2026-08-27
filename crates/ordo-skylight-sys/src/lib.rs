//! The project's entire undocumented-ABI surface, quarantined.
//!
//! Everything Ordo needs from macOS that Apple does not publish — the
//! SkyLight space APIs and `_AXUIElementGetWindow` — gets declared here and
//! nowhere else. Private symbols are what break on macOS point releases, so
//! keeping them in one auditable crate is what makes an OS update a diff of
//! this file rather than an archaeology dig.
//!
//! Planned surface (declarations land with their first caller, milestone M2+):
//! - `SLSMainConnectionID`
//! - `SLSCopyManagedDisplaySpaces` — displays -> ordered space lists -> current
//! - `SLSCopySpacesForWindows` — window -> space assignment
//! - `SLSPerformAsynchronousBridgedWindowManagementOperation` — window -> space
//!   moves without SIP (non-exported symbol; resolved by symbol-table scan)
//! - dock-swipe gesture synthesis fields for animation-free space switching
//! - `_AXUIElementGetWindow` — AXUIElement -> CGWindowID

/// WindowServer connection id (`SLSMainConnectionID`).
pub type CgsConnectionId = i32;

/// A native space id as SkyLight reports it. This type never crosses into
/// `ordo-core` — the core speaks 1-based workspace ordinals only, and the
/// native backend owns the translation.
pub type CgsSpaceId = u64;
