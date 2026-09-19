//! The `notify` adapter — inbound `org.freedesktop.Notifications`
//! (spec 1.2) on the session bus, made legible on the Bus as the
//! `notify` service.
//!
//! Foreign apps call Notify; the adapter stores each notification,
//! gives it its own expiry timer (no polling anywhere), projects the
//! live set as `notify.n<id>.*` props and publishes `notify.changed`
//! events. The mesh drives it back the other way: `notify.send`
//! creates a notification exactly as if Notify had been called (any
//! mesh node may post to this desktop), `notify.close` /
//! `notify.invoke` surface as NotificationClosed / ActionInvoked
//! signals. A name conflict is a hard failure, not a replacement: the
//! run errors with a clear reason, the supervisor backs off, and a
//! human hands the name over with `dbusd.adapter.disable`.
//!
//! Lifetime doctrine: the D-Bus server, the notification state and the
//! session connection SURVIVE a Bus outage. The Bus client is
//! reconnected inside the run (the `dbusd` citizen's reconnect
//! doctrine) and the props-diff baseline survives the outage, so no
//! live notification is lost and `org.freedesktop.Notifications` is
//! never released for the taking. The run ends only when the session
//! bus dies (the name cannot be served without it), the supervisor
//! stops it, or an internal fault makes progress impossible.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant, SystemTime};

#[cfg(test)]
use std::sync::atomic::{AtomicBool, AtomicUsize};

use anyhow::{Result, anyhow};
use cosmix_client::IncomingCommand;
use cosmix_props_core::publish::{build_props_changed_message, props_changed_topic};
use cosmix_props_core::{PropPath, PropTree, PropValue};
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};
use tokio::task::AbortHandle;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::OwnedValue;

use crate::adapter::{Adapter, AdapterCtx, BoxRunFuture};

pub const BUS_SERVICE: &str = "notify";
pub const DBUS_NAME: &str = "org.freedesktop.Notifications";
pub const DBUS_PATH: &str = "/org/freedesktop/Notifications";
/// The notifications spec version this server implements.
pub const SPEC_VERSION: &str = "1.2";
/// One domain event topic per notification lifecycle change.
pub const TOPIC_NOTIFY_CHANGED: &str = "notify.changed";

/// Capabilities actually implemented — nothing is claimed that this
/// server does not do: notifications are not persisted across daemon
/// restarts, so "persistence" is not advertised.
pub const CAPABILITIES: &[&str] = &["actions", "body"];

/// `expire_timeout` -1 (server default) with non-critical urgency.
pub const DEFAULT_EXPIRY: Duration = Duration::from_secs(8);
/// Live notifications before the oldest non-critical one is expired.
pub const MAX_LIVE: usize = 256;
/// Per-string cap for what is stored (and therefore lands in props and
/// event bodies); longer input is truncated, never stored whole.
pub const MAX_TEXT_BYTES: usize = 8 * 1024;
/// Action pairs kept per notification; the rest are dropped.
pub const MAX_ACTIONS: usize = 32;
/// Depth of the publisher backlog; overflow drops events (a gap in
/// `event_seq` says so — `notify.props.get` is the truth).
const EVENT_CAPACITY: usize = 64;
/// Bus publish/response budget, matching the other citizens.
const PUBLISH_TIMEOUT: Duration = Duration::from_secs(60);
/// Total stored-bytes budget across live notifications, enforced like
/// the count cap: evict the oldest (non-critical first) as expired.
pub const MAX_STORED_BYTES: usize = 16 * 1024 * 1024;
/// Any effective expiry is clamped to at least this: a notification
/// whose timer would beat the Notify reply must not close before the
/// client has even received its id.
pub const MIN_EXPIRY: Duration = Duration::from_secs(1);
/// How long the run waits before re-dialing the Bus after a Bus
/// outage (the run itself survives the outage).
const BUS_RECONNECT_DELAY: Duration = Duration::from_secs(60);
/// Budget for deregistering a superseded Bus client.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(30);

// ───────────────────────────── core types ─────────────────────────────

/// Notification urgency, from the `urgency` hint (0/1/2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Urgency {
    Low,
    #[default]
    Normal,
    Critical,
}

impl Urgency {
    pub fn from_byte(byte: u8) -> Self {
        match byte {
            0 => Self::Low,
            2 => Self::Critical,
            _ => Self::Normal,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Normal => "normal",
            Self::Critical => "critical",
        }
    }
}

/// NotificationClosed reason codes, spec §Signals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    /// 1 — the notification timed out (its own timer, or cap eviction).
    Expired,
    /// 2 — dismissed: a `notify.close`/`notify.invoke` from the mesh
    /// counts as the user's hand.
    Dismissed,
    /// 3 — a CloseNotification call.
    Requested,
    /// 4 — reserved/undefined; this server never emits it, the code
    /// exists so the mapping is total.
    #[allow(dead_code)]
    Undefined,
}

impl CloseReason {
    pub fn code(self) -> u32 {
        match self {
            Self::Expired => 1,
            Self::Dismissed => 2,
            Self::Requested => 3,
            Self::Undefined => 4,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Expired => "expired",
            Self::Dismissed => "dismissed",
            Self::Requested => "closed",
            Self::Undefined => "undefined",
        }
    }
}

/// Where a notification entered: a D-Bus Notify call, or the mesh
/// (`notify.send` — any node may post to this desktop).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Dbus,
    Mesh,
}

impl Origin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dbus => "dbus",
            Self::Mesh => "mesh",
        }
    }
}

/// One action key plus its label, as Notify carries them in pairs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Action {
    pub key: String,
    pub label: String,
}

/// The `expire_timeout` argument once normalised: -1 (or any negative)
/// is the server default, 0 is never.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpireTimeout {
    Default,
    Never,
    Millis(u32),
}

impl ExpireTimeout {
    fn from_dbus(ms: i32) -> Self {
        match ms {
            0 => Self::Never,
            n if n < 0 => Self::Default,
            n => Self::Millis(n as u32),
        }
    }
}

/// A notification as stored — every string already capped, actions
/// already trimmed, expiry already resolved to a deadline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub id: u32,
    pub app: String,
    pub icon: String,
    pub summary: String,
    pub body: String,
    pub urgency: Urgency,
    pub actions: Vec<Action>,
    /// Wall-clock expiry — display only (`n<id>.expires_at`); None =
    /// never expires.
    pub expires_at: Option<SystemTime>,
    /// Monotonic expiry deadline — the truth for due checks and
    /// timers; a wall-clock step can never delay or hasten it. None =
    /// never expires.
    pub expires_mono: Option<Instant>,
    pub created_at: SystemTime,
    /// Monotonic insertion sequence (the core's `event_seq` at the
    /// moment this record was inserted): eviction order is THIS, not
    /// the wall-clock `created_at`, which a clock step can reorder.
    pub seq: u64,
    /// The `resident` hint: action activation does not close it.
    pub resident: bool,
    /// The `transient` hint, passed through for the display side.
    pub transient: bool,
    pub desktop_entry: Option<String>,
    pub image_path: Option<String>,
    /// An `image-data`-style pixel payload was present; the pixels
    /// themselves are never stored or put in props.
    pub image_data: bool,
    pub origin: Origin,
}

/// Normalised creation arguments shared by the Notify method and the
/// `notify.send` verb, so both create notifications through exactly
/// the same path.
#[derive(Debug, Clone)]
pub struct CreateArgs {
    pub app: String,
    pub replaces_id: u32,
    pub icon: String,
    pub summary: String,
    pub body: String,
    pub actions: Vec<Action>,
    pub urgency: Urgency,
    pub resident: bool,
    pub transient: bool,
    pub desktop_entry: Option<String>,
    pub image_path: Option<String>,
    pub image_data: bool,
    pub timeout: ExpireTimeout,
    pub origin: Origin,
}

/// One published lifecycle change, stamped under the core lock with a
/// per-run monotonic `seq` (a gap means events were dropped — re-read
/// `notify.props.get`). A Created event carries the record as it was
/// created, so a notification created and closed in quick succession
/// still publishes its full body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotifyEvent {
    pub seq: u64,
    pub id: u32,
    pub kind: NotifyEventKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotifyEventKind {
    Created {
        replaced: bool,
        record: Box<Notification>,
    },
    Closed {
        reason: CloseReason,
    },
    ActionInvoked {
        action: String,
    },
}

impl NotifyEventKind {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Created { replaced: true, .. } => "notification.replaced",
            Self::Created {
                replaced: false, ..
            } => "notification.created",
            Self::Closed { .. } => "notification.closed",
            Self::ActionInvoked { .. } => "notification.action_invoked",
        }
    }
}

/// Cap a string at [`MAX_TEXT_BYTES`] on a char boundary, marking the
/// cut: input from either side of the boundary is bounded before
/// anything is stored or published. The 3-byte ellipsis is budgeted,
/// so the capped string never exceeds the cap.
pub fn cap_string(input: &str) -> String {
    const ELLIPSIS: &str = "\u{2026}";
    if input.len() <= MAX_TEXT_BYTES {
        return input.to_string();
    }
    let mut cut = MAX_TEXT_BYTES - ELLIPSIS.len();
    while !input.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}{ELLIPSIS}", &input[..cut])
}

/// Notify's flat `key,label,key,label` list → capped action pairs.
fn actions_from_dbus(flat: &[String]) -> Vec<Action> {
    flat.chunks(2)
        .take(MAX_ACTIONS)
        .filter_map(|pair| {
            Some(Action {
                key: cap_string(pair.first()?),
                label: cap_string(pair.get(1)?),
            })
        })
        .collect()
}

/// Resolve an expiry argument against the urgency into the effective
/// time-to-live: the server default is 8 s for low/normal and never
/// for critical; 0 is always never; any finite expiry is clamped to at
/// least [`MIN_EXPIRY`] (a notification must not close before the
/// client has received its id).
fn resolve_expiry(timeout: ExpireTimeout, urgency: Urgency) -> Option<Duration> {
    let ttl = match timeout {
        ExpireTimeout::Never => return None,
        ExpireTimeout::Default => match urgency {
            Urgency::Critical => return None,
            Urgency::Low | Urgency::Normal => DEFAULT_EXPIRY,
        },
        ExpireTimeout::Millis(ms) => Duration::from_millis(u64::from(ms)),
    };
    Some(ttl.max(MIN_EXPIRY))
}

// ─────────────────────────── the pure core ────────────────────────────

/// The notification set and its event counter — pure, no buses, fully
/// testable. Every mutation returns the events it produced; the wired
/// layer stamps them into the publish channel under the same lock, so
/// channel order is always seq order.
#[derive(Debug)]
pub struct NotifyCore {
    notifications: BTreeMap<u32, Notification>,
    event_seq: u64,
    cap: usize,
    max_stored_bytes: usize,
}

/// Process-wide id clock, shared by every adapter run in this process:
/// a restarted run must never hand out an id a caller may still hold —
/// its `replaces_id` / `CloseNotification` would otherwise hit an
/// unrelated notification. `0` is the "unseeded" value; the first
/// allocation seeds from the wall clock, so a daemon restart does not
/// reuse recent ids either. Ids are never 0 (`0` is Notify's
/// "no replaces_id"). Caller-supplied `replaces_id`s (live or dead) are
/// reserved against this clock with `fetch_max`, so an id one app
/// adopted can never be allocated to another — but an id the clock
/// itself never issued is only ever seen by adoption, which is the
/// caller's explicit request.
static ID_CLOCK: AtomicU32 = AtomicU32::new(0);

impl NotifyCore {
    pub fn new(cap: usize, max_stored_bytes: usize) -> Self {
        Self {
            notifications: BTreeMap::new(),
            event_seq: 0,
            cap,
            max_stored_bytes,
        }
    }

    pub fn count(&self) -> usize {
        self.notifications.len()
    }

    pub fn event_seq(&self) -> u64 {
        self.event_seq
    }

    pub fn get(&self, id: u32) -> Option<&Notification> {
        self.notifications.get(&id)
    }

    /// Live records ordered by id (the order props and list show).
    pub fn records(&self) -> BTreeMap<u32, Notification> {
        self.notifications.clone()
    }

    fn stamp(&mut self, id: u32, kind: NotifyEventKind) -> NotifyEvent {
        self.event_seq = self.event_seq.saturating_add(1);
        NotifyEvent {
            seq: self.event_seq,
            id,
            kind,
        }
    }

    /// Fresh id from the process-wide [`ID_CLOCK`]: never 0, skipping
    /// any id still live (possible only after a u32 wrap). The wrap
    /// continues from 1 (`max(1)`), never a re-seed — a wall-clock
    /// re-seed could jump back to ids handed out moments ago.
    fn allocate_id(&mut self) -> u32 {
        loop {
            let id = ID_CLOCK
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                    Some(if current == 0 {
                        id_seed()
                    } else {
                        current.wrapping_add(1).max(1)
                    })
                })
                .expect("the closure always proposes a value");
            if id != 0 && !self.notifications.contains_key(&id) {
                return id;
            }
        }
    }

    /// Apply a creation. `replaces_id` semantics per spec: a live id is
    /// reused and its record replaced (no Closed event — the id simply
    /// carries on); a non-zero id that is NOT live is created under
    /// that very id (the caller's explicit request; the adopted id is
    /// reserved against [`ID_CLOCK`] so the clock never hands it to
    /// another app, live or closed); 0 allocates a fresh one. Any
    /// insert — fresh OR replace — that would pass the live cap or the
    /// stored-bytes budget first evicts the oldest notification —
    /// non-critical first, else the oldest critical — with reason 1: no
    /// caller, mesh or D-Bus, may grow the live set without bound. A
    /// replace counts its size delta against the budget (the outgoing
    /// record is excluded from eviction — its slot is being refilled,
    /// not grown); the record being replaced is never chosen as a
    /// victim.
    pub fn create(
        &mut self,
        args: &CreateArgs,
        now: SystemTime,
        mono: Instant,
    ) -> (u32, Vec<NotifyEvent>) {
        let replacing = args.replaces_id != 0 && self.notifications.contains_key(&args.replaces_id);
        let id = if args.replaces_id != 0 {
            // Reserve the adopted id: the clock must point PAST it, or
            // the next allocation — which hands out the clock's current
            // value — would re-issue the very id just adopted. +1
            // wraps to 0 at u32::MAX (no higher value exists): a
            // MAX-adopting caller races only the full u32 wrap, 4
            // billion allocations later.
            ID_CLOCK.fetch_max(args.replaces_id.wrapping_add(1), Ordering::SeqCst);
            args.replaces_id
        } else {
            self.allocate_id()
        };
        let ttl = resolve_expiry(args.timeout, args.urgency);
        let mut record = Notification {
            id,
            app: cap_string(&args.app),
            icon: cap_string(&args.icon),
            summary: cap_string(&args.summary),
            body: cap_string(&args.body),
            urgency: args.urgency,
            actions: args
                .actions
                .iter()
                .take(MAX_ACTIONS)
                .map(|action| Action {
                    key: cap_string(&action.key),
                    label: cap_string(&action.label),
                })
                .collect(),
            expires_at: ttl.map(|ttl| now + ttl),
            expires_mono: ttl.map(|ttl| mono + ttl),
            created_at: now,
            seq: 0,
            resident: args.resident,
            transient: args.transient,
            desktop_entry: args.desktop_entry.as_deref().map(cap_string),
            image_path: args.image_path.as_deref().map(cap_string),
            image_data: args.image_data,
            origin: args.origin,
        };
        let mut events = Vec::new();
        // Budget and cap, replace or not. `replaced_bytes` is what the
        // id being re-inserted currently occupies: its slot is
        // refilled, so only the delta counts, and the eviction below
        // never picks the id itself as a victim.
        let prospective = notification_bytes(&record);
        let replaced_bytes = self.notifications.get(&id).map_or(0, notification_bytes);
        loop {
            let count_ok = replacing || self.notifications.len() < self.cap;
            let bytes_ok =
                self.stored_bytes() - replaced_bytes + prospective <= self.max_stored_bytes;
            if count_ok && bytes_ok {
                break;
            }
            let victim = self
                .notifications
                .iter()
                .filter(|(live, _)| **live != id)
                .min_by_key(|(live, record)| {
                    // Insertion order, not wall clock: a clock step must
                    // never reorder eviction.
                    (record.urgency == Urgency::Critical, record.seq, **live)
                })
                .map(|(live, _)| *live);
            let Some(victim) = victim else {
                // Only the incoming record itself is left (or the map
                // holds nothing else): insert it — a single record may
                // exceed a small budget, same as a single record at the
                // count cap of 1.
                break;
            };
            self.notifications.remove(&victim);
            events.push(self.stamp(
                victim,
                NotifyEventKind::Closed {
                    reason: CloseReason::Expired,
                },
            ));
        }
        // The insertion sequence: the seq the Created event below will
        // carry, after any eviction stamps above have moved the counter.
        record.seq = self.event_seq.saturating_add(1);
        self.notifications.insert(id, record.clone());
        events.push(self.stamp(
            id,
            NotifyEventKind::Created {
                replaced: replacing,
                record: Box::new(record),
            },
        ));
        (id, events)
    }

    /// Close by id (any reason); `None` when the id is not live.
    pub fn close(&mut self, id: u32, reason: CloseReason) -> Option<NotifyEvent> {
        self.notifications.remove(&id)?;
        Some(self.stamp(id, NotifyEventKind::Closed { reason }))
    }

    /// Close by id with reason 1, but only if the record is still live
    /// and its monotonic deadline has actually passed — a stale timer
    /// from before a replace (longer deadline) must not close anything,
    /// and a wall-clock step must never delay or hasten the close.
    pub fn expire_if_due(&mut self, id: u32, mono_now: Instant) -> Option<NotifyEvent> {
        let due = self.notifications.get(&id).is_some_and(|record| {
            record
                .expires_mono
                .is_some_and(|deadline| deadline <= mono_now)
        });
        if due {
            self.close(id, CloseReason::Expired)
        } else {
            None
        }
    }

    /// Activate an action: emits ActionInvoked, then closes with
    /// reason 2 unless the notification is resident.
    pub fn invoke(
        &mut self,
        id: u32,
        action: &str,
    ) -> Result<(Vec<NotifyEvent>, bool), &'static str> {
        let (known_action, resident) = self
            .notifications
            .get(&id)
            .map(|record| {
                (
                    record
                        .actions
                        .iter()
                        .any(|candidate| candidate.key == action),
                    record.resident,
                )
            })
            .ok_or("unknown id")?;
        if !known_action {
            return Err("unknown action");
        }
        let mut events = vec![self.stamp(
            id,
            NotifyEventKind::ActionInvoked {
                action: action.to_string(),
            },
        )];
        let closed = !resident;
        if closed {
            self.notifications.remove(&id);
            events.push(self.stamp(
                id,
                NotifyEventKind::Closed {
                    reason: CloseReason::Dismissed,
                },
            ));
        }
        Ok((events, closed))
    }

    /// Approximate stored bytes across the live set, for the budget
    /// check (an estimate: strings plus a fixed allowance per record
    /// and per action).
    fn stored_bytes(&self) -> usize {
        self.notifications.values().map(notification_bytes).sum()
    }
}

