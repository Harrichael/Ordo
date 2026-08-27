# Ordo keyboard shortcuts

## Ordo shortcuts

| Shortcut | Action |
| --- | --- |
| Cmd + Left | Switch to previous workspace |
| Cmd + Right | Switch to next workspace |
| Alt + Tab | Focus most-recently-used window in the current workspace |
| Alt + Shift + Tab | Focus MRU window in the current workspace and monitor |
| Alt + Backtick (`) | Focus MRU window in the current workspace and app |
| Ctrl + Alt + Cmd + Esc (twice within 2s) | Rescue / kill switch: disengage and gather windows back on-screen |
| (unbound) | Move the focused window to the other monitor |

When Ordo changes focus, the mouse pointer follows to the center of the newly
focused window. New windows open on the focused workspace and monitor.

## Standard macOS shortcuts (context and conflicts)

| Shortcut | Action | Note |
| --- | --- | --- |
| Ctrl + Left / Right | macOS native "switch Spaces" | Left to macOS; Ordo uses Cmd+arrows instead |
| Cmd + Tab | macOS app switcher | Unchanged by Ordo |
| Cmd + Backtick (`) | Cycle windows of the active app | Ordo uses Alt+Backtick to avoid clobbering this |
| Ctrl + Up | Mission Control | Unchanged |
| Ctrl + Down | App Expose (windows of the current app) | Unchanged |

## Deliberate conflict

Cmd + Left / Right normally means start/end-of-line in text fields and
Back/Forward in browsers. Ordo intercepts them globally for workspace switching,
so those default behaviors are overridden while Ordo is active — an accepted
tradeoff.
