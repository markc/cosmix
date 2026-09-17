# Term

The Bevy frontend lives here (`src/main.rs`, `src/mouse_input.rs` and their
tests). Everything else (PTY, grid, tabs, raster, Bus and the native-session
lane, with their tests, the test broker and the gate notes) is in
`../../crates/cosmix-term-core`: start with its `CODEX.md` and
`tests/README.md`. The `check-*.mix` gates in this directory run that crate's
tests.