/// The per-record share of the stored-bytes budget: every stored
/// string plus the fixed parts.
fn notification_bytes(record: &Notification) -> usize {
    std::mem::size_of::<Notification>()
        + record.app.len()
        + record.icon.len()
        + record.summary.len()
        + record.body.len()
        + record
            .actions
            .iter()
            .map(|action| std::mem::size_of::<Action>() + action.key.len() + action.label.len())
            .sum::<usize>()
        + record.desktop_entry.as_deref().map_or(0, str::len)
        + record.image_path.as_deref().map_or(0, str::len)
}

/// Seed for [`ID_CLOCK`]: wall-clock milliseconds truncated to u32,
/// never 0.
fn id_seed() -> u32 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|since| since.as_millis() as u32)
        .unwrap_or(0)
        .max(1)
}

// ───────────────────────── props projection ───────────────────────────

/// Read-only `notify.props.*` projection: `count` plus one subtree per
/// live notification. A closed notification's leaves vanish from the
/// tree (the props.changed diff carries them away as nulls).
pub struct NotifyProps {
    leaves: Vec<(PropPath, PropValue)>,
}

impl NotifyProps {
    pub fn new(records: &BTreeMap<u32, Notification>) -> Self {
        let mut leaves = Vec::with_capacity(1 + records.len() * 8);
        push(&mut leaves, "count", (records.len() as u64).into());
        for (id, record) in records {
            let base = format!("n{id}");
            push(
                &mut leaves,
                &format!("{base}.app"),
                record.app.as_str().into(),
            );
            push(
                &mut leaves,
                &format!("{base}.summary"),
                record.summary.as_str().into(),
            );
            push(
                &mut leaves,
                &format!("{base}.body"),
                record.body.as_str().into(),
            );
            push(
                &mut leaves,
                &format!("{base}.icon"),
                record.icon.as_str().into(),
            );
            push(
                &mut leaves,
                &format!("{base}.urgency"),
                record.urgency.as_str().into(),
            );
            push(
                &mut leaves,
                &format!("{base}.actions"),
                PropValue::List(
                    record
                        .actions
                        .iter()
                        .map(|action| {
                            PropValue::Object(BTreeMap::from([
                                ("key".to_string(), PropValue::from(action.key.as_str())),
                                ("label".to_string(), PropValue::from(action.label.as_str())),
                            ]))
                        })
                        .collect(),
                ),
            );
            push(
                &mut leaves,
                &format!("{base}.expires_at"),
                match record.expires_at {
                    Some(at) => PropValue::from(rfc3339(at)),
                    None => PropValue::Null,
                },
            );
            push(
                &mut leaves,
                &format!("{base}.created_at"),
                rfc3339(record.created_at).as_str().into(),
            );
            push(
                &mut leaves,
                &format!("{base}.resident"),
                record.resident.into(),
            );
            push(
                &mut leaves,
                &format!("{base}.transient"),
                record.transient.into(),
            );
            push(
                &mut leaves,
                &format!("{base}.origin"),
                record.origin.as_str().into(),
            );
            if let Some(entry) = &record.desktop_entry {
                push(
                    &mut leaves,
                    &format!("{base}.desktop_entry"),
                    entry.as_str().into(),
                );
            }
            if let Some(path) = &record.image_path {
                push(
                    &mut leaves,
                    &format!("{base}.image_path"),
                    path.as_str().into(),
                );
            }
            if record.image_data {
                push(&mut leaves, &format!("{base}.image_data"), true.into());
            }
        }
        Self { leaves }
    }
}

impl PropTree for NotifyProps {
    fn snapshot(&self) -> PropValue {
        cosmix_props_core::tree::build_snapshot(self.leaves.clone())
    }

    fn list(&self) -> Vec<PropPath> {
        self.leaves.iter().map(|(path, _)| path.clone()).collect()
    }

    fn describe(&self, path: &PropPath) -> Option<cosmix_props_core::PropDescribe> {
        if !self.leaves.iter().any(|(candidate, _)| candidate == path) {
            return None;
        }
        let leaf = path.as_str().rsplit('.').next()?;
        let description = match leaf {
            "count" => cosmix_props_core::PropDescribe::leaf(
                path.clone(),
                cosmix_props_core::PropType::Number,
                "Live notifications in this adapter run.",
            ),
            "app" | "summary" | "body" | "icon" | "desktop_entry" | "image_path" => {
                cosmix_props_core::PropDescribe::leaf(
                    path.clone(),
                    cosmix_props_core::PropType::String,
                    "Notification text; input beyond the stored-string cap is truncated.",
                )
            }
            "urgency" => {
                let mut description = cosmix_props_core::PropDescribe::leaf(
                    path.clone(),
                    cosmix_props_core::PropType::String,
                    "Urgency from the urgency hint.",
                );
                description.enum_values = Some(
                    [Urgency::Low, Urgency::Normal, Urgency::Critical]
                        .iter()
                        .map(|urgency| urgency.as_str().to_string())
                        .collect(),
                );
                description
            }
            "actions" => cosmix_props_core::PropDescribe::leaf(
                path.clone(),
                cosmix_props_core::PropType::List,
                "Action pairs [{key,label}] as Notify carried them.",
            ),
            "expires_at" => cosmix_props_core::PropDescribe::leaf(
                path.clone(),
                cosmix_props_core::PropType::String,
                "RFC3339 expiry deadline; null when the notification never expires.",
            ),
            "created_at" => cosmix_props_core::PropDescribe::leaf(
                path.clone(),
                cosmix_props_core::PropType::String,
                "RFC3339 creation time (this adapter run).",
            ),
            "resident" => cosmix_props_core::PropDescribe::leaf(
                path.clone(),
                cosmix_props_core::PropType::Bool,
                "The resident hint: activating an action does not close it.",
            ),
            "transient" => cosmix_props_core::PropDescribe::leaf(
                path.clone(),
                cosmix_props_core::PropType::Bool,
                "The transient hint: passed through for the display side.",
            ),
            "image_data" => cosmix_props_core::PropDescribe::leaf(
                path.clone(),
                cosmix_props_core::PropType::Bool,
                "An image-data pixel payload was present; the pixels are not stored.",
            ),
            "origin" => {
                let mut description = cosmix_props_core::PropDescribe::leaf(
                    path.clone(),
                    cosmix_props_core::PropType::String,
                    "Where it entered: a D-Bus Notify call or the mesh (notify.send).",
                );
                description.enum_values = Some(
                    [Origin::Dbus, Origin::Mesh]
                        .iter()
                        .map(|origin| origin.as_str().to_string())
                        .collect(),
                );
                description
            }
            _ => return None,
        };
        Some(description)
    }
}

fn push(leaves: &mut Vec<(PropPath, PropValue)>, path: &str, value: PropValue) {
    if let Ok(path) = PropPath::new(path) {
        leaves.push((path, value));
    }
}

fn rfc3339(moment: SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(moment)
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

// ──────────────────────── the wired shared state ──────────────────────

/// The core plus its publish channel and a handle to the signal
/// emitter, shared by the D-Bus interface and the Bus verb dispatch.
/// The emitter is deliberately WEAK: it owns the zbus connection, and
/// the connection's object server owns the interface, which owns this
/// shared state — a strong emitter here would be a cycle that keeps
/// the connection (and its bus names) alive after the run ends. The
/// server (owned by the run) holds the strong emitter; once it drops,
/// a late emission finds nothing and logs.
#[derive(Debug)]
pub(crate) struct NotifyShared {
    core: Mutex<NotifyCore>,
    events: mpsc::Sender<NotifyEvent>,
    emitter: Weak<SignalEmitter<'static>>,
    /// One live expiry timer per expiring notification, by id. A timer
    /// is aborted the moment its notification is replaced, closed or
    /// evicted, and every timer dies with the run — replace spam can
    /// never accumulate detached sleepers. Every arm/disarm happens
    /// under the CORE lock (never the timers lock alone): arm order
    /// then always matches record-mutation order, so a concurrent
    /// create/replace on one id can never leave the live record with a
    /// timer for the wrong deadline — or none at all.
    timers: Mutex<HashMap<u32, AbortHandle>>,
    /// Test hook: panic the publisher loop on its next event, to prove
    /// the run ends when the publisher task dies.
    #[cfg(test)]
    panic_publisher: AtomicBool,
    /// Test visibility: how many timer tasks this server started have
    /// finished (aborted superseds included) — proves superseded timers
    /// really are cancelled, not just overwritten in the map.
    #[cfg(test)]
    timers_finished: Arc<AtomicUsize>,
}

/// Test hooks at module scope: a yield before every NotificationClosed
/// emission (an expiry task that aborts itself dies exactly there,
/// deterministically, instead of racing on the socket's first Pending).
#[cfg(test)]
static YIELD_BEFORE_EMIT: AtomicBool = AtomicBool::new(false);

impl NotifyShared {
    fn new(
        cap: usize,
        max_stored_bytes: usize,
        emitter: Weak<SignalEmitter<'static>>,
        events: mpsc::Sender<NotifyEvent>,
    ) -> Self {
        Self {
            core: Mutex::new(NotifyCore::new(cap, max_stored_bytes)),
            events,
            emitter,
            timers: Mutex::new(HashMap::new()),
            #[cfg(test)]
            panic_publisher: AtomicBool::new(false),
            #[cfg(test)]
            timers_finished: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn lock(&self) -> MutexGuard<'_, NotifyCore> {
        self.core
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn lock_timers(&self) -> MutexGuard<'_, HashMap<u32, AbortHandle>> {
        self.timers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Abort and forget the timer for `id`, if any (the notification
    /// was closed, evicted, or replaced by a never-expiring record).
    /// Must be called with the CORE lock held — see [`Self::timers`].
    fn disarm_timer(&self, id: u32) {
        if let Some(handle) = self.lock_timers().remove(&id) {
            handle.abort();
        }
    }

    /// Remove the timer entry for `id` WITHOUT aborting it. This is the
    /// expiry path's own disarm: the task registered under `id` is the
    /// one running, and aborting it would cancel the very
    /// NotificationClosed emission it is about to make. The entry must
    /// still go — under the same core lock as the close, so a
    /// concurrent create adopting `id` arms its timer fresh instead of
    /// aborting this task as "superseded".
    fn take_timer(&self, id: u32) {
        self.lock_timers().remove(&id);
    }

    /// Abort every live timer: the run is ending.
    fn abort_timers(&self) {
        for (_, handle) in self.lock_timers().drain() {
            handle.abort();
        }
    }

    /// Live expiry timers (test visibility for the cancellation
    /// contract).
    #[cfg(test)]
    fn timer_count(&self) -> usize {
        self.lock_timers().len()
    }

    /// Test visibility: timer tasks started by this server that have
    /// finished (aborted or run to completion).
    #[cfg(test)]
    fn timers_finished(&self) -> usize {
        self.timers_finished.load(Ordering::SeqCst)
    }

    /// Stamp-ordered enqueue: called with the lock held, so two
    /// concurrent creates can never deliver seq 6 before seq 5.
    fn enqueue(&self, events: &[NotifyEvent]) {
        for event in events {
            if self.events.try_send(event.clone()).is_err() {
                eprintln!(
                    "cosmix-dbusd notify: event dropped (backlog full): {} id {} (event_seq {})",
                    event.kind.name(),
                    event.id,
                    event.seq
                );
            }
        }
    }

    /// The (count, event_seq) the info/watch verbs surface.
    pub fn status(&self) -> (usize, u64) {
        let core = self.lock();
        (core.count(), core.event_seq())
    }

    pub fn records(&self) -> BTreeMap<u32, Notification> {
        self.lock().records()
    }

    pub fn props(&self) -> NotifyProps {
        NotifyProps::new(&self.records())
    }

    /// Create a notification (the Notify method or `notify.send`):
    /// apply under the lock, arm/disarm the per-notification timer under
    /// that SAME lock (see [`Self::timers`]), disarm evicted
    /// predecessors under it too, then emit NotificationClosed for the
    /// evictions outside the lock. Returns the id — exactly what Notify
    /// replies with.
    pub async fn create(shared: &Arc<Self>, args: CreateArgs) -> u32 {
        let (id, events) = {
            let mut core = shared.lock();
            let (id, events) = core.create(&args, SystemTime::now(), Instant::now());
            match core.get(id).and_then(|record| record.expires_mono) {
                Some(deadline) => arm_timer(shared, id, deadline),
                None => shared.disarm_timer(id),
            }
            for event in &events {
                if matches!(event.kind, NotifyEventKind::Closed { .. }) {
                    shared.disarm_timer(event.id);
                }
            }
            shared.enqueue(&events);
            (id, events)
        };
        for event in &events {
            if let NotifyEventKind::Closed { reason } = event.kind {
                shared.emit_closed(event.id, reason).await;
            }
        }
        id
    }

    /// CloseNotification (reason 3), `notify.close` (reason 2) and the
    /// expiry timer (reason 1) all land here. Returns false for an
    /// unknown id — the D-Bus method replies with an error (the caller
    /// may be acting on stale state), the verb refuses.
    pub async fn close(&self, id: u32, reason: CloseReason) -> bool {
        let closed = {
            let mut core = self.lock();
            let event = core.close(id, reason);
            if let Some(event) = &event {
                self.enqueue(std::slice::from_ref(event));
                self.disarm_timer(id);
            }
            event.is_some()
        };
        if closed {
            self.emit_closed(id, reason).await;
        }
        closed
    }

    /// `notify.invoke`: ActionInvoked, then close (reason 2) unless
    /// the resident hint says otherwise. Errors are the verb's
    /// refusals.
    pub async fn invoke(&self, id: u32, action: &str) -> Result<bool, &'static str> {
        let (events, closed) = {
            let mut core = self.lock();
            let (events, closed) = core.invoke(id, action)?;
            self.enqueue(&events);
            if closed {
                self.disarm_timer(id);
            }
            (events, closed)
        };
        for event in &events {
            match &event.kind {
                NotifyEventKind::ActionInvoked { action } => {
                    self.emit_invoked(id, action).await;
                }
                NotifyEventKind::Closed { reason } => {
                    self.emit_closed(id, *reason).await;
                }
                NotifyEventKind::Created { .. } => {}
            }
        }
        Ok(closed)
    }

    /// The expiry timer's callback: close with reason 1 only if still
    /// due (a replace may have pushed the deadline out, or the record
    /// is already gone). The disarm is [`Self::take_timer`], NOT an
    /// abort: the task running this IS the timer being removed, and
    /// aborting it would silently drop the NotificationClosed signal
    /// about to be emitted.
    pub async fn expire(&self, id: u32) {
        let closed = {
            let mut core = self.lock();
            let event = core.expire_if_due(id, Instant::now());
            if let Some(event) = &event {
                self.enqueue(std::slice::from_ref(event));
                self.take_timer(id);
            }
            event.is_some()
        };
        if closed {
            self.emit_closed(id, CloseReason::Expired).await;
        }
    }

    async fn emit_closed(&self, id: u32, reason: CloseReason) {
        // Test hook: one yield before the emission — an expiry task
        // that disarm-aborts itself dies exactly here, deterministically.
        #[cfg(test)]
        if YIELD_BEFORE_EMIT.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        let Some(emitter) = self.emitter.upgrade() else {
            eprintln!(
                "cosmix-dbusd notify: NotificationClosed({id}, {}) dropped — adapter shutting down",
                reason.code()
            );
            return;
        };
        if let Err(error) = emitter.notification_closed(id, reason.code()).await {
            eprintln!("cosmix-dbusd notify: NotificationClosed emission failed: {error}");
        }
    }

    async fn emit_invoked(&self, id: u32, action: &str) {
        let Some(emitter) = self.emitter.upgrade() else {
            eprintln!(
                "cosmix-dbusd notify: ActionInvoked({id}, {action}) dropped — adapter shutting down"
            );
            return;
        };
        if let Err(error) = emitter.action_invoked(id, action).await {
            eprintln!("cosmix-dbusd notify: ActionInvoked emission failed: {error}");
        }
    }
}

/// Arm (or re-arm, on replace) the expiry timer for `id`: sleep until
/// the monotonic deadline on tokio's clock, then close with reason 1
/// only if still due. No polling loop anywhere. The handle is kept so
/// a replace, close, eviction or the run's end can abort it; any timer
/// it supersedes is aborted here and gone for good. The timer holds
/// only a weak pointer: it must never keep the adapter alive, and a
/// dropped adapter's late fire finds nothing to close. Called with the
/// CORE lock held — see [`NotifyShared::timers`].
fn arm_timer(shared: &Arc<NotifyShared>, id: u32, deadline: Instant) {
    let weak: Weak<NotifyShared> = Arc::downgrade(shared);
    #[cfg(test)]
    let finished = Arc::clone(&shared.timers_finished);
    let handle = tokio::spawn(async move {
        // Dropped when this task finishes for ANY reason — aborted
        // mid-sleep (a superseded timer) or run to completion — so a
        // test can see that superseded timers really were cancelled.
        #[cfg(test)]
        let _done = TimerDone(finished);
        tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
        if let Some(shared) = weak.upgrade() {
            shared.expire(id).await;
        }
    });
    let mut timers = shared.lock_timers();
    if let Some(superseded) = timers.insert(id, handle.abort_handle()) {
        superseded.abort();
    }
}

/// Drop guard for [`NotifyShared::timers_finished`] (test visibility
/// only).
#[cfg(test)]
struct TimerDone(Arc<AtomicUsize>);

#[cfg(test)]
impl Drop for TimerDone {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

// ─────────────────────── the D-Bus interface ──────────────────────────

/// The `org.freedesktop.Notifications` server. Method bodies stay
/// thin: parse, hand to [`NotifyShared`], reply.
struct Notifications {
    shared: Arc<NotifyShared>,
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl Notifications {
    #[allow(clippy::too_many_arguments)] // the signature is fixed by spec
    async fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: Vec<String>,
        hints: HashMap<String, OwnedValue>,
        expire_timeout: i32,
    ) -> u32 {
        NotifyShared::create(
            &self.shared,
            CreateArgs::from_dbus(
                app_name,
                replaces_id,
                app_icon,
                summary,
                body,
                &actions,
                &hints,
                expire_timeout,
            ),
        )
        .await
    }

    /// Spec 1.2: an unknown id is an error reply — the caller may be
    /// acting on stale state and deserves to know — never a silent
    /// success and never a signal.
    async fn close_notification(&self, id: u32) -> Result<(), zbus::fdo::Error> {
        if self.shared.close(id, CloseReason::Requested).await {
            Ok(())
        } else {
            Err(zbus::fdo::Error::UnknownObject(format!(
                "no notification {id}"
            )))
        }
    }

    fn get_capabilities(&self) -> Vec<String> {
        CAPABILITIES
            .iter()
            .map(|capability| capability.to_string())
            .collect()
    }

    fn get_server_information(&self) -> (String, String, String, String) {
        let build = cosmix_buildinfo::build_info!();
        (
            "cosmix".into(),
            "cosmix".into(),
            build.version.to_string(),
            SPEC_VERSION.to_string(),
        )
    }

    #[zbus(signal)]
    async fn notification_closed(
        emitter: &SignalEmitter<'_>,
        id: u32,
        reason: u32,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn action_invoked(
        emitter: &SignalEmitter<'_>,
        id: u32,
        action_key: &str,
    ) -> zbus::Result<()>;
}

impl CreateArgs {
    #[allow(clippy::too_many_arguments)] // the signature is fixed by spec
    fn from_dbus(
        app: &str,
        replaces_id: u32,
        icon: &str,
        summary: &str,
        body: &str,
        actions: &[String],
        hints: &HashMap<String, OwnedValue>,
        expire_timeout: i32,
    ) -> Self {
        let urgency = hints
            .get("urgency")
            .and_then(urgency_from_hint)
            .map_or(Urgency::Normal, Urgency::from_byte);
        Self {
            app: app.to_string(),
            replaces_id,
            icon: icon.to_string(),
            summary: summary.to_string(),
            body: body.to_string(),
            actions: actions_from_dbus(actions),
            urgency,
            resident: hint_bool(hints, &["resident"]),
            transient: hint_bool(hints, &["transient"]),
            desktop_entry: hint_string(hints, &["desktop-entry"]),
            image_path: hint_string(hints, &["image-path", "image_path"]),
            image_data: hints.keys().any(|key| {
                matches!(
                    key.as_str(),
                    "image-data" | "image_data" | "icon_data" | "icon-data"
                )
            }),
            timeout: ExpireTimeout::from_dbus(expire_timeout),
            origin: Origin::Dbus,
        }
    }
}

fn hint_bool(hints: &HashMap<String, OwnedValue>, keys: &[&str]) -> bool {
    keys.iter()
        .find_map(|key| hints.get(*key))
        .and_then(|value| bool::try_from(value.clone()).ok())
        .unwrap_or(false)
}

/// The spec says the `urgency` hint is a byte, but non-conforming
/// clients send it as an int32 or uint32 — accept all three widths.
fn urgency_from_hint(value: &OwnedValue) -> Option<u8> {
    if let Ok(byte) = u8::try_from(value.clone()) {
        return Some(byte);
    }
    if let Ok(wide) = i32::try_from(value.clone()) {
        return u8::try_from(wide).ok();
    }
    if let Ok(wide) = u32::try_from(value.clone()) {
        return u8::try_from(wide).ok();
    }
    None
}

fn hint_string(hints: &HashMap<String, OwnedValue>, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| hints.get(*key))
        .and_then(|value| String::try_from(value.clone()).ok())
}

// ─────────────────── the Bus client abstraction ───────────────────────

/// The Bus operations the run needs, as a trait so the reconnect and
/// reply behaviour is testable without a live broker. `NodedClient` is
/// the production implementation; tests inject a fake.
trait BusClient: Send + Sync {
    /// Publish one event on a topic via the broker.
    fn publish_event(
        &self,
        topic: &str,
        message: cosmix_bus::bus::BusMessage,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>>;

    /// Reply to one incoming command.
    fn respond<'a>(
        &'a self,
        command: &'a IncomingCommand,
        rc: u8,
        body: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;
}

impl BusClient for cosmix_client::NodedClient {
    fn publish_event(
        &self,
        topic: &str,
        message: cosmix_bus::bus::BusMessage,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
        let topic = topic.to_string();
        Box::pin(async move {
            let headers = BTreeMap::from([
                ("name".to_string(), topic),
                ("retain".to_string(), "false".to_string()),
            ]);
            let wire = message.to_wire();
            self.send_with_headers("noded", "topic.publish", &headers, &wire)
                .await
        })
    }

    fn respond<'a>(
        &'a self,
        command: &'a IncomingCommand,
        rc: u8,
        body: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move { self.respond(command, rc, body).await })
    }
}

/// One live Bus connection as the run's Bus side: the client (publish
/// plus reply), the incoming command stream, and an optional close that
/// deregisters the service before the next re-dial.
struct BusSession {
    client: Arc<dyn BusClient>,
    incoming: mpsc::UnboundedReceiver<IncomingCommand>,
    close: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
}

/// The run's current Bus client, stamped with the session generation it
/// belongs to. The publisher reports this generation when a publish
/// faults, so a fault left over from a superseded session is recognised
/// and ignored instead of tearing down the healthy one serving now.
#[derive(Clone)]
struct BusClientSlot {
    generation: u64,
    client: Arc<dyn BusClient>,
}

/// How the run obtains Bus sessions — reconnectable by design. The
/// production connector dials through the adapter context; tests
/// script it.
trait BusConnector: Send {
    fn connect(&mut self) -> Pin<Box<dyn Future<Output = Result<BusSession>> + Send + '_>>;
}

/// The production connector: the adapter context's registered Bus
/// connection, fresh per (re)connect.
struct CtxBusConnector<'a> {
    ctx: &'a AdapterCtx,
}

impl BusConnector for CtxBusConnector<'_> {
    fn connect(&mut self) -> Pin<Box<dyn Future<Output = Result<BusSession>> + Send + '_>> {
        Box::pin(async move {
            let client = Arc::new(self.ctx.connect_bus().await?);
            let incoming = client
                .incoming_async()
                .await
                .ok_or_else(|| anyhow!("notify: incoming Bus channel already taken"))?;
            let closer = Arc::clone(&client);
            Ok(BusSession {
                client,
                incoming,
                close: Some(Box::pin(async move {
                    if tokio::time::timeout(CLOSE_TIMEOUT, closer.close())
                        .await
                        .is_err()
                    {
                        eprintln!("cosmix-dbusd notify: Bus client close timed out");
                    }
                })),
            })
        })
    }
}

