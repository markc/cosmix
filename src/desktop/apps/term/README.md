# CosMix Term tabs

The `cosmix-term` package supplies the `term` binary and Wayland app ID
`dev.cosmix.term`. Help → About shows the component and crate version.
Children start in `$HOME` with `TERM=xterm-256color`; `xterm-rio` is P2.2.
PTY damage wakes the reactive event loop (focused/unfocused: 16/33 ms).
The preserved core retains `TERM_SPIKE_FONT`, ASCII input, an underline
cursor, cell clipping and diagnostic timing rings. Shaping, wide/combining
glyph layout, panes and configuration remain later work.

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
