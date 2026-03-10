---
title: cosmix-scripts
section: 7
date: 2026-03-09
description: Built-in cosmix utility scripts
---

# NAME

cosmix-scripts — built-in Lua utility scripts for cosmix

# DESCRIPTION

These scripts live in `_bin/` and are executable via `cosmix run <name>` or
directly as `_bin/<name>` (using the `#!/usr/bin/env -S cosmix run` shebang).

# SCRIPTS

## Desktop Utilities

- **appkill** — kill an application by window query
- **winpick** — interactive window picker (select and activate)
- **winmove** — move/resize windows programmatically
- **displays** — list display outputs and geometry

## System Information

- **sysinfo** — show system information (CPU, memory, disk, uptime)
- **demo** — demonstrate cosmix Lua API (windows, workspaces, clipboard)

## Media & Input

- **volume** — adjust PulseAudio/PipeWire volume
- **playerctl** — control media players (play, pause, next, prev)
- **screenshot** — capture screenshot with optional region selection

## Productivity

- **quicknote** — quick note-taking via dialog input
- **timer** — countdown timer with desktop notification
- **calc** — calculator using dialog input
- **clipnotify** — notify on clipboard changes
- **cliptransform** — transform clipboard content (upper, lower, trim, etc.)

## System Control

- **powermenu** — power menu dialog (shutdown, reboot, suspend, logout)
- **launcher** — application launcher using dialog list

## Daemon

- **daemon-demo** — demonstrate daemon IPC and clip list features

## Infrastructure

- **nsctl** — NetServa infrastructure CLI (see nsctl(1))

# CREATING SCRIPTS

Create a file in `_bin/` with the cosmix shebang:

```lua
#!/usr/bin/env -S cosmix run
-- My script description

local wins = cosmix.windows()
for _, w in ipairs(wins) do
    print(w.app_id .. ": " .. w.title)
end
```

Make it executable: `chmod +x _bin/myscript`

Scripts have access to the full `cosmix.*` Lua API including HTTP, SQLite,
formatting, file I/O, and all desktop automation functions.

# MODULE SCRIPTS

For complex functionality, create a module in `_lib/cosmix/` and a thin
`_bin/` entry point:

```lua
#!/usr/bin/env -S cosmix run
local mymod = require("cosmix.mymodule")
mymod.run(arg)
```

# FILES

- **_bin/** — executable scripts
- **_lib/cosmix/** — Lua modules
- **~/.config/cosmix/scripts/** — user scripts

# SEE ALSO

cosmix(1), nsctl(1)
