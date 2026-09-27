//! `dopus` — the CosMix twin-pane file manager (iced), the windowed frontend
//! of the headless [`cosmix_dopus_core`]. P2 is twin panes: a Places sidebar,
//! a draggable divider, per-pane location bars, and `dopus.open` PATHs that
//! land in the panes (first → left, second → right). P3 adds the file
//! operations: a modal dialog surface ([`view::dialogs`]) over the core's
//! confirm/prompt reservations, the `file.*` keyboard actions, and
//! `xdg-open` for `OpenFile`.
//!
//! The behavioural spec lives in the core (`cosmix-dopus-core`'s "The app
//! contract" — seven laws); this crate honours it:
//!
//! - law 1 (`tick` every frame for core maintenance):
//!   [`app`], [`view::rows`].
//! - law 2 (drain the channel, feed every event through `on_event` once, on
//!   one thread): [`app`] (the `STREAMS` bridge feeds `Msg::Core`).
//! - law 3 (answer every dialog): [`app`] (the modal queue renders the
//!   oldest outstanding reservation and answers it through the dialog
//!   surface; dismissal is fail-closed), [`headless`] (immediate
//!   fail-closed answers — no Bus verb can raise a dialog headless).
//! - law 4 (spawn the `OpenFile` handler): [`app`] (`xdg-open` detached,
//!   spawn failures become a status line); [`headless`] logs a refusal —
//!   headless never spawns.
//! - law 5 (`ascending: true` when switching sort columns): [`app`]
//!   (per-pane headers and sort headers alike).
//! - law 6 (pre-validate prompt fields with `validate_filename`):
//!   [`view::dialogs`] (live feedback in the field; the OK button and the
//!   Enter path refuse to fire while the name is invalid — the core
//!   re-validates at resolution).
//! - law 7 (`set_split_ratio` from the divider): [`app`] (`Msg::Split`),
//!   [`view::panes::Divider`].

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
