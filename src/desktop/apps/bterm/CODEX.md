# BTerm

The **Bevy** terminal frontend. Package `cosmix-bterm`, binary `bterm`, Bus
name `bterm` serving `bterm.*`.

Renamed from `term` on 2026-09-21 (TODO-term T1/D1). The global name `term`
belongs to the incoming iced+wgpu frontend: two binaries cannot both own it,
and T5's A/B weight comparison requires both running at once. **This is not a
deprecation.** D6 keeps bterm indefinitely as the reference implementation, the
A/B control for every weight claim, and the only frontend proven against the
native-session lane. It stops being the default; it does not stop existing.

The frontend lives here (`src/main.rs`, `src/mouse_input.rs` and their tests).
Everything else — PTY, grid, tabs, raster, Bus and the native-session lane,
with their tests, the test broker and the gate notes — is in
`../../crates/cosmix-term-core`: start with its `CODEX.md` and `tests/README.md`.
That crate is **shared with the iced frontend and hardcodes neither name**: it
keeps one canonical `term.*` spelling for its 147 handler arms and rewrites the
wire prefix at the dispatch boundary (`bus::canonical_verb`), which is what
keeps the two frontends behaviourally identical rather than merely similar.

The `check-*.mix` gates in this directory run that crate's tests. All of them
are run **from `src/`**, not from here.
