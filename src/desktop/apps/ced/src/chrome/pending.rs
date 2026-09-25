//! **Temporary** (E1f, deleted at the E1d/E1f integration): the Controller
//! additions the chrome calls, proposed to E1d as additive methods on
//! `Controller` (plus `Effect::Prompt`). Until E1d lands them, this trait
//! lets `app.rs` compile against the Stage S stub; every body is the same
//! `todo!()` the stub itself uses. At integration the trait and this file go
//! away and the calls resolve to E1d's inherent methods of the same names.

use cosmix_edit_client::highlight::ResultTag;
use cosmix_edit_client::types::{Intent, TabId};

use crate::actions::ActionId;
use crate::controller::{Controller, Effect};
use crate::session::Session;
use crate::verbs::{EditInfo, LayoutReply};

/// A decision the controller needs from a human (window only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Prompt {
    /// `edit.close` refused `CONFLICT dirty`: this tab holds the last view.
    CloseDirty { tab: TabId, intent: Intent },
    /// A plain save refused `disk_modified`.
    DiskModified { tab: TabId, intent: Intent },
    /// On launch: restored buffers nobody holds (§3.8).
    Recovered { buffers: Vec<RecoveredRow> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveredRow {
    pub buffer: String,
    pub path: Option<String>,
    pub name: String,
    pub bytes: Option<usize>,
}

#[allow(unused_variables)]
pub trait ControllerExt {
    fn on_action_args(&mut self, tab: Option<TabId>, action: ActionId, args: Option<serde_json::Value>, intent: Intent) -> Vec<Effect> {
        todo!("ced E1d")
    }
    fn select_tab(&mut self, tab: TabId) -> Vec<Effect> {
        todo!("ced E1d")
    }
    fn dismiss_conflict(&mut self, tab: TabId, rev: u64) {
        todo!("ced E1d")
    }
    fn keep_mine(&mut self, tab: TabId, intent: Intent) -> Vec<Effect> {
        todo!("ced E1d")
    }
    fn take_service(&mut self, tab: TabId) -> Vec<Effect> {
        todo!("ced E1d")
    }
    fn keep_as_new(&mut self, tab: TabId, intent: Intent) -> Vec<Effect> {
        todo!("ced E1d")
    }
    fn open_recovered(&mut self, buffer: &str, intent: Intent) -> Vec<Effect> {
        todo!("ced E1d")
    }
    fn discard_recovered(&mut self, buffer: &str) -> Vec<Effect> {
        todo!("ced E1d")
    }
    fn set_layout(&mut self, layout: Option<LayoutReply>) {
        todo!("ced E1d")
    }
    fn record_frame(&mut self, view_us: u64, next_frame_us: Option<u64>) {
        todo!("ced E1d")
    }
    fn lint_capture(&mut self, tab: TabId, cfg: u64) -> Option<(ResultTag, String, Option<std::path::PathBuf>)> {
        todo!("ced E1d")
    }
    fn on_lint(&mut self, tab: TabId, tag: ResultTag, result: Result<String, String>) -> Vec<Effect> {
        todo!("ced E1d")
    }
    fn session(&self) -> Session {
        todo!("ced E1d")
    }
    fn edit_info(&self) -> Option<&EditInfo> {
        todo!("ced E1d")
    }
    /// Prompts raised since the last call (becomes `Effect::Prompt`).
    fn take_prompts(&mut self) -> Vec<Prompt> {
        todo!("ced E1d")
    }
}

impl ControllerExt for Controller {}
