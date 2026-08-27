// The imperative shell arrives in later milestones (observe-only first, then
// hotkeys, then workspace mutations). M1 ships only the pure core.
fn main() {
    let state = ordo_core::State::new();
    println!(
        "ordo (M1, core only): {} workspaces modeled, no shell yet. Run `cargo test`.",
        state.workspace_count
    );
}
