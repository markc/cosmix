//! Adapter states, the shared registry of adapter statuses, and the
//! state-change events the supervisor feeds to the Bus publisher.

use std::collections::BTreeMap;
use std::time::SystemTime;

/// Lifecycle state of one supervised adapter, as exposed on the
/// `dbusd.adapters` verb and the `dbusd.adapters.<name>.*` props.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterStateKind {
    /// Launched, has not yet signalled that it is serving.
    Starting,
    /// Running and serving its domain.
    Running,
    /// Failed (error, panic, or unexpected exit); waiting out backoff
    /// before the next launch.
    Backoff,
    /// Not running: disabled by config or by `dbusd.adapter.disable`.
    Disabled,
    /// The run could not be stopped: it ignored both its stop signal
    /// and the abort (a run that never yields cannot be preempted).
    /// Its Bus service and D-Bus names may still be held until the
    /// process restarts; the lifecycle keeps answering commands, and
    /// enable/restart relaunch alongside the leaked task.
    Stuck,
}

impl AdapterStateKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Backoff => "backoff",
            Self::Disabled => "disabled",
            Self::Stuck => "stuck",
        }
    }
}

/// One adapter's supervision status — one row of `dbusd.adapters`.
#[derive(Debug, Clone, PartialEq)]
pub struct AdapterStatus {
    pub name: String,
    /// The Bus service name the adapter registers under (its domain).
    pub service: String,
    pub state: AdapterStateKind,
    /// Launches of the adapter's run task after the first one in this
    /// daemon process — backoff restarts, `dbusd.adapter.restart`, and
    /// enable-after-disable all count.
    pub restarts: u64,
    /// Why the adapter last failed; sticky through backoff and the next
    /// starting attempt, cleared when it reaches `running` again.
    pub last_error: Option<String>,
    /// When the current state was entered.
    pub since: SystemTime,
}

/// A state change, handed to the citizen layer for publication as a
/// `dbusd.adapter.changed` event plus `dbusd.props.changed` diffs.
#[derive(Debug, Clone, PartialEq)]
pub struct AdapterEvent {
    /// Status after the change.
    pub status: AdapterStatus,
    /// State before the change (`status.state` is the new one).
    pub previous: AdapterStateKind,
    /// Per-daemon-session monotonic sequence (1, 2, …), stamped on
    /// every published state event. A gap between successive events
    /// means events were dropped (backlog overflow); the props surface
    /// (`dbusd.props.get`) is the truth.
    pub seq: u64,
}

/// Registry of all adapters' statuses, shared between the supervision
/// tasks (writers) and the citizen dispatch/props surface (readers).
/// Ordered by name so listings are stable.
#[derive(Debug, Default)]
pub struct RegistryState {
    pub adapters: BTreeMap<String, AdapterStatus>,
    /// Counter behind [`AdapterEvent::seq`]; advanced under the registry
    /// lock so the order events are handed to the publisher is the
    /// order subscribers observe.
    pub next_event_seq: u64,
}
