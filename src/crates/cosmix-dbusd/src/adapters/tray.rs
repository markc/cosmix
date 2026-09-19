//! The `tray` adapter — inbound StatusNotifierWatcher + item proxying.
//!
//! Cosmix is the system-tray *host*: this adapter owns
//! `org.kde.StatusNotifierWatcher` on the desktop session bus (refusing to
//! replace an existing owner — a human hands the name over via
//! `dbusd.adapter.disable`/`enable`), accepts StatusNotifierItem
//! registrations (by bus name or by object path, resolving the caller as
//! KDE/libappindicator do), and mirrors every item onto the Bus as the
//! `tray` service: per-item props under `tray.i<n>.*`, `tray.item.*`
//! events, and verbs for activation, scrolling, icons and the item's
//! com.canonical.dbusmenu menu. Item lifetime is tracked by
//! `NameOwnerChanged` — an item whose connection vanishes is removed; no
//! polling anywhere. Every D-Bus call to an item runs under a timeout so
//! a hung app can never wedge the adapter.
//!
//! Only three things end a run: the stop signal, session-bus death
//! (observed on the zbus connection's closed signal), or a real internal
//! fault. A Bus (mesh) outage never does: the broker reconnects the
//! `tray` Bus client inside the run and the publisher keeps its diff
//! baseline, so items survive the outage instead of being wiped by a
//! restart.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Result, anyhow};
use cosmix_client::{IncomingCommand, NodedClient};
use cosmix_props_core::publish::{build_props_changed_message, props_changed_topic};
use cosmix_props_core::tree::build_snapshot;
use cosmix_props_core::{PropDescribe, PropPath, PropTree, PropType, PropValue};
use futures_util::StreamExt;
use serde_json::{Value as Json, json};
use tokio::sync::{mpsc, watch};
use tokio::task::{JoinHandle, JoinSet};
use zbus::fdo::{self, DBusProxy};
use zbus::message::{Header, Message, Type as MessageType};
use zbus::names::WellKnownName;
use zbus::object_server::SignalEmitter;
use zbus::proxy::{Builder as ProxyBuilder, CacheProperties};
use zbus::zvariant::{self, Dict, OwnedValue, Value};
use zbus::{Connection, MatchRule, MessageStream, Proxy, interface};

use crate::adapter::{Adapter, AdapterCtx, BoxRunFuture};

const WATCHER_NAME: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
/// Only the tests need the interface name as a value: the interface
/// macro below takes it as a literal.
#[cfg(test)]
const WATCHER_IFACE: &str = "org.kde.StatusNotifierWatcher";
const ITEM_IFACE: &str = "org.kde.StatusNotifierItem";
const MENU_IFACE: &str = "com.canonical.dbusmenu";
/// Where items live when registered by bus name (the SNI convention).
const DEFAULT_ITEM_PATH: &str = "/StatusNotifierItem";
/// Items registering by object path sit on the caller's own connection.
const DBUS_SERVICE: &str = "org.freedesktop.DBus";
/// The sentinel some items use in the Menu property for "no menu".
const NO_DBUSMENU_PATH: &str = "/NO_DBUSMENU";
/// dbusmenu property names requested from GetLayout. `icon-data` (a
/// pixmap per node) is deliberately absent: menu labels are tiny, node
/// pixmaps are not, and the tray host does not render menus.
const MENU_PROPERTY_NAMES: [&str; 7] = [
    "type",
    "label",
    "enabled",
    "visible",
    "children-display",
    "toggle-type",
    "toggle-state",
];

pub const BUS_SERVICE: &str = "tray";
pub const TOPIC_ITEM_ADDED: &str = "tray.item.added";
pub const TOPIC_ITEM_CHANGED: &str = "tray.item.changed";
pub const TOPIC_ITEM_REMOVED: &str = "tray.item.removed";

// Resource bounds. An SNI item is a small metadata record; these caps keep
// a hostile or broken app from ballooning the adapter's memory.
const MAX_ITEMS: usize = 64;
/// Per registering connection: one client must not be able to squat the
/// whole tray with items it does not own.
const MAX_ITEMS_PER_OWNER: usize = 8;
/// External StatusNotifierHost registrations, pruned when their
/// connection vanishes.
const MAX_HOSTS: usize = 64;
const MAX_STRING_CHARS: usize = 4096;
const MAX_PIXMAP_BYTES: usize = 1024 * 1024;
/// Cap on any single raw D-Bus reply body BEFORE deserialization: zbus
/// accepts messages up to 128 MiB, and deserializing a pixmap as
/// per-byte values amplifies it before the 1 MiB pixmap cap could
/// apply. A reply over the cap is dropped, never decoded.
const MAX_RAW_REPLY_BYTES: usize = 4 * 1024 * 1024;
/// Tighter cap for pixmap-bearing property replies (`IconPixmap`,
/// `ToolTip`): one stored pixmap is at most [`MAX_PIXMAP_BYTES`], so a
/// larger total can only be hostile or broken. Applied for the same
/// reason as [`MAX_RAW_REPLY_BYTES`].
const MAX_PIXMAP_REPLY_BYTES: usize = MAX_PIXMAP_BYTES + MAX_PIXMAP_BYTES / 4;
const MAX_MENU_NODES: usize = 512;
/// How many recent events the ring keeps for `tray.info` / tests.
const RECENT_EVENTS: usize = 128;

// Every D-Bus call to an item is bounded: a hung app surfaces as a
// refusal, never as a wedged adapter.
const ITEM_CALL_TIMEOUT: Duration = Duration::from_secs(2);
const MENU_CALL_TIMEOUT: Duration = Duration::from_secs(3);
const REFRESH_BUDGET: Duration = Duration::from_secs(3);
/// Owner resolution inside the watcher interface: bounded so a wedged
/// resolution surfaces as an error reply, never a hang.
const RESOLVE_TIMEOUT: Duration = Duration::from_millis(250);
/// Minimum spacing between property refreshes of one item: an item
/// flooding New* signals cannot loop its own refresh.
const MIN_REFRESH_INTERVAL: Duration = Duration::from_millis(250);
/// Property refreshes in flight at once (each holds a decoded reply).
const MAX_CONCURRENT_REFRESHES: usize = 8;
/// Verb dispatches in flight at once on the Bus side.
const MAX_CONCURRENT_VERBS: usize = 8;

// Bus-side timings, mirroring the daemon citizen's broker loop.
const BUS_PUBLISH_TIMEOUT: Duration = Duration::from_secs(60);
const BUS_RECONNECT_DELAY: Duration = Duration::from_secs(60);
const BUS_CLOSE_TIMEOUT: Duration = Duration::from_secs(3);
/// How long `run_until` waits for the broker to close its client before
/// aborting it (the supervisor's stop window is the outer bound).
const BROKER_DRAIN: Duration = Duration::from_secs(4);

/// Shared item state: adapters' zbus threads, the interface getters, the
/// Bus dispatch and the publisher all read/write under one lock.
type TrayStore = Arc<Mutex<TrayState>>;

fn lock_state(store: &TrayStore) -> MutexGuard<'_, TrayState> {
    // Poison-recover like the supervision registry: the map stays
    // structurally sound across a panicking writer.
    store
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ===========================================================================
// Pure core: items, events, state
// ===========================================================================

/// One StatusNotifierItem pixmap: `a(iiay)` — width, height and ARGB32
/// data in network byte order (big-endian), row-major, no row padding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pixmap {
    pub width: i32,
    pub height: i32,
    pub data: Vec<u8>,
}

/// The projection of `org.kde.StatusNotifierItem` this adapter surfaces.
/// `IconPixmap` is deliberately NOT raw pixels in the props: only the
/// largest size is recorded (as `pixmap_width`/`pixmap_height`) and the
/// pixels are served by the `tray.icon` verb.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ItemProps {
    pub id: String,
    pub title: String,
    pub category: String,
    /// SNI default when the item does not implement it: "Active".
    pub status: String,
    pub icon_name: String,
    pub icon_theme_path: String,
    pub attention_icon_name: String,
    pub tooltip_title: String,
    pub tooltip_description: String,
    pub tooltip_icon: String,
    /// Object path of the item's com.canonical.dbusmenu menu, if any.
    pub menu: Option<String>,
    pub item_is_menu: bool,
    pub pixmap: Option<Pixmap>,
}

impl ItemProps {
    fn clamp_strings(&mut self) {
        for field in [
            &mut self.id,
            &mut self.title,
            &mut self.category,
            &mut self.status,
            &mut self.icon_name,
            &mut self.icon_theme_path,
            &mut self.attention_icon_name,
            &mut self.tooltip_title,
            &mut self.tooltip_description,
            &mut self.tooltip_icon,
        ] {
            truncate(field, MAX_STRING_CHARS);
        }
        if let Some(menu) = &mut self.menu {
            truncate(menu, MAX_STRING_CHARS);
        }
    }
}

fn truncate(string: &mut String, limit: usize) {
    if string.chars().count() > limit {
        *string = string.chars().take(limit).collect();
    }
}

/// One tracked tray item. `service` is the bus name under which it
/// registered (a well-known name, or the caller's unique name for the
/// path form); `owner` is the unique name of the connection currently
/// serving it — the key for NameOwnerChanged reaping. Identity (and
/// dedup) is the `(service, path)` pair: a connection may host several
/// path-form items.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayItem {
    pub key: String,
    pub service: String,
    pub path: String,
    pub owner: String,
    pub props: ItemProps,
}

impl TrayItem {
    /// The name under which the watcher advertises the item —
    /// `service + path` for every item, matching KDE's watcher (so a
    /// panel can tell two indicators on one connection apart).
    pub fn registered_name(&self) -> String {
        format!("{}{}", self.service, self.path)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemEventKind {
    Added,
    Changed,
    Removed,
}

impl ItemEventKind {
    fn event_name(self) -> &'static str {
        match self {
            Self::Added => "item.added",
            Self::Changed => "item.changed",
            Self::Removed => "item.removed",
        }
    }

    fn topic(self) -> &'static str {
        match self {
            Self::Added => TOPIC_ITEM_ADDED,
            Self::Changed => TOPIC_ITEM_CHANGED,
            Self::Removed => TOPIC_ITEM_REMOVED,
        }
    }
}

/// One item lifecycle event, stamped with the per-run monotonic
/// `event_seq` (a gap means events were dropped — re-read the props).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemEvent {
    pub kind: ItemEventKind,
    pub key: String,
    /// The raw bus name the item registered under.
    pub service: String,
    /// The watcher-surface name (`service + path`) the item is
    /// advertised as — what Registered/Unregistered signals carry.
    pub registered: String,
    pub seq: u64,
}

/// Pure item registry: no bus, no zbus — unit-testable alone.
#[derive(Debug, Default)]
pub struct TrayState {
    items: BTreeMap<String, TrayItem>,
    next_key: u64,
    next_event_seq: u64,
    host_registered: bool,
    hosts: BTreeSet<String>,
    recent_events: VecDeque<ItemEvent>,
    /// Property replies refused by the raw-size cap since the run
    /// started (surfaced by `tray.info`).
    oversized_reads: usize,
}

impl TrayState {
    /// Register an item, replacing any existing item with the same
    /// `(service, path)` (re-registration is KDE's update idiom — and a
    /// replacement is always allowed, even at the item cap). A new item
    /// is refused past [`MAX_ITEMS`] or past [`MAX_ITEMS_PER_OWNER`]
    /// items for the registering connection. Returns the fresh key plus
    /// the events (removed first, then added). Keys are `i<n>` with a
    /// never-reused counter: stable for the item's lifetime, gone after
    /// removal.
    pub fn register(
        &mut self,
        service: String,
        path: String,
        owner: String,
        props: ItemProps,
    ) -> std::result::Result<(String, Vec<ItemEvent>), String> {
        let replacing = self
            .items
            .values()
            .any(|item| item.service == service && item.path == path);
        if !replacing {
            if self.items.len() >= MAX_ITEMS {
                return Err(format!(
                    "tray item limit reached ({MAX_ITEMS}); refusing to track {service}{path}"
                ));
            }
            let per_owner = self
                .items
                .values()
                .filter(|item| item.owner == owner)
                .count();
            if per_owner >= MAX_ITEMS_PER_OWNER {
                return Err(format!(
                    "tray per-connection item limit reached ({MAX_ITEMS_PER_OWNER}); \
                     refusing to track {service}{path} for {owner}"
                ));
            }
        }
        let mut events = Vec::new();
        let doomed: Vec<String> = self
            .items
            .values()
            .filter(|item| item.service == service && item.path == path)
            .map(|item| item.key.clone())
            .collect();
        for key in doomed {
            if let Some(item) = self.items.remove(&key) {
                events.push(self.stamp(ItemEventKind::Removed, &item));
            }
        }
        let key = format!("i{}", self.next_key);
        self.next_key += 1;
        let item = TrayItem {
            key: key.clone(),
            service,
            path,
            owner,
            props,
        };
        let event = self.stamp(ItemEventKind::Added, &item);
        let key = item.key.clone();
        self.items.insert(key.clone(), item);
        events.push(event);
        Ok((key, events))
    }

    /// Whether a registration should be accepted — the interface's
    /// synchronous check, so a refusal is a D-Bus error reply rather
    /// than an OK followed by a silent drop.
    pub fn can_accept(&self, service: &str, path: &str, owner: &str) -> bool {
        if self
            .items
            .values()
            .any(|item| item.service == service && item.path == path)
        {
            // Replacement: always accepted (m2).
            return true;
        }
        self.items.len() < MAX_ITEMS
            && self
                .items
                .values()
                .filter(|item| item.owner == owner)
                .count()
                < MAX_ITEMS_PER_OWNER
    }

    /// Apply a fresh property read. No event when nothing changed — a
    /// signal storm on an unchanged item stays invisible.
    pub fn apply_props(&mut self, key: &str, props: ItemProps) -> Vec<ItemEvent> {
        let Some(item) = self.items.get_mut(key) else {
            return Vec::new();
        };
        if item.props == props {
            return Vec::new();
        }
        let (key, service, path) = (item.key.clone(), item.service.clone(), item.path.clone());
        item.props = props;
        vec![self.stamp_parts(ItemEventKind::Changed, key, service, path)]
    }

    /// Reap items after a `NameOwnerChanged`: an item goes when its
    /// registered bus name is released or moves to another owner, or
    /// when its connection itself vanishes (a unique name losing its
    /// owner). A connection merely releasing an *unrelated* name (MPRIS,
    /// anything) reaps nothing. Host registrations from a vanished
    /// connection are pruned here too.
    pub fn remove_by_bus_change(
        &mut self,
        name: &str,
        old_owner: &str,
        new_owner: &str,
    ) -> Vec<ItemEvent> {
        // A unique name going empty-owner IS its connection dying.
        let connection_vanished = new_owner.is_empty() && name == old_owner;
        let mut events = Vec::new();
        let doomed: Vec<String> = self
            .items
            .values()
            .filter(|item| {
                (item.service == name && new_owner.is_empty())
                    || (item.service == name && !old_owner.is_empty() && !new_owner.is_empty())
                    || (connection_vanished && item.owner == old_owner)
            })
            .map(|item| item.key.clone())
            .collect();
        for key in doomed {
            if let Some(item) = self.items.remove(&key) {
                events.push(self.stamp(ItemEventKind::Removed, &item));
            }
        }
        if connection_vanished {
            self.hosts.remove(name);
        }
        events
    }

    fn stamp(&mut self, kind: ItemEventKind, item: &TrayItem) -> ItemEvent {
        self.stamp_parts(
            kind,
            item.key.clone(),
            item.service.clone(),
            item.path.clone(),
        )
    }

    fn stamp_parts(
        &mut self,
        kind: ItemEventKind,
        key: String,
        service: String,
        path: String,
    ) -> ItemEvent {
        self.next_event_seq += 1;
        let event = ItemEvent {
            kind,
            key,
            service: service.clone(),
            registered: format!("{service}{path}"),
            seq: self.next_event_seq,
        };
        self.recent_events.push_back(event.clone());
        while self.recent_events.len() > RECENT_EVENTS {
            self.recent_events.pop_front();
        }
        event
    }

    pub fn item(&self, key: &str) -> Option<&TrayItem> {
        self.items.get(key)
    }

    /// The item served at `owner`+`path` — how an incoming SNI signal is
    /// routed back to its item (signals carry the sender's unique name).
    pub fn item_by_address(&self, owner: &str, path: &str) -> Option<&TrayItem> {
        self.items
            .values()
            .find(|item| item.owner == owner && item.path == path)
    }

    /// Items in registration order (key order, numerically).
    pub fn items_in_order(&self) -> Vec<&TrayItem> {
        let mut items: Vec<&TrayItem> = self.items.values().collect();
        items.sort_by_key(|item| key_number(&item.key));
        items
    }

    /// The `RegisteredStatusNotifierItems` surface: `service + path` for
    /// every item (the caller's unique name + path for the path form),
    /// matching KDE's watcher.
    pub fn registered_names(&self) -> Vec<String> {
        self.items_in_order()
            .into_iter()
            .map(|item| item.registered_name())
            .collect()
    }

    pub fn count(&self) -> usize {
        self.items.len()
    }

    pub fn event_seq(&self) -> u64 {
        self.next_event_seq
    }

    pub fn recent_events(&self) -> &VecDeque<ItemEvent> {
        &self.recent_events
    }

    pub fn note_oversized_read(&mut self) {
        self.oversized_reads += 1;
    }

    pub fn oversized_reads(&self) -> usize {
        self.oversized_reads
    }

    /// Cosmix is the host: set once the watcher name is acquired.
    pub fn set_host_registered(&mut self, registered: bool) {
        self.host_registered = registered;
    }

    pub fn host_registered(&self) -> bool {
        self.host_registered
    }

    /// Record an external StatusNotifierHost registration. `Ok(true)`
    /// when this is a new host (the caller emits
    /// StatusNotifierHostRegistered); an error when the host cap is
    /// full.
    pub fn add_host(&mut self, caller: String) -> std::result::Result<bool, String> {
        if self.hosts.contains(&caller) {
            return Ok(false);
        }
        if self.hosts.len() >= MAX_HOSTS {
            return Err(format!("tray host limit reached ({MAX_HOSTS})"));
        }
        self.hosts.insert(caller);
        Ok(true)
    }

    pub fn hosts(&self) -> Vec<String> {
        self.hosts.iter().cloned().collect()
    }
}

fn key_number(key: &str) -> u64 {
    key.strip_prefix('i')
        .and_then(|rest| rest.parse().ok())
        .unwrap_or(u64::MAX)
}

// ===========================================================================
// Props projection: tray.count + tray.i<n>.*
// ===========================================================================

/// Read-only property projection for `tray.props.*`, built per dispatch
/// from a locked-state snapshot (no lock is held while props-core runs).
pub struct TrayProps {
    leaves: Vec<(PropPath, PropValue)>,
}

impl TrayProps {
    pub fn new(state: &TrayState) -> Self {
        let mut leaves = Vec::with_capacity(2 + state.count() * 14);
        push(&mut leaves, "count", (state.count() as u64).into());
        for item in state.items_in_order() {
            let prefix = format!("{}.", item.key);
            let props = &item.props;
            push(
                &mut leaves,
                &format!("{prefix}id"),
                props.id.as_str().into(),
            );
            push(
                &mut leaves,
                &format!("{prefix}title"),
                props.title.as_str().into(),
            );
            push(
                &mut leaves,
                &format!("{prefix}category"),
                props.category.as_str().into(),
            );
            push(
                &mut leaves,
                &format!("{prefix}status"),
                props.status.as_str().into(),
            );
            push(
                &mut leaves,
                &format!("{prefix}icon_name"),
                props.icon_name.as_str().into(),
            );
            push(
                &mut leaves,
                &format!("{prefix}icon_theme_path"),
                props.icon_theme_path.as_str().into(),
            );
            push(
                &mut leaves,
                &format!("{prefix}attention_icon_name"),
                props.attention_icon_name.as_str().into(),
            );
            push(
                &mut leaves,
                &format!("{prefix}tooltip"),
                props.tooltip_title.as_str().into(),
            );
            push(
                &mut leaves,
                &format!("{prefix}has_menu"),
                props.menu.is_some().into(),
            );
            push(
                &mut leaves,
                &format!("{prefix}item_is_menu"),
                props.item_is_menu.into(),
            );
            push(
                &mut leaves,
                &format!("{prefix}service"),
                item.service.as_str().into(),
            );
            push(
                &mut leaves,
                &format!("{prefix}path"),
                item.path.as_str().into(),
            );
            // The pixmap leaves exist only when a pixmap is known: the
            // pixels themselves are served by `tray.icon`, never a prop.
            if let Some(pixmap) = &props.pixmap {
                push(
                    &mut leaves,
                    &format!("{prefix}pixmap_width"),
                    i64::from(pixmap.width).into(),
                );
                push(
                    &mut leaves,
                    &format!("{prefix}pixmap_height"),
                    i64::from(pixmap.height).into(),
                );
            }
        }
        Self { leaves }
    }
}

