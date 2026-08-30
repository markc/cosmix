//! In-process change-notification fanout.
//!
//! Per spec §subscribe: a `tokio::sync::broadcast` channel keyed by
//! `(SetId, ContainerId)`. The channel is created lazily on the first
//! subscriber and is published to *after* the per-set transaction
//! commits (publishing inside the transaction would let observers see
//! events that later fail to commit).
//!
//! Capacity is bounded; slow subscribers may receive `Lagged` from
//! `broadcast::Receiver::recv`. Per spec the v1 commitment is "wakes,
//! doesn't deadlock" — sub-50ms p99 latency is a v1.x tuning topic.

use crate::types::{ContainerEvent, ContainerId, SetId};
use parking_lot::Mutex;
use std::collections::HashMap;
use tokio::sync::broadcast;

/// Default per-channel capacity. 256 is a comfortable IDLE window;
/// any consumer that falls more than 256 events behind will see a
/// Lagged signal and should re-fetch.
pub const DEFAULT_CAPACITY: usize = 256;

pub struct Notifier {
    senders: Mutex<HashMap<(SetId, ContainerId), broadcast::Sender<ContainerEvent>>>,
    capacity: usize,
}

impl Notifier {
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            senders: Mutex::new(HashMap::new()),
            capacity,
        }
    }

    pub fn subscribe(
        &self,
        set: &SetId,
        container: &ContainerId,
    ) -> broadcast::Receiver<ContainerEvent> {
        let mut g = self.senders.lock();
        let sender = g
            .entry((*set, *container))
            .or_insert_with(|| broadcast::channel(self.capacity).0);
        sender.subscribe()
    }

    /// Publish an event. No-op if there are no subscribers yet, or
    /// if all subscribers have dropped — broadcast::send returns
    /// Err in those cases and we ignore it.
    pub fn publish(&self, set: &SetId, container: &ContainerId, event: ContainerEvent) {
        let g = self.senders.lock();
        if let Some(sender) = g.get(&(*set, *container)) {
            let _ = sender.send(event);
        }
    }

    /// Drop the channel for a container, e.g. on container delete.
    /// Existing receivers will see the channel close and exit their
    /// recv loop.
    pub fn drop_channel(&self, set: &SetId, container: &ContainerId) {
        self.senders.lock().remove(&(*set, *container));
    }
}

impl Default for Notifier {
    fn default() -> Self {
        Self::new()
    }
}
