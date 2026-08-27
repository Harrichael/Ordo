//! Exercise the shipped NativeBackend switch path end to end (dev tool):
//! switch every display to the given workspace ordinal and report honestly.
//!
//! Run: cargo run --example backend_switch -- 2   (then `-- 1` to come back)

use ordo::platform::native_backend;
use ordo_core::WorkspaceId;

fn main() {
    let target: u8 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(2);
    let backend = native_backend();
    let result = backend.borrow_mut().switch_workspace(WorkspaceId(target));
    match result {
        Ok(()) => println!("switched all displays to workspace {target} — verified."),
        Err(e) => println!("switch to {target} failed: {e}"),
    }
}