impl PropTree for TrayProps {
    fn snapshot(&self) -> PropValue {
        build_snapshot(self.leaves.clone())
    }

    fn list(&self) -> Vec<PropPath> {
        self.leaves.iter().map(|(path, _)| path.clone()).collect()
    }

    fn describe(&self, path: &PropPath) -> Option<PropDescribe> {
        if !self.leaves.iter().any(|(candidate, _)| candidate == path) {
            return None;
        }
        let leaf = path.as_str().rsplit('.').next()?;
        let description = match leaf {
            "count" => PropDescribe::leaf(
                path.clone(),
                PropType::Number,
                "Number of tracked StatusNotifierItems.",
            ),
            "id" => PropDescribe::leaf(
                path.clone(),
                PropType::String,
                "The item's SNI Id (its own app-chosen identity).",
            ),
            "title" => PropDescribe::leaf(
                path.clone(),
                PropType::String,
                "Item title; refreshed on its NewTitle signal.",
            ),
            "category" => PropDescribe::leaf(path.clone(), PropType::String, "SNI Category."),
            "status" => {
                let mut description = PropDescribe::leaf(
                    path.clone(),
                    PropType::String,
                    "SNI Status: Active, Passive, or NeedsAttention.",
                );
                description.enum_values = Some(
                    ["Active", "Passive", "NeedsAttention"]
                        .map(String::from)
                        .to_vec(),
                );
                description
            }
            "icon_name" => PropDescribe::leaf(
                path.clone(),
                PropType::String,
                "Themed icon name; refreshed on NewIcon.",
            ),
            "icon_theme_path" => PropDescribe::leaf(
                path.clone(),
                PropType::String,
                "Extra theme search path the item asks hosts to use.",
            ),
            "attention_icon_name" => PropDescribe::leaf(
                path.clone(),
                PropType::String,
                "Icon for the NeedsAttention state; refreshed on NewAttentionIcon.",
            ),
            "tooltip" => PropDescribe::leaf(
                path.clone(),
                PropType::String,
                "The ToolTip title (the description rides tray.list).",
            ),
            "has_menu" => PropDescribe::leaf(
                path.clone(),
                PropType::Bool,
                "Whether the item exposes a com.canonical.dbusmenu menu.",
            ),
            "item_is_menu" => PropDescribe::leaf(
                path.clone(),
                PropType::Bool,
                "The item's ItemIsMenu hint (click opens the menu, not Activate).",
            ),
            "service" => PropDescribe::leaf(
                path.clone(),
                PropType::String,
                "Bus name the item registered under (unique name for path-form items).",
            ),
            "path" => PropDescribe::leaf(
                path.clone(),
                PropType::String,
                "Object path of the item's org.kde.StatusNotifierItem.",
            ),
            "pixmap_width" | "pixmap_height" => PropDescribe::leaf(
                path.clone(),
                PropType::Number,
                "Largest IconPixmap size; pixels via the tray.icon verb.",
            ),
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

// ===========================================================================
// Menu layout: com.canonical.dbusmenu tree → JSON
// ===========================================================================

/// One menu node from GetLayout `(ia{sv}av)`: id, sparse property dict
/// (dbusmenu defaults apply for absent keys), variant-wrapped children.
/// A node whose children were cut by the budget or a malformed subtree
/// carries `"truncated": true` — a partial tree is never presented as
/// complete.
pub(crate) fn menu_node_json(value: &Value<'_>, budget: &mut usize) -> Option<Json> {
    let value = match value {
        Value::Value(inner) => inner.as_ref(),
        node => node,
    };
    let Value::Structure(structure) = value else {
        return None;
    };
    let fields = structure.fields();
    if fields.len() != 3 || *budget == 0 {
        return None;
    }
    *budget -= 1;
    let Value::I32(id) = &fields[0] else {
        return None;
    };
    let Value::Dict(properties) = &fields[1] else {
        return None;
    };
    let Value::Array(children) = &fields[2] else {
        return None;
    };
    let mut label = dict_str(properties, "label").unwrap_or_default();
    truncate(&mut label, MAX_STRING_CHARS);
    let mut node_type = dict_str(properties, "type").unwrap_or_else(|| "standard".into());
    truncate(&mut node_type, MAX_STRING_CHARS);
    let mut toggle_type = dict_str(properties, "toggle-type");
    if let Some(toggle_type) = &mut toggle_type {
        truncate(toggle_type, MAX_STRING_CHARS);
    }
    let mut truncated = false;
    let mut children_json = Vec::new();
    for child in children.iter() {
        match menu_node_json(child, budget) {
            Some(node) => children_json.push(node),
            // Budget spent or a malformed subtree: stop and say so
            // instead of presenting a silently partial tree.
            None => {
                truncated = true;
                break;
            }
        }
    }
    Some(json!({
        "id": id,
        "label": label,
        // dbusmenu defaults: absent means enabled/visible.
        "enabled": dict_bool(properties, "enabled").unwrap_or(true),
        "visible": dict_bool(properties, "visible").unwrap_or(true),
        "type": node_type,
        "toggle_type": toggle_type,
        "toggle_state": dict_i32(properties, "toggle-state"),
        "truncated": truncated,
        "children": children_json,
    }))
}

/// Lookup in a sparse dbusmenu property dict. Values in an `a{sv}` ride
/// the wire (and zvariant's builders) as variants — unwrap one level,
/// and accept the bare form too.
fn dict_with<T>(
    dict: &Dict<'_, '_>,
    key: &str,
    extract: impl Fn(&Value<'_>) -> Option<T>,
) -> Option<T> {
    for (candidate, entry) in dict.iter() {
        if let Value::Str(name) = candidate
            && name.as_str() == key
        {
            let entry = match entry {
                Value::Value(inner) => inner.as_ref(),
                value => value,
            };
            return extract(entry);
        }
    }
    None
}

fn dict_str(dict: &Dict<'_, '_>, key: &str) -> Option<String> {
    dict_with(dict, key, |value| match value {
        Value::Str(text) => Some(text.to_string()),
        _ => None,
    })
}

fn dict_bool(dict: &Dict<'_, '_>, key: &str) -> Option<bool> {
    dict_with(dict, key, |value| match value {
        Value::Bool(flag) => Some(*flag),
        _ => None,
    })
}

fn dict_i32(dict: &Dict<'_, '_>, key: &str) -> Option<i32> {
    dict_with(dict, key, |value| match value {
        Value::I32(number) => Some(*number),
        _ => None,
    })
}

// ===========================================================================
// base64 (pixels for tray.icon; hand-rolled to keep the dependency set)
// ===========================================================================

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub(crate) fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(BASE64[(triple >> 18) as usize & 0x3f] as char);
        out.push(BASE64[(triple >> 12) as usize & 0x3f] as char);
        out.push(if chunk.len() > 1 {
            BASE64[(triple >> 6) as usize & 0x3f] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            BASE64[triple as usize & 0x3f] as char
        } else {
            '='
        });
    }
    out
}

// ===========================================================================
// D-Bus side: the watcher interface
// ===========================================================================

/// Registration requests hop from the interface (which must reply fast)
/// to the run loop, which mutates state, emits signals and publishes.
/// The interface fully resolves a registration — service, path and
/// owner — because the run loop must never await the bus (a resolution
/// in the loop stalls the item-signal drain, and with >128 queued item
/// signals zbus's reader blocks on the full stream: the reply never
/// arrives and the adapter wedges). Here, instead, resolution runs on
/// zbus's own dispatch task, bounded by [`RESOLVE_TIMEOUT`].
enum WatcherMsg {
    RegisterItem {
        service: String,
        path: String,
        owner: String,
    },
    RegisterHost {
        caller: String,
    },
}

struct WatcherIface {
    store: TrayStore,
    registrations: mpsc::Sender<WatcherMsg>,
}

/// Errors that just mean "the item does not implement this property":
/// the SNI default applies instead of failing the read.
fn is_unknown_property(error: &zbus::Error) -> bool {
    matches!(
        error,
        zbus::Error::MethodError(name, _, _)
            if name.as_str() == "org.freedesktop.DBus.Error.UnknownProperty"
                || name.as_str() == "org.freedesktop.DBus.Error.InvalidArgs"
    )
}

#[interface(name = "org.kde.StatusNotifierWatcher")]
impl WatcherIface {
    /// `RegisterStatusNotifierItem(s)` — the argument is the item's bus
    /// name (object at /StatusNotifierItem) or its object path (item on
    /// the caller's own connection), as KDE/libappindicator accept both.
    /// Everything checkable synchronously is checked here and refused
    /// with a D-Bus error — never an OK reply followed by a silent
    /// drop. A name-form registration must come from the name's owner:
    /// one client cannot squat the tray with items served by others.
    async fn register_status_notifier_item(
        &self,
        service_or_path: &str,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> fdo::Result<()> {
        let caller = header
            .sender()
            .map(|sender| sender.to_string())
            .ok_or_else(|| {
                fdo::Error::Failed("RegisterStatusNotifierItem needs a sender".into())
            })?;
        if service_or_path.is_empty() || service_or_path.len() > MAX_STRING_CHARS {
            return Err(fdo::Error::InvalidArgs(
                "RegisterStatusNotifierItem requires a bus name or object path".into(),
            ));
        }
        let (service, path, owner) = if service_or_path.starts_with('/') {
            // Path form: the item sits on the caller's own connection.
            let object_path = zvariant::ObjectPath::try_from(service_or_path)
                .map_err(|error| fdo::Error::InvalidArgs(format!("not an object path: {error}")))?;
            (caller.clone(), object_path.to_string(), caller)
        } else {
            let name = zbus::names::BusName::try_from(service_or_path)
                .map_err(|error| fdo::Error::InvalidArgs(format!("not a bus name: {error}")))?;
            // Bounded owner resolution off the run loop (see WatcherMsg).
            let dbus = DBusProxy::new(connection)
                .await
                .map_err(|error| fdo::Error::Failed(format!("bus proxy failed: {error}")))?;
            let owner = match tokio::time::timeout(
                RESOLVE_TIMEOUT,
                dbus.get_name_owner(name.clone()),
            )
            .await
            {
                Ok(Ok(owner)) => owner,
                Ok(Err(_)) => {
                    return Err(fdo::Error::Failed(format!(
                        "{service_or_path} has no owner on the bus"
                    )));
                }
                Err(_) => {
                    return Err(fdo::Error::Failed(
                        "owner resolution timed out; retry the registration".into(),
                    ));
                }
            };
            if owner.as_str() != caller {
                return Err(fdo::Error::AccessDenied(format!(
                    "only {service_or_path}'s owner may register it as a tray item"
                )));
            }
            (
                service_or_path.to_string(),
                DEFAULT_ITEM_PATH.to_string(),
                owner.to_string(),
            )
        };
        if !lock_state(&self.store).can_accept(&service, &path, &owner) {
            return Err(fdo::Error::LimitsExceeded(format!(
                "tray item limits reached ({MAX_ITEMS} total, {MAX_ITEMS_PER_OWNER} per \
                 connection); refusing to track {service}{path}"
            )));
        }
        self.registrations
            .try_send(WatcherMsg::RegisterItem {
                service,
                path,
                owner,
            })
            .map_err(|_| fdo::Error::Failed("tray adapter is busy; retry".into()))
    }

    /// `RegisterStatusNotifierHost(s)` — recorded (cosmix is itself the
    /// host); a first-time registration makes the run loop emit
    /// `StatusNotifierHostRegistered`.
    async fn register_status_notifier_host(
        &self,
        #[zbus(header)] header: Header<'_>,
    ) -> fdo::Result<()> {
        let caller = header
            .sender()
            .map(|sender| sender.to_string())
            .ok_or_else(|| {
                fdo::Error::Failed("RegisterStatusNotifierHost needs a sender".into())
            })?;
        let accepted = {
            let hosts = lock_state(&self.store).hosts();
            hosts.len() < MAX_HOSTS || hosts.contains(&caller)
        };
        if !accepted {
            return Err(fdo::Error::LimitsExceeded(format!(
                "tray host limit reached ({MAX_HOSTS})"
            )));
        }
        self.registrations
            .try_send(WatcherMsg::RegisterHost { caller })
            .map_err(|_| fdo::Error::Failed("tray adapter is busy; retry".into()))
    }

    #[zbus(property)]
    async fn registered_status_notifier_items(&self) -> fdo::Result<Vec<String>> {
        Ok(lock_state(&self.store).registered_names())
    }

    /// True while this adapter runs — cosmix is the host.
    #[zbus(property)]
    async fn is_status_notifier_host_registered(&self) -> fdo::Result<bool> {
        Ok(lock_state(&self.store).host_registered())
    }

    #[zbus(property)]
    async fn protocol_version(&self) -> fdo::Result<i32> {
        Ok(0)
    }

    #[zbus(signal)]
    async fn status_notifier_item_registered(
        emitter: &SignalEmitter<'_>,
        service: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn status_notifier_item_unregistered(
        emitter: &SignalEmitter<'_>,
        service: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn status_notifier_host_registered(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

// ===========================================================================
// D-Bus side: reading items
// ===========================================================================

/// Why a property read gave up before producing a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadStop {
    Timeout,
    Transport,
    Oversized,
}

/// One item property, read individually via `org.freedesktop.DBus
/// .Properties.Get` — never `GetAll`: a pixmap inside a reply is
/// deserialized per byte into values, so the raw reply body is checked
/// against `cap` BEFORE any deserialization (zbus accepts messages up
/// to 128 MiB; the cap bounds what a hostile item can make the adapter
/// decode). `Ok(None)` = not implemented or the wrong type (the SNI
/// default applies); `Ok(Some(Err(..)))` = the item is unreachable or
/// the reply was oversized (the caller keeps its last-known props).
async fn read_property(
    connection: &Connection,
    service: &str,
    path: &str,
    name: &str,
    cap: usize,
) -> std::result::Result<Option<OwnedValue>, ReadStop> {
    let read = async {
        let proxy = ProxyBuilder::<Proxy>::new(connection)
            .destination(service.to_string())?
            .path(path.to_string())?
            .interface("org.freedesktop.DBus.Properties")?
            .cache_properties(CacheProperties::No)
            .build()
            .await?;
        let interface = zbus::names::InterfaceName::from_static_str(ITEM_IFACE)?;
        proxy.call_method("Get", &(interface, name)).await
    };
    let reply = match tokio::time::timeout(REFRESH_BUDGET, read).await {
        Ok(Ok(reply)) => reply,
        Ok(Err(error)) if is_unknown_property(&error) => return Ok(None),
        Ok(Err(error)) => {
            eprintln!("cosmix-dbusd: tray: reading {service}{path} {name} failed: {error}");
            return Err(ReadStop::Transport);
        }
        Err(_) => {
            eprintln!(
                "cosmix-dbusd: tray: reading {service}{path} {name} exceeded the \
                 {REFRESH_BUDGET:?} budget"
            );
            return Err(ReadStop::Timeout);
        }
    };
    let body = reply.body();
    if body.len() > cap {
        return Err(ReadStop::Oversized);
    }
    match body.deserialize::<OwnedValue>() {
        Ok(value) => Ok(Some(value)),
        Err(_) => Ok(None),
    }
}

/// What a refresh produced: fresh props (with any oversized pixmap
/// dropped, and a marker so `tray.info` can count the refusal), or
/// nothing (the item was unreachable — its last-known props stand).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RefreshOutcome {
    Props(ItemProps),
    /// Props whose pixmap-bearing reads were refused by the size cap.
    Oversized(ItemProps),
    Unreachable,
}

/// Read one item's property set as [`ItemProps`] within
/// [`REFRESH_BUDGET`], one typed property at a time (see
/// [`read_property`]). Missing properties take their SNI defaults.
pub(crate) async fn fetch_item_props(
    connection: &Connection,
    service: &str,
    path: &str,
) -> RefreshOutcome {
    async fn string(
        connection: &Connection,
        service: &str,
        path: &str,
        name: &str,
    ) -> std::result::Result<String, ReadStop> {
        match read_property(connection, service, path, name, MAX_RAW_REPLY_BYTES).await {
            Ok(Some(value)) => Ok(String::try_from(value).unwrap_or_default()),
            Ok(None) => Ok(String::new()),
            Err(stop) => Err(stop),
        }
    }
    // The pixmap-bearing properties share one oversized marker: the
    // refresh still lands, without the pixmaps, and the refusal is
    // counted.
    async fn pixmap_property(
        connection: &Connection,
        service: &str,
        path: &str,
        name: &str,
        oversized: &mut bool,
    ) -> std::result::Result<Option<Value<'static>>, ReadStop> {
        match read_property(connection, service, path, name, MAX_PIXMAP_REPLY_BYTES).await {
            Ok(Some(value)) => Ok(Some(Value::from(value))),
            Ok(None) => Ok(None),
            Err(ReadStop::Oversized) => {
                *oversized = true;
                Ok(None)
            }
            Err(stop) => Err(stop),
        }
    }
    let read = async {
        let id = string(connection, service, path, "Id").await?;
        let title = string(connection, service, path, "Title").await?;
        let category = string(connection, service, path, "Category").await?;
        let status = string(connection, service, path, "Status").await?;
        let icon_name = string(connection, service, path, "IconName").await?;
        let icon_theme_path = string(connection, service, path, "IconThemePath").await?;
        let attention_icon_name = string(connection, service, path, "AttentionIconName").await?;
        let item_is_menu =
            match read_property(connection, service, path, "ItemIsMenu", MAX_RAW_REPLY_BYTES).await
            {
                Ok(Some(value)) => bool::try_from(value).unwrap_or(false),
                Ok(None) => false,
                Err(stop) => return Err(stop),
            };
        let menu = match read_property(connection, service, path, "Menu", MAX_RAW_REPLY_BYTES).await
        {
            Ok(Some(value)) => zvariant::OwnedObjectPath::try_from(value)
                .ok()
                .map(|path| path.to_string()),
            Ok(None) => None,
            Err(stop) => return Err(stop),
        };
        let mut oversized = false;
        let pixmap =
            match pixmap_property(connection, service, path, "IconPixmap", &mut oversized).await? {
                Some(value) => largest_pixmap(&value),
                None => None,
            };
        let tooltip =
            match pixmap_property(connection, service, path, "ToolTip", &mut oversized).await? {
                Some(value) => parse_tooltip(&value),
                None => None,
            };
        let (tooltip_icon, tooltip_title, tooltip_description) = match tooltip {
            Some((icon, title, description)) => (icon, title, description),
            None => (String::new(), String::new(), String::new()),
        };
        Ok((
            ItemProps {
                id,
                title,
                category,
                status,
                icon_name,
                icon_theme_path,
                attention_icon_name,
                tooltip_title,
                tooltip_description,
                tooltip_icon,
                // The SNI sentinel for "no menu" is not a menu.
                menu: menu.filter(|path| path != NO_DBUSMENU_PATH && !path.is_empty()),
                item_is_menu,
                pixmap,
            },
            oversized,
        ))
    };
    match tokio::time::timeout(REFRESH_BUDGET, read).await {
        Ok(Ok((mut props, oversized))) => {
            if props.status.is_empty() {
                // SNI's documented default when an item does not
                // implement Status.
                props.status = "Active".into();
            }
            props.clamp_strings();
            if oversized {
                RefreshOutcome::Oversized(props)
            } else {
                RefreshOutcome::Props(props)
            }
        }
        Ok(Err(stop)) => {
            eprintln!(
                "cosmix-dbusd: tray: reading {service}{path} stopped ({stop:?}); keeping \
                 last-known props"
            );
            RefreshOutcome::Unreachable
        }
        Err(_) => {
            eprintln!(
                "cosmix-dbusd: tray: reading {service}{path} exceeded the {REFRESH_BUDGET:?} \
                 budget"
            );
            RefreshOutcome::Unreachable
        }
    }
}

/// ToolTip is `(s a(iiay) s s)`: icon name, pixmaps, title, description.
fn parse_tooltip(value: &Value<'_>) -> Option<(String, String, String)> {
    let Value::Structure(structure) = value else {
        return None;
    };
    let fields = structure.fields();
    if fields.len() != 4 {
        return None;
    }
    let icon = field_str(&fields[0])?;
    let title = field_str(&fields[2]).unwrap_or_default();
    let description = field_str(&fields[3]).unwrap_or_default();
    Some((icon, title, description))
}

/// Keep the largest `a(iiay)` entry that is well-formed: positive
/// dimensions, `data.len() == width * height * 4` (ARGB32), and at most
/// [`MAX_PIXMAP_BYTES`] of data. Mismatched or oversized entries are
/// dropped.
fn largest_pixmap(value: &Value<'_>) -> Option<Pixmap> {
    let Value::Array(entries) = value else {
        return None;
    };
    let mut best: Option<Pixmap> = None;
    for entry in entries.iter() {
        let Value::Structure(structure) = entry else {
            continue;
        };
        let fields = structure.fields();
        if fields.len() != 3 {
            continue;
        }
        let (Value::I32(width), Value::I32(height), Value::Array(bytes)) =
            (&fields[0], &fields[1], &fields[2])
        else {
            continue;
        };
        if *width <= 0 || *height <= 0 {
            continue;
        }
        let area = u64::from(*width as u32) * u64::from(*height as u32);
        if area > u64::try_from(MAX_PIXMAP_BYTES).expect("cap fits u64") {
            continue;
        }
        let data: Vec<u8> = bytes
            .iter()
            .filter_map(|byte| match byte {
                Value::U8(byte) => Some(*byte),
                _ => None,
            })
            .collect();
        if data.len() != (area * 4) as usize || data.len() > MAX_PIXMAP_BYTES {
            continue;
        }
        let better = best.as_ref().is_none_or(|current| {
            area > u64::from(current.width as u32) * u64::from(current.height as u32)
        });
        if better {
            best = Some(Pixmap {
                width: *width,
                height: *height,
                data,
            });
        }
    }
    best
}

fn field_str(value: &Value<'_>) -> Option<String> {
    match value {
        Value::Str(text) => Some(text.to_string()),
        _ => None,
    }
}

// ===========================================================================
// D-Bus side: calling items (all bounded by timeouts)
// ===========================================================================

async fn item_proxy(connection: &Connection, item: &TrayItem) -> zbus::Result<Proxy<'static>> {
    ProxyBuilder::<Proxy>::new(connection)
        .destination(item.service.clone())?
        .path(item.path.clone())?
        .interface(ITEM_IFACE)?
        .cache_properties(CacheProperties::No)
        .build()
        .await
}

async fn menu_proxy(connection: &Connection, item: &TrayItem) -> zbus::Result<Proxy<'static>> {
    // Callers refuse menu-less items first; this is the backstop.
    let menu_path = item.props.menu.clone().ok_or(zbus::Error::Unsupported)?;
    ProxyBuilder::<Proxy>::new(connection)
        .destination(item.service.clone())?
        .path(menu_path)?
        .interface(MENU_IFACE)?
        .cache_properties(CacheProperties::No)
        .build()
        .await
}

/// Call `Activate`/`SecondaryActivate`/`ContextMenu` with `(x, y)`.
/// The proxy build sits INSIDE the timeout: nothing on the path to a
/// hostile item may run unbounded, even if the build is I/O-free today.
async fn call_item_xy(
    connection: &Connection,
    item: &TrayItem,
    member: &str,
    x: i32,
    y: i32,
) -> std::result::Result<(), String> {
    let work = async {
        let proxy = item_proxy(connection, item)
            .await
            .map_err(|error| format!("item {member} failed: {error}"))?;
        proxy
            .call_method(member, &(x, y))
            .await
            .map(|_| ())
            .map_err(|error| format!("item {member} failed: {error}"))
    };
    match tokio::time::timeout(ITEM_CALL_TIMEOUT, work).await {
        Ok(result) => result,
        Err(_) => Err(format!(
            "item did not answer {member} within {:?}; it may be hung",
            ITEM_CALL_TIMEOUT
        )),
    }
}

async fn call_item_scroll(
    connection: &Connection,
    item: &TrayItem,
    delta: i32,
    orientation: &str,
) -> std::result::Result<(), String> {
    let work = async {
        let proxy = item_proxy(connection, item)
            .await
            .map_err(|error| format!("item Scroll failed: {error}"))?;
        proxy
            .call_method("Scroll", &(delta, orientation))
            .await
            .map(|_| ())
            .map_err(|error| format!("item Scroll failed: {error}"))
    };
    match tokio::time::timeout(ITEM_CALL_TIMEOUT, work).await {
        Ok(result) => result,
        Err(_) => Err(format!(
            "item did not answer Scroll within {:?}; it may be hung",
            ITEM_CALL_TIMEOUT
        )),
    }
}

/// Read the menu layout: `AboutToShow(0)` first (dbusmenu's hook for
/// servers that build submenus lazily — errors ignored), then
/// `GetLayout(0, -1, propertyNames)` with the names pinned to the small
/// set the adapter surfaces, `icon-data` excluded — a pixmap per node
/// would make one menu read a multi-megabyte affair. The raw reply body
/// is capped before deserialization like any item read. The whole read,
/// proxy build included, runs inside [`MENU_CALL_TIMEOUT`].
async fn call_menu_layout(
    connection: &Connection,
    item: &TrayItem,
) -> std::result::Result<(u32, Json), String> {
    let work = async {
        let proxy = menu_proxy(connection, item)
            .await
            .map_err(|error| format!("menu read failed: {error}"))?;
        let _ = proxy.call_method("AboutToShow", &0_i32).await;
        let properties: Vec<&str> = MENU_PROPERTY_NAMES.to_vec();
        let reply = proxy
            .call_method("GetLayout", &(0_i32, -1_i32, properties))
            .await
            .map_err(|error| format!("menu read failed: {error}"))?;
        let body = reply.body();
        if body.len() > MAX_RAW_REPLY_BYTES {
            return Err(format!(
                "menu layout exceeded the raw reply cap ({MAX_RAW_REPLY_BYTES} bytes)"
            ));
        }
        let (revision, layout) = body
            .deserialize::<(u32, Value)>()
            .map_err(|error| format!("menu layout had an unexpected shape: {error}"))?;
        let mut budget = MAX_MENU_NODES;
        let layout = menu_node_json(&layout, &mut budget)
            .ok_or_else(|| "menu layout had an unexpected shape".to_string())?;
        Ok((revision, layout))
    };
    match tokio::time::timeout(MENU_CALL_TIMEOUT, work).await {
        Ok(result) => result,
        Err(_) => Err(format!(
            "item did not answer the menu read within {:?}; it may be hung",
            MENU_CALL_TIMEOUT
        )),
    }
}

/// The dbusmenu `Event(id, "clicked", data, timestamp)` request body —
/// the wire shape is `(i s v u)`: the data a single variant wrapping
/// int32, the timestamp a u32 (the unit test pins this).
fn click_event_body(node_id: i32) -> (i32, &'static str, Value<'static>, u32) {
    (
        node_id,
        "clicked",
        Value::Value(Box::new(Value::I32(0))),
        unix_millis() as u32,
    )
}

/// dbusmenu `Event(id, "clicked", data, timestamp)`.
async fn call_menu_click(
    connection: &Connection,
    item: &TrayItem,
    node_id: i32,
) -> std::result::Result<(), String> {
    let work = async {
        let proxy = menu_proxy(connection, item)
            .await
            .map_err(|error| format!("menu click failed: {error}"))?;
        let body = click_event_body(node_id);
        proxy
            .call_method("Event", &body)
            .await
            .map_err(|error| format!("menu click failed: {error}"))
    };
    match tokio::time::timeout(ITEM_CALL_TIMEOUT, work).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(format!(
            "item did not answer Event within {:?}; it may be hung",
            ITEM_CALL_TIMEOUT
        )),
    }
}

fn unix_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis() as i64)
        .unwrap_or(0)
}

