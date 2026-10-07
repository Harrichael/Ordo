# Floating windows Ordo manages: the screenshot window

Open question. What's built works, but it may need rethinking.

## The problem
The user keeps macOS's screenshot window open and works in it like an app window. Ordo now manages it, so it parks with its workspace and comes back with it. But it always stays above every ordinary window on its workspace, so it can't be sent behind the window the user is working in.

## What it is (measured)
- Its owner is `com.apple.screencaptureui`, a background app: activation policy accessory, so no Dock icon and no app menu.
- AX lists one window, "Screenshot". Its subrole is `AXSystemDialog`, its position is settable, and AXMainWindow has no value.
- It's on **window-server layer 3** (NSFloatingWindowLevel) and stayed there throughout a 60 s sample (run 58):
  - while the user typed in it;
  - while focus was in kitty or Chrome;
  - while parked off-screen on another workspace;
  - after each restore.

  Nothing Ordo does sets it floating. Run 55's probe, taken before Ordo managed it, saw layer 3 too.
- The same app owns windows Ordo leaves alone:
  - the crop overlay (layer 24, full display, not in AXWindows);
  - the capture bar (layer 1499, 800x50);
  - a few off-screen helpers. Spent capture bars stay allocated off screen after a capture.
- Focus: while it's key, its own `AXFrontmost` is true, but NSWorkspace, `_SLPSGetFrontProcess` and the previous regular app's `AXFrontmost` all go on naming that regular app.
- `kCGWindowIsOnscreen` stays 1 while the window is parked off the display edge.

## What's built (b0e1bb7 and after)
- **Named, not inferred.** `MANAGED_BACKGROUND_APPS` (`ax.rs`) lists the background apps Ordo manages; today only the screenshot tool. Nothing a window reports tells this window from a launcher's panel or a menu-bar popover. Which one the user works in is the user's word.
- **Ordinary layers only.** For a background app, only windows at layer 3 or below are managed (`ax::admits`). The capture bar and overlay are left alone, so a capture in progress moves with the user as one thing. The user called that "honestly perfect".
- **Asked first for focus.** The focus read asks background apps before regular ones (`frontmost_app`).
- **Never hidden.** It's parked but never hidden, since no Dock icon would show it again, rescue included (`Desktop::can_hide`). Nor is it sent an un-hide.
- **Left out of the restack.** The restack covers layer 0 only (`desired_stack`), because a floating window is above every ordinary one whatever is raised.
- **Focus held by a window Ordo doesn't manage.** When the key window is one Ordo filtered out (the capture bar mid-capture), the snapshot says so (`key_unmanaged`), and the core neither records nor fights focus.

## Why its layer can't be changed
- **Accessibility:** there's no level attribute.
- **`SLSSetWindowLevel`:** the window server accepts it only from the window's owning connection, or from the Dock's "universal owner" connection.
- **yabai's route:** yabai changes layers by injecting a scripting addition into Dock.app. That needs SIP partly disabled. Not a foundation for Ordo.
- **Injecting into or patching screencaptureui:** also needs SIP off; it's a signed system app.

## Options considered
1. **Leave it floating.** This is what's built. It parks with its workspace but covers whatever is under it.
2. **Park it when not in use.** When focus moves to another window on its workspace, park it as if it were on another workspace. Bring it back when the user returns to it: Alt-Tab (it stays in the MRU order), or a switch back.
   - On top only while in use.
   - The cost: you can't glance at it while working elsewhere, and you can't click it from behind.
   - Needs a rule for "returns to it" that isn't a focus fight, and care so the restore doesn't race the focus grant.
3. **Give it a workspace of its own.** The user carries it there (Ctrl+Cmd+arrow) and switches when needed. No code.
4. **Shrink or move it aside when unfocused** instead of parking it: a corner, a smaller frame. A middle ground, but writing frames to a window the user positioned fights their own placement.
5. **Drop it from management again** and accept it on every workspace. That was the original complaint.

## Related
- `docs/loose-ends.md` § "The restack conflates the top window with the key window": a floating declared focus isn't taken back by the worker after an un-hide, only by `enforce_focus` on the next look.
- Ghost cleanup: a missed window is kept only while on screen (`still_there` in `platform/mod.rs`). Spent capture bars and Chrome's ordered-out windows are dropped as closed.
