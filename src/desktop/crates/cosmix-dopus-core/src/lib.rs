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

pub mod config;
pub mod events;
pub mod model;
pub mod ops;
mod worker;

pub use config::{ConfigFile, DOpusConfig, PaneConfig, SortColumn, CURRENT_SCHEMA};
pub use events::{ConfirmAnswer, CoreEvent, PromptKind};
pub use model::{
    AvailabilitySnapshot, DopusCore, DropAction, DropActionMask, DropModifiers, FileEntry,
    NavigationHistory, PaneId, PaneModel, VisibleRow,
};
pub use ops::{FileOpKind, FileOperation};