// ===========================================================================
// Bus side: verbs + props
// ===========================================================================

/// Dispatch one incoming `tray.*` command. Mesh-open per LAW
/// 2026-09-15: no caller authorization. Unknown ids and bad arguments
/// are refusals (rc 10), never panics; every call into an item is
/// timeout-bounded so a hung app cannot wedge the service loop.
async fn dispatch(
    command: &IncomingCommand,
    store: &TrayStore,
    connection: &Connection,
) -> (u8, String) {
    if let Some(suffix) = command.command.strip_prefix("tray.props.") {
        return dispatch_props(command, store, suffix);
    }
    let args = resolve_args(command);
    match command.command.as_str() {
        "tray.ping" => (
            0,
            json!({"pong": true, "service": BUS_SERVICE, "schema": "tray.v1"}).to_string(),
        ),
        "tray.info" => {
            let state = lock_state(store);
            (
                0,
                json!({
                    "name": BUS_SERVICE,
                    "schema": "tray.v1",
                    "props_level": "L2",
                    "watcher": WATCHER_NAME,
                    "items": state.count(),
                    "hosts": state.hosts(),
                    "event_seq": state.event_seq(),
                    "oversized_reads": state.oversized_reads(),
                    "recent_events": state.recent_events().iter().map(event_json).collect::<Vec<_>>(),
                })
                .to_string(),
            )
        }
        "tray.list" => {
            let state = lock_state(store);
            (
                0,
                json!({
                    "items": state.items_in_order().into_iter().map(item_json).collect::<Vec<_>>(),
                    "count": state.count(),
                    "hosts": state.hosts(),
                })
                .to_string(),
            )
        }
        "tray.activate" => dispatch_xy(args.as_ref(), store, connection, "Activate").await,
        "tray.secondary_activate" => {
            dispatch_xy(args.as_ref(), store, connection, "SecondaryActivate").await
        }
        "tray.context_menu" => dispatch_xy(args.as_ref(), store, connection, "ContextMenu").await,
        "tray.scroll" => dispatch_scroll(args.as_ref(), store, connection).await,
        "tray.icon" => dispatch_icon(args.as_ref(), store),
        "tray.menu" => dispatch_menu(args.as_ref(), store, connection).await,
        "tray.menu.click" => dispatch_menu_click(args.as_ref(), store, connection).await,
        _ => (
            10,
            json!({"error": format!("unknown tray verb: {}", command.command)}).to_string(),
        ),
    }
}

fn dispatch_props(command: &IncomingCommand, store: &TrayStore, suffix: &str) -> (u8, String) {
    if suffix == "watch" {
        let state = lock_state(store);
        return (
            0,
            json!({
                "topic": props_changed_topic(BUS_SERVICE),
                "domain_topics": [TOPIC_ITEM_ADDED, TOPIC_ITEM_CHANGED, TOPIC_ITEM_REMOVED],
                // event_seq shape, one shape everywhere: a JSON number in
                // verb and event bodies; a decimal string in BusMessage
                // headers. Here it is the counter AT WATCH TIME — every
                // event after it is new to this subscriber.
                "event_seq": state.event_seq(),
                "event_seq_note": "per-adapter-session monotonic counter; each event carries \
                                   its own event_seq — a gap means events were dropped, \
                                   re-read tray.props.get",
                "bootstrap": "subscribe on this connection, then read tray.props.get",
            })
            .to_string(),
        );
    }
    let props = TrayProps::new(&lock_state(store));
    let args = resolve_args(command);
    let response = cosmix_props_core::bus::dispatch_props(&props, suffix, args.as_ref(), true);
    (response.rc.clamp(0, 255) as u8, response.body)
}

/// The item argument shared by every item-addressed verb: `{id: "i<n>"}`.
/// Returns the item cloned out of the store (calls run unlocked).
fn wanted_item(args: Option<&Json>, store: &TrayStore) -> std::result::Result<TrayItem, String> {
    let id = args
        .and_then(|args| args.get("id"))
        .and_then(Json::as_str)
        .ok_or_else(|| "this verb requires args.id".to_string())?;
    lock_state(store)
        .item(id)
        .cloned()
        .ok_or_else(|| format!("unknown tray item: {id}"))
}

fn arg_i32(args: Option<&Json>, key: &str) -> i32 {
    args.and_then(|args| args.get(key))
        .and_then(Json::as_i64)
        .and_then(|value| i32::try_from(value).ok())
        .unwrap_or(0)
}

async fn dispatch_xy(
    args: Option<&Json>,
    store: &TrayStore,
    connection: &Connection,
    member: &str,
) -> (u8, String) {
    let item = match wanted_item(args, store) {
        Ok(item) => item,
        Err(error) => return refusal(error),
    };
    let (x, y) = (arg_i32(args, "x"), arg_i32(args, "y"));
    match call_item_xy(connection, &item, member, x, y).await {
        Ok(()) => (
            0,
            json!({"ok": true, "id": item.key, "action": member.to_lowercase()}).to_string(),
        ),
        Err(error) => refusal(error),
    }
}

async fn dispatch_scroll(
    args: Option<&Json>,
    store: &TrayStore,
    connection: &Connection,
) -> (u8, String) {
    let item = match wanted_item(args, store) {
        Ok(item) => item,
        Err(error) => return refusal(error),
    };
    let Some(delta) = args
        .and_then(|args| args.get("delta"))
        .and_then(Json::as_i64)
        .and_then(|value| i32::try_from(value).ok())
    else {
        return refusal("tray.scroll requires a numeric args.delta");
    };
    let Some(orientation) = args
        .and_then(|args| args.get("orientation"))
        .and_then(Json::as_str)
    else {
        return refusal("tray.scroll requires args.orientation");
    };
    if orientation != "horizontal" && orientation != "vertical" {
        return refusal("tray.scroll orientation must be horizontal or vertical");
    }
    match call_item_scroll(connection, &item, delta, orientation).await {
        Ok(()) => (
            0,
            json!({"ok": true, "id": item.key, "delta": delta, "orientation": orientation})
                .to_string(),
        ),
        Err(error) => refusal(error),
    }
}

fn dispatch_icon(args: Option<&Json>, store: &TrayStore) -> (u8, String) {
    let item = match wanted_item(args, store) {
        Ok(item) => item,
        Err(error) => return refusal(error),
    };
    let Some(pixmap) = item.props.pixmap else {
        return refusal(format!("item {} has no pixmap", item.key));
    };
    (
        0,
        json!({
            "id": item.key,
            "width": pixmap.width,
            "height": pixmap.height,
            // Raw IconPixmap bytes: ARGB32, network byte order, row-major.
            "encoding": "argb32-network-order",
            "argb_b64": base64_encode(&pixmap.data),
        })
        .to_string(),
    )
}

async fn dispatch_menu(
    args: Option<&Json>,
    store: &TrayStore,
    connection: &Connection,
) -> (u8, String) {
    let item = match wanted_item(args, store) {
        Ok(item) => item,
        Err(error) => return refusal(error),
    };
    if item.props.menu.is_none() {
        return refusal(format!("item {} has no menu", item.key));
    }
    match call_menu_layout(connection, &item).await {
        Ok((revision, layout)) => (
            0,
            json!({"id": item.key, "revision": revision, "layout": layout}).to_string(),
        ),
        Err(error) => refusal(error),
    }
}

async fn dispatch_menu_click(
    args: Option<&Json>,
    store: &TrayStore,
    connection: &Connection,
) -> (u8, String) {
    let item = match wanted_item(args, store) {
        Ok(item) => item,
        Err(error) => return refusal(error),
    };
    if item.props.menu.is_none() {
        return refusal(format!("item {} has no menu", item.key));
    }
    let Some(node_id) = args
        .and_then(|args| args.get("item"))
        .and_then(Json::as_i64)
        .and_then(|value| i32::try_from(value).ok())
    else {
        return refusal("tray.menu.click requires a numeric args.item (the menu node id)");
    };
    match call_menu_click(connection, &item, node_id).await {
        Ok(()) => (
            0,
            json!({"ok": true, "id": item.key, "item": node_id, "event": "clicked"}).to_string(),
        ),
        Err(error) => refusal(error),
    }
}

fn refusal(error: impl Into<String>) -> (u8, String) {
    (10, json!({"error": error.into()}).to_string())
}

fn item_json(item: &TrayItem) -> Json {
    json!({
        "key": item.key,
        "id": item.props.id,
        "title": item.props.title,
        "category": item.props.category,
        "status": item.props.status,
        "icon_name": item.props.icon_name,
        "icon_theme_path": item.props.icon_theme_path,
        "attention_icon_name": item.props.attention_icon_name,
        "tooltip_title": item.props.tooltip_title,
        "tooltip_description": item.props.tooltip_description,
        "tooltip_icon": item.props.tooltip_icon,
        "has_menu": item.props.menu.is_some(),
        "menu": item.props.menu,
        "item_is_menu": item.props.item_is_menu,
        "pixmap": item.props.pixmap.as_ref().map(|pixmap| json!({
            "width": pixmap.width,
            "height": pixmap.height,
        })),
        "service": item.service,
        "path": item.path,
    })
}

fn event_json(event: &ItemEvent) -> Json {
    json!({
        "event": event.kind.event_name(),
        "key": event.key,
        "service": event.service,
        // What the watcher surface advertises (service + path).
        "registered": event.registered,
        // The one shape: a JSON number here, a decimal string in
        // BusMessage headers.
        "event_seq": event.seq,
    })
}

fn resolve_args(command: &IncomingCommand) -> Option<Json> {
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

// ===========================================================================
// Bus side: event publisher + reconnecting broker
// ===========================================================================

/// One state mutation's worth of outbound Bus traffic: the item events
/// plus the props snapshot at that moment (the publisher diffs this
/// against its baseline — kept across publish failures and reconnects,
/// exactly like the daemon citizen's F8 contract).
struct BusBatch {
    events: Vec<ItemEvent>,
    snapshot: PropValue,
    cause: &'static str,
}

/// The one Bus operation the publisher needs, as a trait so the
/// publishing contract (diffs against the surviving baseline, event
/// stamps) is testable without a live broker. `NodedClient` is the
/// production implementation.
trait EventPublisher: Send + Sync {
    fn publish_event(
        &self,
        topic: &str,
        message: cosmix_bus::bus::BusMessage,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>>;
}

impl EventPublisher for NodedClient {
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
            self.send_with_headers("noded", "topic.publish", &headers, &message.to_wire())
                .await
        })
    }
}

async fn wait_for_bus_client(
    clients: &mut watch::Receiver<Option<Arc<dyn EventPublisher>>>,
) -> Result<Arc<dyn EventPublisher>> {
    loop {
        if let Some(client) = clients.borrow_and_update().clone() {
            return Ok(client);
        }
        if clients.changed().await.is_err() {
            return Err(anyhow!("tray bus client channel ended"));
        }
    }
}

async fn run_event_publisher(
    mut batches: mpsc::Receiver<BusBatch>,
    mut clients: watch::Receiver<Option<Arc<dyn EventPublisher>>>,
    faults: mpsc::Sender<()>,
) -> Result<()> {
    let mut baseline: Option<PropValue> = None;
    while let Some(batch) = batches.recv().await {
        let client = match wait_for_bus_client(&mut clients).await {
            Ok(client) => client,
            Err(error) => return Err(error),
        };
        let mut sent = Ok(());
        if let Some(old) = baseline.as_ref() {
            for (path, old_value, new_value) in cosmix_props_core::diff(old, &batch.snapshot) {
                let mut message =
                    build_props_changed_message(&path, &old_value, &new_value, batch.cause);
                message.set(
                    "event_seq",
                    &batch.events.last().map_or(0, |e| e.seq).to_string(),
                );
                sent = client
                    .publish_event(&props_changed_topic(BUS_SERVICE), message)
                    .await;
                if sent.is_err() {
                    break;
                }
            }
        }
        if sent.is_ok() {
            for event in &batch.events {
                let mut message = cosmix_bus::bus::BusMessage::new();
                message.set("command", event.kind.topic());
                message.set("event_seq", &event.seq.to_string());
                message.body = json!({
                    "event": event.kind.event_name(),
                    "event_seq": event.seq,
                    "data": {
                        "key": event.key,
                        "service": event.service,
                        "registered": event.registered,
                    },
                })
                .to_string();
                sent = client.publish_event(event.kind.topic(), message).await;
                if sent.is_err() {
                    break;
                }
            }
        }
        match sent {
            Ok(()) => baseline = Some(batch.snapshot),
            Err(error) => {
                eprintln!("cosmix-dbusd: tray: event publish failed: {error:#}");
                let _ = faults.try_send(());
                // Baseline deliberately kept: the next event re-diffs the
                // whole outage window for subscribers that stayed on.
            }
        }
    }
    Ok(())
}