// ─────────────────── the Bus publisher (events out) ───────────────────

/// Consume notification events and publish them on whichever Bus
/// client the run currently holds (`clients` is `None` while the Bus
/// side is down: the publisher parks — events buffer up to the channel
/// capacity, overflow drops with a log): `notify.props.changed` diffs
/// against the surviving baseline (the `dbusd` citizen's doctrine: a
/// re-sent diff is idempotent, a dropped one is a silent gap) plus one
/// `notify.changed` event per change. The baseline starts as the EMPTY
/// tree, so the run's very first change publishes its diff too. Events
/// stamped under one core lock arrive as a batch; the batch's
/// coalesced diff is stamped with the LAST `event_seq` it covers. A
/// publish failure hands the run a fault stamped with the client's
/// session GENERATION (the run reconnects that Bus client) rather than
/// losing the baseline.
async fn run_publisher(
    shared: Weak<NotifyShared>,
    mut events: mpsc::Receiver<NotifyEvent>,
    mut clients: watch::Receiver<Option<BusClientSlot>>,
    faults: mpsc::Sender<u64>,
) -> Result<()> {
    let mut baseline = NotifyProps::new(&BTreeMap::new()).snapshot();
    loop {
        let Some(mut event) = events.recv().await else {
            return Err(anyhow!("notify: event stream ended"));
        };
        let Some(shared) = shared.upgrade() else {
            return Err(anyhow!("notify: adapter state dropped"));
        };
        // Test hook: prove the run ends when the publisher task dies.
        #[cfg(test)]
        if shared.panic_publisher.load(Ordering::SeqCst) {
            panic!("scripted publisher panic (test hook)");
        }
        // Drain the rest of the batch (every event stamped under the
        // same core lock) so the diff covers the batch's whole effect.
        let mut batch = Vec::new();
        loop {
            batch.push(event);
            event = match events.try_recv() {
                Ok(next) => next,
                Err(_) => break,
            };
        }
        let last_seq = batch.last().expect("a batch has one event").seq;
        let slot = match wait_for_client(&mut clients).await {
            Ok(slot) => slot,
            Err(error) => return Err(error),
        };
        let snapshot = shared.props().snapshot();
        let mut failed = false;
        for (path, old_value, new_value) in cosmix_props_core::diff(&baseline, &snapshot) {
            let mut message =
                build_props_changed_message(&path, &old_value, &new_value, "notify.event");
            message.set("event_seq", &last_seq.to_string());
            if publish_with_timeout(&*slot.client, &props_changed_topic(BUS_SERVICE), message)
                .await
                .is_err()
            {
                failed = true;
                break;
            }
        }
        if !failed {
            for event in &batch {
                let mut message = cosmix_bus::bus::BusMessage::new();
                message.set("command", event.kind.name());
                message.set("event_seq", &event.seq.to_string());
                message.body = event_body(event).to_string();
                if publish_with_timeout(&*slot.client, TOPIC_NOTIFY_CHANGED, message)
                    .await
                    .is_err()
                {
                    failed = true;
                    break;
                }
            }
        }
        if failed {
            eprintln!(
                "cosmix-dbusd notify: event publish failed; faulting the Bus client (generation {})",
                slot.generation
            );
            let _ = faults.try_send(slot.generation);
            // The baseline survives the failure AND the reconnect it
            // triggers: a subscriber that stayed connected through the
            // outage has seen exactly up to the baseline and needs the
            // accumulated diff on the next event.
            continue;
        }
        baseline = snapshot;
    }
}

/// Publish one message under the Bus budget: a wedged publish faults
/// the client instead of parking the publisher forever.
async fn publish_with_timeout(
    client: &dyn BusClient,
    topic: &str,
    message: cosmix_bus::bus::BusMessage,
) -> Result<()> {
    match tokio::time::timeout(PUBLISH_TIMEOUT, client.publish_event(topic, message)).await {
        Ok(outcome) => outcome,
        Err(_) => Err(anyhow!("publish timed out after {PUBLISH_TIMEOUT:?}")),
    }
}

/// The publisher's view of the run's Bus client: park while the run is
/// between Bus connections (initial dial or reconnect).
async fn wait_for_client(
    clients: &mut watch::Receiver<Option<BusClientSlot>>,
) -> Result<BusClientSlot> {
    loop {
        if let Some(slot) = clients.borrow_and_update().clone() {
            return Ok(slot);
        }
        // No Bus connection right now: park until one appears. Events
        // buffer in the bounded channel meanwhile; overflow is dropped
        // at the source with a log, and props.get is the bootstrap.
        if clients.changed().await.is_err() {
            return Err(anyhow!("notify: publisher client channel ended"));
        }
    }
}

fn event_body(event: &NotifyEvent) -> Value {
    let data = match &event.kind {
        NotifyEventKind::Created { replaced, record } => json!({
            "replaced": replaced,
            "notification": notification_json(record),
        }),
        NotifyEventKind::Closed { reason } => json!({
            "reason": reason.code(),
            "reason_name": reason.as_str(),
        }),
        NotifyEventKind::ActionInvoked { action } => json!({"action": action}),
    };
    json!({
        "event": event.kind.name(),
        "event_seq": event.seq,
        "id": event.id,
        "data": data,
    })
}

fn notification_json(record: &Notification) -> Value {
    json!({
        "id": record.id,
        "app": record.app,
        "summary": record.summary,
        "body": record.body,
        "icon": record.icon,
        "urgency": record.urgency.as_str(),
        "actions": record.actions.iter().map(|action| json!({
            "key": action.key, "label": action.label,
        })).collect::<Vec<_>>(),
        "expires_at": record.expires_at.map(rfc3339),
        "created_at": rfc3339(record.created_at),
        "resident": record.resident,
        "transient": record.transient,
        "desktop_entry": record.desktop_entry,
        "image_path": record.image_path,
        "image_data": record.image_data,
        "origin": record.origin.as_str(),
    })
}

// ──────────────────── the Bus verb dispatch (in) ──────────────────────

/// One incoming `notify.*` command. Mesh-open per LAW 2026-09-15: no
/// caller authorization on any verb — including `notify.send`, which
/// is the point (any mesh node may post to this desktop). Unknown
/// verbs, unknown ids and bad arguments are refusals (rc 10), never
/// panics.
pub(crate) async fn dispatch(
    shared: &Arc<NotifyShared>,
    command: &IncomingCommand,
) -> (u8, String) {
    if let Some(suffix) = command.command.strip_prefix("notify.props.") {
        if suffix == "watch" {
            let (_, event_seq) = shared.status();
            return (
                0,
                json!({
                    "topic": props_changed_topic(BUS_SERVICE),
                    "domain_topics": [TOPIC_NOTIFY_CHANGED],
                    "event_seq": event_seq,
                    "event_sequence": "per-adapter-run monotonic event_seq on every event; \
                                       a gap means events were dropped — re-read notify.props.get",
                    "bootstrap": "subscribe on this connection, then read notify.props.get",
                })
                .to_string(),
            );
        }
        let props = shared.props();
        let args = resolve_args(command);
        let response = cosmix_props_core::bus::dispatch_props(&props, suffix, args.as_ref(), true);
        return (response.rc.clamp(0, 255) as u8, response.body);
    }

    let args = resolve_args(command);
    match command.command.as_str() {
        "notify.ping" => (
            0,
            json!({"pong": true, "service": BUS_SERVICE, "schema": "notify.v1"}).to_string(),
        ),
        "notify.info" => {
            let (count, event_seq) = shared.status();
            let build = cosmix_buildinfo::build_info!();
            (
                0,
                json!({
                    "name": BUS_SERVICE,
                    "schema": "notify.v1",
                    "props_level": "L2",
                    "binary": build.pkg,
                    "version": build.version,
                    "git_sha": build.git_sha,
                    "git_dirty": build.git_dirty,
                    "build_time": build.build_time,
                    "count": count,
                    "event_seq": event_seq,
                    "dbus": {
                        "name": DBUS_NAME,
                        "path": DBUS_PATH,
                        "spec": SPEC_VERSION,
                        "capabilities": CAPABILITIES,
                    },
                })
                .to_string(),
            )
        }
        "notify.list" => {
            let records = shared.records();
            (
                0,
                json!({
                    "notifications": records.values().map(|record| json!({
                        "id": record.id,
                        "app": record.app,
                        "summary": record.summary,
                        "urgency": record.urgency.as_str(),
                        "origin": record.origin.as_str(),
                    })).collect::<Vec<_>>(),
                    "count": records.len(),
                })
                .to_string(),
            )
        }
        "notify.close" => {
            let Some(id) = arg_id(args.as_ref()) else {
                return refusal("notify.close requires a numeric args.id");
            };
            if shared.close(id, CloseReason::Dismissed).await {
                (
                    0,
                    json!({
                        "ok": true,
                        "id": id,
                        "reason": CloseReason::Dismissed.code(),
                    })
                    .to_string(),
                )
            } else {
                refusal(&format!("unknown id: {id}"))
            }
        }
        "notify.invoke" => {
            let Some(id) = arg_id(args.as_ref()) else {
                return refusal("notify.invoke requires a numeric args.id");
            };
            let Some(action) = args
                .as_ref()
                .and_then(|args| args.get("action"))
                .and_then(Value::as_str)
                .filter(|action| !action.is_empty())
                .map(str::to_string)
            else {
                return refusal("notify.invoke requires args.action");
            };
            match shared.invoke(id, &action).await {
                Ok(closed) => (
                    0,
                    json!({"ok": true, "id": id, "action": action, "closed": closed}).to_string(),
                ),
                Err(reason) => refusal(&format!("{reason}: id {id}")),
            }
        }
        "notify.send" => match parse_send_args(args.as_ref()) {
            Ok(create) => {
                let id = NotifyShared::create(shared, create).await;
                (0, json!({"ok": true, "id": id}).to_string())
            }
            Err(error) => refusal(&error),
        },
        _ => refusal(&format!("unknown notify verb: {}", command.command)),
    }
}

fn refusal(message: &str) -> (u8, String) {
    (10, json!({"error": message}).to_string())
}

fn arg_id(args: Option<&Value>) -> Option<u32> {
    args.and_then(|args| args.get("id")).and_then(|id| {
        id.as_u64()
            .filter(|id| *id <= u32::MAX as u64)
            .map(|id| id as u32)
    })
}

/// `notify.send` arguments → [`CreateArgs`]. `summary` is required;
/// everything else has a default or is optional: `body`, `app`,
/// `icon`, `urgency` (name or 0/1/2), `timeout` (ms; negative or
/// "default" = server default, 0 or "never" = never), `actions`
/// ([["key","label"],…] or [{key,label},…] or Notify's flat
/// ["key","label",…]), `resident`, `transient`, `desktop_entry`,
/// `image_path`.
fn parse_send_args(args: Option<&Value>) -> Result<CreateArgs, String> {
    let args = args.ok_or("notify.send requires arguments")?;
    let summary = args
        .get("summary")
        .and_then(Value::as_str)
        .filter(|summary| !summary.is_empty())
        .ok_or("notify.send requires a non-empty args.summary")?
        .to_string();
    Ok(CreateArgs {
        app: args
            .get("app")
            .and_then(Value::as_str)
            .unwrap_or("cosmix")
            .to_string(),
        replaces_id: 0,
        icon: args
            .get("icon")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        summary,
        body: args
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        actions: parse_actions(args.get("actions"))?,
        urgency: parse_urgency(args.get("urgency"))?,
        resident: args
            .get("resident")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        transient: args
            .get("transient")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        desktop_entry: string_field(args, &["desktop_entry", "desktop-entry"]),
        image_path: string_field(args, &["image_path", "image-path"]),
        image_data: false,
        timeout: parse_timeout(args.get("timeout"))?,
        origin: Origin::Mesh,
    })
}

fn string_field(args: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| args.get(*key))
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|value| !value.is_empty())
}

fn parse_urgency(value: Option<&Value>) -> Result<Urgency, String> {
    match value {
        None | Some(Value::Null) => Ok(Urgency::Normal),
        Some(Value::String(name)) => match name.as_str() {
            "low" => Ok(Urgency::Low),
            "normal" => Ok(Urgency::Normal),
            "critical" => Ok(Urgency::Critical),
            _ => Err(format!("unknown urgency: {name} (low, normal, critical)")),
        },
        Some(Value::Number(number)) => {
            let byte = number
                .as_u64()
                .filter(|byte| *byte <= 2)
                .ok_or("urgency must be 0, 1 or 2")? as u8;
            Ok(Urgency::from_byte(byte))
        }
        Some(_) => Err("urgency must be a name or 0/1/2".into()),
    }
}

fn parse_timeout(value: Option<&Value>) -> Result<ExpireTimeout, String> {
    match value {
        None | Some(Value::Null) => Ok(ExpireTimeout::Default),
        Some(Value::String(word)) => match word.as_str() {
            "default" => Ok(ExpireTimeout::Default),
            "never" => Ok(ExpireTimeout::Never),
            _ => Err(format!(
                "unknown timeout: {word} (a number of ms, \"default\" or \"never\")"
            )),
        },
        Some(Value::Number(number)) => {
            let ms = number
                .as_i64()
                .ok_or("timeout must be an integer number of milliseconds")?;
            let ms = i32::try_from(ms).map_err(|_| format!("timeout out of range: {ms}"))?;
            Ok(ExpireTimeout::from_dbus(ms))
        }
        Some(_) => Err("timeout must be a number of ms, \"default\" or \"never\"".into()),
    }
}

