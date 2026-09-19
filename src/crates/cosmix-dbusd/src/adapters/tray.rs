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

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
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
use tokio::task::JoinHandle;
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

pub const BUS_SERVICE: &str = "tray";
pub const TOPIC_ITEM_ADDED: &str = "tray.item.added";
pub const TOPIC_ITEM_CHANGED: &str = "tray.item.changed";
pub const TOPIC_ITEM_REMOVED: &str = "tray.item.removed";

// Resource bounds. An SNI item is a small metadata record; these caps keep
// a hostile or broken app from ballooning the adapter's memory.
const MAX_ITEMS: usize = 64;
const MAX_STRING_CHARS: usize = 4096;
const MAX_PIXMAP_BYTES: usize = 1024 * 1024;
const MAX_MENU_NODES: usize = 512;
/// How many recent events the ring keeps for `tray.info` / tests.
const RECENT_EVENTS: usize = 128;

// Every D-Bus call to an item is bounded: a hung app surfaces as a
// refusal, never as a wedged adapter.
const ITEM_CALL_TIMEOUT: Duration = Duration::from_secs(2);
const MENU_CALL_TIMEOUT: Duration = Duration::from_secs(3);
const REFRESH_BUDGET: Duration = Duration::from_secs(3);

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
/// serving it — the key for NameOwnerChanged reaping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayItem {
    pub key: String,
    pub service: String,
    pub path: String,
    pub owner: String,
    pub props: ItemProps,
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
    pub service: String,
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
}

impl TrayState {
    /// Register an item, replacing any existing item with the same
    /// service (re-registration is KDE's update idiom). Refuses past
    /// [`MAX_ITEMS`]. Returns the fresh key plus the events (removed
    /// first, then added). Keys are `i<n>` with a never-reused counter:
    /// stable for the item's lifetime, gone after removal.
    pub fn register(
        &mut self,
        service: String,
        path: String,
        owner: String,
        props: ItemProps,
    ) -> std::result::Result<(String, Vec<ItemEvent>), String> {
        if self.items.len() >= MAX_ITEMS {
            return Err(format!(
                "tray item limit reached ({MAX_ITEMS}); refusing to track {service}"
            ));
        }
        let mut events = Vec::new();
        let doomed: Vec<String> = self
            .items
            .values()
            .filter(|item| item.service == service)
            .map(|item| item.key.clone())
            .collect();
        for key in doomed {
            if let Some(item) = self.items.remove(&key) {
                events.push(self.stamp(ItemEventKind::Removed, item.key, item.service));
            }
        }
        let key = format!("i{}", self.next_key);
        self.next_key += 1;
        self.items.insert(
            key.clone(),
            TrayItem {
                key: key.clone(),
                service: service.clone(),
                path,
                owner,
                props,
            },
        );
        events.push(self.stamp(ItemEventKind::Added, key.clone(), service));
        Ok((key, events))
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
        let (key, service) = (item.key.clone(), item.service.clone());
        item.props = props;
        vec![self.stamp(ItemEventKind::Changed, key, service)]
    }

    /// Reap items after a `NameOwnerChanged`: when `new_owner` is empty,
    /// every item owned by the vanished connection goes, plus items whose
    /// registered service lost its name; when the name merely moved
    /// owners, items registered under that service go (the instance they
    /// were registered from is gone).
    pub fn remove_by_bus_change(
        &mut self,
        name: &str,
        old_owner: &str,
        new_owner: &str,
    ) -> Vec<ItemEvent> {
        let mut events = Vec::new();
        let doomed: Vec<String> = self
            .items
            .values()
            .filter(|item| {
                item.owner == old_owner
                    || (item.service == name && new_owner.is_empty())
                    || (item.service == name && !old_owner.is_empty() && !new_owner.is_empty())
            })
            .map(|item| item.key.clone())
            .collect();
        for key in doomed {
            if let Some(item) = self.items.remove(&key) {
                events.push(self.stamp(ItemEventKind::Removed, item.key, item.service));
            }
        }
        events
    }