async fn run_bus_broker(
    store: TrayStore,
    session: Connection,
    clients: watch::Sender<Option<Arc<dyn EventPublisher>>>,
    mut faults: mpsc::Receiver<()>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let build = cosmix_buildinfo::build_info!();
    let provenance = cosmix_bus::RegisterProvenance::from_parts(
        build.pkg,
        build.version,
        build.git_sha,
        build.git_dirty,
        build.build_time,
        cosmix_buildinfo::now_rfc3339(),
    );
    loop {
        let connection = tokio::select! {
            biased;
            changed = shutdown.changed() => {
                changed.map_err(|_| anyhow!("shutdown coordinator ended"))?;
                return Ok(());
            }
            connection = tokio::time::timeout(
                BUS_PUBLISH_TIMEOUT,
                cosmix_config::client_helpers::connect_default_with_provenance(
                    BUS_SERVICE,
                    provenance.clone(),
                ),
            ) => connection,
        };
        match connection {
            Ok(Ok(client)) => {
                let client: Arc<NodedClient> = Arc::new(client);
                let publisher: Arc<dyn EventPublisher> = client.clone();
                let _ = clients.send(Some(publisher));
                eprintln!("cosmix-dbusd: tray: registered as '{BUS_SERVICE}'");
                let shutdown_for_serve = shutdown.clone();
                let stopping = tokio::select! {
                    biased;
                    changed = shutdown.changed() => {
                        changed.map_err(|_| anyhow!("shutdown coordinator ended"))?;
                        true
                    }
                    fault = faults.recv() => {
                        if fault.is_none() {
                            return Err(anyhow!("tray publisher task ended unexpectedly"));
                        }
                        eprintln!("cosmix-dbusd: tray: publisher fault; reconnecting bus client");
                        false
                    }
                    _ = serve_bus_client(
                        Arc::clone(&client),
                        Arc::clone(&store),
                        session.clone(),
                        shutdown_for_serve,
                    ) => false,
                };
                let _ = clients.send(None);
                if tokio::time::timeout(BUS_CLOSE_TIMEOUT, client.close())
                    .await
                    .is_err()
                {
                    eprintln!("cosmix-dbusd: tray: bus client close timed out");
                }
                while faults.try_recv().is_ok() {}
                if stopping {
                    return Ok(());
                }
                eprintln!("cosmix-dbusd: tray: bus disconnected; retrying in 60 s");
            }
            Ok(Err(error)) => {
                eprintln!("cosmix-dbusd: tray: bus unavailable; retrying in 60 s: {error}");
            }
            Err(_) => {
                eprintln!("cosmix-dbusd: tray: bus connection timed out; retrying in 60 s");
            }
        }
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                changed.map_err(|_| anyhow!("shutdown coordinator ended"))?;
                return Ok(());
            }
            _ = tokio::time::sleep(BUS_RECONNECT_DELAY) => {}
        }
    }
}

/// The one Bus operation the command loop needs to answer a verb — a
/// trait for the same reason as [`EventPublisher`]: the serving
/// contract (concurrent dispatch) is testable without a live broker.
trait CommandResponder: Send + Sync {
    fn respond(
        &self,
        command: &IncomingCommand,
        rc: u8,
        body: &str,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>>;
}

impl CommandResponder for NodedClient {
    fn respond(
        &self,
        command: &IncomingCommand,
        rc: u8,
        body: &str,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
        let to = command.from.clone();
        let command_name = command.command.clone();
        let id = command.id.clone();
        let body = body.to_string();
        Box::pin(async move {
            let reply = tokio::time::timeout(
                BUS_PUBLISH_TIMEOUT,
                self.respond_parts(&to, &command_name, id.as_deref(), rc, &body),
            )
            .await;
            match reply {
                Ok(result) => result,
                Err(_) => Err(anyhow!("bus response timed out")),
            }
        })
    }
}

async fn serve_bus_client(
    client: Arc<NodedClient>,
    store: TrayStore,
    session: Connection,
    shutdown: watch::Receiver<bool>,
) {
    let Some(incoming) = client.incoming_async().await else {
        return;
    };
    serve_commands(incoming, store, session, client, shutdown).await;
}

/// Serve one Bus connection's commands. Each verb dispatches in its own
/// task, bounded by [`MAX_CONCURRENT_VERBS`]: an item hanging on its
/// 2-3 s timeout delays only its own verb, never `tray.list` or
/// `tray.props.get` behind it. A response failure ends this connection
/// (the broker reconnects); commands stop (bus death) end it too.
async fn serve_commands(
    mut incoming: mpsc::UnboundedReceiver<IncomingCommand>,
    store: TrayStore,
    session: Connection,
    responder: Arc<dyn CommandResponder>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut dispatches: JoinSet<()> = JoinSet::new();
    let (fail_tx, mut fail_rx) = mpsc::channel::<String>(1);
    loop {
        // Check the current value too: a watch cloned after the flip
        // would otherwise never see `changed()` fire.
        if *shutdown.borrow_and_update() {
            return;
        }
        tokio::select! {
            biased;
            _ = shutdown.changed() => return,
            failure = fail_rx.recv() => {
                if let Some(reason) = failure {
                    eprintln!("cosmix-dbusd: tray: {reason}; reconnecting");
                }
                return;
            }
            done = dispatches.join_next(), if !dispatches.is_empty() => {
                let _ = done;
            }
            command = incoming.recv() => {
                let Some(command) = command else { return };
                if dispatches.len() >= MAX_CONCURRENT_VERBS {
                    // Bounded concurrency: wait for a slot before
                    // taking the next command.
                    let _ = dispatches.join_next().await;
                }
                let responder = Arc::clone(&responder);
                let store = Arc::clone(&store);
                let session = session.clone();
                let fail = fail_tx.clone();
                dispatches.spawn(async move {
                    let (rc, body) = dispatch(&command, &store, &session).await;
                    if let Err(error) = responder.respond(&command, rc, &body).await {
                        let _ = fail.try_send(format!("bus response failed: {error:#}"));
                    }
                });
            }
        }
    }
}

// ===========================================================================
// The adapter
// ===========================================================================

/// A running tray adapter: the watcher name held on the session bus, the
/// item-tracking run loop, and the `tray` Bus service. Split out of
/// [`Adapter::run`] so the tests drive the real machinery on a private
/// bus without a Bus broker.
pub(crate) struct TrayHost {
    store: TrayStore,
    connection: Connection,
    emitter: SignalEmitter<'static>,
    owner_changes: MessageStream,
    item_signals: MessageStream,
    registrations: mpsc::Receiver<WatcherMsg>,
    refresh_tx: mpsc::Sender<(String, RefreshOutcome)>,
    refresh_rx: mpsc::Receiver<(String, RefreshOutcome)>,
    /// Keys with a fetch in flight / signalled again while in flight.
    refresh_active: BTreeSet<String>,
    refresh_queued: BTreeSet<String>,
    /// Property fetches owned by the host — aborted when the run ends,
    /// so no detached task holds a connection clone past the run.
    refresh_tasks: JoinSet<()>,
    /// Per-item refresh rate limit (a New* signal flood cannot loop the
    /// item's own property reads).
    refresh_gate: RefreshGate,
    /// Keys whose next refresh is waiting out the gate, and when due.
    refresh_scheduled: BTreeMap<String, std::time::Instant>,
    batches: mpsc::Sender<BusBatch>,
    broker: JoinHandle<Result<()>>,
    publisher: JoinHandle<Result<()>>,
    internal_shutdown: watch::Sender<bool>,
}

/// Per-item minimum spacing between property refreshes, pure so the
/// spacing rule is unit-testable alone.
#[derive(Debug, Default)]
struct RefreshGate {
    last: HashMap<String, std::time::Instant>,
}

impl RefreshGate {
    /// Whether a refresh of `key` may fire at `now` — recording the
    /// firing if it does.
    fn admit(&mut self, key: &str, now: std::time::Instant) -> bool {
        match self.last.get(key) {
            Some(last) if now.duration_since(*last) < MIN_REFRESH_INTERVAL => false,
            _ => {
                self.last.insert(key.to_string(), now);
                true
            }
        }
    }

    fn retire(&mut self, key: &str) {
        self.last.remove(key);
    }
}

impl TrayHost {
    /// Build the session connection with the watcher interface served,
    /// then take `org.kde.StatusNotifierWatcher` — refusing (failing)
    /// when it is already owned: no replacement, a human hands the name
    /// over via `dbusd.adapter.disable`/`enable`.
    pub(crate) async fn start(address: &str) -> Result<Self> {
        let store: TrayStore = Arc::new(Mutex::new(TrayState::default()));
        let (registration_tx, registrations) = mpsc::channel(64);
        let (refresh_tx, refresh_rx) = mpsc::channel(64);
        let (batches_tx, batches_rx) = mpsc::channel(64);

        let address: zbus::address::Address = address
            .parse()
            .map_err(|error| anyhow!("invalid session bus address {address:?}: {error}"))?;
        let connection = zbus::connection::Builder::address(address)?
            .serve_at(
                WATCHER_PATH,
                WatcherIface {
                    store: Arc::clone(&store),
                    registrations: registration_tx,
                },
            )?
            .build()
            .await
            .map_err(|error| anyhow!("tray adapter session bus dial failed: {error}"))?;
        let dbus = DBusProxy::new(&connection)
            .await
            .map_err(|error| anyhow!("tray adapter bus proxy failed: {error}"))?;

        let name = WellKnownName::from_static_str(WATCHER_NAME)?;
        if dbus.name_has_owner(name.clone().into()).await? {
            let owner = dbus
                .get_name_owner(name.clone().into())
                .await
                .map(|owner| owner.to_string())
                .unwrap_or_else(|_| "unknown".into());
            return Err(anyhow!(
                "{WATCHER_NAME} is already owned by {owner}; the tray adapter does not \
                 replace an existing watcher — dbusd.adapter.disable {{name: \"tray\"}} \
                 and re-enable it once the other owner releases the name"
            ));
        }
        // No ReplaceExisting (never steal), no AllowReplacement (never be
        // stolen without a human), DoNotQueue (fail, don't wait in line).
        match dbus
            .request_name(name, fdo::RequestNameFlags::DoNotQueue.into())
            .await?
        {
            fdo::RequestNameReply::PrimaryOwner => {}
            reply => {
                return Err(anyhow!(
                    "{WATCHER_NAME} is already owned (request_name said {reply}); the \
                     tray adapter does not replace an existing watcher"
                ));
            }
        }
        let emitter = SignalEmitter::new(&connection, WATCHER_PATH)
            .map_err(|error| anyhow!("tray adapter signal emitter failed: {error}"))?;
        lock_state(&store).set_host_registered(true);
        if let Err(error) = WatcherIface::status_notifier_host_registered(&emitter).await {
            eprintln!("cosmix-dbusd: tray: StatusNotifierHostRegistered signal failed: {error}");
        }

        let owner_rule = MatchRule::builder()
            .msg_type(MessageType::Signal)
            .sender(DBUS_SERVICE)?
            .interface(DBUS_SERVICE)?
            .member("NameOwnerChanged")?
            .build();
        let owner_changes = MessageStream::for_match_rule(owner_rule, &connection, Some(64))
            .await
            .map_err(|error| anyhow!("tray adapter owner-watch failed: {error}"))?;
        let item_rule = MatchRule::builder()
            .msg_type(MessageType::Signal)
            .interface(ITEM_IFACE)?
            .build();
        // The item-signal rule cannot be narrowed further: New* signals
        // carry no arguments (no arg filters) and come from every item
        // connection (no single sender) — interface-only is the
        // narrowest rule expressible. The buffer is small because the
        // run loop drains this stream constantly; the serve loop never
        // awaits the bus, so a flood cannot wedge it (see WatcherMsg).
        let item_signals = MessageStream::for_match_rule(item_rule, &connection, Some(128))
            .await
            .map_err(|error| anyhow!("tray adapter item-watch failed: {error}"))?;

        let (client_tx, client_rx) = watch::channel::<Option<Arc<dyn EventPublisher>>>(None);
        let (fault_tx, fault_rx) = mpsc::channel(1);
        let publisher = tokio::spawn(run_event_publisher(batches_rx, client_rx, fault_tx));
        let (internal_shutdown, shutdown_rx) = watch::channel(false);
        let broker = tokio::spawn(run_bus_broker(
            Arc::clone(&store),
            connection.clone(),
            client_tx,
            fault_rx,
            shutdown_rx,
        ));

        Ok(Self {
            store,
            connection,
            emitter,
            owner_changes,
            item_signals,
            registrations,
            refresh_tx,
            refresh_rx,
            refresh_active: BTreeSet::new(),
            refresh_queued: BTreeSet::new(),
            refresh_tasks: JoinSet::new(),
            refresh_gate: RefreshGate::default(),
            refresh_scheduled: BTreeMap::new(),
            batches: batches_tx,
            broker,
            publisher,
            internal_shutdown,
        })
    }

    /// Test-only access to the shared store (the run loop owns it
    /// otherwise).
    #[cfg(test)]
    pub(crate) fn store(&self) -> TrayStore {
        Arc::clone(&self.store)
    }

    /// Test-only access to the session connection (verb dispatch dials
    /// items through the watcher's own connection).
    #[cfg(test)]
    pub(crate) fn connection(&self) -> &Connection {
        &self.connection
    }

    /// Serve until `stop` fires. Returns when the adapter is done for;
    /// dropping everything owned here releases the watcher name.
    pub(crate) async fn run_until(mut self, mut stop: watch::Receiver<bool>) -> Result<()> {
        let outcome = self.serve(&mut stop).await;
        // Stop the Bus side cleanly (bounded), then return: the session
        // connection drops with `self`, releasing the watcher name.
        let _ = self.internal_shutdown.send(true);
        if tokio::time::timeout(BROKER_DRAIN, &mut self.broker)
            .await
            .is_err()
        {
            self.broker.abort();
        }
        self.publisher.abort();
        // Refresh fetches die with the run: none of them may keep a
        // connection clone alive past it.
        self.refresh_tasks.abort_all();
        outcome
    }

    async fn serve(&mut self, stop: &mut watch::Receiver<bool>) -> Result<()> {
        loop {
            // Computed before the select: an arm expression may not
            // borrow `self` while the `recv()` arms hold it mutably.
            let refresh_due = self.next_refresh_due();
            tokio::select! {
                biased;
                changed = stop.changed() => {
                    changed.map_err(|_| anyhow!("stop channel ended"))?;
                    if *stop.borrow_and_update() {
                        return Ok(());
                    }
                }
                // Session-bus death, observed authoritatively on the
                // zbus connection itself: one of the three run-enders
                // (stop, session death, internal fault). A Bus (mesh)
                // outage is NOT one — the broker reconnects inside the
                // run and the items survive.
                _ = self.connection.closed() => {
                    return Err(anyhow!("session bus connection closed"));
                }
                message = self.registrations.recv() => match message {
                    None => return Err(anyhow!("watcher interface channel ended")),
                    Some(WatcherMsg::RegisterItem { service, path, owner }) => {
                        self.handle_register(service, path, owner).await;
                    }
                    Some(WatcherMsg::RegisterHost { caller }) => {
                        self.handle_register_host(caller).await;
                    }
                },
                message = self.owner_changes.next() => match message {
                    None => {
                        return Err(anyhow!(
                            "session bus connection ended (NameOwnerChanged stream)"
                        ))
                    }
                    Some(Ok(message)) => {
                        self.handle_owner_change(&message).await;
                    }
                    Some(Err(error)) => {
                        eprintln!("cosmix-dbusd: tray: owner-change receive failed: {error}")
                    }
                },
                message = self.item_signals.next() => match message {
                    None => {
                        return Err(anyhow!(
                            "session bus connection ended (StatusNotifierItem stream)"
                        ))
                    }
                    Some(Ok(message)) => self.handle_item_signal(&message),
                    Some(Err(error)) => {
                        eprintln!("cosmix-dbusd: tray: item signal receive failed: {error}")
                    }
                },
                done = self.refresh_rx.recv() => match done {
                    None => return Err(anyhow!("refresh channel ended")),
                    Some((key, outcome)) => self.handle_refresh_done(key, outcome),
                },
                _ = tokio::time::sleep_until(refresh_due) => {
                    self.fire_due_refreshes();
                }
            }
        }
    }

    /// The earliest gate-delayed refresh deadline (a year out when
    /// nothing is scheduled — tokio's far future is private).
    fn next_refresh_due(&self) -> tokio::time::Instant {
        self.refresh_scheduled
            .values()
            .copied()
            .map(tokio::time::Instant::from_std)
            .min()
            .unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_secs(365 * 86400))
    }

    fn fire_due_refreshes(&mut self) {
        let now = std::time::Instant::now();
        let due: Vec<String> = self
            .refresh_scheduled
            .iter()
            .filter(|(_, due)| **due <= now)
            .map(|(key, _)| key.clone())
            .collect();
        for key in due {
            self.refresh(&key, now);
        }
    }

    /// A registration, fully resolved by the interface (service, path,
    /// owner): the loop only mutates state, emits and publishes — it
    /// never awaits the bus here (see [`WatcherMsg`]). The item lands
    /// immediately with empty props; a property fetch fills it in.
    async fn handle_register(&mut self, service: String, path: String, owner: String) {
        let registered =
            lock_state(&self.store).register(service, path, owner, ItemProps::default());
        match registered {
            Ok((key, events)) => {
                self.emit_item_events(&events).await;
                self.publish(events, "sni.registration");
                self.refresh(&key, std::time::Instant::now());
            }
            Err(error) => eprintln!("cosmix-dbusd: tray: {error}"),
        }
    }

    /// A first-time external host registration gets the
    /// StatusNotifierHostRegistered signal (cosmix itself registered at
    /// startup; repeats for a known host are just recorded).
    async fn handle_register_host(&mut self, caller: String) {
        let newly = lock_state(&self.store)
            .add_host(caller)
            .unwrap_or_else(|error| {
                eprintln!("cosmix-dbusd: tray: {error}");
                false
            });
        if newly
            && let Err(error) = WatcherIface::status_notifier_host_registered(&self.emitter).await
        {
            eprintln!("cosmix-dbusd: tray: StatusNotifierHostRegistered signal failed: {error}");
        }
    }

    async fn handle_owner_change(&mut self, message: &Message) {
        let Ok((name, old_owner, new_owner)) =
            message.body().deserialize::<(String, String, String)>()
        else {
            return;
        };
        let events = lock_state(&self.store).remove_by_bus_change(&name, &old_owner, &new_owner);
        for event in &events {
            self.refresh_gate.retire(&event.key);
            self.refresh_scheduled.remove(&event.key);
            self.refresh_queued.remove(&event.key);
        }
        if events.is_empty() {
            return;
        }
        self.emit_item_events(&events).await;
        self.publish(events, "sni.owner_change");
    }

    /// Any org.kde.StatusNotifierItem signal (NewTitle, NewIcon,
    /// NewStatus, NewToolTip, NewAttentionIcon, …) refreshes the item it
    /// came from — matched by the sending connection and object path.
    fn handle_item_signal(&mut self, message: &Message) {
        let header = message.header();
        let (Some(sender), Some(path)) = (header.sender(), header.path()) else {
            return;
        };
        let Some(key) = lock_state(&self.store)
            .item_by_address(sender.as_str(), path.as_str())
            .map(|item| item.key.clone())
        else {
            return;
        };
        self.refresh(&key, std::time::Instant::now());
    }

    fn handle_refresh_done(&mut self, key: String, outcome: RefreshOutcome) {
        self.refresh_active.remove(&key);
        if lock_state(&self.store).item(&key).is_none() {
            self.refresh_gate.retire(&key);
            self.refresh_scheduled.remove(&key);
            self.refresh_queued.remove(&key);
            return;
        }
        let events = match outcome {
            RefreshOutcome::Props(props) => lock_state(&self.store).apply_props(&key, props),
            RefreshOutcome::Oversized(props) => {
                let mut state = lock_state(&self.store);
                state.note_oversized_read();
                eprintln!(
                    "cosmix-dbusd: tray: item {key} pixmap reply exceeded the \
                     {MAX_PIXMAP_REPLY_BYTES}-byte cap; pixmap dropped"
                );
                state.apply_props(&key, props)
            }
            // Unreachable: keep the last-known props — a refresh error
            // must not wipe known good props with blanks.
            RefreshOutcome::Unreachable => Vec::new(),
        };
        if !events.is_empty() {
            self.publish(events, "sni.refresh");
        }
        if self.refresh_queued.remove(&key) && lock_state(&self.store).item(&key).is_some() {
            self.refresh(&key, std::time::Instant::now());
        }
    }

