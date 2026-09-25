//! The Bus thread (ced E1 plan §2): a current-thread tokio runtime on its own
//! OS thread (the `cosmix-term-core/src/bus.rs:68-107` shape) holding a
//! `SupervisedClient` registered as `ced` (or `--service NAME`) with
//! `fatal_on_registration_rejection(true)`. It correlates requests, arms
//! one-shot timers and deadlines, subscribes topics (`edit.changed`,
//! `theme.changed`, `noded.props.changed`), forwards connection edges, and
//! hands `ced.*` commands to the Controller. Deliveries reach iced through an
//! unbounded futures channel exposed as a `Subscription` (no poll thread).
//! Stage S freezes the signatures; Stage E1d implements them.

use cosmix_edit_client::types::Incoming;
use iced::futures::channel::mpsc::UnboundedReceiver;

use crate::controller::{BusCommand, Effect};

/// Everything the bus thread delivers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    Incoming(Incoming),
    Command(BusCommand),
}

/// Why the Bus could not be started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartError {
    /// Another instance owns the service name (single-instance forward).
    NameTaken,
    /// noded refused registration for another reason (message).
    Rejected(String),
    /// No broker reachable.
    Unreachable(String),
}

/// The handle the Controller's host uses to perform [`Effect`]s that need the
/// Bus (sends, replies, timers, subscriptions).
pub struct BusHandle {
    _private: (),
}

impl BusHandle {
    pub fn perform(&self, effect: &Effect) {
        let _ = effect;
        todo!("ced E1d")
    }
}

/// Start the bus thread registered as `service`.
pub fn spawn(service: &str) -> Result<(BusHandle, UnboundedReceiver<Delivery>), StartError> {
    let _ = service;
    todo!("ced E1d")
}

/// Single-instance probe (plan §4.8): an anonymous `ced.ping` with a 500 ms
/// deadline; `true` when an instance answered. Then `forward_open` sends the
/// argv paths as `ced.open`.
pub fn probe_running(service: &str) -> bool {
    let _ = service;
    todo!("ced E1d")
}

pub fn forward_open(service: &str, paths: &[String]) -> Result<(), String> {
    let _ = (service, paths);
    todo!("ced E1d")
}
