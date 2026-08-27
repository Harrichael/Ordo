//! Mouse warping — "the pointer follows focus".
//!
//! `CGWarpMouseCursorPosition` moves the cursor without synthesizing a move
//! event. On its own it triggers a ~0.25s window during which macOS suppresses
//! real hardware movement (the cursor feels stuck); re-associating mouse and
//! cursor immediately afterward cancels that suppression. Hiding the cursor
//! until it moves is intentionally not attempted: that is an AppKit,
//! frontmost-app-only capability and cannot be driven from a background agent.

use objc2_core_foundation::CGPoint;
use objc2_core_graphics::{CGAssociateMouseAndMouseCursorPosition, CGWarpMouseCursorPosition};
use ordo_core::Point;

pub fn warp_to(p: Point) {
    let _ = CGWarpMouseCursorPosition(CGPoint { x: p.x, y: p.y });
    // Without this the pointer is frozen for a quarter second — long enough to
    // feel broken right after a focus switch.
    let _ = CGAssociateMouseAndMouseCursorPosition(true);
}
