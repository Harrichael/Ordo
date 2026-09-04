# Ordo keyboard shortcuts

## Ordo shortcuts

| Shortcut | Action |
| --- | --- |
| Cmd + Left | Switch to previous workspace |
| Cmd + Right | Switch to next workspace |
| Alt + Tab | Focus most-recently-used window in the current workspace |
| Alt + Shift + Tab | Focus MRU window in the current workspace and monitor |
| Alt + Backtick (`) | Focus MRU window in the current workspace and app |
| Ctrl + Alt + Tab | Focus MRU window on the other monitor |
| Cmd + Shift + Left / Right | Move the focused window to the previous / next virtual monitor (focus and mouse go with it; a hidden monitor is viewed) |
| Cmd + Alt + J / K | View the previous / next virtual monitor: focus its MRU window and, where displays are short, show its windows in place of the current monitor's (no wrap) |
| Cmd + Alt + V | Virtualization on / off: off collapses every virtual monitor onto the displays present |
| Ctrl + Cmd + Left / Right | Carry the focused window to the previous / next workspace and switch there with it |
| Alt + End | Demote the focused window: back of the MRU history, back of the visual stack, focus moves to the next one |
| Ctrl + Alt + Cmd + Esc (twice within 2s) | Rescue / kill switch: disengage and gather windows back on-screen |
| Ctrl + Alt + Cmd + O | Engage WITH the state file: undo a rescue or bring a `--paused` run alive, loading the saved organization |
| Ctrl + Alt + Cmd + R | Engage WITHOUT the state file: blank workspace model; the file is neither read nor written until S or O |
| Ctrl + Alt + Cmd + S | Save state: turn the state file back on and persist the current arrangement as the new truth |

When Ordo changes focus, the mouse pointer follows to the center of the newly
focused window. New windows open on the focused workspace and monitor.

Virtual monitors are positions, left to right, not displays: unplug the
external display and its monitor's windows are hidden (or, with virtualization
off, collapsed onto the laptop); plug it back in and they return to it. The
viewed monitor is an anchor — it is always on screen, and Cmd+Alt+J/K move it
one step, so on a rig with a display per monitor they are focus jumps.

## Standard macOS shortcuts (context and conflicts)

| Shortcut | Action | Note |
| --- | --- | --- |
| Ctrl + Left / Right | macOS native "switch Spaces" | Left to macOS; Ordo uses Cmd+arrows instead |
| Cmd + Tab | macOS app switcher | Unchanged by Ordo |
| Cmd + Backtick (`) | Cycle windows of the active app | Ordo uses Alt+Backtick to avoid clobbering this |
| Ctrl + Up | Mission Control | Unchanged |
| Ctrl + Down | App Expose (windows of the current app) | Unchanged |

## Deliberate conflicts

Cmd + Left / Right normally means start/end-of-line in text fields and
Back/Forward in browsers, and Cmd + Shift + Left / Right normally selects to
line start/end. Ordo intercepts both chord families globally, so those default
behaviors are overridden while Ordo is active — an accepted tradeoff.
