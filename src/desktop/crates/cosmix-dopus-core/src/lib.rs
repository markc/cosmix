//! Headless twin-pane file-manager core for dopus.
//!
//! Logic and laws ported from `src/desktop/apps/filemgr` (Bevy/ctk), which
//! remains untouched until retirement. The behavioural spec — navigation
//! history, sort order, stale-listing rejection, the directory-count queue,
//! operation single-flight, the config poison pill — lives here so the future
//! iced frontend is identical to filemgr by construction.
//!
//! No Bevy, no iced, no tokio: plain std threads, `mpsc` channels and
//! atomics. The frontend owns the window, calls the [`model::DopusCore`]
//! mutators from its update loop, and renders from
//! [`model::DopusCore::visible_rows`].
//!
//! Event flow: worker threads send raw [`events::CoreEvent`] replies on the
//! `mpsc::Receiver` returned by [`model::DopusCore::new`]. The app feeds each
//! received event back through [`model::DopusCore::on_event`], which validates
//! it against the current generations (stale replies are dropped) and returns
//! the derived view-facing events (status lines, prompts, open-file requests).
//! [`model::DopusCore::tick`] drives the per-frame work — count dispatch and
//! the config settle debounce — and drains the same derived-event queue.
//!
//! # The app contract
//!
//! The core enforces everything it can; these laws live only in the frontend
//! (filemgr got them from Bevy's schedule or ctk, which the core does not
//! have). An app that breaks one silently changes behaviour:
//!
//! 1. **Call `tick(now)` every frame** with a monotonic `Instant`. Count
//!    dispatch recovery and the entire config debounce advance nowhere else.
//!    (The frontend also re-formats relative modified times on its own clock —
//!    filemgr's 60 s `ModifiedTimeRefresh` cycle has no core counterpart.)
//! 2. **Drain the channel and feed every event through `on_event`, exactly
//!    once, on one thread.** The channel is unbounded; an app that stops
//!    draining lets worker replies accumulate without bound.
//! 3. **Answer every dialog.** Each `ConfirmRequested`/`PromptRequested`
//!    token must eventually reach `confirm`/`prompt_text` (dismissal:
//!    `prompt_text(token, None)`). A token the app loses does not block the
//!    core, but its dialog is gone — recover with
//!    [`model::DopusCore::outstanding_reservations`] plus
//!    [`model::DopusCore::withdraw`].
//! 4. **Spawn the handler for `OpenFile`** (filemgr ran `xdg-open` inline,
//!    browser.rs:3270) and surface spawn failures as a status line.
//! 5. **Pass `ascending: true` when switching sort columns** — filemgr's
//!    column switch hardcoded ascending (browser.rs:3189); the core API
//!    accepts any flag.
//! 6. **Pre-validate prompt fields with [`model::validate_filename`]** for
//!    immediate feedback. The core re-checks at resolution (an invalid name
//!    is not a resolution), but the field-level refusal is the UX filemgr
//!    had.
//! 7. **Drive `set_split_ratio` from the divider drag** — persistence
//!    derives from core state only, so an app that never sets it persists
//!    the startup ratio.

pub mod config;
pub mod events;
pub mod model;
pub mod ops;
mod worker;

pub use config::{ConfigFile, DOpusConfig, PaneConfig, SortColumn, CURRENT_SCHEMA};
pub use events::{ConfirmAnswer, CoreEvent, PromptKind};
pub use model::{
    AvailabilitySnapshot, DopusCore, DropAction, DropActionMask, DropModifiers, FileEntry,
    NavigationHistory, PaneId, PaneModel, VisibleRow, format_child_count, format_modified_at,
    format_size, pane_summary, sanitise_display_path, sanitise_display_text, validate_filename,
};
pub use ops::{FileOpKind, FileOperation};
