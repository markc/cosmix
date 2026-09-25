//! The Controller (ced E1 plan §2): tabs, the global `event_seq`, the op-id
//! generator, `ced.*` verbs, `ced.wait` waiters, reattach. iced-free — the GUI
//! (`app.rs`) and `--headless` (`headless.rs`) drive the same Controller.
//! Stage S freezes the signatures; Stage E1d implements them.
//!
//! Contracts:
//! - Every `edit.changed` event is routed to its buffer's mirror; the global
//!   `last_event_seq` (initialised from the first event after (re)subscribe)
//!   detects gaps → every Live mirror `suspect()`s; so do `resync all` and the
//!   Bus reconnect edge. An epoch change (event/reply epoch, `epoch_mismatch`,
//!   `edit` reappearing in noded `services.registered`) → reattach every tab
//!   (plan §3.8).
//! - Bus-driven actions carry `Intent::bus(tab, caller_key)`; window input
//!   carries `Intent::ui(tab)`; the intent travels through every async
//!   completion (paste, dialogs, find/replace).
//! - `ced.wait` (plan §4.8): evaluated at registration and after EVERY
//!   controller transition; a one-shot deadline timer; cancelled on tab close
//!   / Bus loss. Never a loop.

use cosmix_edit_client::diag::Diagnostics;
use cosmix_edit_client::highlight::Highlight;
use cosmix_edit_client::mirror::Mirror;
use cosmix_edit_client::model::EditorModel;
use cosmix_edit_client::types::{Incoming, Intent, Notice, Outgoing, TabId};

use crate::actions::ActionId;
use crate::config::Config;
use crate::editor::EditorMsg;

/// One tab = one buffer view.
pub struct Tab {
    pub id: TabId,
    /// `None` until `edit.open` answers (or after an open failure).
    pub mirror: Option<Mirror>,
    pub editor: EditorModel,
    pub highlight: Highlight,
    pub diagnostics: Diagnostics,
    /// Path given by the user (kept across reattach).
    pub path: Option<String>,
}

/// A `ced.*` Bus request, as the bus thread hands it over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BusCommand {
    /// Correlation for the reply (the bus thread answers `respond(id, …)`).
    pub id: u64,
    pub verb: String,
    /// JSON body.
    pub body: String,
    /// Attested caller key: `local:<from>`, `mesh:<service>@<peer>`, `anon`.
    pub caller_key: String,
}

/// What the Controller asks its host (GUI or headless loop) to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// Send a request; `req` correlates the eventual `Incoming::Reply`.
    Send { req: u64, out: Outgoing },
    /// Answer a `ced.*` Bus command.
    Respond { id: u64, rc: u8, body: String },
    /// Arm a one-shot timer (`Incoming::Timer{id}` after `ms`).
    Timer { id: u64, ms: u64 },
    /// Subscribe a topic (idempotent).
    Subscribe { topic: String },
    /// Show something in the chrome.
    Notice { tab: Option<TabId>, notice: Notice },
    /// Write the clipboard (`primary`: the selection clipboard).
    ClipboardWrite { text: String, primary: bool },
    /// Read the clipboard; the result comes back as `on_paste` with `intent`.
    ClipboardRead { primary: bool, intent: Intent },
    /// Persist the session (debounced by the host).
    SaveSession,
    /// Detach and exit (plan D13).
    Quit,
}

pub struct Controller {
    _private: (),
}

impl Controller {
    /// `run_id`: random per process start (op ids); `headless`: no window.
    pub fn new(config: Config, run_id: u32, headless: bool) -> Self {
        let _ = (config, run_id, headless);
        todo!("ced E1d")
    }

    /// The Bus came up: subscribe topics, ping `edit`, reattach the session.
    pub fn start(&mut self) -> Vec<Effect> {
        todo!("ced E1d")
    }

    /// A transport delivery (reply, topic, timer, deadline, connection edge).
    pub fn on_incoming(&mut self, incoming: Incoming) -> Vec<Effect> {
        let _ = incoming;
        todo!("ced E1d")
    }

    /// A `ced.*` / `app.*` Bus request.
    pub fn on_bus_command(&mut self, cmd: BusCommand) -> Vec<Effect> {
        let _ = cmd;
        todo!("ced E1d")
    }

    /// A menu / key / Bus action on `tab` (default: the active tab).
    pub fn on_action(&mut self, tab: Option<TabId>, action: ActionId, intent: Intent) -> Vec<Effect> {
        let _ = (tab, action, intent);
        todo!("ced E1d")
    }

    /// A message from a tab's editor widget (window input: `Intent::ui`).
    pub fn on_editor(&mut self, tab: TabId, msg: EditorMsg) -> Vec<Effect> {
        let _ = (tab, msg);
        todo!("ced E1d")
    }

    /// A clipboard read completed for `intent` (applies to `intent.tab`'s
    /// selection as it is now; dropped with a notice if that tab is gone).
    pub fn on_paste(&mut self, intent: Intent, text: Option<String>) -> Vec<Effect> {
        let _ = (intent, text);
        todo!("ced E1d")
    }

    /// Open paths (argv, `ced.open`, file drop, dialog). `path:line[:col]`
    /// suffixes are honoured when that file does not exist.
    pub fn open_paths(&mut self, paths: &[String], intent: Intent) -> Vec<Effect> {
        let _ = (paths, intent);
        todo!("ced E1d")
    }

    pub fn tabs(&self) -> &[Tab] {
        todo!("ced E1d")
    }

    pub fn active(&self) -> Option<TabId> {
        todo!("ced E1d")
    }
}