fn parse_actions(value: Option<&Value>) -> Result<Vec<Action>, String> {
    let Some(Value::Array(entries)) = value else {
        return Ok(Vec::new());
    };
    // Notify's wire form is a flat key,label,key,label list; JSON
    // callers more naturally send pairs or objects. All-strings means
    // flat pairs (an odd count is malformed); otherwise per-entry.
    if entries.iter().all(Value::is_string) {
        let flat = entries
            .iter()
            .map(|entry| entry.as_str().expect("checked above"))
            .collect::<Vec<_>>();
        if flat.len() % 2 != 0 {
            return Err("flat actions must be key,label pairs (odd number of strings)".into());
        }
        return Ok(flat
            .chunks(2)
            .map(|pair| Action {
                key: pair[0].to_string(),
                label: pair[1].to_string(),
            })
            .collect());
    }
    let mut actions = Vec::with_capacity(entries.len());
    for entry in entries {
        let action = match entry {
            Value::Array(pair) if pair.len() == 2 => Action {
                key: pair[0]
                    .as_str()
                    .ok_or("action pair entries must be strings")?
                    .to_string(),
                label: pair[1]
                    .as_str()
                    .ok_or("action pair entries must be strings")?
                    .to_string(),
            },
            Value::Object(object) => Action {
                key: object
                    .get("key")
                    .and_then(Value::as_str)
                    .ok_or("action object requires key")?
                    .to_string(),
                label: object
                    .get("label")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            },
            _ => {
                return Err(
                    "actions must be [key,label] pairs, {key,label} objects, or a flat \
                     key,label string list"
                        .into(),
                );
            }
        };
        if action.key.is_empty() {
            return Err("action keys must be non-empty".into());
        }
        actions.push(action);
    }
    Ok(actions)
}

fn resolve_args(command: &IncomingCommand) -> Option<Value> {
    if let Some(args) = command.header("args")
        && let Ok(value) = serde_json::from_str(args)
    {
        return Some(value);
    }
    if !command.args.is_null() {
        return Some(command.args.clone());
    }
    if !command.body.is_empty()
        && let Ok(value) = serde_json::from_str(&command.body)
    {
        return Some(value);
    }
    None
}

// ──────────────────── server assembly and the adapter ─────────────────

/// A started notify server: the shared state, the strong signal
/// emitter and the publisher task. The run watches the publisher's
/// JoinHandle (a panic or early exit ends the run with an error);
/// dropping the server (or ending the run) aborts it and drops the
/// last strong reference to the emitter — and with it the zbus
/// connection's last reason to stay alive — so the interface, the bus
/// name and every expiry timer go too.
#[derive(Debug)]
pub(crate) struct NotifyServer {
    pub shared: Arc<NotifyShared>,
    /// The strong emitter reference — held for liveness only (it owns
    /// the zbus connection; the shared state reaches it weakly), which
    /// is exactly why nothing reads it.
    _emitter: Arc<SignalEmitter<'static>>,
    publisher: tokio::task::JoinHandle<Result<()>>,
}

impl Drop for NotifyServer {
    fn drop(&mut self) {
        self.publisher.abort();
        self.shared.abort_timers();
    }
}

/// Build the shared state, serve the interface at [`DBUS_PATH`] and
/// claim [`DBUS_NAME`]. The claim never replaces an existing owner:
/// `DoNotQueue` alone, so a name someone else owns fails here with a
/// clear error and the supervisor backs off — a human uses
/// `dbusd.adapter.disable`/`enable` to hand it over. The publisher
/// publishes through whichever Bus client `clients` currently carries
/// (`None` while the Bus side is down).
async fn start_server(
    connection: &zbus::Connection,
    clients: watch::Receiver<Option<BusClientSlot>>,
    cap: usize,
    max_stored_bytes: usize,
) -> Result<(NotifyServer, mpsc::Receiver<u64>)> {
    let emitter = Arc::new(SignalEmitter::new(connection, DBUS_PATH)?);
    let (events_tx, events_rx) = mpsc::channel(EVENT_CAPACITY);
    let shared = Arc::new(NotifyShared::new(
        cap,
        max_stored_bytes,
        Arc::downgrade(&emitter),
        events_tx,
    ));
    connection
        .object_server()
        .at(
            DBUS_PATH,
            Notifications {
                shared: Arc::clone(&shared),
            },
        )
        .await?;
    match connection
        .request_name_with_flags(DBUS_NAME, zbus::fdo::RequestNameFlags::DoNotQueue.into())
        .await
    {
        Ok(_) => {}
        Err(zbus::Error::NameTaken) => {
            return Err(anyhow!(
                "{DBUS_NAME} is already owned by another connection; not replacing it — \
                 stop the other owner, or dbusd.adapter.disable this adapter until the \
                 name is free"
            ));
        }
        Err(error) => {
            return Err(anyhow!("cannot claim {DBUS_NAME}: {error}"));
        }
    }
    let (fault_tx, fault_rx) = mpsc::channel::<u64>(8);
    let publisher_task = tokio::spawn(run_publisher(
        Arc::downgrade(&shared),
        events_rx,
        clients,
        fault_tx,
    ));
    Ok((
        NotifyServer {
            shared,
            _emitter: emitter,
            publisher: publisher_task,
        },
        fault_rx,
    ))
}

// ───────────── the run: Bus driver and lifetime management ────────────

/// Why `serve_bus` stopped serving one session.
enum ServeOutcome {
    /// The command stream ended or a reply failed or timed out —
    /// reconnect the Bus client.
    Reconnect,
    /// Shutdown was signalled — the run is ending.
    Stopped,
}

/// Serve incoming Bus commands on one session. Publish faults are
/// acted on only here, BETWEEN commands: a fault must never cancel a
/// dispatch mid-flight (its state changes may already be applied while
/// the D-Bus signal or the reply has not gone out). A fault stamped
/// with an older session's generation is a leftover from a superseded
/// session and is ignored outright. Every dispatch and reply is raced
/// against shutdown, so a stop is honoured mid-flight without waiting
/// out a wedged dispatch or the reply budget.
async fn serve_bus(
    session: &mut BusSession,
    shared: &Arc<NotifyShared>,
    faults: &mut mpsc::Receiver<u64>,
    generation: u64,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<ServeOutcome> {
    loop {
        let command = tokio::select! {
            biased;
            changed = shutdown.changed() => {
                match changed {
                    Ok(_) if !*shutdown.borrow_and_update() => continue,
                    _ => return Ok(ServeOutcome::Stopped),
                }
            }
            fault = faults.recv() => {
                match fault {
                    // The publisher task is gone (its sender dropped);
                    // the run's watch on the publisher reports why.
                    None => return Err(anyhow!("notify: publisher fault channel ended")),
                    Some(seen) if seen != generation => {
                        // A stale fault from a superseded session.
                        continue;
                    }
                    Some(_) => return Ok(ServeOutcome::Reconnect),
                }
            }
            command = session.incoming.recv() => {
                match command {
                    Some(command) => command,
                    None => return Ok(ServeOutcome::Reconnect),
                }
            }
        };
        let handled = tokio::select! {
            biased;
            changed = shutdown.changed() => {
                match changed {
                    Ok(_) if !*shutdown.borrow_and_update() => None,
                    _ => return Ok(ServeOutcome::Stopped),
                }
            }
            handled = handle_command(&*session.client, shared, &command) => Some(handled),
        };
        let Some(outcome) = handled else {
            // A spurious (non-true) shutdown change: nothing to redo,
            // keep serving.
            continue;
        };
        if let Err(error) = outcome {
            eprintln!("cosmix-dbusd notify: {error:#}; reconnecting the Bus client");
            return Ok(ServeOutcome::Reconnect);
        }
    }
}

/// Dispatch one command and reply on the session's client. Ok means
/// replied; Err means the reply failed or timed out (reconnect).
async fn handle_command(
    client: &dyn BusClient,
    shared: &Arc<NotifyShared>,
    command: &IncomingCommand,
) -> Result<()> {
    let (rc, body) = dispatch(shared, command).await;
    match tokio::time::timeout(PUBLISH_TIMEOUT, client.respond(command, rc, &body)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(anyhow!("Bus response failed ({error:#})")),
        Err(_) => Err(anyhow!("Bus response timed out")),
    }
}

/// Sleep `delay`, waking immediately on shutdown. Ok(true) means stop.
async fn sleep_or_stop(shutdown: &mut watch::Receiver<bool>, delay: Duration) -> Result<bool> {
    tokio::select! {
        biased;
        changed = shutdown.changed() => {
            changed.map_err(|_| anyhow!("notify: stop channel ended"))?;
            Ok(*shutdown.borrow_and_update())
        }
        _ = tokio::time::sleep(delay) => Ok(false),
    }
}

/// The Bus side of the run, J1-citizen style: dial, serve, and on any
/// Bus trouble (command stream end, publish fault, reply failure or
/// timeout, a failed dial) close the client and re-dial after
/// `reconnect_delay`. The notify server, its state and the session
/// connection all survive — only the Bus client is replaced, and the
/// publisher's props baseline survives with it. Each installed client
/// carries the next session GENERATION and publish faults are stamped
/// with the generation that faulted, so a stale fault from a superseded
/// session can never tear down the healthy one serving now. Ok is
/// returned only on shutdown; Err is an internal fault.
async fn run_bus<C: BusConnector>(
    mut connector: C,
    clients: watch::Sender<Option<BusClientSlot>>,
    mut faults: mpsc::Receiver<u64>,
    mut shutdown: watch::Receiver<bool>,
    shared: Arc<NotifyShared>,
    reconnect_delay: Duration,
) -> Result<()> {
    let mut generation: u64 = 0;
    loop {
        let mut session = tokio::select! {
            biased;
            changed = shutdown.changed() => {
                changed.map_err(|_| anyhow!("notify: stop channel ended"))?;
                if *shutdown.borrow_and_update() {
                    return Ok(());
                }
                continue;
            }
            session = connector.connect() => match session {
                Ok(session) => session,
                Err(error) => {
                    eprintln!(
                        "cosmix-dbusd notify: Bus unavailable ({error:#}); retrying in {reconnect_delay:?}"
                    );
                    if sleep_or_stop(&mut shutdown, reconnect_delay).await? {
                        return Ok(());
                    }
                    continue;
                }
            },
        };
        generation += 1;
        let _ = clients.send(Some(BusClientSlot {
            generation,
            client: Arc::clone(&session.client),
        }));
        // Faults are consumed inside serve_bus, between commands; any
        // fault left in the channel belongs to THIS generation or a
        // older one, and stale ones are filtered by generation there.
        let outcome = serve_bus(
            &mut session,
            &shared,
            &mut faults,
            generation,
            &mut shutdown,
        )
        .await;
        let _ = clients.send(None);
        if let Some(close) = session.close.take() {
            close.await;
        }
        match outcome? {
            ServeOutcome::Stopped => return Ok(()),
            ServeOutcome::Reconnect => {
                eprintln!("cosmix-dbusd notify: Bus disconnected; retrying in {reconnect_delay:?}");
                if sleep_or_stop(&mut shutdown, reconnect_delay).await? {
                    return Ok(());
                }
            }
        }
    }
}

/// The run's core: hold the notify server and the session connection
/// for the whole run, drive the Bus side through `connector`, and end
/// the run only on shutdown, session-bus death or an internal fault —
/// a Bus outage is waited out inside the run, with the notifications
/// and the D-Bus name intact.
async fn notify_run<C: BusConnector>(
    session: zbus::Connection,
    mut server: NotifyServer,
    clients: watch::Sender<Option<BusClientSlot>>,
    faults: mpsc::Receiver<u64>,
    connector: C,
    mut shutdown: watch::Receiver<bool>,
    reconnect_delay: Duration,
) -> Result<()> {
    // A pinned local (not spawned): dropping it at run end cancels the
    // driver wherever it is — no detached task, no 'static connector.
    let mut bus = Box::pin(run_bus(
        connector,
        clients,
        faults,
        shutdown.clone(),
        Arc::clone(&server.shared),
        reconnect_delay,
    ));
    loop {
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                match changed {
                    Err(_) => break Err(anyhow!("notify: stop channel ended")),
                    Ok(_) if !*shutdown.borrow_and_update() => continue,
                    Ok(_) => break Ok(()),
                }
            }
            _ = session.closed() => {
                break Err(anyhow!(
                    "notify: session bus connection lost; ending the run so the \
                     supervisor re-dials"
                ));
            }
            result = &mut bus => {
                // The driver returns Ok only on shutdown — which the
                // biased shutdown arm above sees first. Anything else
                // is an internal fault.
                break match result {
                    Ok(()) => Err(anyhow!("notify: Bus driver stopped unexpectedly")),
                    Err(error) => Err(anyhow!("notify: Bus driver failed: {error:#}")),
                };
            }
            joined = &mut server.publisher => {
                // The publisher is watched: a panic or an early exit
                // ends the run with an error instead of silently
                // halting publishing while everything else looks
                // healthy.
                break match joined {
                    Ok(Ok(())) => Err(anyhow!("notify: publisher task stopped unexpectedly")),
                    Ok(Err(error)) => Err(anyhow!("notify: publisher task failed: {error:#}")),
                    Err(error) => Err(anyhow!(
                        "notify: publisher task ended without finishing: {error}"
                    )),
                };
            }
        }
    }
}

/// The `notify` adapter: inbound `org.freedesktop.Notifications` server
/// bridged to the `notify` Bus service. One zbus connection (the
/// served name) for the whole run; the Bus client is a local that is
/// REPLACED, not ended, on Bus trouble — notifications, props state
/// and the D-Bus name survive a Bus outage. Only session-bus death,
/// shutdown or an internal fault ends the run, withdrawing everything.
#[derive(Default)]
pub struct NotifyAdapter;

impl Adapter for NotifyAdapter {
    fn name(&self) -> &'static str {
        "notify"
    }

    fn bus_service(&self) -> &'static str {
        BUS_SERVICE
    }

    fn run(self: Box<Self>, mut ctx: AdapterCtx) -> BoxRunFuture {
        Box::pin(async move {
            let mut shutdown = ctx.shutdown().clone();
            // Setup races shutdown: a stop must not wait out a dial or
            // a name claim.
            let session = tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    changed.map_err(|_| anyhow!("notify: stop channel ended"))?;
                    return Ok(());
                }
                session = ctx.connect_session_bus() => session
                    .map_err(|error| anyhow!("notify: session bus: {error:#}"))?,
            };
            let (clients_tx, clients_rx) = watch::channel(None);
            let (server, faults) = tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    changed.map_err(|_| anyhow!("notify: stop channel ended"))?;
                    return Ok(());
                }
                started = start_server(&session, clients_rx, MAX_LIVE, MAX_STORED_BYTES) => {
                    started?
                }
            };
            ctx.signal_ready();
            notify_run(
                session,
                server,
                clients_tx,
                faults,
                CtxBusConnector { ctx: &ctx },
                shutdown,
                BUS_RECONNECT_DELAY,
            )
            .await
        })
    }
}

