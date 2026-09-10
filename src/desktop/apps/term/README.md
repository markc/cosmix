# CosMix Term tabs

The `cosmix-term` package supplies the `term` binary and Wayland app ID
`dev.cosmix.term`. Help → About shows the component and crate version.
Children start in `$HOME` with the startup-selected `TERM` (see below).
PTY damage wakes the reactive event loop (focused/unfocused: 16/33 ms).
The preserved core retains `TERM_SPIKE_FONT`, ASCII input, a steady
cursor, cell clipping and diagnostic timing rings. Shaping, wide/combining
glyph layout and panes remain later work.

Each tab owns one independent Mix PTY and terminal Machine. Background output
continues draining. The active tab alone is rasterised; switching tabs forces
a complete upload, including switches requested through the Bus.

Use the themed tab buttons to select a tab and `+` to create one. File contains
New Tab, Close Tab and Quit. Ctrl+Shift+T opens a tab, Ctrl+Shift+W closes the
active tab, and Ctrl+PageDown / Ctrl+PageUp cycle forwards / backwards. These
shortcuts are intercepted before terminal input. Open menus suspend PTY input.
Closing the active tab selects its right neighbour, or the left neighbour at
the end. Closing the last tab quits after bounded terminal shutdown.

The diagnostic `term` Bus service accepts body-only numeric IDs for
`term.tab.select` and `term.tab.close`. `term.tab.new` opens and activates a tab;
`term.tabs` lists stable IDs, selection, titles, dimensions and child PIDs.
`term.snapshot` and `term.type` target the active tab. Requests remain limited
to 8192 bytes and replies to a two-second timeout. This self-asserted service
is diagnostic only: authenticated per-instance identity remains gated on P0-I.

Headless tab tests launch real Mix children where `/opt/cosmix/bin/mix` exists
and print an explicit skip otherwise. The existing terminal core tests remain
unchanged. GUI interaction and presented-frame evidence require runtime checks.

From `src/`, run `mix desktop/apps/term/check-no-x11.mix` to check the locked
feature graph. The gate rejects x11, xcb, rio-window and softbuffer.

## Startup configuration

Copy [term.example.conf.mix](term.example.conf.mix) to
`$XDG_CONFIG_HOME/cosmix/term.conf.mix`, falling back to
`~/.config/cosmix/term.conf.mix`. Empty or relative XDG paths fall back to HOME.
The existing CosMix strict-data parser reads this file once; it cannot execute
Mix code. Missing, unreadable, malformed or invalid files log once to stderr
and use all defaults. Unknown keys are rejected. There is no automatic write
or live reload.

| Key | Values | Default |
| --- | --- | --- |
| `font_px` | Number, 6–48 logical pixels | 13 |
| `scrollback` | Integer, 0–1000000 history lines per tab | 1000 (existing Crosswords limit) |
| `cursor` | `"block"` (inverted cell) or `"underline"` | `"underline"` |

A valid `TERM_FONT_PX` overrides `font_px`, which overrides the default.
Invalid environment values are ignored. The resolved size survives fractional
scale changes. Every tab receives the configured history limit; zero disables
history. Cursor styles are steady, without blinking or application style overrides.

`term --print-config` prints resolved settings as JSON, including `TERM`, and
exits before checking Wayland or starting fonts, PTYs or the Bus. It works
without a GUI; config diagnostics go to stderr, JSON to stdout.

## Optional xterm-rio terminfo

[assets/rio.terminfo](assets/rio.terminfo) is the unmodified Rio source from
the same revision as the rio-vt dependency, with a provenance/licence header.
To install it, invoke `tic -x rio.terminfo` from the assets directory. With
Mix, use `run_argv_must(["tic", "-x", "rio.terminfo"])`. For an explicit
per-user destination, pass `-o` and the absolute path to your `~/.terminfo`
directory. Use `infocmp -x xterm-rio` to check the installed entry.

At startup Term runs `infocmp -x xterm-rio` directly, without a shell, discarding
its output. Success selects `TERM=xterm-rio` for every child; missing infocmp,
missing terminfo or any probe failure selects `TERM=xterm-256color`. This uses
infocmp's normal `TERMINFO`, `TERMINFO_DIRS`, user and system database lookup,
including database formats handled by the installed ncurses tools. Term never
runs tic or installs terminfo. The selected name is visible in `--print-config`;
Bus verb shapes and HELP gating are unchanged.
