//! Ordo's imperative shell.
//!
//! The pure decision core is [`ordo_core`]; this crate is everything the onion
//! keeps on the outside — observing macOS, executing effects, logging, and the
//! kill switch. The dependency arrow points inward only: the shell knows the
//! core, never the reverse.
//!
//! Module map:
//! - [`engine`] — the single serial loop: event in, core, log, effects out.
//! - [`logger`] / [`replay`] — the structured log and its replay checker.
//! - [`schema`] — the log's schema and the migration chain that grows it.
//! - [`ports`] — the two traits the engine talks to the world through.
//! - [`backend`] — the workspace-backend boundary. The implementations live
//!   in their own crates (`ordo-emulated`; SkyLight FFI in
//!   `ordo-skylight-sys`), bound to the trait under [`platform`].
//! - [`clock`] — the one place time is read.
//! - [`platform`] — the macOS FFI: displays, Accessibility, SkyLight, mouse.
//! - [`rescue`] — the log-driven recovery gather.

pub mod backend;
pub mod clock;
pub mod engine;
pub mod keys;
pub mod logger;
pub mod ports;
pub mod replay;
pub mod rescue;
pub mod schema;

#[cfg(target_os = "macos")]
pub mod platform;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