    /// Fetch the item's props out-of-band (a hung item must not stall
    /// the run loop); coalesce per item while a fetch is in flight and
    /// rate-limit to one fetch per [`MIN_REFRESH_INTERVAL`] — a signal
    /// flood cannot loop the item's property reads. Gate-delayed
    /// refreshes fire from the serve loop when due, so the freshest
    /// signal still converges.
    fn refresh(&mut self, key: &str, now: std::time::Instant) {
        if self.refresh_active.contains(key) {
            self.refresh_queued.insert(key.to_string());
            return;
        }
        if self.refresh_tasks.len() >= MAX_CONCURRENT_REFRESHES {
            self.refresh_queued.insert(key.to_string());
            return;
        }
        if !self.refresh_gate.admit(key, now) {
            self.refresh_scheduled
                .entry(key.to_string())
                .or_insert(now + MIN_REFRESH_INTERVAL);
            return;
        }
        self.refresh_scheduled.remove(key);
        let (service, path) = {
            let state = lock_state(&self.store);
            let Some(item) = state.item(key) else {
                self.refresh_gate.retire(key);
                return;
            };
            (item.service.clone(), item.path.clone())
        };
        let (connection, key) = (self.connection.clone(), key.to_string());
        let done = self.refresh_tx.clone();
        self.refresh_active.insert(key.clone());
        self.refresh_tasks.spawn(async move {
            let outcome = fetch_item_props(&connection, &service, &path).await;
            let _ = done.send((key, outcome)).await;
        });
    }

    /// Emit the watcher signals for a batch (Registered for Added,
    /// Unregistered for Removed) on the session bus — carrying the
    /// `service + path` surface name, matching KDE's watcher.
    async fn emit_item_events(&mut self, events: &[ItemEvent]) {
        for event in events {
            let result = match event.kind {
                ItemEventKind::Added => {
                    WatcherIface::status_notifier_item_registered(&self.emitter, &event.registered)
                        .await
                }
                ItemEventKind::Removed => {
                    WatcherIface::status_notifier_item_unregistered(
                        &self.emitter,
                        &event.registered,
                    )
                    .await
                }
                ItemEventKind::Changed => Ok(()),
            };
            if let Err(error) = result {
                eprintln!(
                    "cosmix-dbusd: tray: watcher signal for {} failed: {error}",
                    event.kind.event_name()
                );
            }
        }
    }

    /// Hand a mutation's events to the Bus publisher with the props
    /// snapshot at that moment. Dropped batches show as event_seq gaps —
    /// the documented signal to re-read tray.props.get.
    fn publish(&mut self, events: Vec<ItemEvent>, cause: &'static str) {
        if events.is_empty() {
            return;
        }
        let snapshot = TrayProps::new(&lock_state(&self.store)).snapshot();
        if self
            .batches
            .try_send(BusBatch {
                events,
                snapshot,
                cause,
            })
            .is_err()
        {
            eprintln!("cosmix-dbusd: tray: event batch dropped (backlog full)");
        }
    }
}

/// The `tray` adapter: `name`/`service` "tray"; run = start the host and
/// serve until the supervisor stops this launch.
#[derive(Default)]
pub struct TrayAdapter;

