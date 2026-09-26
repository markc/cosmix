//! Core events for dopus: raw worker replies plus derived view-facing events.
//!
//! Ported from src/desktop/apps/filemgr/src/browser.rs (Bevy/ctk); filemgr
//! stays untouched until retirement. Worker replies (`ListingArrived`,
//! `CountArrived`, `OperationArrived`) replace filemgr's per-inbox `try_recv`
//! types (`ListingReply` browser.rs:309, `DirectoryCountReply` browser.rs:326,
//! `OperationReply` browser.rs:368). The view-facing variants replace the ctk
//! interaction service and Bevy message writers: prompts and confirms are
//! token-keyed (v1 has no OS drag-and-drop, but the reservation state machine
//! is ported now while fresh).

use std::path::PathBuf;

use crate::config::DOpusConfig;
use crate::model::{FileEntry, PaneId};
use crate::ops::FileOpKind;

/// An event flowing through the core. Worker threads send the `*Arrived`
/// variants on the channel the app holds; [`crate::model::DopusCore::on_event`]
/// validates them and derives the rest for the view.
#[derive(Clone, Debug)]
pub enum CoreEvent {
    ListingArrived {
        pane: PaneId,
        generation: u64,
        path: PathBuf,
        root: bool,
        result: Result<Vec<FileEntry>, String>,
    },
    CountArrived {
        pane: PaneId,
        generation: u64,
        path: PathBuf,
        count: Option<usize>,
    },
    OperationArrived {
        kind: FileOpKind,
        source_pane: PaneId,
        result: Result<String, String>,
    },
    ListingStarted {
        pane: PaneId,
    },
    SelectionChanged {
        pane: PaneId,
    },
    Status {
        pane: Option<PaneId>,
        text: String,
    },
    InfoChanged,
    ConfirmRequested {
        token: u64,
        message: String,
    },
    PromptRequested {
        token: u64,
        kind: PromptKind,
        initial: String,
    },
    /// A non-directory selection was opened. filemgr spawned `xdg-open`
    /// inline (browser.rs:3270); in the core the spawn stays in the app —
    /// the core only reports the intent.
    OpenFile(PathBuf),
    /// The session config settled after the 0.35 s debounce (filemgr
    /// `persist_config`, browser.rs:3564) AND was actually persisted (or the
    /// core holds no [`crate::config::ConfigFile`]). A poison-pill refusal or
    /// a write failure does NOT emit this — an app mirroring "persisted" on
    /// it is never lied to; failures arrive as [`CoreEvent::Status`].
    ConfigSettled(DOpusConfig),
    /// Both panes were relisted after an operation reply — every visible tree
    /// is stale and must be re-read.
    RefreshAll,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptKind {
    NewFolder,
    Rename,
}

/// The user's answer to a [`CoreEvent::ConfirmRequested`]. Anything other
/// than an explicit yes fails closed: the reservation is consumed and no
/// operation runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfirmAnswer {
    Yes,
    No,
}
