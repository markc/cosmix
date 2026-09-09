# CosMix Term

P2.0 provides the `cosmix-term` package and `term` binary, with Wayland app ID
`dev.cosmix.term`. The CTK shell has File → Quit and Help → About (component
and crate version), a dark theme, and one terminal pane below the menu bar.

The child is `/opt/cosmix/bin/mix`, started in `$HOME` with
`TERM=xterm-256color`; product terminfo `xterm-rio` is P2.2. PTY damage wakes
the event loop directly; focused/unfocused reactive intervals are 16/33 ms.
Opening either menu suspends terminal keyboard input. Child exit and window
close retain the bounded Machine shutdown, PTY close and direct-child reap.

The P1 terminal, raster and metrics modules are preserved verbatim, including
the `TERM_SPIKE_FONT` override, ASCII keyboard scope, steady underline cursor,
cell clipping, diagnostic timing rings and two headless tests. Shaping, wide
and combining glyph layout, tabs, panes and configuration are later work.

Bus service `term` exposes INFO/HELP, `term.snapshot`, and `term.type` with
the existing 8192-byte request cap. DIAGNOSTIC surface — full ABP control
(windows/tabs/panes/sessions per SPEC) is P3a, gated on authenticated
per-instance identity (P0-I); this self-asserted `term` name is a placeholder,
not the shipped multi-user identity. Timings are process-side diagnostics,
not presented-frame evidence.

From the repository `src/` directory, run
`mix desktop/apps/term/check-no-x11.mix` to check the locked feature graph.
The package selector is `cosmix-term` (Cargo's `-p` selects a package, not the
`term` binary). The gate rejects x11, xcb, rio-window and softbuffer.