impl Adapter for TrayAdapter {
    fn name(&self) -> &'static str {
        "tray"
    }

    fn bus_service(&self) -> &'static str {
        BUS_SERVICE
    }

    fn run(self: Box<Self>, mut ctx: AdapterCtx) -> BoxRunFuture {
        Box::pin(async move {
            let address = ctx
                .session_bus()
                .address()
                .map_err(|reason| anyhow!("session bus unavailable: {reason}"))?
                .to_string();
            let host = TrayHost::start(&address).await?;
            ctx.signal_ready();
            host.run_until(ctx.shutdown().clone()).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use zbus::zvariant::{Array, Signature, StructureBuilder};

    /// The SNI pixmap wire type `a(iiay)`, as tuples (zvariant-native).
    type WirePixmap = (i32, i32, Vec<u8>);
    /// The SNI ToolTip wire type `(s a(iiay) s s)`.
    type WireToolTip = (String, Vec<WirePixmap>, String, String);

    fn props(id: &str, title: &str) -> ItemProps {
        ItemProps {
            id: id.into(),
            title: title.into(),
            status: "Active".into(),
            ..ItemProps::default()
        }
    }

    fn registered(state: &mut TrayState, service: &str) -> String {
        registered_by(state, service, ":1.42")
    }

    fn registered_by(state: &mut TrayState, service: &str, owner: &str) -> String {
        state
            .register(
                service.into(),
                DEFAULT_ITEM_PATH.into(),
                owner.into(),
                props("app", "App"),
            )
            .expect("register")
            .0
    }

    // ------------------------------------------------------------------
    // Pure core
    // ------------------------------------------------------------------

    #[test]
    fn registration_assigns_stable_keys_and_monotonic_events() {
        let mut state = TrayState::default();
        let (first, events) = state
            .register(
                "org.kde.StatusNotifierItem-1".into(),
                DEFAULT_ITEM_PATH.into(),
                ":1.10".into(),
                props("app", "App"),
            )
            .expect("register");
        assert_eq!(first, "i0");
        let second = registered(&mut state, "org.kde.StatusNotifierItem-2");
        assert_eq!(second, "i1");
        // First event is the first item's Added; seq strictly increases.
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, ItemEventKind::Added);
        assert_eq!(events[0].key, "i0");
        let seqs: Vec<u64> = state
            .recent_events()
            .iter()
            .map(|event| event.seq)
            .collect();
        assert_eq!(seqs, vec![1, 2]);
        assert_eq!(state.count(), 2);
        // The watcher surface is service + path for every item (M4).
        assert_eq!(
            state.registered_names(),
            vec![
                "org.kde.StatusNotifierItem-1/StatusNotifierItem".to_string(),
                "org.kde.StatusNotifierItem-2/StatusNotifierItem".to_string(),
            ]
        );
        assert_eq!(
            events[0].registered,
            "org.kde.StatusNotifierItem-1/StatusNotifierItem"
        );
    }

    #[test]
    fn re_registration_replaces_the_same_service() {
        let mut state = TrayState::default();
        let first = registered(&mut state, "org.kde.StatusNotifierItem-1");
        assert_eq!(first, "i0");
        let (second, events) = state
            .register(
                "org.kde.StatusNotifierItem-1".into(),
                DEFAULT_ITEM_PATH.into(),
                ":1.43".into(),
                props("app", "App2"),
            )
            .expect("register");
        // The replacement gets a fresh key; the old key is gone.
        assert_eq!(second, "i1");
        assert_eq!(state.count(), 1);
        assert!(state.item("i0").is_none());
        let kinds: Vec<ItemEventKind> = events.iter().map(|event| event.kind).collect();
        assert_eq!(kinds, vec![ItemEventKind::Removed, ItemEventKind::Added]);
        assert_eq!(events[0].key, "i0");
        assert_eq!(events[1].key, "i1");
    }

    /// M4: identity (and dedup) is (service, path) — two path-form items
    /// on one connection are two items, and re-registering one path
    /// replaces only that one. On the old code (dedupe by service) the
    /// second registration replaced the first: count would be 1.
    #[test]
    fn path_form_items_dedupe_on_service_and_path() {
        let mut state = TrayState::default();
        let first = state
            .register(
                ":1.10".into(),
                "/org/ayatana/NotificationItem/one".into(),
                ":1.10".into(),
                props("app", "One"),
            )
            .expect("register")
            .0;
        let second = state
            .register(
                ":1.10".into(),
                "/org/ayatana/NotificationItem/two".into(),
                ":1.10".into(),
                props("app", "Two"),
            )
            .expect("register")
            .0;
        assert_ne!(first, second);
        assert_eq!(state.count(), 2, "two indicators on one connection");
        // Re-registering path one replaces only it.
        let (third, events) = state
            .register(
                ":1.10".into(),
                "/org/ayatana/NotificationItem/one".into(),
                ":1.10".into(),
                props("app", "One again"),
            )
            .expect("re-register at the same path");
        assert_eq!(state.count(), 2);
        assert!(state.item(&first).is_none());
        assert!(state.item(&second).is_some());
        let kinds: Vec<ItemEventKind> = events.iter().map(|event| event.kind).collect();
        assert_eq!(kinds, vec![ItemEventKind::Removed, ItemEventKind::Added]);
        assert_eq!(third, "i2");
        // Both surface separately, as service+path.
        assert_eq!(
            state.registered_names(),
            vec![
                ":1.10/org/ayatana/NotificationItem/two".to_string(),
                ":1.10/org/ayatana/NotificationItem/one".to_string(),
            ]
        );
    }

    #[test]
    fn registration_is_refused_past_the_item_cap() {
        let mut state = TrayState::default();
        for number in 0..MAX_ITEMS {
            registered_by(
                &mut state,
                &format!("org.kde.StatusNotifierItem-{number}"),
                &format!(":1.{number}"),
            );
        }
        assert_eq!(state.count(), MAX_ITEMS);
        let refused = state
            .register(
                "org.kde.StatusNotifierItem-overflow".into(),
                DEFAULT_ITEM_PATH.into(),
                ":1.99".into(),
                ItemProps::default(),
            )
            .expect_err("the cap must refuse");
        assert!(refused.contains("limit"), "{refused}");
    }

    /// m2: at the cap, re-registering an already-tracked item is a
    /// replacement, not a new item — it must succeed. The old code
    /// refused it (cap checked before dedup).
    #[test]
    fn re_registration_at_the_cap_succeeds() {
        let mut state = TrayState::default();
        for number in 0..MAX_ITEMS {
            registered_by(
                &mut state,
                &format!("org.kde.StatusNotifierItem-{number}"),
                &format!(":1.{number}"),
            );
        }
        let (key, events) = state
            .register(
                "org.kde.StatusNotifierItem-1".into(),
                DEFAULT_ITEM_PATH.into(),
                ":1.1".into(),
                props("app", "Refreshed"),
            )
            .expect("re-registration at the cap replaces");
        assert_eq!(state.count(), MAX_ITEMS);
        assert_eq!(key, format!("i{MAX_ITEMS}"));
        let kinds: Vec<ItemEventKind> = events.iter().map(|event| event.kind).collect();
        assert_eq!(kinds, vec![ItemEventKind::Removed, ItemEventKind::Added]);
    }

    /// m3: one connection cannot fill the tray from every other app's
    /// names — the per-owner cap refuses the ninth item for an owner.
    #[test]
    fn registration_is_capped_per_connection() {
        let mut state = TrayState::default();
        for number in 0..MAX_ITEMS_PER_OWNER {
            registered_by(
                &mut state,
                &format!("org.kde.StatusNotifierItem-{number}"),
                ":1.42",
            );
        }
        let refused = state
            .register(
                "org.kde.StatusNotifierItem-more".into(),
                DEFAULT_ITEM_PATH.into(),
                ":1.42".into(),
                ItemProps::default(),
            )
            .expect_err("the per-connection cap must refuse");
        assert!(refused.contains("per-connection"), "{refused}");
        // A different connection is unaffected, and replacements at the
        // per-owner cap are allowed.
        registered_by(&mut state, "org.kde.StatusNotifierItem-other", ":1.43");
        state
            .register(
                "org.kde.StatusNotifierItem-0".into(),
                DEFAULT_ITEM_PATH.into(),
                ":1.42".into(),
                props("app", "Again"),
            )
            .expect("replacement at the per-owner cap");
        assert_eq!(state.count(), MAX_ITEMS_PER_OWNER + 1);
    }

    #[test]
    fn owner_vanish_and_name_loss_remove_their_items() {
        let mut state = TrayState::default();
        let gone = registered(&mut state, "org.kde.StatusNotifierItem-gone");
        let keeper = registered(&mut state, "org.kde.StatusNotifierItem-keeper");
        // All registered under owner :1.42 by `registered`; the unique
        // name :1.42 vanishing (name == old_owner, new owner empty).
        let events = state.remove_by_bus_change(":1.42", ":1.42", "");
        assert_eq!(events.len(), 2);
        assert!(state.item(&gone).is_none() && state.item(&keeper).is_none());

        // A well-known name losing its owner (connection alive) also
        // removes the item registered under it ...
        let mut state = TrayState::default();
        registered(&mut state, "org.kde.StatusNotifierItem-move");
        assert_eq!(
            state
                .remove_by_bus_change("org.kde.StatusNotifierItem-move", ":1.5", "")
                .len(),
            1
        );
        // ... and so does the name moving to a different owner.
        let mut state = TrayState::default();
        registered(&mut state, "org.kde.StatusNotifierItem-move2");
        assert_eq!(
            state
                .remove_by_bus_change("org.kde.StatusNotifierItem-move2", ":1.5", ":1.6")
                .len(),
            1
        );
    }

    /// M3: a connection releasing an UNRELATED name (MPRIS, anything)
    /// must not reap that connection's items. The old code removed any
    /// item whose owner matched old_owner, whatever the name.
    #[test]
    fn an_unrelated_name_release_reaps_nothing() {
        let mut state = TrayState::default();
        registered(&mut state, "org.kde.StatusNotifierItem-stay");
        let events = state.remove_by_bus_change("org.mpris.MediaPlayer2.player", ":1.42", "");
        assert!(events.is_empty(), "{events:?}");
        assert_eq!(state.count(), 1);
        // Same for a name the connection still holds moving nowhere.
        assert!(
            state
                .remove_by_bus_change("org.other.name", ":1.42", ":1.77")
                .is_empty()
        );
        assert_eq!(state.count(), 1);
    }

    /// m7: host registrations are deduped, capped, and pruned when
    /// their connection vanishes.
    #[test]
    fn hosts_are_deduped_capped_and_pruned_with_their_connections() {
        let mut state = TrayState::default();
        assert!(state.add_host(":1.5".into()).expect("first host"));
        assert!(!state.add_host(":1.5".into()).expect("repeat host"));
        for number in 6..6 + MAX_HOSTS - 1 {
            state
                .add_host(format!(":1.{number}"))
                .expect("host below the cap");
        }
        let refused = state
            .add_host(":1.99".into())
            .expect_err("the host cap must refuse");
        assert!(refused.contains("host limit"), "{refused}");
        assert_eq!(state.hosts().len(), MAX_HOSTS);
        // The vanished connection's host registration goes with it.
        state.remove_by_bus_change(":1.5", ":1.5", "");
        assert!(!state.hosts().contains(&":1.5".to_string()));
        // A name release by a live connection prunes no host.
        state.remove_by_bus_change("org.mpris.MediaPlayer2.x", ":1.6", "");
        assert!(state.hosts().contains(&":1.6".to_string()));
    }

    #[test]
    fn apply_props_only_events_on_a_real_change() {
        let mut state = TrayState::default();
        let key = registered(&mut state, "org.kde.StatusNotifierItem-1");
        let same = props("app", "App");
        assert!(state.apply_props(&key, same).is_empty());
        let changed = props("app", "New title");
        let events = state.apply_props(&key, changed);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, ItemEventKind::Changed);
        assert_eq!(state.item(&key).unwrap().props.title, "New title");
        // A vanished key is a no-op, not a panic.
        assert!(state.apply_props("i9", props("app", "App")).is_empty());
    }

    #[test]
    fn strings_are_clamped_to_the_bound() {
        let mut clamped = props("app", &"x".repeat(MAX_STRING_CHARS + 10));
        clamped.clamp_strings();
        assert_eq!(clamped.title.chars().count(), MAX_STRING_CHARS);
    }

    #[test]
    fn props_snapshot_covers_items_and_the_pixmap_size() {
        let mut state = TrayState::default();
        state.set_host_registered(true);
        let key = registered(&mut state, "org.kde.StatusNotifierItem-1");
        let tree = TrayProps::new(&state);
        let snapshot: Json = (&tree.snapshot()).into();
        assert_eq!(snapshot["count"], 1);
        assert_eq!(snapshot[key.as_str()]["title"], "App");
        assert_eq!(snapshot[key.as_str()]["status"], "Active");
        assert_eq!(snapshot[key.as_str()]["has_menu"], false);
        assert!(snapshot[key.as_str()].get("pixmap_width").is_none());

        let with_pixmap = ItemProps {
            pixmap: Some(Pixmap {
                width: 4,
                height: 4,
                data: vec![0; 64],
            }),
            ..props("app", "App")
        };
        state.apply_props(&key, with_pixmap);
        let tree = TrayProps::new(&state);
        let snapshot: Json = (&tree.snapshot()).into();
        assert_eq!(snapshot[key.as_str()]["pixmap_width"], 4);
        assert_eq!(snapshot[key.as_str()]["pixmap_height"], 4);
        // Pixels never ride the props surface.
        assert!(snapshot[key.as_str()].get("pixmap_data").is_none());
        let list: Vec<String> = tree.list().iter().map(|path| path.to_string()).collect();
        assert!(list.contains(&format!("{key}.icon_name")));
        assert!(list.contains(&format!("{key}.pixmap_width")));
    }

    /// m6: the refresh gate spaces one item's refreshes at least
    /// MIN_REFRESH_INTERVAL apart (pure core of the coalescing rule).
    #[test]
    fn refresh_gate_waits_for_the_minimum_interval() {
        let mut gate = RefreshGate::default();
        let start = std::time::Instant::now();
        assert!(gate.admit("i0", start));
        // A signal storm inside the interval is deferred ...
        assert!(!gate.admit("i0", start + MIN_REFRESH_INTERVAL / 5));
        assert!(!gate.admit("i0", start + MIN_REFRESH_INTERVAL * 4 / 5));
        // ... and the next one after it fires.
        assert!(gate.admit("i0", start + MIN_REFRESH_INTERVAL));
        // Other items are not gated by it.
        assert!(gate.admit("i1", start));
        gate.retire("i0");
        assert!(gate.admit("i0", start + MIN_REFRESH_INTERVAL / 2));
    }

    /// n1: one event_seq shape — a JSON number in bodies, and the watch
    /// reply documents it (the old body carried a separate
    /// "event_sequence" prose key).
    #[test]
    fn watch_body_carries_the_documented_event_seq_shape() {
        let store: TrayStore = Arc::new(Mutex::new(TrayState::default()));
        lock_state(&store).add_host(":1.5".into()).expect("host");
        let (rc, body) = dispatch_props(&command("tray.props.watch", Json::Null), &store, "watch");
        assert_eq!(rc, 0);
        let watch: Json = serde_json::from_str(&body).expect("watch body is JSON");
        assert_eq!(watch["event_seq"], 0, "the current counter, as a number");
        assert!(
            watch["event_seq_note"].as_str().is_some(),
            "the shape is documented in the body: {watch}"
        );
        assert!(
            watch.get("event_sequence").is_none(),
            "one shape, no second event_seq-ish key: {watch}"
        );
    }

    // ------------------------------------------------------------------
    // Pure helpers: menu tree + pixmaps + base64 + click body
    // ------------------------------------------------------------------

    fn menu_value(
        id: i32,
        entries: &[(&str, Value<'static>)],
        children: Vec<Value<'static>>,
    ) -> Value<'static> {
        let key_signature: Signature = "s".try_into().expect("key signature");
        let value_signature: Signature = "v".try_into().expect("value signature");
        let mut dict = Dict::new(&key_signature, &value_signature);
        for (key, value) in entries {
            dict.add(key.to_string(), value.clone())
                .expect("dict entry");
        }
        Value::Structure(
            StructureBuilder::new()
                .append_field(Value::I32(id))
                .append_field(Value::Dict(dict))
                .append_field(Value::Array(Array::from(children)))
                .build()
                .expect("structure"),
        )
    }

    #[test]
    fn menu_tree_maps_the_dbusmenu_fields() {
        let nested = menu_value(8, &[("label", Value::from("Nested"))], vec![]);
        let open = menu_value(5, &[("label", Value::from("Open"))], vec![]);
        let check = menu_value(
            6,
            &[
                ("label", Value::from("Check")),
                ("toggle-type", Value::from("checkmark")),
                ("toggle-state", Value::I32(1)),
                ("enabled", Value::Bool(false)),
            ],
            vec![],
        );
        let separator = menu_value(7, &[("type", Value::from("separator"))], vec![]);
        let submenu = menu_value(
            9,
            &[
                ("label", Value::from("Sub")),
                ("children-display", Value::from("submenu")),
            ],
            // Children go in as plain nodes: `Array::from` frames them
            // as variants per the "av" signature (a manual Value::Value
            // here would double-wrap and defeat the parser).
            vec![nested],
        );
        let root = menu_value(0, &[], vec![open, check, separator, submenu]);
        let mut budget = MAX_MENU_NODES;
        let json = menu_node_json(&root, &mut budget).expect("layout parses");
        // dbusmenu defaults: absent enabled/visible are true.
        assert_eq!(json["id"], 0);
        assert_eq!(json["enabled"], true);
        assert_eq!(json["visible"], true);
        assert_eq!(json["type"], "standard");
        assert_eq!(json["truncated"], false);
        let children = json["children"].as_array().unwrap();
        assert_eq!(children.len(), 4);
        assert_eq!(children[0]["label"], "Open");
        assert_eq!(children[0]["enabled"], true);
        assert_eq!(children[1]["enabled"], false);
        assert_eq!(children[1]["toggle_type"], "checkmark");
        assert_eq!(children[1]["toggle_state"], 1);
        assert_eq!(children[2]["type"], "separator");
        assert_eq!(children[3]["children"][0]["label"], "Nested");
    }

    /// m8: a tree cut by the node budget says so — "truncated": true on
    /// the node whose children were cut. The old code dropped them
    /// silently (no truncated key anywhere).
    #[test]
    fn menu_tree_is_capped_and_says_it_is_truncated() {
        let leaf = || menu_value(1, &[("label", Value::from("L"))], vec![]);
        let mut root_children = Vec::new();
        for _ in 0..=MAX_MENU_NODES {
            root_children.push(leaf());
        }
        let root = menu_value(0, &[], root_children);
        let mut budget = MAX_MENU_NODES;
        let json = menu_node_json(&root, &mut budget).expect("root parses");
        // The root consumed one node of the budget; the children get the
        // rest — and not one more.
        assert_eq!(
            json["children"].as_array().unwrap().len(),
            MAX_MENU_NODES - 1
        );
        assert_eq!(budget, 0);
        assert_eq!(
            json["truncated"], true,
            "a budget-cut tree must announce it: {json}"
        );
    }

    /// n3: menu node type strings are clamped like labels (a hostile
    /// item cannot stuff megabytes through the "type" or "toggle-type"
    /// keys).
    #[test]
    fn menu_node_type_strings_are_clamped() {
        let huge = "x".repeat(MAX_STRING_CHARS + 10);
        let node = menu_value(
            3,
            &[
                ("type", Value::from(huge.clone())),
                ("toggle-type", Value::from(huge)),
            ],
            vec![],
        );
        let mut budget = MAX_MENU_NODES;
        let json = menu_node_json(&node, &mut budget).expect("node parses");
        assert_eq!(
            json["type"].as_str().unwrap().chars().count(),
            MAX_STRING_CHARS
        );
        assert_eq!(
            json["toggle_type"].as_str().unwrap().chars().count(),
            MAX_STRING_CHARS
        );
    }

    /// m11: a pixmap entry only counts when data.len() == w*h*4 — the
    /// old code accepted any length under the byte cap.
    #[test]
    fn pixmaps_validate_their_dimensions() {
        // Array::from(Vec<Value>) wraps each element in a variant
        // ("av") — NOT the a(iiay) wire shape largest_pixmap parses;
        // Array::new + append is signature-checked and unwrapped.
        let build = |entries: Vec<WirePixmap>| {
            let pixmap_signature: Signature = "(iiay)".try_into().expect("entry signature");
            let mut array = Array::new(&pixmap_signature);
            let byte_signature: Signature = "y".try_into().expect("byte signature");
            for (width, height, data) in entries {
                let mut bytes = Array::new(&byte_signature);
                for byte in data {
                    bytes.append(Value::U8(byte)).expect("append byte");
                }
                array
                    .append(Value::Structure(
                        StructureBuilder::new()
                            .append_field(Value::I32(width))
                            .append_field(Value::I32(height))
                            .append_field(Value::Array(bytes))
                            .build()
                            .expect("structure"),
                    ))
                    .expect("append pixmap entry");
            }
            largest_pixmap(&Value::Array(array))
        };
        // Declared 4x4 but only 10 bytes of data: dropped.
        assert!(build(vec![(4, 4, vec![0; 10])]).is_none());
        // Exact ARGB32 payload: kept.
        let exact = build(vec![(4, 4, vec![0x5a; 64])]).expect("valid pixmap kept");
        assert_eq!((exact.width, exact.height), (4, 4));
        // Largest valid entry wins over a smaller valid one.
        let largest =
            build(vec![(2, 2, vec![1; 16]), (8, 8, vec![2; 256])]).expect("valid pixmaps kept");
        assert_eq!(largest.width, 8);
    }

    /// B1: the Event body is `(i s v u)` — u32 timestamp, the data a
    /// SINGLE variant wrapping int32. The old body was (isvx): an i64
    /// timestamp no conforming dbusmenu server accepts.
    #[test]
    fn the_click_event_body_is_isvu_with_a_single_variant() {
        let body = click_event_body(5);
        let context = zvariant::serialized::Context::new_dbus(zvariant::NATIVE_ENDIAN, 0);
        let encoded = zvariant::to_bytes(context, &body).expect("encodes");
        // Decoding as (i32, String, variant, u32) proves the wire shape;
        // an i64 timestamp (signature x) would not decode as u32.
        let (decoded, _): ((i32, String, zvariant::OwnedValue, u32), usize) =
            encoded.deserialize().expect("decodes as (isvu)");
        assert_eq!(decoded.0, 5);
        assert_eq!(decoded.1, "clicked");
        assert!(
            matches!(
                Value::from(decoded.2),
                Value::Value(ref inner) if matches!(inner.as_ref(), Value::I32(0))
            ),
            "the data is exactly one variant wrapping int32"
        );
        assert_eq!(
            <(i32, String, Value<'_>, u32) as zvariant::Type>::SIGNATURE.to_string(),
            "(isvu)"
        );
    }

    #[test]
    fn base64_matches_the_standard_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        let big: Vec<u8> = (0..=255u8).cycle().take(3000).collect();
        assert_eq!(base64_encode(&big).len(), big.len().div_ceil(3) * 4);
    }

    // ------------------------------------------------------------------
    // The Bus publisher (m13): diff publishing, baseline survival, the
    // one event_seq shape — against a fake, no broker needed.
    // ------------------------------------------------------------------

    struct FakePublisher {
        published: Mutex<Vec<(String, cosmix_bus::bus::BusMessage)>>,
        broken: AtomicBool,
    }

    impl FakePublisher {
        fn topics(&self) -> Vec<String> {
            self.published
                .lock()
                .expect("published")
                .iter()
                .map(|(topic, _)| topic.clone())
                .collect()
        }
    }

    impl EventPublisher for FakePublisher {
        fn publish_event(
            &self,
            topic: &str,
            message: cosmix_bus::bus::BusMessage,
        ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
            if self.broken.load(Ordering::Relaxed) {
                return Box::pin(std::future::ready(Err(anyhow!("bus down"))));
            }
            self.published
                .lock()
                .expect("published")
                .push((topic.to_string(), message));
            Box::pin(std::future::ready(Ok(())))
        }
    }

    fn batch_for(state: &mut TrayState, cause: &'static str) -> BusBatch {
        // Register a fresh item (or change its title) and snapshot.
        let count = state.count();
        let events = if count == 0 {
            state
                .register(
                    "org.kde.StatusNotifierItem-pub".into(),
                    DEFAULT_ITEM_PATH.into(),
                    ":1.42".into(),
                    props("app", "One"),
                )
                .expect("register")
                .1
        } else {
            state.apply_props("i0", props("app", "Two"))
        };
        let snapshot = TrayProps::new(state).snapshot();
        BusBatch {
            events,
            snapshot,
            cause,
        }
    }

    async fn spawn_publisher(
        fake: Arc<FakePublisher>,
    ) -> (
        mpsc::Sender<BusBatch>,
        watch::Sender<Option<Arc<dyn EventPublisher>>>,
        mpsc::Receiver<()>,
        JoinHandle<Result<()>>,
    ) {
        let (batches_tx, batches_rx) = mpsc::channel(64);
        let (clients_tx, clients_rx) = watch::channel(Some(fake as Arc<dyn EventPublisher>));
        let (faults_tx, faults_rx) = mpsc::channel(1);
        let task = tokio::spawn(run_event_publisher(batches_rx, clients_rx, faults_tx));
        (batches_tx, clients_tx, faults_rx, task)
    }

    /// The first successful batch sets the baseline without a diff; the
    /// next batch publishes the props diff against it plus the domain
    /// event — event_seq as a number in the body, a decimal string in
    /// the message headers (n1's one shape).
    #[tokio::test]
    async fn the_publisher_diffs_against_the_surviving_baseline() {
        let fake = Arc::new(FakePublisher {
            published: Mutex::new(Vec::new()),
            broken: AtomicBool::new(false),
        });
        let (batches, _clients, _faults, publisher) = spawn_publisher(Arc::clone(&fake)).await;

        let mut state = TrayState::default();
        let first = batch_for(&mut state, "test");
        let first_seq = first.events[0].seq;
        batches.send(first).await.expect("batch one");
        let second = batch_for(&mut state, "test");
        let second_seq = second.events[0].seq;
        batches.send(second).await.expect("batch two");
        drop(batches);
        publisher
            .await
            .expect("publisher task")
            .expect("publisher ok");

        let topics = fake.topics();
        assert_eq!(
            topics,
            vec![
                TOPIC_ITEM_ADDED.to_string(),
                props_changed_topic(BUS_SERVICE),
                TOPIC_ITEM_CHANGED.to_string()
            ],
            "added, then the title diff, then changed"
        );
        let guard = fake.published.lock().expect("published");
        // n1: header event_seq is a decimal string of the body's number.
        let added = &guard[0].1;
        let first_stamped = first_seq.to_string();
        assert_eq!(added.get("event_seq"), Some(first_stamped.as_str()));
        let body: Json = serde_json::from_str(&added.body).expect("event body is JSON");
        assert_eq!(body["event_seq"], first_seq);
        assert_eq!(body["data"]["key"], "i0");
        assert_eq!(
            body["data"]["registered"],
            "org.kde.StatusNotifierItem-pub/StatusNotifierItem"
        );
        // The diff carries the old and new title.
        let diff = &guard[1].1;
        assert!(
            diff.body.contains("One") && diff.body.contains("Two"),
            "{}",
            diff.body
        );
        let second_stamped = second_seq.to_string();
        assert_eq!(diff.get("event_seq"), Some(second_stamped.as_str()));
    }

    /// The F8/J2 contract: a publish failure keeps the baseline, so the
    /// next successful batch re-diffs the whole outage window (old
    /// value from BEFORE the outage, not the lost intermediate one).
    #[tokio::test]
    async fn a_failed_publish_keeps_the_baseline_for_the_outage_diff() {
        let fake = Arc::new(FakePublisher {
            published: Mutex::new(Vec::new()),
            broken: AtomicBool::new(false),
        });
        let (batches, _clients, mut faults, publisher) = spawn_publisher(Arc::clone(&fake)).await;

        let mut state = TrayState::default();
        // One good batch establishes the baseline ("One") — and is
        // processed before the outage starts (no send/processing race).
        batches
            .send(batch_for(&mut state, "test"))
            .await
            .expect("batch one");
        wait_until(Duration::from_secs(5), || async {
            fake.published.lock().expect("published").len() == 1
        })
        .await;
        // The outage: this batch's publishes fail; the baseline stays.
        fake.broken.store(true, Ordering::Relaxed);
        batches
            .send(batch_for(&mut state, "test"))
            .await
            .expect("batch two");
        tokio::time::timeout(Duration::from_secs(5), faults.recv())
            .await
            .expect("the publisher faults the broker")
            .expect("fault channel alive");
        // Recovery: a third batch succeeds and must diff from "One"
        // straight to the current title — the intermediate "Two" was
        // never delivered, so it must not appear as an old value.
        fake.broken.store(false, Ordering::Relaxed);
        let events = state.apply_props("i0", props("app", "Three"));
        assert!(!events.is_empty());
        let third = BusBatch {
            events,
            snapshot: TrayProps::new(&state).snapshot(),
            cause: "test",
        };
        batches.send(third).await.expect("batch three");
        drop(batches);
        publisher
            .await
            .expect("publisher task")
            .expect("publisher ok");

        let guard = fake.published.lock().expect("published");
        let diff = guard
            .iter()
            .map(|(_, message)| message)
            .find(|message| {
                message.get("command") == Some("props.changed") && message.body.contains("Three")
            })
            .unwrap_or_else(|| {
                panic!(
                    "an outage-window diff mentioning Three exists; published: {:?}",
                    guard
                        .iter()
                        .map(|(topic, message)| (topic, message.body.clone()))
                        .collect::<Vec<_>>()
                )
            });
        assert!(
            diff.body.contains("One"),
            "diffed from before the outage: {}",
            diff.body
        );
    }

    // ------------------------------------------------------------------
    // D-Bus-side test doubles
    // ------------------------------------------------------------------

    type CallLog = Arc<Mutex<Vec<String>>>;

    /// How the test item answers IconPixmap.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum PixmapMode {
        Normal,
        /// A single >4 MiB pixmap: the raw-reply cap must refuse it
        /// before any deserialization (M2).
        Oversized,
    }

    #[derive(Debug, Default)]
    struct TestItemState {
        title: String,
        hang_activate: bool,
        menu_path: &'static str,
        pixmap_oversized: bool,
        /// Title property reads — how the m6 coalescing test counts
        /// refreshes.
        title_reads: AtomicUsize,
    }

    impl TestItemState {
        fn new() -> Self {
            Self {
                title: "Original title".into(),
                menu_path: "/MenuBarItem",
                ..Self::default()
            }
        }
    }

    #[derive(Debug)]
    struct TestItem {
        state: Arc<Mutex<TestItemState>>,
        calls: CallLog,
    }

    fn pixmap(width: i32) -> WirePixmap {
        (width, width, vec![0x5a; (width * width * 4) as usize])
    }

    #[interface(name = "org.kde.StatusNotifierItem")]
    impl TestItem {
        #[zbus(property)]
        fn id(&self) -> fdo::Result<String> {
            Ok("sni-test-1".into())
        }

        #[zbus(property)]
        fn title(&self) -> fdo::Result<String> {
            let state = self.state.lock().expect("item state");
            state.title_reads.fetch_add(1, Ordering::Relaxed);
            Ok(state.title.clone())
        }

        #[zbus(property)]
        fn category(&self) -> fdo::Result<String> {
            Ok("SystemServices".into())
        }

        #[zbus(property)]
        fn status(&self) -> fdo::Result<String> {
            Ok("Active".into())
        }

        #[zbus(property)]
        fn icon_name(&self) -> fdo::Result<String> {
            Ok("test-icon".into())
        }

        #[zbus(property)]
        fn icon_theme_path(&self) -> fdo::Result<String> {
            Ok("/usr/share/icons".into())
        }

        #[zbus(property)]
        fn attention_icon_name(&self) -> fdo::Result<String> {
            Ok("test-attention-icon".into())
        }

        #[zbus(property)]
        fn tool_tip(&self) -> fdo::Result<WireToolTip> {
            Ok((
                "tip-icon".into(),
                Vec::new(),
                "Tip title".into(),
                "Tip description".into(),
            ))
        }

        #[zbus(property)]
        fn menu(&self) -> fdo::Result<zvariant::OwnedObjectPath> {
            let menu = self.state.lock().expect("item state").menu_path;
            Ok(menu.try_into().expect("menu path"))
        }

        #[zbus(property)]
        fn item_is_menu(&self) -> fdo::Result<bool> {
            Ok(false)
        }

        #[zbus(property)]
        fn icon_pixmap(&self) -> fdo::Result<Vec<WirePixmap>> {
            let oversized = self.state.lock().expect("item state").pixmap_oversized;
            if oversized {
                // 1024 * 1280 * 4 = 5 MiB: over every cap, still well
                // under zbus's 128 MiB message limit — only the
                // adapter's own raw-reply cap can stop it.
                Ok(vec![(1024, 1280, vec![0x41; 5 * 1024 * 1024])])
            } else {
                Ok(vec![pixmap(2), pixmap(4)])
            }
        }

        async fn activate(&self, x: i32, y: i32) {
            // Bind before the await: a lock temporary in the `if`
            // condition would be held across it (clippy agrees).
            let hang = self.state.lock().expect("item state").hang_activate;
            if hang {
                // A hung app: this call never replies.
                std::future::pending::<()>().await;
            }
            self.calls
                .lock()
                .expect("calls")
                .push(format!("Activate({x},{y})"));
        }

        async fn secondary_activate(&self, x: i32, y: i32) {
            self.calls
                .lock()
                .expect("calls")
                .push(format!("SecondaryActivate({x},{y})"));
        }

        async fn context_menu(&self, x: i32, y: i32) {
            self.calls
                .lock()
                .expect("calls")
                .push(format!("ContextMenu({x},{y})"));
        }

        async fn scroll(&self, delta: i32, orientation: &str) {
            self.calls
                .lock()
                .expect("calls")
                .push(format!("Scroll({delta},{orientation})"));
        }

        #[zbus(signal)]
        async fn new_title(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
    }

    struct TestMenu {
        events: CallLog,
    }

    #[interface(name = "com.canonical.dbusmenu")]
    impl TestMenu {
        async fn get_layout(
            &self,
            _parent: i32,
            _depth: i32,
            properties: Vec<String>,
        ) -> fdo::Result<(u32, Value<'static>)> {
            self.events
                .lock()
                .expect("menu events")
                .push(format!("GetLayout({properties:?})"));
            let nested = menu_value(8, &[("label", Value::from("Nested"))], vec![]);
            let open = menu_value(5, &[("label", Value::from("Open"))], vec![]);
            let check = menu_value(
                6,
                &[
                    ("label", Value::from("Check")),
                    ("toggle-type", Value::from("checkmark")),
                    ("toggle-state", Value::I32(1)),
                ],
                vec![],
            );
            let separator = menu_value(7, &[("type", Value::from("separator"))], vec![]);
            let submenu = menu_value(
                9,
                &[("children-display", Value::from("submenu"))],
                vec![nested],
            );
            let root = menu_value(
                0,
                &[("label", Value::from("Root"))],
                vec![open, check, separator, submenu],
            );
            let Value::Structure(structure) = root else {
                unreachable!("menu_value builds a structure");
            };
            // The layout goes out as the bare struct — exactly the
            // `(u(ia{sv}av))` shape real dbusmenu servers reply with.
            Ok((7, Value::Structure(structure)))
        }

        /// dbusmenu Event is `(i s v u)`: the typed u32 timestamp makes
        /// this fixture REJECT the old (isvx) body — the click test then
        /// fails on the old client (B1's proving test).
        async fn event(&self, id: i32, event_id: &str, _data: Value<'_>, _timestamp: u32) {
            self.events
                .lock()
                .expect("menu events")
                .push(format!("Event({id},{event_id})"));
        }

        async fn about_to_show(&self, id: i32) -> fdo::Result<bool> {
            self.events
                .lock()
                .expect("menu events")
                .push(format!("AboutToShow({id})"));
            Ok(true)
        }
    }

    /// How to spawn a test app.
    struct TestAppSpec<'a> {
        address: &'a str,
        well_known: Option<&'static str>,
        item_path: &'static str,
        menu_path: &'static str,
        pixmap: PixmapMode,
    }

    impl<'a> TestAppSpec<'a> {
        fn new(
            address: &'a str,
            well_known: Option<&'static str>,
            item_path: &'static str,
        ) -> Self {
            Self {
                address,
                well_known,
                item_path,
                menu_path: "/MenuBarItem",
                pixmap: PixmapMode::Normal,
            }
        }
    }

    /// One test app: a connection serving an SNI item (and its menu),
    /// optionally under a well-known name.
    struct TestApp {
        connection: Connection,
        state: Arc<Mutex<TestItemState>>,
        calls: CallLog,
        emitter: SignalEmitter<'static>,
    }

    impl TestApp {
        async fn spawn(spec: TestAppSpec<'_>) -> Self {
            let state = Arc::new(Mutex::new({
                let mut item = TestItemState::new();
                item.menu_path = spec.menu_path;
                item.pixmap_oversized = spec.pixmap == PixmapMode::Oversized;
                item
            }));
            let calls: CallLog = Arc::new(Mutex::new(Vec::new()));
            let address: zbus::address::Address = spec.address.parse().expect("test bus address");
            let mut builder = zbus::connection::Builder::address(address)
                .expect("builder")
                .serve_at(
                    spec.item_path,
                    TestItem {
                        state: Arc::clone(&state),
                        calls: Arc::clone(&calls),
                    },
                )
                .expect("serve item")
                .serve_at(
                    spec.menu_path,
                    TestMenu {
                        events: Arc::clone(&calls),
                    },
                )
                .expect("serve menu");
            if let Some(well_known) = spec.well_known {
                builder = builder
                    .name(WellKnownName::from_static_str(well_known).expect("well-known name"))
                    .expect("own name");
            }
            let connection = builder.build().await.expect("test app connects");
            let emitter = SignalEmitter::new(&connection, spec.item_path).expect("emitter");
            Self {
                connection,
                state,
                calls,
                emitter,
            }
        }

        /// A second SNI item on the SAME connection at another path
        /// (M4: two path-form indicators, one connection).
        async fn serve_second_item(&self, path: &'static str) {
            self.connection
                .object_server()
                .at(
                    zvariant::ObjectPath::from_static_str(path).expect("item path"),
                    TestItem {
                        state: Arc::new(Mutex::new(TestItemState::new())),
                        calls: Arc::clone(&self.calls),
                    },
                )
                .await
                .expect("second item served");
        }
    }

    /// A private `dbus-daemon --session` per test. Without dbus-daemon
    /// the integration tests FAIL loudly (m12) unless
    /// COSMIX_SKIP_DBUS_TESTS=1 says otherwise.
    struct PrivateBus {
        child: tokio::process::Child,
        address: String,
    }

    async fn private_bus() -> Option<PrivateBus> {
        use tokio::io::AsyncBufReadExt as _;

        if std::env::var_os("COSMIX_SKIP_DBUS_TESTS").is_some() {
            eprintln!("skipped: COSMIX_SKIP_DBUS_TESTS is set");
            return None;
        }
        let mut child = tokio::process::Command::new("dbus-daemon")
            .args(["--session", "--print-address", "--nofork"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn();
        if let Err(error) = &mut child {
            panic!(
                "dbus-daemon is not available ({error}) — the tray adapter integration tests \
                 need one; install dbus or set COSMIX_SKIP_DBUS_TESTS=1"
            );
        }
        let mut child = child.expect("spawned");
        let stdout = child.stdout.take().expect("piped stdout");
        let mut address = String::new();
        let mut reader = tokio::io::BufReader::new(stdout);
        match tokio::time::timeout(Duration::from_secs(10), reader.read_line(&mut address)).await {
            Ok(Ok(_)) if !address.trim().is_empty() => {
                address = address.trim().to_string();
            }
            _ => {
                let _ = child.start_kill();
                panic!("dbus-daemon printed no address");
            }
        }
        Some(PrivateBus { child, address })
    }

    impl Drop for PrivateBus {
        fn drop(&mut self) {
            let _ = self.child.start_kill();
        }
    }

    async fn plain_connection(address: &str) -> Connection {
        let parsed: zbus::address::Address = address.parse().expect("address parses");
        zbus::connection::Builder::address(parsed)
            .expect("builder")
            .build()
            .await
            .expect("connect")
    }

    async fn watcher_proxy(connection: &Connection) -> Proxy<'static> {
        ProxyBuilder::<Proxy>::new(connection)
            .destination(WATCHER_NAME)
            .expect("destination")
            .path(WATCHER_PATH)
            .expect("path")
            .interface(WATCHER_IFACE)
            .expect("interface")
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .expect("watcher proxy")
    }

    async fn register_with_watcher(connection: &Connection, argument: &str) {
        watcher_proxy(connection)
            .await
            .call_method("RegisterStatusNotifierItem", &[argument])
            .await
            .expect("RegisterStatusNotifierItem replies");
    }

    /// The raw reply of a RegisterStatusNotifierItem call — error name
    /// and all, so refusal tests can pin the D-Bus error (m1).
    async fn register_reply(
        connection: &Connection,
        argument: &str,
    ) -> std::result::Result<(), String> {
        let reply = watcher_proxy(connection)
            .await
            .call_method("RegisterStatusNotifierItem", &[argument])
            .await;
        match reply {
            Ok(_) => Ok(()),
            Err(zbus::Error::MethodError(name, message, _)) => {
                Err(format!("{}: {}", name, message.unwrap_or_default()))
            }
            Err(error) => Err(format!("unexpected reply failure: {error}")),
        }
    }

    fn command(verb: &str, args: Json) -> IncomingCommand {
        IncomingCommand {
            from: "alpha".into(),
            command: verb.into(),
            id: Some("1".into()),
            args,
            body: String::new(),
            headers: BTreeMap::new(),
        }
    }

    async fn verb(
        store: &TrayStore,
        session: &Connection,
        verb_name: &str,
        args: Json,
    ) -> (u8, Json) {
        let (rc, body) = dispatch(&command(verb_name, args), store, session).await;
        (
            rc,
            serde_json::from_str(&body).expect("verb bodies are JSON"),
        )
    }

    async fn wait_until<F, Fut>(limit: Duration, mut probe: F)
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = bool>,
    {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            if probe().await {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for the probed condition"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// The tray host plus its run task, stopped on drop of the test.
    struct RunningHost {
        store: TrayStore,
        session: Connection,
        stop: watch::Sender<bool>,
        task: JoinHandle<Result<()>>,
    }

    async fn spawn_host(address: &str) -> RunningHost {
        let host = TrayHost::start(address).await.expect("host starts");
        let store = host.store();
        let session = host.connection().clone();
        let (stop, stop_rx) = watch::channel(false);
        let task = tokio::spawn(async move { host.run_until(stop_rx).await });
        RunningHost {
            store,
            session,
            stop,
            task,
        }
    }

    impl Drop for RunningHost {
        fn drop(&mut self) {
            let _ = self.stop.send(true);
        }
    }

    // ------------------------------------------------------------------
    // Integration: real dbus-daemon, real zbus, real watcher
    // ------------------------------------------------------------------

    /// Registration by bus name AND by object path both land in the item
    /// props, the watcher's protocol surface, and the event stream.
    #[tokio::test]
    async fn items_register_by_name_and_path_and_surface_as_props() {
        let Some(bus) = private_bus().await else {
            return;
        };
        let host = spawn_host(&bus.address).await;

        // Item 1: registers by bus name from its own connection.
        let app1 = TestApp::spawn(TestAppSpec::new(
            &bus.address,
            Some("org.kde.StatusNotifierItem-test-1"),
            DEFAULT_ITEM_PATH,
        ))
        .await;
        register_with_watcher(&app1.connection, "org.kde.StatusNotifierItem-test-1").await;
        // Item 2: registers by object path (a second connection).
        let app2 =
            TestApp::spawn(TestAppSpec::new(&bus.address, None, "/org/test/SecondItem")).await;
        register_with_watcher(&app2.connection, "/org/test/SecondItem").await;

        // Both items surface with their SNI properties read back.
        wait_until(Duration::from_secs(10), || async {
            let (rc, list) = verb(&host.store, &host.session, "tray.list", Json::Null).await;
            rc == 0
                && list["count"] == 2
                && list["items"][0]["title"] == "Original title"
                && list["items"][1]["title"] == "Original title"
        })
        .await;
        let (rc, props) = verb(
            &host.store,
            &host.session,
            "tray.props.get",
            json!({"path": "i0.icon_name"}),
        )
        .await;
        assert_eq!(rc, 0);
        assert_eq!(props, "test-icon");
        let (_, tip) = verb(
            &host.store,
            &host.session,
            "tray.props.get",
            json!({"path": "i0.tooltip"}),
        )
        .await;
        assert_eq!(tip, "Tip title");
        let (_, menu) = verb(
            &host.store,
            &host.session,
            "tray.props.get",
            json!({"path": "i1.has_menu"}),
        )
        .await;
        assert_eq!(menu, true);
        let (_, service) = verb(
            &host.store,
            &host.session,
            "tray.props.get",
            json!({"path": "i1.service"}),
        )
        .await;
        // The path-form item is registered under the caller's unique name.
        assert!(
            service.as_str().expect("unique name").starts_with(':'),
            "path-form items register under the caller's unique name: {service}"
        );

        // The watcher's own protocol surface, read by an independent
        // client, matches the SNI spec — with the KDE service+path
        // naming for every item (M4).
        let client = plain_connection(&bus.address).await;
        let properties = fdo::PropertiesProxy::builder(&client)
            .destination(WATCHER_NAME)
            .expect("destination")
            .path(WATCHER_PATH)
            .expect("path")
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .expect("watcher properties proxy");
        let all = properties
            .get_all(zbus::names::InterfaceName::from_static_str(WATCHER_IFACE).expect("iface"))
            .await
            .expect("watcher properties");
        let items = Vec::<String>::try_from(
            all.get("RegisteredStatusNotifierItems")
                .expect("property present")
                .clone(),
        )
        .expect("string array");
        assert!(
            items.contains(&"org.kde.StatusNotifierItem-test-1/StatusNotifierItem".to_string())
        );
        assert!(
            items
                .iter()
                .any(|item| { item.starts_with(':') && item.ends_with("/org/test/SecondItem") })
        );
        assert!(
            bool::try_from(
                all.get("IsStatusNotifierHostRegistered")
                    .expect("property present")
                    .clone()
            )
            .expect("bool")
        );
        assert_eq!(
            i32::try_from(
                all.get("ProtocolVersion")
                    .expect("property present")
                    .clone()
            )
            .expect("i32"),
            0
        );

        // The event stream saw both additions, with monotonic seq.
        let (_, info) = verb(&host.store, &host.session, "tray.info", Json::Null).await;
        let events = info["recent_events"].as_array().expect("events");
        let added: Vec<&Json> = events
            .iter()
            .filter(|event| event["event"] == "item.added")
            .collect();
        assert_eq!(added.len(), 2, "both registrations produced events");
        assert!(
            added[0]["event_seq"].as_u64() < added[1]["event_seq"].as_u64(),
            "event_seq is monotonic"
        );
    }

    /// M4 in the flesh: two path-form indicators from ONE connection are
    /// both tracked and both advertised. The old code (dedupe by
    /// service) collapsed them into one item.
    #[tokio::test]
    async fn two_path_form_items_from_one_connection_register_separately() {
        let Some(bus) = private_bus().await else {
            return;
        };
        let host = spawn_host(&bus.address).await;
        let app = TestApp::spawn(TestAppSpec::new(&bus.address, None, "/org/test/FirstItem")).await;
        app.serve_second_item("/org/test/SecondItem").await;

        register_with_watcher(&app.connection, "/org/test/FirstItem").await;
        register_with_watcher(&app.connection, "/org/test/SecondItem").await;

        wait_until(Duration::from_secs(10), || async {
            let (_, list) = verb(&host.store, &host.session, "tray.list", Json::Null).await;
            list["count"] == 2
                && list["items"][0]["path"] == "/org/test/FirstItem"
                && list["items"][1]["path"] == "/org/test/SecondItem"
        })
        .await;

        let unique = app
            .connection
            .unique_name()
            .expect("unique name")
            .to_string();
        let client = plain_connection(&bus.address).await;
        let properties = fdo::PropertiesProxy::builder(&client)
            .destination(WATCHER_NAME)
            .expect("destination")
            .path(WATCHER_PATH)
            .expect("path")
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .expect("watcher properties proxy");
        let all = properties
            .get_all(zbus::names::InterfaceName::from_static_str(WATCHER_IFACE).expect("iface"))
            .await
            .expect("watcher properties");
        let items = Vec::<String>::try_from(
            all.get("RegisteredStatusNotifierItems")
                .expect("property present")
                .clone(),
        )
        .expect("string array");
        let first = format!("{unique}/org/test/FirstItem");
        let second = format!("{unique}/org/test/SecondItem");
        assert!(
            items.contains(&first) && items.contains(&second),
            "both indicators advertised: {items:?}"
        );
    }

    /// m1/m3: registrations that can be judged synchronously get a real
    /// D-Bus error, not an OK followed by a silent drop — and a bus
    /// name may only be registered by its owner.
    #[tokio::test]
    async fn bad_registrations_get_dbus_errors_not_ok_then_drop() {
        let Some(bus) = private_bus().await else {
            return;
        };
        let host = spawn_host(&bus.address).await;
        let owner = TestApp::spawn(TestAppSpec::new(
            &bus.address,
            Some("org.kde.StatusNotifierItem-mine"),
            DEFAULT_ITEM_PATH,
        ))
        .await;
        // A third connection tries to register the name the owner holds.
        let squatter = plain_connection(&bus.address).await;
        let refused = register_reply(&squatter, "org.kde.StatusNotifierItem-mine")
            .await
            .expect_err("registering someone else's name must be refused");
        assert!(
            refused.contains("AccessDenied"),
            "an ownership refusal: {refused}"
        );
        // Invalid bus names and unknown names are refused too.
        let invalid = register_reply(&squatter, "not a bus name!!")
            .await
            .expect_err("an invalid name must be refused");
        assert!(invalid.contains("InvalidArgs"), "{invalid}");
        let unknown = register_reply(&squatter, "org.kde.StatusNotifierItem.nobody")
            .await
            .expect_err("a name with no owner must be refused");
        assert!(unknown.contains("no owner"), "{unknown}");
        // The owner's own registration still works.
        register_with_watcher(&owner.connection, "org.kde.StatusNotifierItem-mine").await;
        wait_until(Duration::from_secs(10), || async {
            verb(
                &host.store,
                &host.session,
                "tray.props.get",
                json!({"path": "count"}),
            )
            .await
            .1 == 1
        })
        .await;
    }

    /// M1: a flood of item signals during a name-form registration must
    /// not wedge the adapter — the owner resolution never sits in the
    /// serve loop blocking the signal drain. On the old code the inline
    /// get_name_owner awaited while the 128-slot signal stream filled
    /// and zbus's reader blocked: the reply never arrived and the second
    /// item never registered.
    #[tokio::test]
    async fn a_signal_flood_does_not_wedge_registration() {
        let Some(bus) = private_bus().await else {
            return;
        };
        let host = spawn_host(&bus.address).await;
        let app1 = TestApp::spawn(TestAppSpec::new(
            &bus.address,
            Some("org.kde.StatusNotifierItem-flood-1"),
            DEFAULT_ITEM_PATH,
        ))
        .await;
        register_with_watcher(&app1.connection, "org.kde.StatusNotifierItem-flood-1").await;
        wait_until(Duration::from_secs(10), || async {
            verb(
                &host.store,
                &host.session,
                "tray.props.get",
                json!({"path": "count"}),
            )
            .await
            .1 == 1
        })
        .await;

        // The flood: 4000 NewTitle signals, then a name-form
        // registration whose owner must still be resolvable.
        for _ in 0..4000 {
            TestItem::new_title(&app1.emitter)
                .await
                .expect("NewTitle emitted");
        }
        let app2 = TestApp::spawn(TestAppSpec::new(
            &bus.address,
            Some("org.kde.StatusNotifierItem-flood-2"),
            DEFAULT_ITEM_PATH,
        ))
        .await;
        register_with_watcher(&app2.connection, "org.kde.StatusNotifierItem-flood-2").await;

        wait_until(Duration::from_secs(10), || async {
            verb(
                &host.store,
                &host.session,
                "tray.props.get",
                json!({"path": "count"}),
            )
            .await
            .1 == 2
        })
        .await;
        let (rc, list) = verb(&host.store, &host.session, "tray.list", Json::Null).await;
        assert_eq!(rc, 0, "the adapter still answers verbs: {list}");
    }

    /// M2: an item whose IconPixmap reply exceeds the raw-size cap is
    /// registered, its pixmap refused without decoding, and the refusal
    /// counted. The old code had no cap (and no counter): it happily
    /// decoded multi-megabyte per-byte values before the 1 MiB pixmap
    /// cap applied.
    #[tokio::test]
    async fn an_oversized_pixmap_is_refused_and_counted() {
        let Some(bus) = private_bus().await else {
            return;
        };
        let host = spawn_host(&bus.address).await;
        let mut spec = TestAppSpec::new(
            &bus.address,
            Some("org.kde.StatusNotifierItem-huge"),
            DEFAULT_ITEM_PATH,
        );
        spec.pixmap = PixmapMode::Oversized;
        let app = TestApp::spawn(spec).await;
        register_with_watcher(&app.connection, "org.kde.StatusNotifierItem-huge").await;

        // The item registers and its string props land ...
        wait_until(Duration::from_secs(10), || async {
            verb(
                &host.store,
                &host.session,
                "tray.props.get",
                json!({"path": "i0.title"}),
            )
            .await
            .1 == "Original title"
        })
        .await;
        // ... the pixmap does not, and the cap path was taken.
        wait_until(Duration::from_secs(10), || async {
            verb(&host.store, &host.session, "tray.info", Json::Null)
                .await
                .1["oversized_reads"]
                .as_u64()
                .unwrap_or(0)
                >= 1
        })
        .await;
        let (_, list) = verb(&host.store, &host.session, "tray.list", Json::Null).await;
        assert!(
            list["items"][0]["pixmap"].is_null(),
            "the oversized pixmap never landed: {}",
            list["items"][0]
        );
        let (rc, body) = verb(&host.store, &host.session, "tray.icon", json!({"id": "i0"})).await;
        assert_eq!(rc, 10, "tray.icon refuses: {body}");
        // The adapter itself is unharmed.
        let (rc, list) = verb(&host.store, &host.session, "tray.list", Json::Null).await;
        assert_eq!(rc, 0);
        assert_eq!(list["count"], 1);
    }

    /// m6: a NewTitle flood spread over time is coalesced — the item's
    /// property set is read at most a handful of times, not once per
    /// signal. The old code read it for every single signal.
    #[tokio::test]
    async fn signal_floods_are_refresh_coalesced() {
        let Some(bus) = private_bus().await else {
            return;
        };
        let host = spawn_host(&bus.address).await;
        let app = TestApp::spawn(TestAppSpec::new(
            &bus.address,
            Some("org.kde.StatusNotifierItem-storm"),
            DEFAULT_ITEM_PATH,
        ))
        .await;
        register_with_watcher(&app.connection, "org.kde.StatusNotifierItem-storm").await;
        wait_until(Duration::from_secs(10), || async {
            verb(
                &host.store,
                &host.session,
                "tray.props.get",
                json!({"path": "i0.title"}),
            )
            .await
            .1 == "Original title"
        })
        .await;

        let reads_before = app
            .state
            .lock()
            .expect("item state")
            .title_reads
            .load(Ordering::Relaxed);
        for _ in 0..80 {
            TestItem::new_title(&app.emitter)
                .await
                .expect("NewTitle emitted");
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        // Let the trailing (gate-delayed) refresh land.
        tokio::time::sleep(Duration::from_secs(1)).await;
        let reads = app
            .state
            .lock()
            .expect("item state")
            .title_reads
            .load(Ordering::Relaxed)
            - reads_before;
        assert!(
            reads <= 25,
            "80 signals over ~2.4 s must not mean 80 refreshes (gate: \
             {MIN_REFRESH_INTERVAL:?}); title reads: {reads}"
        );
        // And the item's props still answer.
        let (_, title) = verb(
            &host.store,
            &host.session,
            "tray.props.get",
            json!({"path": "i0.title"}),
        )
        .await;
        assert_eq!(title, "Original title");
    }

    /// NewTitle on the item refreshes the props surface and produces a
    /// changed event.
    #[tokio::test]
    async fn new_title_refreshes_the_item_props() {
        let Some(bus) = private_bus().await else {
            return;
        };
        let host = spawn_host(&bus.address).await;
        let app = TestApp::spawn(TestAppSpec::new(
            &bus.address,
            Some("org.kde.StatusNotifierItem-title"),
            DEFAULT_ITEM_PATH,
        ))
        .await;
        register_with_watcher(&app.connection, "org.kde.StatusNotifierItem-title").await;
        wait_until(Duration::from_secs(10), || async {
            verb(
                &host.store,
                &host.session,
                "tray.props.get",
                json!({"path": "i0.title"}),
            )
            .await
            .1 == "Original title"
        })
        .await;

        app.state.lock().expect("item state").title = "Renamed title".into();
        TestItem::new_title(&app.emitter)
            .await
            .expect("NewTitle emitted");

        wait_until(Duration::from_secs(10), || async {
            verb(
                &host.store,
                &host.session,
                "tray.props.get",
                json!({"path": "i0.title"}),
            )
            .await
            .1 == "Renamed title"
        })
        .await;
        let (_, info) = verb(&host.store, &host.session, "tray.info", Json::Null).await;
        assert!(
            info["recent_events"]
                .as_array()
                .expect("events")
                .iter()
                .any(|event| event["event"] == "item.changed"),
            "the refresh produced a changed event"
        );
    }

    /// m7: an external StatusNotifierHost registration is recorded AND
    /// signalled. The old code only recorded it (no signal).
    #[tokio::test]
    async fn external_host_registrations_are_signalled() {
        let Some(bus) = private_bus().await else {
            return;
        };
        let host = spawn_host(&bus.address).await;
        let client = plain_connection(&bus.address).await;
        let rule = MatchRule::builder()
            .msg_type(MessageType::Signal)
            .interface(WATCHER_IFACE)
            .expect("interface")
            .member("StatusNotifierHostRegistered")
            .expect("member")
            .build();
        let mut signals = MessageStream::for_match_rule(rule, &client, Some(8))
            .await
            .expect("match rule");

        watcher_proxy(&client)
            .await
            .call_method("RegisterStatusNotifierHost", &())
            .await
            .expect("RegisterStatusNotifierHost replies");

        let signal = tokio::time::timeout(Duration::from_secs(10), signals.next())
            .await
            .expect("StatusNotifierHostRegistered within 10 s")
            .expect("stream alive")
            .expect("signal decodes");
        assert_eq!(
            signal.header().member().expect("member").as_str(),
            "StatusNotifierHostRegistered"
        );
        let host_name = client.unique_name().expect("unique name").to_string();
        wait_until(Duration::from_secs(10), || async {
            verb(&host.store, &host.session, "tray.info", Json::Null)
                .await
                .1["hosts"]
                .as_array()
                .is_some_and(|hosts| {
                    hosts
                        .iter()
                        .any(|name| name.as_str() == Some(host_name.as_str()))
                })
        })
        .await;
    }

    /// An item's connection dropping removes it: props vanish, and the
    /// watcher emits StatusNotifierItemUnregistered with the service
    /// + path surface name.
    #[tokio::test]
    async fn a_dropped_connection_removes_the_item_and_unregisters() {
        let Some(bus) = private_bus().await else {
            return;
        };
        let host = spawn_host(&bus.address).await;
        let app = TestApp::spawn(TestAppSpec::new(
            &bus.address,
            Some("org.kde.StatusNotifierItem-doomed"),
            DEFAULT_ITEM_PATH,
        ))
        .await;
        register_with_watcher(&app.connection, "org.kde.StatusNotifierItem-doomed").await;
        wait_until(Duration::from_secs(10), || async {
            verb(
                &host.store,
                &host.session,
                "tray.props.get",
                json!({"path": "count"}),
            )
            .await
            .1 == 1
        })
        .await;

        // Listen for the Unregistered signal before dropping.
        let client = plain_connection(&bus.address).await;
        let rule = MatchRule::builder()
            .msg_type(MessageType::Signal)
            .interface(WATCHER_IFACE)
            .expect("interface")
            .member("StatusNotifierItemUnregistered")
            .expect("member")
            .build();
        let mut unregistered = MessageStream::for_match_rule(rule, &client, Some(8))
            .await
            .expect("match rule");

        drop(app);
        let message = tokio::time::timeout(Duration::from_secs(10), unregistered.next())
            .await
            .expect("signal within 10 s")
            .expect("stream alive")
            .expect("signal decodes");
        let (service,): (String,) = message.body().deserialize().expect("signal body");
        assert_eq!(
            service,
            "org.kde.StatusNotifierItem-doomed/StatusNotifierItem"
        );

        wait_until(Duration::from_secs(10), || async {
            verb(
                &host.store,
                &host.session,
                "tray.props.get",
                json!({"path": "count"}),
            )
            .await
            .1 == 0
        })
        .await;
        let (_, list) = verb(&host.store, &host.session, "tray.list", Json::Null).await;
        assert_eq!(list["items"].as_array().expect("items").len(), 0);
    }

    /// tray.activate/secondary/context_menu/scroll reach the item's SNI
    /// methods; bad arguments and unknown ids are refusals.
    #[tokio::test]
    async fn activation_verbs_reach_the_item() {
        let Some(bus) = private_bus().await else {
            return;
        };
        let host = spawn_host(&bus.address).await;
        let app = TestApp::spawn(TestAppSpec::new(
            &bus.address,
            Some("org.kde.StatusNotifierItem-activate"),
            DEFAULT_ITEM_PATH,
        ))
        .await;
        register_with_watcher(&app.connection, "org.kde.StatusNotifierItem-activate").await;
        wait_until(Duration::from_secs(10), || async {
            verb(
                &host.store,
                &host.session,
                "tray.props.get",
                json!({"path": "count"}),
            )
            .await
            .1 == 1
        })
        .await;

        let (rc, body) = verb(
            &host.store,
            &host.session,
            "tray.activate",
            json!({"id": "i0", "x": 5, "y": 7}),
        )
        .await;
        assert_eq!(rc, 0, "{body}");
        assert_eq!(body["ok"], true);
        let (rc, _) = verb(
            &host.store,
            &host.session,
            "tray.secondary_activate",
            json!({"id": "i0"}),
        )
        .await;
        assert_eq!(rc, 0);
        let (rc, _) = verb(
            &host.store,
            &host.session,
            "tray.context_menu",
            json!({"id": "i0", "x": 1, "y": 2}),
        )
        .await;
        assert_eq!(rc, 0);
        let (rc, _) = verb(
            &host.store,
            &host.session,
            "tray.scroll",
            json!({"id": "i0", "delta": -3, "orientation": "vertical"}),
        )
        .await;
        assert_eq!(rc, 0);

        // Snapshot, don't bind the guard: this lint cannot see through
        // an explicit drop before the awaits below.
        let calls = app.calls.lock().expect("calls").clone();
        assert!(calls.contains(&"Activate(5,7)".to_string()), "{calls:?}");
        assert!(
            calls.contains(&"SecondaryActivate(0,0)".to_string()),
            "{calls:?}"
        );
        assert!(calls.contains(&"ContextMenu(1,2)".to_string()), "{calls:?}");
        assert!(
            calls.contains(&"Scroll(-3,vertical)".to_string()),
            "{calls:?}"
        );

        // Refusals, never panics: unknown id, missing id, bad orientation.
        let (rc, body) = verb(
            &host.store,
            &host.session,
            "tray.activate",
            json!({"id": "i77"}),
        )
        .await;
        assert_eq!(rc, 10);
        assert!(
            body["error"]
                .as_str()
                .expect("error")
                .contains("unknown tray item")
        );
        let (rc, _) = verb(&host.store, &host.session, "tray.activate", Json::Null).await;
        assert_eq!(rc, 10);
        let (rc, body) = verb(
            &host.store,
            &host.session,
            "tray.scroll",
            json!({"id": "i0", "delta": 1, "orientation": "diagonal"}),
        )
        .await;
        assert_eq!(rc, 10);
        assert!(
            body["error"]
                .as_str()
                .expect("error")
                .contains("orientation")
        );
        let (rc, body) = verb(&host.store, &host.session, "tray.absurd", Json::Null).await;
        assert_eq!(rc, 10);
        assert!(
            body["error"]
                .as_str()
                .expect("error")
                .contains("unknown tray verb")
        );
    }

    /// tray.menu returns the com.canonical.dbusmenu layout as a JSON
    /// tree — after AboutToShow(0), with icon-data excluded from the
    /// property request — and tray.menu.click delivers the Event with
    /// the (isvu) body the u32-typed fixture demands (B1).
    #[tokio::test]
    async fn menu_verbs_read_the_layout_and_deliver_clicks() {
        let Some(bus) = private_bus().await else {
            return;
        };
        let host = spawn_host(&bus.address).await;
        let app = TestApp::spawn(TestAppSpec::new(
            &bus.address,
            Some("org.kde.StatusNotifierItem-menu"),
            DEFAULT_ITEM_PATH,
        ))
        .await;
        register_with_watcher(&app.connection, "org.kde.StatusNotifierItem-menu").await;
        wait_until(Duration::from_secs(10), || async {
            verb(
                &host.store,
                &host.session,
                "tray.props.get",
                json!({"path": "i0.has_menu"}),
            )
            .await
            .1 == true
        })
        .await;

        let (rc, menu) = verb(&host.store, &host.session, "tray.menu", json!({"id": "i0"})).await;
        assert_eq!(rc, 0, "{menu}");
        assert_eq!(menu["revision"], 7);
        assert_eq!(menu["layout"]["truncated"], false);
        let children = menu["layout"]["children"].as_array().expect("children");
        assert_eq!(children.len(), 4);
        assert_eq!(children[0]["id"], 5);
        assert_eq!(children[0]["label"], "Open");
        assert_eq!(children[0]["enabled"], true);
        assert_eq!(children[1]["toggle_type"], "checkmark");
        assert_eq!(children[1]["toggle_state"], 1);
        assert_eq!(children[2]["type"], "separator");
        assert_eq!(children[3]["children"][0]["label"], "Nested");

        // The read hit the menu the dbusmenu way: AboutToShow(0) before
        // GetLayout (m9), and no icon-data in the property request (M2).
        let calls = app.calls.lock().expect("calls").clone();
        let about = calls
            .iter()
            .position(|call| call == "AboutToShow(0)")
            .expect("AboutToShow(0) ran before the layout read");
        let layout = calls
            .iter()
            .position(|call| call.starts_with("GetLayout("))
            .expect("GetLayout ran");
        assert!(about < layout, "{calls:?}");
        assert!(
            calls
                .iter()
                .any(|call| call.starts_with("GetLayout(") && !call.contains("icon-data")),
            "the property names exclude icon-data: {calls:?}"
        );

        let (rc, body) = verb(
            &host.store,
            &host.session,
            "tray.menu.click",
            json!({"id": "i0", "item": 5}),
        )
        .await;
        assert_eq!(rc, 0, "{body}");
        assert!(
            app.calls
                .lock()
                .expect("calls")
                .contains(&"Event(5,clicked)".to_string()),
            "the click reached the item's dbusmenu"
        );
        let (rc, body) = verb(
            &host.store,
            &host.session,
            "tray.menu.click",
            json!({"id": "i0"}),
        )
        .await;
        assert_eq!(rc, 10);
        assert!(body["error"].as_str().expect("error").contains("args.item"));
    }

    /// m10: the "/NO_DBUSMENU" sentinel Menu path means no menu — the
    /// old code treated it as a real path.
    #[tokio::test]
    async fn the_no_dbusmenu_sentinel_is_menuless() {
        let Some(bus) = private_bus().await else {
            return;
        };
        let host = spawn_host(&bus.address).await;
        let mut spec = TestAppSpec::new(
            &bus.address,
            Some("org.kde.StatusNotifierItem-sentinel"),
            DEFAULT_ITEM_PATH,
        );
        spec.menu_path = NO_DBUSMENU_PATH;
        let app = TestApp::spawn(spec).await;
        register_with_watcher(&app.connection, "org.kde.StatusNotifierItem-sentinel").await;
        wait_until(Duration::from_secs(10), || async {
            verb(
                &host.store,
                &host.session,
                "tray.props.get",
                json!({"path": "i0.title"}),
            )
            .await
            .1 == "Original title"
        })
        .await;
        let (_, has_menu) = verb(
            &host.store,
            &host.session,
            "tray.props.get",
            json!({"path": "i0.has_menu"}),
        )
        .await;
        assert_eq!(has_menu, false);
        let (rc, body) = verb(&host.store, &host.session, "tray.menu", json!({"id": "i0"})).await;
        assert_eq!(rc, 10, "{body}");
        assert!(
            body["error"]
                .as_str()
                .expect("error")
                .contains("has no menu"),
            "{body}"
        );
    }

    /// tray.icon serves the largest pixmap's pixels; the props never
    /// carry them.
    #[tokio::test]
    async fn the_icon_verb_serves_the_largest_pixmap() {
        let Some(bus) = private_bus().await else {
            return;
        };
        let host = spawn_host(&bus.address).await;
        let app = TestApp::spawn(TestAppSpec::new(
            &bus.address,
            Some("org.kde.StatusNotifierItem-icon"),
            DEFAULT_ITEM_PATH,
        ))
        .await;
        register_with_watcher(&app.connection, "org.kde.StatusNotifierItem-icon").await;
        wait_until(Duration::from_secs(10), || async {
            verb(
                &host.store,
                &host.session,
                "tray.props.get",
                json!({"path": "i0.pixmap_width"}),
            )
            .await
            .1 == 4
        })
        .await;

        let (rc, icon) = verb(&host.store, &host.session, "tray.icon", json!({"id": "i0"})).await;
        assert_eq!(rc, 0, "{icon}");
        assert_eq!(icon["width"], 4);
        assert_eq!(icon["height"], 4);
        assert_eq!(icon["encoding"], "argb32-network-order");
        let pixels = base64_decode(icon["argb_b64"].as_str().expect("base64"));
        assert_eq!(pixels.len(), 4 * 4 * 4);
        assert!(pixels.iter().all(|byte| *byte == 0x5a));
    }

    /// An item whose Activate never replies: the verb times out with a
    /// refusal, and the adapter keeps running and answering.
    #[tokio::test]
    async fn a_hung_item_times_out_and_the_adapter_survives() {
        let Some(bus) = private_bus().await else {
            return;
        };
        let host = spawn_host(&bus.address).await;
        let app = TestApp::spawn(TestAppSpec::new(
            &bus.address,
            Some("org.kde.StatusNotifierItem-hung"),
            DEFAULT_ITEM_PATH,
        ))
        .await;
        register_with_watcher(&app.connection, "org.kde.StatusNotifierItem-hung").await;
        wait_until(Duration::from_secs(10), || async {
            verb(
                &host.store,
                &host.session,
                "tray.props.get",
                json!({"path": "count"}),
            )
            .await
            .1 == 1
        })
        .await;

        app.state.lock().expect("item state").hang_activate = true;
        let started = tokio::time::Instant::now();
        let (rc, body) = verb(
            &host.store,
            &host.session,
            "tray.activate",
            json!({"id": "i0"}),
        )
        .await;
        assert_eq!(rc, 10, "{body}");
        assert!(
            body["error"]
                .as_str()
                .expect("error")
                .contains("did not answer"),
            "{body}"
        );
        let elapsed = started.elapsed();
        assert!(
            elapsed >= ITEM_CALL_TIMEOUT && elapsed < ITEM_CALL_TIMEOUT * 3,
            "the refusal came from the timeout, not instantly or after minutes: {elapsed:?}"
        );

        // The adapter is still up and serving.
        assert!(!host.task.is_finished(), "the adapter run must survive");
        let (rc, list) = verb(&host.store, &host.session, "tray.list", Json::Null).await;
        assert_eq!(rc, 0);
        assert_eq!(list["count"], 1);
    }

    /// m5: a verb stuck on a hung item does not delay the verbs behind
    /// it — dispatch is concurrent (bounded). Against the old
    /// serialized serve loop, tray.list behind a hung activate waited
    /// out the whole 2 s item timeout.
    #[tokio::test]
    async fn a_hung_item_does_not_delay_other_verbs() {
        let Some(bus) = private_bus().await else {
            return;
        };
        let host = spawn_host(&bus.address).await;
        let app = TestApp::spawn(TestAppSpec::new(
            &bus.address,
            Some("org.kde.StatusNotifierItem-slow"),
            DEFAULT_ITEM_PATH,
        ))
        .await;
        register_with_watcher(&app.connection, "org.kde.StatusNotifierItem-slow").await;
        wait_until(Duration::from_secs(10), || async {
            verb(
                &host.store,
                &host.session,
                "tray.props.get",
                json!({"path": "count"}),
            )
            .await
            .1 == 1
        })
        .await;
        app.state.lock().expect("item state").hang_activate = true;

        struct RecordingResponder {
            answered: Mutex<Vec<(String, tokio::time::Instant)>>,
        }
        impl CommandResponder for RecordingResponder {
            fn respond(
                &self,
                command: &IncomingCommand,
                _rc: u8,
                _body: &str,
            ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
                self.answered
                    .lock()
                    .expect("answered")
                    .push((command.command.clone(), tokio::time::Instant::now()));
                Box::pin(std::future::ready(Ok(())))
            }
        }

        let (commands_tx, commands_rx) = mpsc::unbounded_channel();
        let responder = Arc::new(RecordingResponder {
            answered: Mutex::new(Vec::new()),
        });
        let (_stop, stop_rx) = watch::channel(false);
        let serving = tokio::spawn(serve_commands(
            commands_rx,
            Arc::clone(&host.store),
            host.session.clone(),
            responder.clone() as Arc<dyn CommandResponder>,
            stop_rx,
        ));

        // The hung activate first, then a cheap list behind it.
        commands_tx
            .send(command("tray.activate", json!({"id": "i0"})))
            .expect("send activate");
        tokio::time::sleep(Duration::from_millis(50)).await;
        commands_tx
            .send(command("tray.list", Json::Null))
            .expect("send list");

        wait_until(Duration::from_secs(2), || async {
            let answered = responder.answered.lock().expect("answered");
            answered.iter().any(|(verb, _)| verb == "tray.list")
        })
        .await;
        {
            let answered = responder.answered.lock().expect("answered");
            assert!(
                !answered.iter().any(|(verb, _)| verb == "tray.activate"),
                "the hung item is still hanging: {answered:?}"
            );
        }
        // The hung verb still completes within its own timeout.
        wait_until(
            Duration::from_secs(ITEM_CALL_TIMEOUT.as_secs() + 5),
            || async {
                responder
                    .answered
                    .lock()
                    .expect("answered")
                    .iter()
                    .any(|(verb, _)| verb == "tray.activate")
            },
        )
        .await;
        drop(commands_tx);
        let _ = serving.await;
    }

    /// An already-owned watcher name fails the run with a clear error,
    /// and the existing owner is not replaced.
    #[tokio::test]
    async fn an_owned_watcher_name_fails_the_run_without_replacing() {
        let Some(bus) = private_bus().await else {
            return;
        };
        let squatter = plain_connection(&bus.address).await;
        let dbus = DBusProxy::new(&squatter).await.expect("dbus proxy");
        dbus.request_name(
            WellKnownName::from_static_str(WATCHER_NAME).expect("name"),
            fdo::RequestNameFlags::DoNotQueue.into(),
        )
        .await
        .expect("squatter takes the name");

        let error = match TrayHost::start(&bus.address).await {
            Ok(_) => panic!("the adapter must refuse to start"),
            Err(error) => error,
        };
        let message = format!("{error:#}");
        assert!(
            message.contains(WATCHER_NAME) && message.contains("already owned"),
            "the error names the conflict: {message}"
        );

        // No replacement: the squatter still owns the name.
        assert!(
            dbus.name_has_owner(
                WellKnownName::from_static_str(WATCHER_NAME)
                    .expect("name")
                    .into()
            )
            .await
            .expect("name_has_owner")
        );
        let owner = dbus
            .get_name_owner(
                WellKnownName::from_static_str(WATCHER_NAME)
                    .expect("name")
                    .into(),
            )
            .await
            .expect("owner survives");
        assert_eq!(
            owner.to_string(),
            squatter.unique_name().expect("unique name").to_string()
        );
    }

    /// Session-bus death ends the run — observed on the zbus
    /// connection's own closed signal, with that exact reason. A Bus
    /// (mesh) outage would NOT end it (the broker reconnects); this is
    /// the session-side half of that contract.
    #[tokio::test]
    async fn session_bus_death_ends_the_run() {
        let Some(mut bus) = private_bus().await else {
            return;
        };
        let host = TrayHost::start(&bus.address).await.expect("host starts");
        let store = host.store();
        let session = host.connection().clone();
        let (_stop, stop_rx) = watch::channel(false);
        let task = tokio::spawn(async move { host.run_until(stop_rx).await });
        let app = TestApp::spawn(TestAppSpec::new(
            &bus.address,
            Some("org.kde.StatusNotifierItem-doomed-bus"),
            DEFAULT_ITEM_PATH,
        ))
        .await;
        register_with_watcher(&app.connection, "org.kde.StatusNotifierItem-doomed-bus").await;
        wait_until(Duration::from_secs(10), || async {
            verb(&store, &session, "tray.props.get", json!({"path": "count"}))
                .await
                .1
                == 1
        })
        .await;

        drop(app);
        bus.child.start_kill().expect("kill dbus-daemon");
        let outcome = tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("the run ends within 10 s of session-bus death")
            .expect("run task joinable");
        let message = format!("{:#}", outcome.expect_err("session death fails the run"));
        assert!(
            message.contains("session bus connection closed"),
            "the authoritative closed-signal reason: {message}"
        );
    }

    /// A decode helper for the icon test only (round-trips our encoder).
    fn base64_decode(text: &str) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buffer = 0u32;
        let mut bits = 0u32;
        for character in text.chars() {
            if character == '=' {
                break;
            }
            let value = BASE64
                .iter()
                .position(|candidate| *candidate as char == character)
                .expect("valid base64") as u32;
            buffer = (buffer << 6) | value;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push((buffer >> bits) as u8);
            }
        }
        out
    }
}
