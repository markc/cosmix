//! `dopus` — the CosMix twin-pane file manager (iced), the windowed frontend
//! of the headless [`cosmix_dopus_core`]. P1 is one live pane: a window, the
//! theme, a sorted listing with keyboard selection. The second pane, divider,
//! Places and file-operation UI arrive in P2/P3.
//!
//! The behavioural spec lives in the core (`cosmix-dopus-core`'s "The app
//! contract" — seven laws); this crate honours it:
//!
//! - law 1 (`tick` every frame + the app's own clock for relative times):
//!   [`app`], [`view::rows`].
//! - law 2 (drain the channel, feed every event through `on_event` once, on
//!   one thread): [`app`] (the `STREAMS` bridge feeds `Msg::Core`).
//! - law 3 (answer every dialog): [`app`] (P1 withdraws — nothing wedges),
//!   [`headless`] (fail-closed answers).
//! - law 4 (spawn the `OpenFile` handler): [`app`] (P1 refuses with a status
//!   line; no spawn until P3).
//! - law 5 (`ascending: true` when switching sort columns): [`app`].
//! - law 6 (pre-validate prompt fields with `validate_filename`): moot in P1
//!   (no prompt UI); noted for P2's dialogs.
//! - law 7 (`set_split_ratio` from the divider): moot in P1 (no divider).

pub mod app;
pub mod bus;
pub mod config;
pub mod dirs;
pub mod headless;
pub mod icons;
pub mod keys;
pub mod theme;
pub mod verbs;
pub mod view;