// ───────────────────────────── tests ──────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use std::io::ErrorKind;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::io::AsyncBufReadExt as _;

    // ── harness: a private dbus-daemon and a fake Bus publisher ──

    /// `dbus-daemon --session --print-address --nofork` on a private
    /// socket, one per test: fully parallel, never the live desktop
    /// session. Dropping it kills the daemon.
    struct PrivateBus {
        address: String,
        child: tokio::process::Child,
    }

    impl PrivateBus {
        /// `None` (after a clear message) means dbus-daemon is absent
        /// — the caller skips.
        async fn spawn() -> Option<Self> {
            let child = tokio::process::Command::new("dbus-daemon")
                .args(["--session", "--print-address=1", "--nofork"])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true)
                .spawn();
            let mut child = match child {
                Ok(child) => child,
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    eprintln!(
                        "SKIPPED: dbus-daemon not found — the notify adapter integration \
                         tests need one (install dbus)"
                    );
                    return None;
                }
                Err(error) => panic!("cannot spawn dbus-daemon: {error}"),
            };
            let mut stdout = tokio::io::BufReader::new(child.stdout.take().expect("piped"));
            let mut address = String::new();
            match tokio::time::timeout(Duration::from_secs(10), stdout.read_line(&mut address))
                .await
            {
                Ok(Ok(bytes)) if bytes > 0 => {}
                Ok(_) => panic!("dbus-daemon printed no address: {address:?}"),
                Err(_) => panic!("dbus-daemon did not print its address within 10 s"),
            }
            let address = address.trim().to_string();
            assert!(address.starts_with("unix:"), "bus address: {address}");
            Some(Self { address, child })
        }

        async fn connect(&self) -> zbus::Connection {
            let address: zbus::address::Address = self.address.parse().expect("bus address");
            zbus::connection::Builder::address(address)
                .expect("address builds")
                .build()
                .await
                .expect("connect to the private bus")
        }

        /// Kill the daemon — session-bus death for everyone connected.
        async fn kill(&mut self) {
            self.child.start_kill().expect("kill dbus-daemon");
        }
    }

    /// Records every publication and reply; can be scripted to fail
    /// publishes, hang replies, or hold a failing publish / a reply
    /// until the test releases it (deterministic mid-flight staging for
    /// the fault-race tests).
    #[derive(Default)]
    struct FakePublisher {
        published: Mutex<Vec<(String, cosmix_bus::bus::BusMessage)>>,
        replies: Mutex<Vec<(String, u8, String)>>,
        fail_next: AtomicUsize,
        hang_replies: AtomicBool,
        /// When set, the next scripted failure first parks until
        /// `release_held_publish` — the fault exists but is not
        /// delivered until the test says so.
        hold_next: AtomicBool,
        held_publishes: AtomicUsize,
        held: tokio::sync::Notify,
        /// When set, every reply first parks until
        /// `release_held_replies` — a dispatch that is provably
        /// in-flight when a fault arrives.
        hold_replies: AtomicBool,
        held_replies: AtomicUsize,
        respond_gate: tokio::sync::Notify,
    }

    impl FakePublisher {
        fn bodies(&self, topic: &str) -> Vec<Value> {
            self.published
                .lock()
                .expect("fake publisher lock")
                .iter()
                .filter(|(name, _)| name == topic)
                .map(|(_, message)| serde_json::from_str(&message.body).expect("json body"))
                .collect()
        }

        /// One topic's messages whole, headers included (diff stamps
        /// live in headers).
        fn messages(&self, topic: &str) -> Vec<cosmix_bus::bus::BusMessage> {
            self.published
                .lock()
                .expect("fake publisher lock")
                .iter()
                .filter(|(name, _)| name == topic)
                .map(|(_, message)| message.clone())
                .collect()
        }

        /// The `event_seq` header of the diffs whose body hits `path`,
        /// in publish order.
        fn diff_seqs(&self, path: &str) -> Vec<u64> {
            self.messages(&props_changed_topic(BUS_SERVICE))
                .iter()
                .filter(|message| {
                    serde_json::from_str::<Value>(&message.body)
                        .expect("json body")
                        .get("path")
                        .is_some_and(|seen| seen == path)
                })
                .filter_map(|message| message.get("event_seq"))
                .map(|seq| seq.parse().expect("numeric event_seq header"))
                .collect()
        }

        fn replies_for(&self, command: &str) -> Vec<(u8, String)> {
            self.replies
                .lock()
                .expect("fake publisher lock")
                .iter()
                .filter(|(name, _, _)| name == command)
                .map(|(_, rc, body)| (*rc, body.clone()))
                .collect()
        }

        fn fail_next(&self, count: usize) {
            self.fail_next.store(count, Ordering::SeqCst);
        }

        fn hang_replies(&self) {
            self.hang_replies.store(true, Ordering::SeqCst);
        }

        /// The next scripted publish failure parks until released.
        fn hold_next_failure(&self) {
            self.hold_next.store(true, Ordering::SeqCst);
        }

        fn release_held_publish(&self) {
            self.held.notify_one();
        }

        fn held_publishes(&self) -> usize {
            self.held_publishes.load(Ordering::SeqCst)
        }

        /// Every reply parks until released (a provably in-flight
        /// dispatch).
        fn hold_replies_at_gate(&self) {
            self.hold_replies.store(true, Ordering::SeqCst);
        }

        fn release_held_replies(&self) {
            self.respond_gate.notify_one();
        }

        fn held_replies(&self) -> usize {
            self.held_replies.load(Ordering::SeqCst)
        }
    }

    impl BusClient for FakePublisher {
        fn publish_event(
            &self,
            topic: &str,
            message: cosmix_bus::bus::BusMessage,
        ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
            let topic = topic.to_string();
            Box::pin(async move {
                let pending = self.fail_next.load(Ordering::SeqCst);
                if pending > 0 {
                    if self.hold_next.swap(false, Ordering::SeqCst) {
                        self.held_publishes.fetch_add(1, Ordering::SeqCst);
                        self.held.notified().await;
                    }
                    self.fail_next.store(pending - 1, Ordering::SeqCst);
                    return Err(anyhow!("scripted publish failure"));
                }
                self.published
                    .lock()
                    .expect("fake publisher lock")
                    .push((topic, message));
                Ok(())
            })
        }

        fn respond<'a>(
            &'a self,
            command: &'a IncomingCommand,
            rc: u8,
            body: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
            let recorded = (command.command.clone(), rc, body.to_string());
            let hang = self.hang_replies.load(Ordering::SeqCst);
            let gate = self.hold_replies.load(Ordering::SeqCst);
            Box::pin(async move {
                if gate {
                    self.held_replies.fetch_add(1, Ordering::SeqCst);
                    self.respond_gate.notified().await;
                }
                if hang {
                    return std::future::pending().await;
                }
                self.replies
                    .lock()
                    .expect("fake publisher lock")
                    .push(recorded);
                Ok(())
            })
        }
    }

    /// A scripted Bus for the run-level tests: preloaded sessions
    /// handed to `notify_run`'s connector in order, so a test can drop
    /// a command stream (Bus outage), fail publishes and watch the
    /// run reconnect — all against the REAL run loop.
    #[derive(Clone, Default)]
    struct ScriptedBus {
        sessions: Arc<Mutex<Vec<BusSession>>>,
    }

    impl ScriptedBus {
        fn add(&self) -> ScriptSession {
            let publisher = Arc::new(FakePublisher::default());
            let (sender, incoming) = mpsc::unbounded_channel();
            self.sessions
                .lock()
                .expect("scripted bus lock")
                .push(BusSession {
                    client: Arc::clone(&publisher) as Arc<dyn BusClient>,
                    incoming,
                    close: None,
                });
            ScriptSession { publisher, sender }
        }
    }

    /// One scripted session's handles: what it published and the
    /// sending end of its command stream (drop it to end the stream).
    struct ScriptSession {
        publisher: Arc<FakePublisher>,
        sender: mpsc::UnboundedSender<IncomingCommand>,
    }

    impl BusConnector for ScriptedBus {
        fn connect(&mut self) -> Pin<Box<dyn Future<Output = Result<BusSession>> + Send + '_>> {
            Box::pin(async move {
                self.sessions
                    .lock()
                    .expect("scripted bus lock")
                    .pop()
                    .ok_or_else(|| anyhow!("scripted bus exhausted"))
            })
        }
    }

    #[zbus::proxy(
        interface = "org.freedesktop.Notifications",
        default_service = "org.freedesktop.Notifications",
        default_path = "/org/freedesktop/Notifications"
    )]
    trait NotificationsClient {
        #[allow(clippy::too_many_arguments)] // the signature is fixed by spec
        fn notify(
            &self,
            app_name: &str,
            replaces_id: u32,
            app_icon: &str,
            summary: &str,
            body: &str,
            actions: Vec<String>,
            hints: HashMap<String, OwnedValue>,
            expire_timeout: i32,
        ) -> zbus::Result<u32>;

        fn close_notification(&self, id: u32) -> zbus::Result<()>;

        fn get_capabilities(&self) -> zbus::Result<Vec<String>>;

        fn get_server_information(&self) -> zbus::Result<(String, String, String, String)>;

        #[zbus(signal)]
        fn notification_closed(&self, id: u32, reason: u32);

        #[zbus(signal)]
        fn action_invoked(&self, id: u32, action_key: String);
    }

    /// The adapter wired against a private bus, as `run()` wires it in
    /// production except for the Bus client (the fake publisher stands
    /// in; the verb dispatch IS the production dispatch).
    struct Wired {
        server: NotifyServer,
        publisher: Arc<FakePublisher>,
        /// Held for liveness only: dropping it would end the publisher's
        /// client channel.
        _clients_tx: watch::Sender<Option<BusClientSlot>>,
        _session: zbus::Connection,
    }

    async fn wired(bus: &PrivateBus) -> (Wired, mpsc::Receiver<u64>) {
        wired_with_cap(bus, MAX_LIVE, MAX_STORED_BYTES).await
    }

    async fn wired_with_cap(
        bus: &PrivateBus,
        cap: usize,
        max_stored_bytes: usize,
    ) -> (Wired, mpsc::Receiver<u64>) {
        let session = bus.connect().await;
        let publisher = Arc::new(FakePublisher::default());
        let (clients_tx, clients_rx) = watch::channel(None);
        let (server, fault_rx) = start_server(&session, clients_rx, cap, max_stored_bytes)
            .await
            .expect("notify server starts and claims the name");
        clients_tx
            .send(Some(BusClientSlot {
                generation: 0,
                client: Arc::clone(&publisher) as Arc<dyn BusClient>,
            }))
            .expect("the publisher channel is alive");
        (
            Wired {
                server,
                publisher,
                _clients_tx: clients_tx,
                _session: session,
            },
            fault_rx,
        )
    }

    /// Poll until `org.freedesktop.Notifications` has an owner on the
    /// private bus (the run-level tests' readiness signal).
    async fn wait_name_owned(connection: &zbus::Connection) {
        let dbus = zbus::fdo::DBusProxy::new(connection)
            .await
            .expect("fdo proxy");
        let name: zbus::names::BusName<'_> = DBUS_NAME.try_into().expect("valid name");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            match dbus.get_name_owner(name.clone()).await {
                Ok(_) => return,
                Err(_) => {
                    assert!(
                        tokio::time::Instant::now() < deadline,
                        "the notify name was never owned"
                    );
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        }
    }

    fn command(verb: &str, args: Value) -> IncomingCommand {
        IncomingCommand {
            from: "alpha".into(),
            command: verb.into(),
            id: Some("1".into()),
            args,
            body: String::new(),
            headers: BTreeMap::new(),
        }
    }

    async fn verb(shared: &Arc<NotifyShared>, name: &str, args: Value) -> (u8, Value) {
        let (rc, body) = dispatch(shared, &command(name, args)).await;
        (rc, serde_json::from_str(&body).expect("verb reply is json"))
    }

    async fn prop(shared: &Arc<NotifyShared>, path: &str) -> Value {
        let (rc, body) =
            dispatch(shared, &command("notify.props.get", json!({"path": path}))).await;
        assert_eq!(rc, 0, "notify.props.get {path}: {body}");
        serde_json::from_str(&body).expect("props reply is json")
    }

    async fn wait_for_events(publisher: &FakePublisher, count: usize) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while publisher.bodies(TOPIC_NOTIFY_CHANGED).len() < count {
            assert!(
                tokio::time::Instant::now() < deadline,
                "expected at least {count} {TOPIC_NOTIFY_CHANGED} events, got {}",
                publisher.bodies(TOPIC_NOTIFY_CHANGED).len()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn next_closed(stream: &mut NotificationClosedStream) -> (u32, u32) {
        let signal = tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .expect("NotificationClosed within 10 s")
            .expect("signal stream stays open");
        let args = signal.args().expect("NotificationClosed args");
        (args.id, args.reason)
    }

    async fn next_invoked(stream: &mut ActionInvokedStream) -> (u32, String) {
        let signal = tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .expect("ActionInvoked within 10 s")
            .expect("signal stream stays open");
        let args = signal.args().expect("ActionInvoked args");
        (args.id, args.action_key)
    }

    async fn no_signal_within(stream: &mut NotificationClosedStream) {
        assert!(
            tokio::time::timeout(Duration::from_millis(250), stream.next())
                .await
                .is_err(),
            "no NotificationClosed may arrive"
        );
    }

    /// Poll `probe` until it returns Some, panicking after `budget`.
    async fn poll_until<T>(
        budget: Duration,
        mut probe: impl FnMut() -> Option<T>,
        what: &str,
    ) -> T {
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            if let Some(value) = probe() {
                return value;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {what}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn urgency_hint(byte: u8) -> HashMap<String, OwnedValue> {
        HashMap::from([("urgency".to_string(), OwnedValue::from(byte))])
    }

    fn resident_hint() -> HashMap<String, OwnedValue> {
        HashMap::from([("resident".to_string(), OwnedValue::from(true))])
    }

    // ── pure core ──

    fn create_args(summary: &str) -> CreateArgs {
        CreateArgs {
            app: "app".into(),
            replaces_id: 0,
            icon: "icon".into(),
            summary: summary.into(),
            body: "body".into(),
            actions: Vec::new(),
            urgency: Urgency::Normal,
            resident: false,
            transient: false,
            desktop_entry: None,
            image_path: None,
            image_data: false,
            timeout: ExpireTimeout::Default,
            origin: Origin::Dbus,
        }
    }

    fn created(events: &[NotifyEvent], replaced: bool) -> bool {
        events.iter().any(|event| {
            matches!(
                &event.kind,
                NotifyEventKind::Created { replaced: got, .. } if *got == replaced
            )
        })
    }

    fn closed(events: &[NotifyEvent], reason: CloseReason) -> bool {
        events.iter().any(|event| {
            matches!(
                &event.kind,
                NotifyEventKind::Closed { reason: got } if *got == reason
            )
        })
    }

    #[test]
    fn expiry_resolution_follows_the_pinned_policy() {
        // -1 (default): 8 s for low and normal ...
        assert_eq!(
            resolve_expiry(ExpireTimeout::Default, Urgency::Normal),
            Some(DEFAULT_EXPIRY)
        );
        assert_eq!(
            resolve_expiry(ExpireTimeout::Default, Urgency::Low),
            Some(DEFAULT_EXPIRY)
        );
        // ... and never for critical.
        assert_eq!(
            resolve_expiry(ExpireTimeout::Default, Urgency::Critical),
            None
        );
        // 0 is never; an explicit timeout wins regardless of urgency,
        // but is clamped to at least MIN_EXPIRY so the notification
        // cannot close before the client has its id.
        assert_eq!(resolve_expiry(ExpireTimeout::Never, Urgency::Normal), None);
        assert_eq!(
            resolve_expiry(ExpireTimeout::Millis(1200), Urgency::Critical),
            Some(Duration::from_millis(1200))
        );
        assert_eq!(
            resolve_expiry(ExpireTimeout::Millis(1), Urgency::Normal),
            Some(MIN_EXPIRY),
            "a very short timeout is clamped to the minimum"
        );
        assert_eq!(
            resolve_expiry(ExpireTimeout::Millis(0), Urgency::Normal),
            Some(MIN_EXPIRY)
        );
    }

    #[test]
    fn replaces_id_reuses_a_live_id_and_adopts_a_dead_one() {
        let mut core = NotifyCore::new(MAX_LIVE, MAX_STORED_BYTES);
        let now = SystemTime::UNIX_EPOCH;
        let mono = Instant::now();
        let (first, events) = core.create(&create_args("one"), now, mono);
        assert_ne!(first, 0, "0 is Notify's no-replaces_id");
        assert!(created(&events, false), "{events:?}");
        assert_eq!(core.get(first).expect("live").summary, "one");

        let mut args = create_args("two");
        args.replaces_id = first;
        let (second, events) = core.create(&args, now, mono);
        assert_eq!(second, first, "a live replaces_id is reused");
        assert!(created(&events, true), "{events:?}");
        assert!(!closed(&events, CloseReason::Expired), "{events:?}");
        assert_eq!(core.count(), 1);
        assert_eq!(core.get(first).expect("still live").summary, "two");

        // A non-zero replaces_id that is not live is created under that
        // very id (spec: the returned id is the replaces_id) — safe
        // because this process never reuses ids.
        let mut stale = create_args("three");
        stale.replaces_id = 42;
        let (adopted, events) = core.create(&stale, now, mono);
        assert_eq!(adopted, 42, "a dead replaces_id is adopted");
        assert!(created(&events, false), "{events:?}");
        assert_eq!(core.count(), 2);
        assert_eq!(
            core.get(42).expect("live under the adopted id").summary,
            "three"
        );
    }

    #[test]
    fn ids_are_process_wide_and_never_restart() {
        // Two cores in one process = an adapter restart: a fresh run
        // must never hand out ids the previous run already gave out.
        let now = SystemTime::now();
        let mono = Instant::now();
        let mut first = NotifyCore::new(MAX_LIVE, MAX_STORED_BYTES);
        let mut ids = Vec::new();
        for index in 0..3 {
            let (id, _) = first.create(&create_args(&format!("first {index}")), now, mono);
            ids.push(id);
        }
        let mut second = NotifyCore::new(MAX_LIVE, MAX_STORED_BYTES);
        let (fresh, _) = second.create(&create_args("after restart"), now, mono);
        assert_ne!(fresh, 0);
        assert!(
            !ids.contains(&fresh),
            "a restarted run never reuses an id (gave {fresh} after {ids:?})"
        );
        let (again, _) = second.create(&create_args("next"), now, mono);
        assert_ne!(again, fresh, "ids stay monotonic inside the run");
    }

    #[test]
    fn cap_evicts_the_oldest_non_critical_as_expired() {
        let mut core = NotifyCore::new(3, MAX_STORED_BYTES);
        let now = SystemTime::UNIX_EPOCH;
        let mono = Instant::now();
        let mut critical = create_args("hold me");
        critical.urgency = Urgency::Critical;
        critical.timeout = ExpireTimeout::Never;
        let (first, _) = core.create(&create_args("first"), now, mono);
        let (critical_id, _) = core.create(&critical, now + Duration::from_secs(1), mono);
        core.create(&create_args("third"), now + Duration::from_secs(2), mono);
        assert_eq!(core.count(), 3);

        let (fourth, events) =
            core.create(&create_args("fourth"), now + Duration::from_secs(3), mono);
        assert_ne!(fourth, 0);
        assert_eq!(core.count(), 3, "the cap holds");
        assert!(closed(&events, CloseReason::Expired), "{events:?}");
        assert!(created(&events, false), "{events:?}");
        assert!(core.get(first).is_none(), "the oldest non-critical went");
        assert!(
            core.get(critical_id).is_some(),
            "critical survives eviction"
        );
        assert!(core.get(fourth).is_some(), "the newcomer is live");

        // A live set of all criticals at the cap evicts the OLDEST
        // CRITICAL as expired (reason 1) — critical buys priority, not
        // unbounded growth.
        let mut core = NotifyCore::new(2, MAX_STORED_BYTES);
        let mut seen = Vec::new();
        for index in 0..3 {
            let mut args = create_args("critical");
            args.urgency = Urgency::Critical;
            args.timeout = ExpireTimeout::Never;
            let (id, events) = core.create(&args, now + Duration::from_secs(index), mono);
            if index == 2 {
                assert!(
                    closed(&events, CloseReason::Expired),
                    "the oldest critical is evicted as expired: {events:?}"
                );
            }
            seen.push(id);
        }
        assert_eq!(core.count(), 2, "the cap holds even for all-critical");
        assert!(core.get(seen[0]).is_none(), "the oldest critical went");
        assert!(core.get(seen[1]).is_some());
        assert!(core.get(seen[2]).is_some());
    }

    #[test]
    fn the_stored_bytes_budget_evicts_the_oldest() {
        // A budget of 2 KiB against ~4 KiB records: the live set stays
        // within budget by expiring the oldest, cap or no cap.
        let mut core = NotifyCore::new(10, 2 * 1024);
        let now = SystemTime::now();
        let mono = Instant::now();
        let mut first = create_args("first");
        first.body = "b".repeat(2 * 1024);
        let (id_first, _) = core.create(&first, now, mono);
        let mut second = create_args("second");
        second.body = "b".repeat(2 * 1024);
        let (id_second, events) = core.create(&second, now + Duration::from_secs(1), mono);
        assert_eq!(core.count(), 1, "the bytes budget holds");
        assert!(
            closed(&events, CloseReason::Expired),
            "the oldest was evicted as expired: {events:?}"
        );
        assert!(core.get(id_first).is_none());
        assert!(core.get(id_second).is_some());
    }

    #[test]
    fn replaces_are_subject_to_the_stored_bytes_budget() {
        // A full live set of tiny records, then every one replaced with
        // a max-size record: the budget must hold on replace too —
        // each replace counts its size delta and evicts the oldest
        // others as expired, never the record being replaced.
        let now = SystemTime::now();
        let mono = Instant::now();
        let mut huge = create_args("m");
        huge.app = "a".repeat(MAX_TEXT_BYTES);
        huge.icon = "i".repeat(MAX_TEXT_BYTES);
        huge.summary = "s".repeat(MAX_TEXT_BYTES);
        huge.body = "b".repeat(MAX_TEXT_BYTES);
        huge.desktop_entry = Some("d".repeat(MAX_TEXT_BYTES));
        huge.image_path = Some("p".repeat(MAX_TEXT_BYTES));
        huge.actions = (0..MAX_ACTIONS)
            .map(|index| Action {
                key: format!("k{index}"),
                label: "l".repeat(MAX_TEXT_BYTES),
            })
            .collect();

        let mut core = NotifyCore::new(MAX_LIVE, MAX_STORED_BYTES);
        let mut ids = Vec::new();
        for index in 0..MAX_LIVE {
            let (id, _) = core.create(&create_args(&format!("tiny {index}")), now, mono);
            ids.push(id);
        }
        assert_eq!(core.count(), MAX_LIVE);
        assert!(
            core.stored_bytes() <= MAX_STORED_BYTES,
            "{} tiny records are within the budget",
            core.count()
        );

        for (round, id) in ids.into_iter().enumerate() {
            // Once the huges overflow the budget, earlier rounds'
            // eviction may legitimately take LATER round's tiny
            // targets too — a target that is already dead is adopted,
            // not replaced. The contract under test: the budget holds
            // every round, a live target is replaced (never evicted),
            // and the target is live after its own insert.
            let live_before = core.get(id).is_some();
            let mut args = huge.clone();
            args.replaces_id = id;
            let (returned, events) = core.create(&args, now, mono);
            assert_eq!(
                returned, id,
                "round {round}: replace or adopt, the id is kept"
            );
            assert!(
                events
                    .iter()
                    .any(|event| event.id == id
                        && matches!(event.kind, NotifyEventKind::Created { .. })),
                "round {round}: a Created event for the id: {events:?}"
            );
            if live_before {
                assert!(
                    events.iter().any(|event| event.id == id
                        && matches!(event.kind, NotifyEventKind::Created { replaced: true, .. })),
                    "round {round}: a live target is replaced, not evicted: {events:?}"
                );
            }
            assert!(
                core.get(id).is_some(),
                "round {round}: the target survives its own insert"
            );
            assert!(
                core.stored_bytes() <= MAX_STORED_BYTES,
                "round {round}: the budget holds on replace too ({} > {})",
                core.stored_bytes(),
                MAX_STORED_BYTES
            );
        }
        assert!(core.count() <= MAX_LIVE, "the count cap holds too");
    }

    /// Tests that bend ID_CLOCK's magnitude serialize here: a near-wrap
    /// store and an adoption's fetch_max jump must not interleave with
    /// each other (plain allocations are always benign — they only
    /// ever issue values the clock already points at).
    static ID_CLOCK_SERIALIZED: Mutex<()> = Mutex::new(());

    #[test]
    fn an_adopted_id_is_reserved_against_the_id_clock() {
        // Adopting a dead caller-supplied id must reserve it so the
        // clock can never hand that id to another app — even after the
        // adopted record closes (a reservation that only holds while
        // the record is live is the same-core live-check, not a
        // reservation). The id sits far past anything the clock itself
        // reaches during this test.
        let _serialized = ID_CLOCK_SERIALIZED.lock().expect("id test lock");
        let mut core = NotifyCore::new(MAX_LIVE, MAX_STORED_BYTES);
        let hot = ID_CLOCK
            .load(Ordering::SeqCst)
            .saturating_add(2_000_000)
            .max(1_000_000);
        let mut args = create_args("adopted");
        args.replaces_id = hot;
        let (id, _) = core.create(&args, SystemTime::now(), Instant::now());
        assert_eq!(id, hot, "a dead replaces_id is adopted under that id");
        assert!(
            ID_CLOCK.load(Ordering::SeqCst) > hot,
            "adopting reserves PAST the id (clock at {} not > {})",
            ID_CLOCK.load(Ordering::SeqCst),
            hot
        );
        // The dangerous window is after the adopted id dies: a fresh
        // create must still never be handed `hot`.
        assert!(core.close(hot, CloseReason::Dismissed).is_some());
        let (fresh, _) = core.create(&create_args("fresh"), SystemTime::now(), Instant::now());
        assert_ne!(fresh, hot, "the clock never hands an adopted id out");
    }

    #[test]
    fn id_clock_wraps_to_one_without_a_wall_clock_reseed() {
        // At the top of the u32 range the last ids run out (MAX-1,
        // MAX) and the clock wraps to 1 — a wall-clock re-seed would
        // jump BACK to ~3e9, re-issuing the ids handed out moments
        // ago. Allocation hands out the clock's current value and
        // moves it past, so three allocations from MAX-1 must all land
        // in the run-out or the small wrapped range.
        let _serialized = ID_CLOCK_SERIALIZED.lock().expect("id test lock");
        ID_CLOCK.store(u32::MAX - 1, Ordering::SeqCst);
        let mut core = NotifyCore::new(MAX_LIVE, MAX_STORED_BYTES);
        let mut issued = Vec::new();
        for index in 0..3 {
            let (id, _) = core.create(
                &create_args(&format!("wrap {index}")),
                SystemTime::now(),
                Instant::now(),
            );
            issued.push(id);
        }
        for id in issued {
            assert!(
                id >= u32::MAX - 1 || (id > 0 && id < 1_000_000),
                "the wrap continues from 1, not a wall-clock re-seed: {id}"
            );
        }
    }

    #[test]
    fn eviction_follows_insertion_order_not_the_wall_clock() {
        // The second record is created with an EARLIER wall clock (an
        // NTP step back): eviction still takes the first-INSERTED
        // record, because the order is the monotonic insertion
        // sequence, not created_at.
        let mut core = NotifyCore::new(2, MAX_STORED_BYTES);
        let mono = Instant::now();
        let late = SystemTime::now();
        let early = late - Duration::from_secs(3600);
        let (first, _) = core.create(&create_args("first"), late, mono);
        let (second, _) = core.create(&create_args("second"), early, mono);
        assert!(
            core.get(second).expect("live").created_at < core.get(first).expect("live").created_at,
            "precondition: second's wall clock is an hour earlier"
        );

        let (_, events) = core.create(&create_args("third"), early, mono);
        assert!(
            events.iter().any(|event| event.id == first
                && matches!(
                    event.kind,
                    NotifyEventKind::Closed {
                        reason: CloseReason::Expired
                    }
                )),
            "the first-INSERTED record is evicted: {events:?}"
        );
        assert!(core.get(first).is_none());
        assert!(
            core.get(second).is_some(),
            "the skewed clock bought nothing"
        );
    }

    #[test]
    fn stored_strings_and_actions_are_capped() {
        let mut core = NotifyCore::new(MAX_LIVE, MAX_STORED_BYTES);
        let huge = "x".repeat(MAX_TEXT_BYTES + 4096);
        let mut args = create_args(&huge);
        args.body = huge.clone();
        args.actions = (0..=MAX_ACTIONS)
            .map(|index| Action {
                key: format!("k{index}"),
                label: format!("l{index}"),
            })
            .collect();
        let (id, _) = core.create(&args, SystemTime::now(), Instant::now());
        let record = core.get(id).expect("live");
        assert!(record.summary.len() <= MAX_TEXT_BYTES);
        assert!(record.summary.ends_with('\u{2026}'), "the cut is marked");
        assert!(record.body.len() <= MAX_TEXT_BYTES);
        assert_eq!(record.actions.len(), MAX_ACTIONS);
        assert!(
            record
                .actions
                .iter()
                .all(|action| action.key.len() <= MAX_TEXT_BYTES)
        );
    }

    #[test]
    fn cap_string_cuts_on_a_char_boundary() {
        assert_eq!(cap_string("short"), "short");
        let multibyte = "\u{e9}".repeat(MAX_TEXT_BYTES + 10);
        let capped = cap_string(&multibyte);
        assert!(
            capped.len() <= MAX_TEXT_BYTES,
            "the cap holds: {}",
            capped.len()
        );
        assert!(capped.ends_with('\u{2026}'), "the cut is marked");
        // ASCII input lands exactly on the cap.
        let ascii = cap_string(&"x".repeat(MAX_TEXT_BYTES + 4096));
        assert_eq!(ascii.len(), MAX_TEXT_BYTES);
        assert!(ascii.ends_with('\u{2026}'));
    }

    #[test]
    fn invoke_refuses_unknowns_and_closes_unless_resident() {
        let mut core = NotifyCore::new(MAX_LIVE, MAX_STORED_BYTES);
        let mut args = create_args("with actions");
        args.actions = vec![
            Action {
                key: "default".into(),
                label: "Show".into(),
            },
            Action {
                key: "reply".into(),
                label: "Reply".into(),
            },
        ];
        let (id, _) = core.create(&args, SystemTime::now(), Instant::now());
        assert_eq!(core.invoke(id, "nope"), Err("unknown action"));
        assert_eq!(core.invoke(999, "default"), Err("unknown id"));

        let (events, closed) = core.invoke(id, "reply").expect("invoked");
        assert!(closed);
        assert!(
            matches!(
                &events[..],
                [
                    NotifyEvent {
                        kind: NotifyEventKind::ActionInvoked { action },
                        ..
                    },
                    NotifyEvent {
                        kind: NotifyEventKind::Closed { reason },
                        ..
                    },
                ] if action == "reply" && *reason == CloseReason::Dismissed
            ),
            "{events:?}"
        );
        assert!(core.get(id).is_none());
        assert_eq!(core.invoke(id, "reply"), Err("unknown id"), "gone for good");

        let mut resident = create_args("resident");
        resident.resident = true;
        resident.actions = args.actions.clone();
        let (id, _) = core.create(&resident, SystemTime::now(), Instant::now());
        let (events, closed) = core.invoke(id, "default").expect("invoked");
        assert!(!closed, "resident survives activation");
        assert!(matches!(
            &events[..],
            [NotifyEvent {
                kind: NotifyEventKind::ActionInvoked { .. },
                ..
            }]
        ));
        assert!(core.get(id).is_some());
    }

    #[test]
    fn a_stale_timer_never_closes_a_replaced_notification() {
        let mut core = NotifyCore::new(MAX_LIVE, MAX_STORED_BYTES);
        // The wall clock is parked at the epoch for the whole test:
        // due-ness must depend only on the monotonic clock, so a
        // wall-clock step backwards can never keep a notification from
        // expiring (nor fire it early).
        let now = SystemTime::UNIX_EPOCH;
        let mono = Instant::now();
        let mut short = create_args("short");
        short.timeout = ExpireTimeout::Millis(50);
        let (id, _) = core.create(&short, now, mono);

        // Replaced with a much longer deadline before the 50 ms timer
        // fires: the stale fire must find the deadline in the future.
        let mut long = create_args("long");
        long.replaces_id = id;
        long.timeout = ExpireTimeout::Millis(5000);
        core.create(&long, now, mono + Duration::from_millis(10));

        assert_eq!(
            core.expire_if_due(id, mono + Duration::from_millis(60)),
            None
        );
        assert!(core.get(id).is_some());
        assert_eq!(
            core.expire_if_due(id, mono + Duration::from_millis(6000)),
            Some(NotifyEvent {
                seq: 3,
                id,
                kind: NotifyEventKind::Closed {
                    reason: CloseReason::Expired
                },
            }),
            "the real deadline closes exactly once"
        );
        assert_eq!(
            core.expire_if_due(id, mono + Duration::from_millis(7000)),
            None
        );
    }

    #[test]
    fn urgency_hint_accepts_byte_int32_and_uint32() {
        let empty: HashMap<String, OwnedValue> = HashMap::new();
        let args = |hints: HashMap<String, OwnedValue>| {
            CreateArgs::from_dbus("app", 0, "", "s", "", &[], &hints, -1)
        };
        // The spec's byte form.
        let byte = HashMap::from([("urgency".to_string(), OwnedValue::from(2u8))]);
        assert_eq!(args(byte).urgency, Urgency::Critical);
        // Non-conforming but seen in the wild: int32 and uint32.
        let int32 = HashMap::from([("urgency".to_string(), OwnedValue::from(0i32))]);
        assert_eq!(args(int32).urgency, Urgency::Low);
        let uint32 = HashMap::from([("urgency".to_string(), OwnedValue::from(2u32))]);
        assert_eq!(args(uint32).urgency, Urgency::Critical);
        // Absent stays normal; an out-of-range width stays normal too.
        assert_eq!(args(empty).urgency, Urgency::Normal);
        let wide = HashMap::from([("urgency".to_string(), OwnedValue::from(9u32))]);
        assert_eq!(args(wide).urgency, Urgency::Normal);
    }

    #[test]
    fn send_args_parse_or_refuse() {
        let full = parse_send_args(Some(&json!({
            "summary": "s", "body": "b", "app": "a", "icon": "i",
            "urgency": "critical", "timeout": 0,
            "actions": [["reply", "Reply"], {"key": "default"}],
            "resident": true, "transient": true,
            "desktop_entry": "org.example.app", "image_path": "file:///x.png",
        })))
        .expect("full args parse");
        assert_eq!(full.urgency, Urgency::Critical);
        assert_eq!(full.timeout, ExpireTimeout::Never);
        assert_eq!(full.replaces_id, 0);
        assert_eq!(
            full.actions,
            vec![
                Action {
                    key: "reply".into(),
                    label: "Reply".into()
                },
                Action {
                    key: "default".into(),
                    label: String::new()
                },
            ]
        );
        assert!(full.resident && full.transient);
        assert_eq!(full.desktop_entry.as_deref(), Some("org.example.app"));
        assert_eq!(full.image_path.as_deref(), Some("file:///x.png"));
        assert_eq!(full.origin, Origin::Mesh);

        let minimal = parse_send_args(Some(&json!({"summary": "s"}))).expect("minimal parses");
        assert_eq!(minimal.app, "cosmix");
        assert_eq!(minimal.urgency, Urgency::Normal);
        assert_eq!(minimal.timeout, ExpireTimeout::Default);
        assert!(minimal.actions.is_empty());

        // Flat key,label list (Notify's own wire shape).
        let flat = parse_send_args(Some(
            &json!({"summary": "s", "actions": ["a", "1", "b", "2"]}),
        ))
        .expect("flat parses");
        assert_eq!(flat.actions.len(), 2);

        for bad in [
            None,
            Some(json!({})),
            Some(json!({"summary": ""})),
            Some(json!({"summary": "s", "urgency": "urgent"})),
            Some(json!({"summary": "s", "urgency": 3})),
            Some(json!({"summary": "s", "timeout": "soon"})),
            Some(json!({"summary": "s", "timeout": 5_000_000_000i64})),
            Some(json!({"summary": "s", "actions": ["odd"]})),
            Some(json!({"summary": "s", "actions": [["only-key"]]})),
            Some(json!({"summary": "s", "actions": [{"label": "no key"}]})),
        ] {
            let error = parse_send_args(bad.as_ref()).expect_err("must refuse");
            assert!(!error.is_empty(), "the refusal says why");
        }

        // Numeric urgency and negative timeout (both lenient forms).
        let numeric = parse_send_args(Some(&json!({"summary": "s", "urgency": 0, "timeout": -1})))
            .expect("numeric forms parse");
        assert_eq!(numeric.urgency, Urgency::Low);
        assert_eq!(numeric.timeout, ExpireTimeout::Default);
    }

    // ── integration: a private dbus-daemon, the real interface ──

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn notify_lands_as_props_and_events() {
        let Some(bus) = PrivateBus::spawn().await else {
            return;
        };
        let (wired, _faults) = wired(&bus).await;
        let client_conn = bus.connect().await;
        let client = NotificationsClientProxy::new(&client_conn)
            .await
            .expect("proxy");
        let mut closed_stream = client.receive_notification_closed().await.expect("stream");

        let capabilities = client.get_capabilities().await.expect("capabilities");
        assert_eq!(
            capabilities,
            vec!["actions".to_string(), "body".to_string()]
        );
        let (name, vendor, version, spec) =
            client.get_server_information().await.expect("server info");
        assert_eq!((name.as_str(), vendor.as_str()), ("cosmix", "cosmix"));
        assert_eq!(spec, SPEC_VERSION);
        assert_eq!(version, cosmix_buildinfo::build_info!().version);

        let id = client
            .notify(
                "TestApp",
                0,
                "test-icon",
                "the summary",
                "the body",
                vec!["default".into(), "Show".into()],
                HashMap::new(),
                -1,
            )
            .await
            .expect("Notify");
        assert_ne!(id, 0, "0 is never handed out (it means no replaces_id)");

        let shared = &wired.server.shared;
        let base = format!("n{id}");
        assert_eq!(prop(shared, "count").await, json!(1));
        assert_eq!(prop(shared, &format!("{base}.app")).await, json!("TestApp"));
        assert_eq!(
            prop(shared, &format!("{base}.summary")).await,
            json!("the summary")
        );
        assert_eq!(
            prop(shared, &format!("{base}.body")).await,
            json!("the body")
        );
        assert_eq!(
            prop(shared, &format!("{base}.icon")).await,
            json!("test-icon")
        );
        assert_eq!(
            prop(shared, &format!("{base}.urgency")).await,
            json!("normal")
        );
        assert_eq!(prop(shared, &format!("{base}.origin")).await, json!("dbus"));
        assert_eq!(
            prop(shared, &format!("{base}.resident")).await,
            json!(false)
        );
        assert_eq!(
            prop(shared, &format!("{base}.transient")).await,
            json!(false),
            "transient is a projected leaf even when unset"
        );
        assert_eq!(
            prop(shared, &format!("{base}.actions")).await,
            json!([{"key": "default", "label": "Show"}])
        );
        assert!(
            prop(shared, &format!("{base}.expires_at"))
                .await
                .is_string(),
            "-1 resolves to the 8 s server default"
        );
        assert!(
            prop(shared, &format!("{base}.created_at"))
                .await
                .is_string()
        );
        let (rc, _) = verb(
            shared,
            "notify.props.get",
            json!({"path": format!("{base}.image_data")}),
        )
        .await;
        assert_eq!(rc, 10, "no image-data leaf without the hint");

        // The created event is published with a stamped seq — and the
        // run's FIRST change publishes a props diff too: the baseline
        // is seeded with the empty tree, not skipped.
        wait_for_events(&wired.publisher, 1).await;
        let events = wired.publisher.bodies(TOPIC_NOTIFY_CHANGED);
        assert_eq!(events[0]["event"], "notification.created");
        assert_eq!(events[0]["event_seq"], 1);
        assert_eq!(events[0]["data"]["notification"]["summary"], "the summary");
        let diffs = wired.publisher.bodies(&props_changed_topic(BUS_SERVICE));
        assert!(
            diffs
                .iter()
                .any(|diff| diff["path"] == "count" && diff["old"] == 0 && diff["new"] == 1),
            "the first change of the run reaches notify.props.changed: {diffs:?}"
        );
        assert!(
            diffs.iter().any(|diff| diff["path"] == format!("n{id}")),
            "the new subtree is in the first diff: {diffs:?}"
        );
        let (rc, info) = verb(shared, "notify.info", Value::Null).await;
        assert_eq!(rc, 0);
        assert_eq!(info["count"], 1);
        assert_eq!(info["event_seq"], 1);
        assert_eq!(info["dbus"]["spec"], SPEC_VERSION);

        // Nothing closed — no close signal may arrive.
        no_signal_within(&mut closed_stream).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn replaces_id_replaces_the_live_notification() {
        let Some(bus) = PrivateBus::spawn().await else {
            return;
        };
        let (wired, _faults) = wired(&bus).await;
        let client_conn = bus.connect().await;
        let client = NotificationsClientProxy::new(&client_conn)
            .await
            .expect("proxy");

        let first = client
            .notify(
                "app",
                0,
                "",
                "first summary",
                "",
                Vec::new(),
                HashMap::new(),
                0,
            )
            .await
            .expect("first Notify");
        let second = client
            .notify(
                "app",
                first,
                "",
                "second summary",
                "second body",
                Vec::new(),
                HashMap::new(),
                0,
            )
            .await
            .expect("second Notify");
        assert_eq!(first, second, "a live replaces_id is reused");

        let shared = &wired.server.shared;
        assert_eq!(prop(shared, "count").await, json!(1), "still one record");
        assert_eq!(
            prop(shared, &format!("n{first}.summary")).await,
            json!("second summary")
        );
        assert_eq!(
            prop(shared, &format!("n{first}.body")).await,
            json!("second body")
        );

        wait_for_events(&wired.publisher, 2).await;
        let events = wired.publisher.bodies(TOPIC_NOTIFY_CHANGED);
        assert_eq!(events[0]["event"], "notification.created");
        assert_eq!(events[1]["event"], "notification.replaced");
        assert_eq!(events[1]["id"], first);
        assert_eq!(events[1]["data"]["replaced"], true);
        let seqs = [
            events[0]["event_seq"].as_u64(),
            events[1]["event_seq"].as_u64(),
        ];
        assert_eq!(seqs, [Some(1), Some(2)], "seq is monotonic");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_notification_closes_with_reason_3() {
        let Some(bus) = PrivateBus::spawn().await else {
            return;
        };
        let (wired, _faults) = wired(&bus).await;
        let client_conn = bus.connect().await;
        let client = NotificationsClientProxy::new(&client_conn)
            .await
            .expect("proxy");
        let mut closed_stream = client.receive_notification_closed().await.expect("stream");

        let id = client
            .notify("app", 0, "", "to close", "", Vec::new(), HashMap::new(), 0)
            .await
            .expect("Notify");
        client
            .close_notification(id)
            .await
            .expect("CloseNotification");

        assert_eq!(next_closed(&mut closed_stream).await, (id, 3));
        let shared = &wired.server.shared;
        assert_eq!(prop(shared, "count").await, json!(0));
        assert!(
            verb(
                shared,
                "notify.props.get",
                json!({"path": format!("n{id}.summary")})
            )
            .await
            .0 == 10,
            "the prop leaf is gone, not empty"
        );
        wait_for_events(&wired.publisher, 2).await;
        let events = wired.publisher.bodies(TOPIC_NOTIFY_CHANGED);
        assert_eq!(events[1]["event"], "notification.closed");
        assert_eq!(events[1]["data"]["reason"], 3);
        assert_eq!(events[1]["data"]["reason_name"], "closed");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn expiry_fires_from_its_own_timer_with_reason_1() {
        let Some(bus) = PrivateBus::spawn().await else {
            return;
        };
        let (wired, _faults) = wired(&bus).await;
        let client_conn = bus.connect().await;
        let client = NotificationsClientProxy::new(&client_conn)
            .await
            .expect("proxy");
        let mut closed_stream = client.receive_notification_closed().await.expect("stream");

        let id = client
            .notify(
                "app",
                0,
                "",
                "will expire",
                "",
                Vec::new(),
                HashMap::new(),
                250,
            )
            .await
            .expect("Notify");
        assert_eq!(prop(&wired.server.shared, "count").await, json!(1));

        assert_eq!(next_closed(&mut closed_stream).await, (id, 1));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while prop(&wired.server.shared, "count").await != json!(0) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "props drain after expiry"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        wait_for_events(&wired.publisher, 2).await;
        let events = wired.publisher.bodies(TOPIC_NOTIFY_CHANGED);
        assert_eq!(events[1]["event"], "notification.closed");
        assert_eq!(events[1]["data"]["reason"], 1);

        // Never-expiring forms stay put within the same window:
        // critical with -1, and any urgency with 0.
        let critical = client
            .notify(
                "app",
                0,
                "",
                "critical",
                "",
                Vec::new(),
                urgency_hint(2),
                -1,
            )
            .await
            .expect("critical Notify");
        let zero = client
            .notify("app", 0, "", "zero", "", Vec::new(), HashMap::new(), 0)
            .await
            .expect("zero Notify");
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(prop(&wired.server.shared, "count").await, json!(2));
        assert_eq!(
            prop(&wired.server.shared, &format!("n{critical}.expires_at")).await,
            Value::Null,
            "critical with -1 never expires"
        );
        assert_eq!(
            prop(&wired.server.shared, &format!("n{zero}.expires_at")).await,
            Value::Null,
            "timeout 0 never expires"
        );
        no_signal_within(&mut closed_stream).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn expiry_emits_closed_even_when_the_emission_must_wait() {
        let Some(bus) = PrivateBus::spawn().await else {
            return;
        };
        // Every NotificationClosed emission first yields once. If the
        // expiry path still aborts its own timer task on disarm, the
        // abort lands exactly at that yield and the signal is silently
        // lost — deterministically, no socket contention needed.
        struct ResetYield;
        impl Drop for ResetYield {
            fn drop(&mut self) {
                YIELD_BEFORE_EMIT.store(false, Ordering::SeqCst);
            }
        }
        YIELD_BEFORE_EMIT.store(true, Ordering::SeqCst);
        let _reset = ResetYield;
        let (_wired, _faults) = wired(&bus).await;
        let client_conn = bus.connect().await;
        let client = NotificationsClientProxy::new(&client_conn)
            .await
            .expect("proxy");
        let mut closed_stream = client.receive_notification_closed().await.expect("stream");

        let id = client
            .notify(
                "app",
                0,
                "",
                "expiring under contention",
                "",
                Vec::new(),
                HashMap::new(),
                250,
            )
            .await
            .expect("Notify");
        assert_eq!(next_closed(&mut closed_stream).await, (id, 1));
        // The reset guard clears YIELD_BEFORE_EMIT on every exit path.
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn invoke_round_trips_action_invoked_and_closes_unless_resident() {
        let Some(bus) = PrivateBus::spawn().await else {
            return;
        };
        let (wired, _faults) = wired(&bus).await;
        let client_conn = bus.connect().await;
        let client = NotificationsClientProxy::new(&client_conn)
            .await
            .expect("proxy");
        let mut closed_stream = client.receive_notification_closed().await.expect("stream");
        let mut invoked_stream = client.receive_action_invoked().await.expect("stream");

        let actions = vec!["reply".into(), "Reply".into()];
        let id = client
            .notify(
                "app",
                0,
                "",
                "with action",
                "",
                actions.clone(),
                HashMap::new(),
                0,
            )
            .await
            .expect("Notify");
        let (rc, body) = verb(
            &wired.server.shared,
            "notify.invoke",
            json!({
                "id": id, "action": "reply",
            }),
        )
        .await;
        assert_eq!(rc, 0, "{body}");
        assert_eq!(body["closed"], true);
        assert_eq!(
            next_invoked(&mut invoked_stream).await,
            (id, "reply".to_string())
        );
        assert_eq!(next_closed(&mut closed_stream).await, (id, 2));
        assert_eq!(prop(&wired.server.shared, "count").await, json!(0));

        // The resident hint keeps the notification open.
        let resident = client
            .notify("app", 0, "", "resident", "", actions, resident_hint(), 0)
            .await
            .expect("resident Notify");
        let (rc, body) = verb(
            &wired.server.shared,
            "notify.invoke",
            json!({
                "id": resident, "action": "reply",
            }),
        )
        .await;
        assert_eq!(rc, 0, "{body}");
        assert_eq!(body["closed"], false);
        assert_eq!(
            next_invoked(&mut invoked_stream).await,
            (resident, "reply".to_string())
        );
        no_signal_within(&mut closed_stream).await;
        assert_eq!(prop(&wired.server.shared, "count").await, json!(1));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn notify_send_creates_a_first_class_notification() {
        let Some(bus) = PrivateBus::spawn().await else {
            return;
        };
        let (wired, _faults) = wired(&bus).await;
        let client_conn = bus.connect().await;
        let client = NotificationsClientProxy::new(&client_conn)
            .await
            .expect("proxy");
        let mut invoked_stream = client.receive_action_invoked().await.expect("stream");

        let (rc, body) = verb(
            &wired.server.shared,
            "notify.send",
            json!({
                "summary": "mesh summary",
                "body": "mesh body",
                "app": "mesh-node",
                "urgency": "critical",
                "timeout": "never",
                "actions": [["reply", "Reply"]],
            }),
        )
        .await;
        assert_eq!(rc, 0, "{body}");
        let id = body["id"].as_u64().expect("an id was assigned") as u32;

        let shared = &wired.server.shared;
        assert_eq!(
            prop(shared, &format!("n{id}.summary")).await,
            json!("mesh summary")
        );
        assert_eq!(
            prop(shared, &format!("n{id}.app")).await,
            json!("mesh-node")
        );
        assert_eq!(
            prop(shared, &format!("n{id}.urgency")).await,
            json!("critical")
        );
        assert_eq!(prop(shared, &format!("n{id}.origin")).await, json!("mesh"));
        assert_eq!(
            prop(shared, &format!("n{id}.expires_at")).await,
            Value::Null
        );

        // First-class: a mesh-created notification invokes like any
        // other — the D-Bus signal is the proof.
        let (rc, _) = verb(
            shared,
            "notify.invoke",
            json!({"id": id, "action": "reply"}),
        )
        .await;
        assert_eq!(rc, 0);
        assert_eq!(
            next_invoked(&mut invoked_stream).await,
            (id, "reply".to_string())
        );

        wait_for_events(&wired.publisher, 1).await;
        let events = wired.publisher.bodies(TOPIC_NOTIFY_CHANGED);
        assert_eq!(events[0]["event"], "notification.created");
        assert_eq!(events[0]["data"]["notification"]["origin"], "mesh");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_owned_name_is_never_replaced() {
        let Some(bus) = PrivateBus::spawn().await else {
            return;
        };
        // A squatter owns the name first — and holds it WITH
        // AllowReplacement, so a regression to a ReplaceExisting claim
        // really would steal the name; this test would catch it.
        let squatter = bus.connect().await;
        squatter
            .request_name_with_flags(
                DBUS_NAME,
                zbus::fdo::RequestNameFlags::AllowReplacement
                    | zbus::fdo::RequestNameFlags::DoNotQueue,
            )
            .await
            .expect("squatter takes the name");
        let dbus = zbus::fdo::DBusProxy::new(&squatter)
            .await
            .expect("fdo proxy");
        let name: zbus::names::BusName<'_> = DBUS_NAME.try_into().expect("valid name");
        let owner_before = dbus.get_name_owner(name.clone()).await.expect("owner");

        let session = bus.connect().await;
        let error = start_server(&session, watch::channel(None).1, MAX_LIVE, MAX_STORED_BYTES)
            .await
            .expect_err("must refuse an owned name");
        assert!(
            format!("{error:#}").contains("already owned"),
            "the error says what to do: {error:#}"
        );
        let owner_after = dbus.get_name_owner(name).await.expect("owner");
        assert_eq!(
            owner_before, owner_after,
            "the existing owner keeps the name"
        );

        // Once the owner leaves, the adapter can take the name over.
        drop(dbus);
        drop(squatter);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let fresh = bus.connect().await;
            match start_server(&fresh, watch::channel(None).1, MAX_LIVE, MAX_STORED_BYTES).await {
                Ok((_server, _faults)) => break,
                Err(retry) if tokio::time::Instant::now() < deadline => {
                    assert!(
                        format!("{retry:#}").contains("already owned"),
                        "unexpected error while waiting for the name: {retry:#}"
                    );
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(retry) => panic!("name never became free: {retry:#}"),
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_the_server_releases_the_name() {
        let Some(bus) = PrivateBus::spawn().await else {
            return;
        };
        let (wired, _faults) = wired(&bus).await;
        assert_eq!(wired.server.shared.status().0, 0);
        drop(wired);

        // The name is free again: a fresh server claims it.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let fresh = bus.connect().await;
            match start_server(&fresh, watch::channel(None).1, MAX_LIVE, MAX_STORED_BYTES).await {
                Ok((_server, _faults)) => break,
                Err(retry) if tokio::time::Instant::now() < deadline => {
                    assert!(
                        format!("{retry:#}").contains("already owned"),
                        "unexpected error while the old name clears: {retry:#}"
                    );
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(retry) => panic!("dropped server did not release the name: {retry:#}"),
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unknown_ids_and_verbs_are_refusals_not_panics() {
        let Some(bus) = PrivateBus::spawn().await else {
            return;
        };
        let (wired, _faults) = wired(&bus).await;
        let shared = &wired.server.shared;

        let (rc, body) = verb(shared, "notify.close", json!({"id": 4242})).await;
        assert_eq!(rc, 10);
        assert!(body["error"].as_str().expect("why").contains("unknown id"));

        let (rc, _) = verb(
            shared,
            "notify.invoke",
            json!({"id": 4242, "action": "default"}),
        )
        .await;
        assert_eq!(rc, 10);
        let (rc, _) = verb(shared, "notify.invoke", json!({"id": 1})).await;
        assert_eq!(rc, 10, "invoke without an action is refused");
        let (rc, _) = verb(shared, "notify.close", Value::Null).await;
        assert_eq!(rc, 10, "close without an id is refused");
        let (rc, body) = verb(shared, "notify.frobnicate", Value::Null).await;
        assert_eq!(rc, 10);
        assert!(
            body["error"]
                .as_str()
                .expect("why")
                .contains("unknown notify verb")
        );
        let (rc, _) = verb(shared, "notify.props.get", json!({"path": "n999.summary"})).await;
        assert_eq!(rc, 10, "props.get of a vanished leaf is a refusal");

        // The D-Bus side stays healthy: CloseNotification of an
        // unknown id is an error reply per spec 1.2 (the caller may be
        // acting on stale state), and the server keeps serving.
        let client_conn = bus.connect().await;
        let client = NotificationsClientProxy::new(&client_conn)
            .await
            .expect("proxy");
        let unknown = client
            .close_notification(999)
            .await
            .expect_err("unknown id: an error reply, not a silent success");
        assert!(
            format!("{unknown}").contains("no notification 999"),
            "the error names the id: {unknown}"
        );
        let id = client
            .notify(
                "app",
                0,
                "",
                "still alive",
                "",
                Vec::new(),
                HashMap::new(),
                0,
            )
            .await
            .expect("the server is still serving");
        assert_eq!(
            prop(shared, &format!("n{id}.summary")).await,
            json!("still alive")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_bus_outage_is_survived_without_losing_the_name() {
        let Some(bus) = PrivateBus::spawn().await else {
            return;
        };
        let session = bus.connect().await;
        let (clients_tx, clients_rx) = watch::channel(None);
        let (server, faults) = start_server(&session, clients_rx, MAX_LIVE, MAX_STORED_BYTES)
            .await
            .expect("notify server starts and claims the name");
        let shared = Arc::clone(&server.shared);
        let scripted = ScriptedBus::default();
        let first = scripted.add();
        let (stop_tx, stop_rx) = watch::channel(false);
        let run_task = tokio::spawn(notify_run(
            session,
            server,
            clients_tx,
            faults,
            scripted.clone(),
            stop_rx,
            Duration::from_millis(50),
        ));
        let poll_conn = bus.connect().await;
        wait_name_owned(&poll_conn).await;
        let client_conn = bus.connect().await;
        let client = NotificationsClientProxy::new(&client_conn)
            .await
            .expect("proxy");

        // Before the outage: one notification lands, one verb is served
        // through the run's own Bus session.
        client
            .notify(
                "app",
                0,
                "",
                "before the outage",
                "",
                Vec::new(),
                HashMap::new(),
                0,
            )
            .await
            .expect("Notify");
        wait_for_events(&first.publisher, 1).await;
        first
            .sender
            .send(command("notify.ping", Value::Null))
            .expect("command reaches the run");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while first.publisher.replies_for("notify.ping").is_empty() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the run never replied to notify.ping"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(first.publisher.replies_for("notify.ping")[0].0, 0);

        // The outage: the Bus publish fails (fault → reconnect).
        first.publisher.fail_next(1);
        client
            .notify(
                "app",
                0,
                "",
                "lost to the publish failure",
                "",
                Vec::new(),
                HashMap::new(),
                0,
            )
            .await
            .expect("Notify still works — the failure is on the Bus side");
        let second = scripted.add();

        // The run SURVIVED: the name is still owned, the notifications
        // are intact (none was closed for the outage's sake), and
        // Notify keeps being served — the D-Bus side never blinked.
        tokio::time::sleep(Duration::from_millis(200)).await;
        wait_name_owned(&poll_conn).await;
        assert_eq!(shared.status().0, 2, "both notifications are still live");
        let (_, list) = verb(&shared, "notify.list", Value::Null).await;
        assert_eq!(list["count"], 2);

        // After the reconnect: the next change publishes on the NEW
        // Bus client, and the props diff carries the accumulated
        // change across the outage window (baseline survived).
        client
            .notify(
                "app",
                0,
                "",
                "after the outage",
                "",
                Vec::new(),
                HashMap::new(),
                0,
            )
            .await
            .expect("Notify");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while second
            .publisher
            .bodies(&props_changed_topic(BUS_SERVICE))
            .is_empty()
        {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the accumulated props diff never published after reconnect"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let diffs = second.publisher.bodies(&props_changed_topic(BUS_SERVICE));
        assert!(
            diffs
                .iter()
                .any(|diff| diff["path"] == "count" && diff["old"] == 1 && diff["new"] == 3),
            "the diff covers the outage window (count 1 -> 3): {diffs:?}"
        );
        // And no notification was closed as part of surviving.
        assert!(
            second
                .publisher
                .bodies(TOPIC_NOTIFY_CHANGED)
                .iter()
                .all(|event| event["event"] != "notification.closed"),
            "surviving the outage closed nothing"
        );

        // The run stops cleanly when asked.
        stop_tx.send(true).expect("stop signal");
        let outcome = tokio::time::timeout(Duration::from_secs(5), run_task)
            .await
            .expect("the run ends on stop")
            .expect("run task alive");
        assert!(outcome.is_ok(), "clean stop: {outcome:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stale_session_fault_does_not_tear_down_the_next_session() {
        let Some(bus) = PrivateBus::spawn().await else {
            return;
        };
        let session = bus.connect().await;
        let (clients_tx, clients_rx) = watch::channel(None);
        let (server, faults) = start_server(&session, clients_rx, MAX_LIVE, MAX_STORED_BYTES)
            .await
            .expect("notify server starts and claims the name");
        let shared = Arc::clone(&server.shared);
        let scripted = ScriptedBus::default();
        // The connector hands sessions out LIFO, so the session added
        // LAST is served FIRST.
        let second = scripted.add();
        let first = scripted.add();
        let (stop_tx, stop_rx) = watch::channel(false);
        let run_task = tokio::spawn(notify_run(
            session,
            server,
            clients_tx,
            faults,
            scripted,
            stop_rx,
            Duration::from_millis(50),
        ));
        let poll_conn = bus.connect().await;
        wait_name_owned(&poll_conn).await;
        let client_conn = bus.connect().await;
        let client = NotificationsClientProxy::new(&client_conn)
            .await
            .expect("proxy");

        // A notification whose event publish is HELD: it will fail, but
        // the fault is only delivered when the test releases it.
        first.publisher.fail_next(1);
        first.publisher.hold_next_failure();
        client
            .notify(
                "app",
                0,
                "",
                "held publish",
                "",
                Vec::new(),
                HashMap::new(),
                0,
            )
            .await
            .expect("Notify");
        poll_until(
            Duration::from_secs(5),
            || (first.publisher.held_publishes() == 1).then_some(()),
            "the publish to park on the hold",
        )
        .await;

        // The Bus outage: the command stream ends, the run reconnects —
        // with the held fault still pending.
        drop(first.sender);
        second
            .sender
            .send(command("notify.ping", Value::Null))
            .expect("command reaches the run");
        poll_until(
            Duration::from_secs(5),
            || (!second.publisher.replies_for("notify.ping").is_empty()).then_some(()),
            "session 2 to be installed and serving",
        )
        .await;

        // NOW the held publish fails: a fault stamped with session 1's
        // generation, arriving while session 2 is healthy. It must be
        // ignored — session 2 keeps serving.
        first.publisher.release_held_publish();
        second
            .sender
            .send(command("notify.ping", Value::Null))
            .expect("command reaches the run");
        poll_until(
            Duration::from_secs(5),
            || (second.publisher.replies_for("notify.ping").len() >= 2).then_some(()),
            "session 2 to still be serving after the stale fault",
        )
        .await;
        assert!(
            !run_task.is_finished(),
            "the run itself is untouched by the stale fault"
        );

        stop_tx.send(true).expect("stop signal");
        let outcome = tokio::time::timeout(Duration::from_secs(5), run_task)
            .await
            .expect("the run ends on stop")
            .expect("run task alive");
        assert!(outcome.is_ok(), "clean stop: {outcome:?}");
        assert_eq!(shared.status().0, 1, "the held-event notification is live");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_publish_fault_does_not_cancel_an_in_flight_dispatch() {
        let Some(bus) = PrivateBus::spawn().await else {
            return;
        };
        let session = bus.connect().await;
        let (clients_tx, clients_rx) = watch::channel(None);
        let (server, faults) = start_server(&session, clients_rx, MAX_LIVE, MAX_STORED_BYTES)
            .await
            .expect("notify server starts and claims the name");
        let shared = Arc::clone(&server.shared);
        let scripted = ScriptedBus::default();
        let second = scripted.add();
        let first = scripted.add();
        let (stop_tx, stop_rx) = watch::channel(false);
        let run_task = tokio::spawn(notify_run(
            session,
            server,
            clients_tx,
            faults,
            scripted,
            stop_rx,
            Duration::from_millis(50),
        ));
        let poll_conn = bus.connect().await;
        wait_name_owned(&poll_conn).await;
        let client_conn = bus.connect().await;
        let client = NotificationsClientProxy::new(&client_conn)
            .await
            .expect("proxy");

        // Reserve a fault: an event publish that parks, then fails.
        first.publisher.fail_next(1);
        first.publisher.hold_next_failure();
        client
            .notify(
                "app",
                0,
                "",
                "held event",
                "",
                Vec::new(),
                HashMap::new(),
                0,
            )
            .await
            .expect("Notify");
        poll_until(
            Duration::from_secs(5),
            || (first.publisher.held_publishes() == 1).then_some(()),
            "the publish to park on the hold",
        )
        .await;

        // A notify.send whose reply is held at a gate: the dispatch is
        // provably in flight (state applied, reply not yet sent).
        first.publisher.hold_replies_at_gate();
        first
            .sender
            .send(command("notify.send", json!({"summary": "mid-dispatch"})))
            .expect("command reaches the run");
        poll_until(
            Duration::from_secs(5),
            || (first.publisher.held_replies() == 1).then_some(()),
            "the dispatch to park at the reply gate",
        )
        .await;
        assert_eq!(
            shared.status().0,
            2,
            "the dispatched create is applied before its reply goes out"
        );

        // The fault lands MID-dispatch. The dispatch must complete: the
        // reply is not cancelled by the fault.
        first.publisher.release_held_publish();
        first.publisher.release_held_replies();
        poll_until(
            Duration::from_secs(5),
            || (!first.publisher.replies_for("notify.send").is_empty()).then_some(()),
            "the in-flight dispatch to finish and reply",
        )
        .await;
        assert_eq!(
            first.publisher.replies_for("notify.send")[0].0,
            0,
            "the reply went out, not a cancellation"
        );

        // The fault is acted on BETWEEN commands: the run reconnects to
        // session 2 and keeps serving.
        second
            .sender
            .send(command("notify.ping", Value::Null))
            .expect("command reaches the run");
        poll_until(
            Duration::from_secs(5),
            || (!second.publisher.replies_for("notify.ping").is_empty()).then_some(()),
            "the run to reconnect and keep serving",
        )
        .await;

        stop_tx.send(true).expect("stop signal");
        let outcome = tokio::time::timeout(Duration::from_secs(5), run_task)
            .await
            .expect("the run ends on stop")
            .expect("run task alive");
        assert!(outcome.is_ok(), "clean stop: {outcome:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dead_publisher_ends_the_run() {
        let Some(bus) = PrivateBus::spawn().await else {
            return;
        };
        let session = bus.connect().await;
        let (clients_tx, clients_rx) = watch::channel(None);
        let (server, faults) = start_server(&session, clients_rx, MAX_LIVE, MAX_STORED_BYTES)
            .await
            .expect("notify server starts and claims the name");
        let shared = Arc::clone(&server.shared);
        let scripted = ScriptedBus::default();
        scripted.add();
        let (_stop_tx, stop_rx) = watch::channel(false);
        let run_task = tokio::spawn(notify_run(
            session,
            server,
            clients_tx,
            faults,
            scripted,
            stop_rx,
            Duration::from_millis(50),
        ));
        let poll_conn = bus.connect().await;
        wait_name_owned(&poll_conn).await;
        let client_conn = bus.connect().await;
        let client = NotificationsClientProxy::new(&client_conn)
            .await
            .expect("proxy");

        // The publisher task dies (a panic). The run must end with a
        // clear error instead of silently carrying on with publishing
        // halted.
        shared.panic_publisher.store(true, Ordering::SeqCst);
        client
            .notify(
                "app",
                0,
                "",
                "panic trigger",
                "",
                Vec::new(),
                HashMap::new(),
                0,
            )
            .await
            .expect("Notify");
        let outcome = tokio::time::timeout(Duration::from_secs(5), run_task)
            .await
            .expect("the run ends when the publisher dies")
            .expect("run task alive");
        let error = outcome.expect_err("a dead publisher ends the run with Err");
        assert!(
            format!("{error:#}").contains("publisher"),
            "the error names the publisher: {error:#}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_replaces_keep_an_expiring_record_expirable() {
        // No D-Bus needed: this races NotifyShared::create itself.
        // Concurrent replaces of one id alternate long (60 s) and short
        // (1 s) deadlines; every task ENDS on the short form, so the
        // record's final deadline is always 1 s. If arm/disarm could
        // happen outside the record lock (an arm landing out of
        // mutation order), the live record could be left with a 60 s
        // timer — or none at all — and would still be live long past
        // its 1 s deadline.
        let (events_tx, mut events_rx) = mpsc::channel(EVENT_CAPACITY);
        tokio::spawn(async move { while events_rx.recv().await.is_some() {} });
        let shared = Arc::new(NotifyShared::new(
            MAX_LIVE,
            MAX_STORED_BYTES,
            Weak::new(),
            events_tx,
        ));
        let mut start = create_args("start");
        start.timeout = ExpireTimeout::Millis(1000);
        let id = NotifyShared::create(&shared, start).await;

        let mut hammer = Vec::new();
        for round in 0..8 {
            let shared = Arc::clone(&shared);
            hammer.push(tokio::spawn(async move {
                for index in 0..10_000 {
                    let mut long = create_args(&format!("long {round}.{index}"));
                    long.replaces_id = id;
                    long.timeout = ExpireTimeout::Millis(60_000);
                    NotifyShared::create(&shared, long).await;
                    let mut short = create_args(&format!("short {round}.{index}"));
                    short.replaces_id = id;
                    short.timeout = ExpireTimeout::Millis(1000);
                    NotifyShared::create(&shared, short).await;
                    tokio::task::yield_now().await;
                }
            }));
        }
        for task in hammer {
            task.await.expect("hammer task");
        }
        poll_until(
            Duration::from_secs(6),
            || (shared.status().0 == 0).then_some(()),
            "the record to expire on its 1 s deadline after the hammer",
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_bus_death_ends_the_run_promptly() {
        let Some(mut bus) = PrivateBus::spawn().await else {
            return;
        };
        let session = bus.connect().await;
        let (clients_tx, clients_rx) = watch::channel(None);
        let (server, faults) = start_server(&session, clients_rx, MAX_LIVE, MAX_STORED_BYTES)
            .await
            .expect("notify server starts and claims the name");
        let scripted = ScriptedBus::default();
        scripted.add();
        let (_stop_tx, stop_rx) = watch::channel(false);
        let run_task = tokio::spawn(notify_run(
            session,
            server,
            clients_tx,
            faults,
            scripted,
            stop_rx,
            Duration::from_millis(50),
        ));
        let poll_conn = bus.connect().await;
        wait_name_owned(&poll_conn).await;

        // The session bus dies: the run must observe it and end with a
        // clear error, so the supervisor backs off and re-dials.
        bus.kill().await;
        let outcome = tokio::time::timeout(Duration::from_secs(10), run_task)
            .await
            .expect("the run ends promptly after session-bus death")
            .expect("run task alive");
        let error = outcome.expect_err("session-bus death ends the run with Err");
        assert!(
            format!("{error:#}").contains("session bus"),
            "the error says what died: {error:#}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn expiry_timers_are_cancelled_by_replace_close_and_run_end() {
        let Some(bus) = PrivateBus::spawn().await else {
            return;
        };
        let (wired, _faults) = wired(&bus).await;
        let shared = Arc::clone(&wired.server.shared);
        let client_conn = bus.connect().await;
        let client = NotificationsClientProxy::new(&client_conn)
            .await
            .expect("proxy");

        // Replace spam: 50 replaces of one id with a huge timeout —
        // exactly ONE live timer may exist, not 50 sleepers.
        let id = client
            .notify(
                "app",
                0,
                "",
                "spam me",
                "",
                Vec::new(),
                HashMap::new(),
                3_600_000,
            )
            .await
            .expect("Notify");
        for index in 0..50 {
            client
                .notify(
                    "app",
                    id,
                    "",
                    &format!("replace {index}"),
                    "",
                    Vec::new(),
                    HashMap::new(),
                    3_600_000,
                )
                .await
                .expect("replace Notify");
        }
        assert_eq!(shared.timer_count(), 1, "one live timer after 50 replaces");
        // And the 50 superseded timer tasks are actually FINISHED
        // (aborted mid-sleep), not just overwritten in the map: every
        // superseded timer's drop guard has fired (the 51st, current
        // timer is still sleeping).
        poll_until(
            Duration::from_secs(5),
            || (shared.timers_finished() >= 50).then_some(()),
            "the 50 superseded timer tasks to finish",
        )
        .await;

        // Close: the last timer goes too.
        client
            .close_notification(id)
            .await
            .expect("CloseNotification");
        assert_eq!(shared.timer_count(), 0, "close cancels the timer");
        poll_until(
            Duration::from_secs(5),
            || (shared.timers_finished() >= 51).then_some(()),
            "the close-cancelled timer task to finish",
        )
        .await;

        // Re-arm once more, then end the run: every timer dies with it.
        client
            .notify(
                "app",
                0,
                "",
                "still armed",
                "",
                Vec::new(),
                HashMap::new(),
                3_600_000,
            )
            .await
            .expect("Notify");
        assert_eq!(shared.timer_count(), 1);
        drop(wired);
        assert_eq!(shared.timer_count(), 0, "all timers die with the run");
        poll_until(
            Duration::from_secs(5),
            || (shared.timers_finished() >= 52).then_some(()),
            "the run-end-aborted timer task to finish",
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stop_is_honoured_mid_dispatch_without_waiting_the_reply_budget() {
        let Some(bus) = PrivateBus::spawn().await else {
            return;
        };
        let session = bus.connect().await;
        let (clients_tx, clients_rx) = watch::channel(None);
        let (server, faults) = start_server(&session, clients_rx, MAX_LIVE, MAX_STORED_BYTES)
            .await
            .expect("notify server starts and claims the name");
        let scripted = ScriptedBus::default();
        let session_one = scripted.add();
        session_one.publisher.hang_replies();
        let (stop_tx, stop_rx) = watch::channel(false);
        let run_task = tokio::spawn(notify_run(
            session,
            server,
            clients_tx,
            faults,
            scripted,
            stop_rx,
            Duration::from_millis(50),
        ));
        let poll_conn = bus.connect().await;
        wait_name_owned(&poll_conn).await;

        // A command whose reply hangs forever (a wedged broker): the
        // in-flight dispatch is raced against shutdown, so the stop is
        // honoured immediately — not after the 60 s reply budget.
        session_one
            .sender
            .send(command("notify.ping", Value::Null))
            .expect("command reaches the run");
        tokio::time::sleep(Duration::from_millis(200)).await;
        stop_tx.send(true).expect("stop signal");
        let outcome = tokio::time::timeout(Duration::from_secs(2), run_task)
            .await
            .expect("stop preempts a hung in-flight reply")
            .expect("run task alive");
        assert!(outcome.is_ok(), "clean stop: {outcome:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn optional_hints_project_as_leaves_without_their_payloads() {
        let Some(bus) = PrivateBus::spawn().await else {
            return;
        };
        let (wired, _faults) = wired(&bus).await;
        let client_conn = bus.connect().await;
        let client = NotificationsClientProxy::new(&client_conn)
            .await
            .expect("proxy");
        let shared = &wired.server.shared;

        let mut hints = HashMap::new();
        let string_hint = |text: &str| {
            OwnedValue::try_from(zbus::zvariant::Value::new(text)).expect("a string hint builds")
        };
        hints.insert("transient".to_string(), OwnedValue::from(true));
        hints.insert("desktop-entry".to_string(), string_hint("org.example.app"));
        hints.insert(
            "image-path".to_string(),
            string_hint("file:///tmp/example.png"),
        );
        hints.insert("image-data".to_string(), string_hint("not-the-pixels"));
        let id = client
            .notify("app", 0, "", "hinted", "", Vec::new(), hints, 0)
            .await
            .expect("Notify");
        let base = format!("n{id}");
        assert_eq!(
            prop(shared, &format!("{base}.transient")).await,
            json!(true)
        );
        assert_eq!(
            prop(shared, &format!("{base}.desktop_entry")).await,
            json!("org.example.app")
        );
        assert_eq!(
            prop(shared, &format!("{base}.image_path")).await,
            json!("file:///tmp/example.png")
        );
        assert_eq!(
            prop(shared, &format!("{base}.image_data")).await,
            json!(true),
            "image-data is recorded as PRESENT"
        );
        // Raw pixels never enter props: no leaf anywhere carries the
        // payload value.
        let (rc, body) = verb(shared, "notify.props.list", Value::Null).await;
        assert_eq!(rc, 0, "{body}");
        let paths = body.as_array().expect("a path list");
        for path in paths {
            let path = path.as_str().expect("a path string");
            assert_ne!(
                prop(shared, path).await,
                json!("not-the-pixels"),
                "no leaf carries the pixel payload ({path})"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_very_short_timeout_is_clamped_to_one_second() {
        let Some(bus) = PrivateBus::spawn().await else {
            return;
        };
        let (wired, _faults) = wired(&bus).await;
        let client_conn = bus.connect().await;
        let client = NotificationsClientProxy::new(&client_conn)
            .await
            .expect("proxy");
        let shared = &wired.server.shared;

        // 10 ms would close before the client has the id; the effective
        // expiry is clamped to at least 1 s.
        let id = client
            .notify("app", 0, "", "blink", "", Vec::new(), HashMap::new(), 10)
            .await
            .expect("Notify");
        let record = shared.records().get(&id).expect("live").clone();
        let ttl = record
            .expires_at
            .expect("a finite expiry")
            .duration_since(SystemTime::now())
            .expect("in the future");
        assert!(
            ttl + Duration::from_millis(50) >= MIN_EXPIRY,
            "the effective expiry is at least {MIN_EXPIRY:?}: {ttl:?}"
        );
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(
            shared.status().0,
            1,
            "still live after 400 ms — the 10 ms timeout was clamped"
        );
        // It does close eventually (the clamp delays, never cancels).
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while shared.status().0 != 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the clamped expiry never fired"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_batch_diff_is_stamped_with_the_batch_last_seq() {
        let Some(bus) = PrivateBus::spawn().await else {
            return;
        };
        // Cap 2: the third create evicts the first — one batch of
        // [Closed(eviction), Created(new)], whose coalesced diff must
        // carry the LAST event_seq of the batch.
        let (wired, _faults) = wired_with_cap(&bus, 2, MAX_STORED_BYTES).await;
        let shared = &wired.server.shared;
        let (_, first) = verb(shared, "notify.send", json!({"summary": "one"})).await;
        let id_one = first["id"].as_u64().expect("id") as u32;
        verb(shared, "notify.send", json!({"summary": "two"})).await;
        wait_for_events(&wired.publisher, 2).await;
        let (_, third) = verb(shared, "notify.send", json!({"summary": "three"})).await;
        let id_three = third["id"].as_u64().expect("id") as u32;
        assert_ne!(id_one, id_three);
        wait_for_events(&wired.publisher, 4).await;

        let events = wired.publisher.bodies(TOPIC_NOTIFY_CHANGED);
        let last_seq = events.last().expect("the batch's events")["event_seq"]
            .as_u64()
            .expect("seq");
        let evicted = events
            .iter()
            .find(|event| event["event"] == "notification.closed")
            .expect("the eviction event");
        assert_eq!(
            evicted["id"].as_u64(),
            Some(u64::from(id_one)),
            "the oldest was evicted"
        );
        // The eviction batch [Closed, Created] publishes ONE coalesced
        // diff stamped with the batch's LAST seq — a subtree gone from
        // the tree diffs at the subtree path, so the removal of the
        // evicted subtree and the addition of the new one both carry it.
        let removed = format!("n{id_one}");
        let added = format!("n{id_three}");
        let removed_stamps = wired.publisher.diff_seqs(&removed);
        assert!(
            removed_stamps.contains(&last_seq),
            "the eviction diff carries the batch's LAST seq ({last_seq}): {removed_stamps:?}"
        );
        assert_eq!(
            wired.publisher.diff_seqs(&added),
            vec![last_seq],
            "the addition diff carries the batch's LAST seq"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_real_notify_send_binary_lands() {
        let Some(bus) = PrivateBus::spawn().await else {
            return;
        };
        let (wired, _faults) = wired(&bus).await;
        let output = tokio::process::Command::new("notify-send")
            .args(["-a", "BinApp", "-t", "3600000", "Real summary", "Real body"])
            .env("DBUS_SESSION_BUS_ADDRESS", &bus.address)
            .output()
            .await;
        let output = match output {
            Ok(output) => output,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                eprintln!("SKIPPED: notify-send not found — install libnotify to run this test");
                return;
            }
            Err(error) => panic!("cannot spawn notify-send: {error}"),
        };
        assert!(
            output.status.success(),
            "notify-send failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let shared = &wired.server.shared;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let records = shared.records();
            if let Some(record) = records.values().next() {
                assert_eq!(record.app, "BinApp");
                assert_eq!(record.summary, "Real summary");
                assert_eq!(record.body, "Real body");
                assert_eq!(record.origin, Origin::Dbus);
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the notify-send notification never landed"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}