    fn stamp(&mut self, kind: ItemEventKind, key: String, service: String) -> ItemEvent {
        self.next_event_seq += 1;
        let event = ItemEvent {
            kind,
            key,
            service,
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

    /// The `RegisteredStatusNotifierItems` surface: the bus name each
    /// item registered under (the caller's unique name for the path
    /// form, matching KDE's watcher).
    pub fn registered_services(&self) -> Vec<String> {
        self.items_in_order()
            .into_iter()
            .map(|item| item.service.clone())
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

    /// Cosmix is the host: set once the watcher name is acquired.
    pub fn set_host_registered(&mut self, registered: bool) {
        self.host_registered = registered;
    }

    pub fn host_registered(&self) -> bool {
        self.host_registered
    }

    /// Record an external StatusNotifierHost registration. Returns true
    /// when this is the first host — normally a moot transition, because
    /// cosmix self-registered as the host at startup.
    pub fn add_host(&mut self, caller: String) -> bool {
        let first = self.hosts.is_empty();
        self.hosts.insert(caller);
        first
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
    let node_type = dict_str(properties, "type").unwrap_or_else(|| "standard".into());
    let mut children_json = Vec::new();
    for child in children.iter() {
        match menu_node_json(child, budget) {
            // Budget spent or a malformed subtree: stop instead of
            // presenting a silently partial tree as complete.
            Some(node) => children_json.push(node),
            None => break,
        }
    }
    Some(json!({
        "id": id,
        "label": label,
        // dbusmenu defaults: absent means enabled/visible.
        "enabled": dict_bool(properties, "enabled").unwrap_or(true),
        "visible": dict_bool(properties, "visible").unwrap_or(true),
        "type": node_type,
        "toggle_type": dict_str(properties, "toggle-type"),
        "toggle_state": dict_i32(properties, "toggle-state"),
        "children": children_json,
    }))
}

fn dict_str(dict: &Dict<'_, '_>, key: &str) -> Option<String> {
    let mut value = None;
    for (candidate, entry) in dict.iter() {
        if let (Value::Str(name), Value::Str(text)) = (candidate, entry)
            && name.as_str() == key
        {
            value = Some(text.to_string());
            break;
        }
    }
    value
}

fn dict_bool(dict: &Dict<'_, '_>, key: &str) -> Option<bool> {
    for (candidate, entry) in dict.iter() {
        if let (Value::Str(name), Value::Bool(flag)) = (candidate, entry)
            && name.as_str() == key
        {
            return Some(*flag);
        }
    }
    None
}

fn dict_i32(dict: &Dict<'_, '_>, key: &str) -> Option<i32> {
    for (candidate, entry) in dict.iter() {
        if let (Value::Str(name), Value::I32(number)) = (candidate, entry)
            && name.as_str() == key
        {
            return Some(*number);
        }
    }
    None
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
/// to the run loop, which resolves owners, fetches props and publishes.
enum WatcherMsg {
    RegisterItem { caller: String, arg: String },
}

struct WatcherIface {
    store: TrayStore,
    registrations: mpsc::Sender<WatcherMsg>,
}

#[interface(name = "org.kde.StatusNotifierWatcher")]
impl WatcherIface {
    /// `RegisterStatusNotifierItem(s)` — the argument is the item's bus
    /// name (object at /StatusNotifierItem) or its object path (item on
    /// the caller's own connection), as KDE/libappindicator accept both.
    async fn register_status_notifier_item(
        &self,
        service_or_path: &str,
        #[zbus(header)] header: Header<'_>,
    ) -> fdo::Result<()> {
        let caller = header
            .sender()
            .map(|sender| sender.to_string())
            .ok_or_else(|| {
                fdo::Error::Failed("RegisterStatusNotifierItem needs a sender".into())
            })?;
        if service_or_path.is_empty() {
            return Err(fdo::Error::InvalidArgs(
                "RegisterStatusNotifierItem requires a bus name or object path".into(),
            ));
        }
        if lock_state(&self.store).count() >= MAX_ITEMS {
            return Err(fdo::Error::LimitsExceeded(format!(
                "tray item limit reached ({MAX_ITEMS})"
            )));
        }
        self.registrations
            .try_send(WatcherMsg::RegisterItem {
                caller,
                arg: service_or_path.to_string(),
            })
            .map_err(|_| fdo::Error::Failed("tray adapter is busy; retry".into()))
    }

    /// `RegisterStatusNotifierHost(s)` — recorded (cosmix is itself the
    /// host, so the host-registered signal fired at startup already).
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
        lock_state(&self.store).add_host(caller);
        Ok(())
    }

    #[zbus(property)]
    async fn registered_status_notifier_items(&self) -> fdo::Result<Vec<String>> {
        Ok(lock_state(&self.store).registered_services())
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

/// Read one item's property set as [`ItemProps`] within
/// [`REFRESH_BUDGET`]. Missing properties take their SNI defaults; a
/// total failure returns `None` (a refresh error must not wipe known
/// good props with blanks).
async fn fetch_item_props(connection: &Connection, service: &str, path: &str) -> Option<ItemProps> {
    let read = async {
        let proxy = fdo::PropertiesProxy::builder(connection)
            .destination(service.to_string())?
            .path(path.to_string())?
            .cache_properties(CacheProperties::No)
            .build()
            .await?;
        let interface = zbus::names::InterfaceName::from_static_str(ITEM_IFACE)?;
        anyhow::Ok(proxy.get_all(interface).await?)
    };
    let all = match tokio::time::timeout(REFRESH_BUDGET, read).await {
        Ok(Ok(all)) => all,
        Ok(Err(error)) => {
            eprintln!("cosmix-dbusd: tray: reading {service}{path} failed: {error}");
            return None;
        }
        Err(_) => {
            eprintln!(
                "cosmix-dbusd: tray: reading {service}{path} exceeded the {REFRESH_BUDGET:?} budget"
            );
            return None;
        }
    };
    Some(parse_item_props(&all))
}

fn parse_item_props(all: &HashMap<String, OwnedValue>) -> ItemProps {
    let string = |key: &str| {
        all.get(key)
            .and_then(|value| String::try_from(value.clone()).ok())
            .unwrap_or_default()
    };
    let mut props = ItemProps {
        id: string("Id"),
        title: string("Title"),
        category: string("Category"),
        status: string("Status"),
        icon_name: string("IconName"),
        icon_theme_path: string("IconThemePath"),
        attention_icon_name: string("AttentionIconName"),
        tooltip_title: String::new(),
        tooltip_description: String::new(),
        tooltip_icon: String::new(),
        menu: all
            .get("Menu")
            .and_then(|value| zvariant::OwnedObjectPath::try_from(value.clone()).ok())
            .map(|path| path.to_string()),
        item_is_menu: all
            .get("ItemIsMenu")
            .and_then(|value| bool::try_from(value.clone()).ok())
            .unwrap_or(false),
        pixmap: all
            .get("IconPixmap")
            .map(|value| Value::from(value.clone()))
            .and_then(|value| largest_pixmap(&value)),
    };
    if props.status.is_empty() {
        // SNI's documented default when an item does not implement Status.
        props.status = "Active".into();
    }
    if let Some(tip) = all
        .get("ToolTip")
        .map(|value| Value::from(value.clone()))
        .and_then(|value| parse_tooltip(&value))
    {
        (
            props.tooltip_icon,
            props.tooltip_title,
            props.tooltip_description,
        ) = tip;
    }
    props.clamp_strings();
    props
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

/// Keep the largest `a(iiay)` entry within [`MAX_PIXMAP_BYTES`].
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
        let data: Vec<u8> = bytes
            .iter()
            .filter_map(|byte| match byte {
                Value::U8(byte) => Some(*byte),
                _ => None,
            })
            .collect();
        if data.len() > MAX_PIXMAP_BYTES {
            continue;
        }
        let area = u64::from(*width as u32) * u64::from(*height as u32);
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
async fn call_item_xy(
    connection: &Connection,
    item: &TrayItem,
    member: &str,
    x: i32,
    y: i32,
) -> std::result::Result<(), String> {
    let proxy = item_proxy(connection, item)
        .await
        .map_err(|error| format!("item {member} failed: {error}"))?;
    match tokio::time::timeout(ITEM_CALL_TIMEOUT, proxy.call_method(member, &(x, y))).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => Err(format!("item {member} failed: {error}")),
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
    let proxy = item_proxy(connection, item)
        .await
        .map_err(|error| format!("item Scroll failed: {error}"))?;
    match tokio::time::timeout(
        ITEM_CALL_TIMEOUT,
        proxy.call_method("Scroll", &(delta, orientation)),
    )
    .await
    {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => Err(format!("item Scroll failed: {error}")),
        Err(_) => Err(format!(
            "item did not answer Scroll within {:?}; it may be hung",
            ITEM_CALL_TIMEOUT
        )),
    }
}

/// GetLayout(0, -1, []) → (revision, layout tree), parsed to JSON.
async fn call_menu_layout(
    connection: &Connection,
    item: &TrayItem,
) -> std::result::Result<(u32, Json), String> {
    let proxy = menu_proxy(connection, item)
        .await
        .map_err(|error| format!("menu read failed: {error}"))?;
    let body = (0_i32, -1_i32, Vec::<String>::new());
    let reply = match tokio::time::timeout(MENU_CALL_TIMEOUT, proxy.call_method("GetLayout", &body))
        .await
    {
        Ok(Ok(reply)) => reply,
        Ok(Err(error)) => return Err(format!("menu read failed: {error}")),
        Err(_) => {
            return Err(format!(
                "item did not answer GetLayout within {:?}; it may be hung",
                MENU_CALL_TIMEOUT
            ));
        }
    };
    let body = reply.body();
    let (revision, layout) = body
        .deserialize::<(u32, Value)>()
        .map_err(|error| format!("menu layout had an unexpected shape: {error}"))?;
    let mut budget = MAX_MENU_NODES;
    let layout = menu_node_json(&layout, &mut budget)
        .ok_or_else(|| "menu layout had an unexpected shape".to_string())?;
    Ok((revision, layout))
}

/// dbusmenu `Event(id, "clicked", v, timestamp)`.
async fn call_menu_click(
    connection: &Connection,
    item: &TrayItem,
    node_id: i32,
) -> std::result::Result<(), String> {
    let proxy = menu_proxy(connection, item)
        .await
        .map_err(|error| format!("menu click failed: {error}"))?;
    let timestamp = unix_millis();
    let data = Value::Value(Box::new(Value::I32(0)));
    let body = (node_id, "clicked", data, timestamp);
    match tokio::time::timeout(ITEM_CALL_TIMEOUT, proxy.call_method("Event", &body)).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => Err(format!("menu click failed: {error}")),
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
                "event_sequence": "per-adapter-session monotonic event_seq on every event; \
                                   a gap means events were dropped — re-read tray.props.get",
                "event_seq": state.event_seq(),
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

async fn publish_bus_message(
    client: &NodedClient,
    topic: &str,
    message: &cosmix_bus::bus::BusMessage,
) -> Result<()> {
    let headers = BTreeMap::from([
        ("name".to_string(), topic.to_string()),
        ("retain".to_string(), "false".to_string()),
    ]);
    client
        .send_with_headers("noded", "topic.publish", &headers, &message.to_wire())
        .await
}

async fn wait_for_bus_client(
    clients: &mut watch::Receiver<Option<Arc<NodedClient>>>,
) -> Result<Arc<NodedClient>> {
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
    mut clients: watch::Receiver<Option<Arc<NodedClient>>>,
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
                sent =
                    publish_bus_message(&client, &props_changed_topic(BUS_SERVICE), &message).await;
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
                    "data": {"key": event.key, "service": event.service},
                })
                .to_string();
                sent = publish_bus_message(&client, event.kind.topic(), &message).await;
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
    clients: watch::Sender<Option<Arc<NodedClient>>>,
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
                let client = Arc::new(client);
                let _ = clients.send(Some(Arc::clone(&client)));
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

async fn serve_bus_client(
    client: Arc<NodedClient>,
    store: TrayStore,
    session: Connection,
    mut shutdown: watch::Receiver<bool>,
) {
    let Some(mut incoming) = client.incoming_async().await else {
        return;
    };
    loop {
        // Check the current value too: a watch cloned after the flip
        // would otherwise never see `changed()` fire.
        if *shutdown.borrow_and_update() {
            return;
        }
        tokio::select! {
            biased;
            _ = shutdown.changed() => return,
            command = incoming.recv() => {
                let Some(command) = command else { return };
                let (rc, body) = dispatch(&command, &store, &session).await;
                match tokio::time::timeout(
                    BUS_PUBLISH_TIMEOUT,
                    client.respond_parts(
                        &command.from,
                        &command.command,
                        command.id.as_deref(),
                        rc,
                        &body,
                    ),
                )
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        eprintln!("cosmix-dbusd: tray: bus response failed; reconnecting: {error}");
                        return;
                    }
                    Err(_) => {
                        eprintln!("cosmix-dbusd: tray: bus response timed out; reconnecting");
                        return;
                    }
                }
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
    dbus: DBusProxy<'static>,
    owner_changes: MessageStream,
    item_signals: MessageStream,
    registrations: mpsc::Receiver<WatcherMsg>,
    refresh_tx: mpsc::Sender<(String, Option<ItemProps>)>,
    refresh_rx: mpsc::Receiver<(String, Option<ItemProps>)>,
    /// Keys with a fetch in flight / signalled again while in flight.
    refresh_active: BTreeSet<String>,
    refresh_queued: BTreeSet<String>,
    batches: mpsc::Sender<BusBatch>,
    broker: JoinHandle<Result<()>>,
    publisher: JoinHandle<Result<()>>,
    internal_shutdown: watch::Sender<bool>,
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
        let item_signals = MessageStream::for_match_rule(item_rule, &connection, Some(128))
            .await
            .map_err(|error| anyhow!("tray adapter item-watch failed: {error}"))?;

        let (client_tx, client_rx) = watch::channel::<Option<Arc<NodedClient>>>(None);
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
            dbus,
            owner_changes,
            item_signals,
            registrations,
            refresh_tx,
            refresh_rx,
            refresh_active: BTreeSet::new(),
            refresh_queued: BTreeSet::new(),
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
        outcome
    }

    async fn serve(&mut self, stop: &mut watch::Receiver<bool>) -> Result<()> {
        loop {
            tokio::select! {
                biased;
                changed = stop.changed() => {
                    changed.map_err(|_| anyhow!("stop channel ended"))?;
                    if *stop.borrow_and_update() {
                        return Ok(());
                    }
                }
                message = self.registrations.recv() => match message {
                    None => return Err(anyhow!("watcher interface channel ended")),
                    Some(WatcherMsg::RegisterItem { caller, arg }) => {
                        self.handle_register(caller, arg).await;
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
                    Some((key, props)) => self.handle_refresh_done(key, props),
                },
            }
        }
    }

    /// Resolve a registration the way KDE/libappindicator expect: an
    /// argument starting with `/` is an object path on the caller's own
    /// connection; anything else is the item's bus name (object at
    /// /StatusNotifierItem). The item is registered immediately (empty
    /// props) and a property fetch fills it in — a changed event lands
    /// when the read completes.
    async fn handle_register(&mut self, caller: String, arg: String) {
        let (service, path) = if arg.starts_with('/') {
            (caller.clone(), arg)
        } else {
            (arg, DEFAULT_ITEM_PATH.to_string())
        };
        let owner = if service == caller {
            caller
        } else {
            let name = match zbus::names::BusName::try_from(service.as_str()) {
                Ok(name) => name,
                Err(error) => {
                    eprintln!(
                        "cosmix-dbusd: tray: registration of invalid bus name {service}: {error}"
                    );
                    return;
                }
            };
            match self.dbus.get_name_owner(name).await {
                Ok(owner) => owner.to_string(),
                Err(error) => {
                    eprintln!(
                        "cosmix-dbusd: tray: registration of {service} has no owner: {error}"
                    );
                    return;
                }
            }
        };
        let registered = lock_state(&self.store).register(
            service.clone(),
            path.clone(),
            owner,
            ItemProps::default(),
        );
        match registered {
            Ok((key, events)) => {
                self.emit_item_events(&events).await;
                self.publish(events, "sni.registration");
                self.refresh(&key);
            }
            Err(error) => eprintln!("cosmix-dbusd: tray: {error}"),
        }
    }

    async fn handle_owner_change(&mut self, message: &Message) {
        let Ok((name, old_owner, new_owner)) =
            message.body().deserialize::<(String, String, String)>()
        else {
            return;
        };
        let events = lock_state(&self.store).remove_by_bus_change(&name, &old_owner, &new_owner);
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
        self.refresh(&key);
    }

    fn handle_refresh_done(&mut self, key: String, props: Option<ItemProps>) {
        self.refresh_active.remove(&key);
        if let Some(props) = props {
            let events = lock_state(&self.store).apply_props(&key, props);
            if !events.is_empty() {
                self.publish(events, "sni.refresh");
            }
        }
        if self.refresh_queued.remove(&key) && lock_state(&self.store).item(&key).is_some() {
            self.refresh(&key);
        }
    }

    /// Fetch the item's props out-of-band (a hung item must not stall
    /// the run loop); coalesce per item while a fetch is in flight.
    fn refresh(&mut self, key: &str) {
        if self.refresh_active.contains(key) {
            self.refresh_queued.insert(key.to_string());
            return;
        }
        let (service, path) = {
            let state = lock_state(&self.store);
            let Some(item) = state.item(key) else {
                return;
            };
            (item.service.clone(), item.path.clone())
        };
        let (connection, key) = (self.connection.clone(), key.to_string());
        let done = self.refresh_tx.clone();
        self.refresh_active.insert(key.clone());
        tokio::spawn(async move {
            let props = fetch_item_props(&connection, &service, &path).await;
            let _ = done.send((key, props)).await;
        });
    }

    /// Emit the watcher signals for a batch (Registered for Added,
    /// Unregistered for Removed) on the session bus.
    async fn emit_item_events(&mut self, events: &[ItemEvent]) {
        for event in events {
            let result = match event.kind {
                ItemEventKind::Added => {
                    WatcherIface::status_notifier_item_registered(&self.emitter, &event.service)
                        .await
                }
                ItemEventKind::Removed => {
                    WatcherIface::status_notifier_item_unregistered(&self.emitter, &event.service)
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
        state
            .register(
                service.into(),
                DEFAULT_ITEM_PATH.into(),
                ":1.42".into(),
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
        assert_eq!(
            state.registered_services(),
            vec![
                "org.kde.StatusNotifierItem-1".to_string(),
                "org.kde.StatusNotifierItem-2".to_string()
            ]
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

    #[test]
    fn registration_is_refused_past_the_item_cap() {
        let mut state = TrayState::default();
        for number in 0..MAX_ITEMS {
            registered(&mut state, &format!("org.kde.StatusNotifierItem-{number}"));
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

    #[test]
    fn owner_vanish_and_name_loss_remove_their_items() {
        let mut state = TrayState::default();
        let gone = registered(&mut state, "org.kde.StatusNotifierItem-gone");
        let keeper = registered(&mut state, "org.kde.StatusNotifierItem-keeper");
        // All registered under owner :1.42 by `registered`.
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
        // An unrelated name change touches nothing.
        let mut state = TrayState::default();
        registered(&mut state, "org.kde.StatusNotifierItem-stay");
        assert!(
            state
                .remove_by_bus_change("org.other", ":1.7", "")
                .is_empty()
        );
        assert_eq!(state.count(), 1);
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

    // ------------------------------------------------------------------
    // Pure helpers: menu tree + base64
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

    #[test]
    fn menu_tree_is_capped_at_the_node_budget() {
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
    // D-Bus-side test doubles
    // ------------------------------------------------------------------

    type CallLog = Arc<Mutex<Vec<String>>>;

    #[derive(Debug)]
    struct TestItemState {
        title: String,
        hang_activate: bool,
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
            Ok(self.state.lock().expect("item state").title.clone())
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
            Ok("/MenuBarItem".try_into().expect("menu path"))
        }

        #[zbus(property)]
        fn item_is_menu(&self) -> fdo::Result<bool> {
            Ok(false)
        }

        #[zbus(property)]
        fn icon_pixmap(&self) -> fdo::Result<Vec<WirePixmap>> {
            Ok(vec![pixmap(2), pixmap(4)])
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
            _properties: Vec<String>,
        ) -> fdo::Result<(u32, Value<'static>)> {
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

        async fn event(&self, id: i32, event_id: &str, _data: Value<'_>, _timestamp: i64) {
            self.events
                .lock()
                .expect("menu events")
                .push(format!("Event({id},{event_id})"));
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
        async fn spawn(
            address: &str,
            well_known: Option<&'static str>,
            item_path: &'static str,
        ) -> Self {
            let state = Arc::new(Mutex::new(TestItemState {
                title: "Original title".into(),
                hang_activate: false,
            }));
            let calls: CallLog = Arc::new(Mutex::new(Vec::new()));
            let address: zbus::address::Address = address.parse().expect("test bus address");
            let mut builder = zbus::connection::Builder::address(address)
                .expect("builder")
                .serve_at(
                    item_path,
                    TestItem {
                        state: Arc::clone(&state),
                        calls: Arc::clone(&calls),
                    },
                )
                .expect("serve item")
                .serve_at(
                    "/MenuBarItem",
                    TestMenu {
                        events: Arc::clone(&calls),
                    },
                )
                .expect("serve menu");
            if let Some(well_known) = well_known {
                builder = builder
                    .name(WellKnownName::from_static_str(well_known).expect("well-known name"))
                    .expect("own name");
            }
            let connection = builder.build().await.expect("test app connects");
            let emitter = SignalEmitter::new(&connection, item_path).expect("emitter");
            Self {
                connection,
                state,
                calls,
                emitter,
            }
        }
    }

    /// A private `dbus-daemon --session` per test; skip (not fail) when
    /// dbus-daemon is not installed.
    struct PrivateBus {
        child: tokio::process::Child,
        address: String,
    }

    async fn private_bus() -> Option<PrivateBus> {
        use tokio::io::AsyncBufReadExt as _;

        let mut child = tokio::process::Command::new("dbus-daemon")
            .args(["--session", "--print-address", "--nofork"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn();
        if let Err(error) = &mut child {
            eprintln!("skipped: dbus-daemon is not available ({error})");
            return None;
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
                eprintln!("skipped: dbus-daemon printed no address");
                return None;
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

    async fn register_with_watcher(connection: &Connection, argument: &str) {
        let watcher = ProxyBuilder::<Proxy>::new(connection)
            .destination(WATCHER_NAME)
            .expect("destination")
            .path(WATCHER_PATH)
            .expect("path")
            .interface(WATCHER_IFACE)
            .expect("interface")
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .expect("watcher proxy");
        watcher
            .call_method("RegisterStatusNotifierItem", &[argument])
            .await
            .expect("RegisterStatusNotifierItem replies");
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
        let app1 = TestApp::spawn(
            &bus.address,
            Some("org.kde.StatusNotifierItem-test-1"),
            DEFAULT_ITEM_PATH,
        )
        .await;
        register_with_watcher(&app1.connection, "org.kde.StatusNotifierItem-test-1").await;
        // Item 2: registers by object path (a second connection).
        let app2 = TestApp::spawn(&bus.address, None, "/org/test/SecondItem").await;
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
        // client, matches the SNI spec.
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
        assert!(items.contains(&"org.kde.StatusNotifierItem-test-1".to_string()));
        assert!(items.iter().any(|item| item.starts_with(':')));
        assert_eq!(
            bool::try_from(
                all.get("IsStatusNotifierHostRegistered")
                    .expect("property present")
                    .clone()
            )
            .expect("bool"),
            true
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

    /// NewTitle on the item refreshes the props surface and produces a
    /// changed event.
    #[tokio::test]
    async fn new_title_refreshes_the_item_props() {
        let Some(bus) = private_bus().await else {
            return;
        };
        let host = spawn_host(&bus.address).await;
        let app = TestApp::spawn(
            &bus.address,
            Some("org.kde.StatusNotifierItem-title"),
            DEFAULT_ITEM_PATH,
        )
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

    /// An item's connection dropping removes it: props vanish, and the
    /// watcher emits StatusNotifierItemUnregistered.
    #[tokio::test]
    async fn a_dropped_connection_removes_the_item_and_unregisters() {
        let Some(bus) = private_bus().await else {
            return;
        };
        let host = spawn_host(&bus.address).await;
        let app = TestApp::spawn(
            &bus.address,
            Some("org.kde.StatusNotifierItem-doomed"),
            DEFAULT_ITEM_PATH,
        )
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
        assert_eq!(service, "org.kde.StatusNotifierItem-doomed");

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
        let app = TestApp::spawn(
            &bus.address,
            Some("org.kde.StatusNotifierItem-activate"),
            DEFAULT_ITEM_PATH,
        )
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

        let calls = app.calls.lock().expect("calls");
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
        drop(calls);

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
    /// tree, and tray.menu.click delivers the Event.
    #[tokio::test]
    async fn menu_verbs_read_the_layout_and_deliver_clicks() {
        let Some(bus) = private_bus().await else {
            return;
        };
        let host = spawn_host(&bus.address).await;
        let app = TestApp::spawn(
            &bus.address,
            Some("org.kde.StatusNotifierItem-menu"),
            DEFAULT_ITEM_PATH,
        )
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
        let children = menu["layout"]["children"].as_array().expect("children");
        assert_eq!(children.len(), 4);
        assert_eq!(children[0]["id"], 5);
        assert_eq!(children[0]["label"], "Open");
        assert_eq!(children[0]["enabled"], true);
        assert_eq!(children[1]["toggle_type"], "checkmark");
        assert_eq!(children[1]["toggle_state"], 1);
        assert_eq!(children[2]["type"], "separator");
        assert_eq!(children[3]["children"][0]["label"], "Nested");

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

    /// tray.icon serves the largest pixmap's pixels; the props never
    /// carry them.
    #[tokio::test]
    async fn the_icon_verb_serves_the_largest_pixmap() {
        let Some(bus) = private_bus().await else {
            return;
        };
        let host = spawn_host(&bus.address).await;
        let app = TestApp::spawn(
            &bus.address,
            Some("org.kde.StatusNotifierItem-icon"),
            DEFAULT_ITEM_PATH,
        )
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
        let app = TestApp::spawn(
            &bus.address,
            Some("org.kde.StatusNotifierItem-hung"),
            DEFAULT_ITEM_PATH,
        )
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
