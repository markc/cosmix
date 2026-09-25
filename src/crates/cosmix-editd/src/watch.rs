//! External-change detection (plan §4.7). Event-driven; no polling.
//!
//! One `notify::RecommendedWatcher` (inotify) watches each open file's PARENT
//! directory non-recursively, refcounted per directory (a file watch would lose
//! track after another editor's rename-over-save). Events set the owning
//! actor's "recheck disk" flag.
//!
//! On recheck the actor stats the bound path: equal to `base` → nothing (this
//! swallows our own saves); otherwise hash — equal to `base` → refresh `base`;
//! different → clean buffer: `reload_minimal` as `tool:disk`, `disk: clean`;
//! dirty: `disk: modified`; gone: `disk: deleted`. Each transition emits a
//! `disk` event and `props.changed`.
//!
//! # Contract: re-registration (frozen)
//! inotify watches a directory INODE. On the watched directory's own removal
//! or move (`IN_DELETE_SELF` / `IN_MOVE_SELF`, surfaced by notify as a remove
//! or rename of the watched path itself) and the `IN_IGNORED` that follows:
//! drop the dead watch, re-resolve the parent BY PATH, and
//! - parent exists → add a new watch, recheck every actor bound under it;
//! - parent missing → mark those buffers `disk: deleted`, watch the nearest
//!   existing ancestor (non-recursive, at most `WATCH_ANCESTOR_DEPTH` levels),
//!   re-arm one level down on each matching create until the parent is back,
//!   then recheck.
//!
//! Watch failure (inotify limit, network fs) → warn once, `disk: unwatched`;
//! the save-time revalidation still guards saves.

use std::collections::HashMap;
use std::path::PathBuf;

use cosmix_edit_core::wire::BufferId;

/// Directory → buffers bound under it. Stage S: shape frozen, behaviour E0b.
#[derive(Debug, Default)]
pub struct WatchTable {
    pub dirs: HashMap<PathBuf, Vec<BufferId>>,
}
