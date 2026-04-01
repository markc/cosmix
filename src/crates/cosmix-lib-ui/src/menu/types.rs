use std::collections::HashMap;
use dioxus::prelude::*;
use serde_json;

// ── AMP menu commands ─────────────────────────────────────────────────────

/// External command that can be sent to the menu bar via AMP.
#[derive(Clone, Debug, PartialEq)]
pub enum MenuCommand {
    /// Open the parent menu and pulse-highlight an item (visual only).
    Highlight { id: String, duration_ms: u32 },
    /// Highlight briefly, then fire the action (simulates a click).
    Invoke { id: String },
    /// Close any open dropdown.
    Close,
}

/// Discoverable menu item info returned by `menu.list`.
#[derive(Clone, Debug)]
pub struct MenuItemInfo {
    pub id: String,
    pub label: String,
    pub shortcut: Option<String>,
    pub enabled: bool,
    pub menu: String,
}

/// A keyboard shortcut modifier + key combination.
#[derive(Clone, Debug, PartialEq)]
pub struct Shortcut {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub key: char,
}

impl Shortcut {
    pub fn ctrl(key: char) -> Self {
        Self { ctrl: true, shift: false, alt: false, key }
    }

    pub fn ctrl_shift(key: char) -> Self {
        Self { ctrl: true, shift: true, alt: false, key }
    }

    /// Human-readable label e.g. "Ctrl+S" or "Ctrl+Shift+S".
    pub fn label(&self) -> String {
        let mut parts = Vec::new();
        if self.ctrl  { parts.push("Ctrl"); }
        if self.shift { parts.push("Shift"); }
        if self.alt   { parts.push("Alt"); }
        parts.push(Box::leak(self.key.to_uppercase().to_string().into_boxed_str()));
        parts.join("+")
    }

    /// Returns true if this shortcut matches the given keyboard event.
    pub fn matches(&self, e: &KeyboardEvent) -> bool {
        use dioxus::prelude::Key;
        let mods = e.modifiers();
        if mods.ctrl() != self.ctrl   { return false; }
        if mods.shift() != self.shift { return false; }
        if mods.alt() != self.alt     { return false; }
        match e.key() {
            Key::Character(ref c) => c.to_lowercase() == self.key.to_lowercase().to_string(),
            _ => false,
        }
    }
}

/// What happens when a menu item is activated.
#[derive(Clone, Debug, PartialEq)]
pub enum MenuAction {
    /// Emit an action ID for the app to handle via `on_action` callback.
    Local(String),
    /// Send an AMP command to a hub service (local or remote mesh node).
    #[cfg(feature = "hub")]
    Amp {
        /// Service name or AMP address e.g. "files" or "files.node.amp"
        to: String,
        /// AMP command e.g. "file.pick"
        command: String,
        /// JSON arguments
        args: serde_json::Value,
    },
    /// No-op (placeholder or disabled item).
    None,
}

/// A single item in a menu.
#[derive(Clone, Debug, PartialEq)]
pub enum MenuItem {
    Action {
        id: String,
        label: String,
        shortcut: Option<Shortcut>,
        action: MenuAction,
        enabled: bool,
    },
    Separator,
    Submenu {
        label: String,
        items: Vec<MenuItem>,
    },
    /// Named injection point — services add items here at runtime via AMP.
    Slot {
        name: String,
    },
}

/// A complete menu bar definition — top-level items must be `Submenu` variants.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MenuBarDef {
    pub menus: Vec<MenuItem>,
}

impl MenuBarDef {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, menu: MenuItem) -> Self {
        self.menus.push(menu);
        self
    }

    /// Collect all actionable menu items for `menu.list` discovery.
    /// Includes items injected into slots.
    pub fn collect_items(&self) -> Vec<MenuItemInfo> {
        let mut out = Vec::new();
        let registry = SLOT_REGISTRY.peek();
        for top in &self.menus {
            match top {
                MenuItem::Submenu { label, items } => {
                    collect_items_recursive(items, label, &registry, &mut out);
                }
                MenuItem::Slot { name } => {
                    for entry in registry.resolve(name) {
                        if let MenuItem::Submenu { label, items } = &entry.item {
                            collect_items_recursive(items, label, &registry, &mut out);
                        }
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// Find which top-level menu index contains an item with the given ID.
    /// Returns (menu_index, cloned MenuItem). Searches both static menus and slot-injected items.
    pub fn find_item(&self, id: &str) -> Option<(usize, MenuItem)> {
        for (idx, top) in self.menus.iter().enumerate() {
            if let MenuItem::Submenu { items, .. } = top {
                if let Some(item) = find_in_items(items, id) {
                    return Some((idx, item.clone()));
                }
            }
        }
        // Also search slot-injected items
        let registry = SLOT_REGISTRY.peek();
        for (idx, top) in self.menus.iter().enumerate() {
            if let MenuItem::Slot { name } = top {
                for entry in registry.resolve(name) {
                    if let MenuItem::Submenu { items, .. } = &entry.item {
                        if let Some(item) = find_in_items(items, id) {
                            return Some((idx, item.clone()));
                        }
                    }
                }
            }
        }
        None
    }
}

fn collect_items_recursive(items: &[MenuItem], menu_label: &str, registry: &SlotRegistry, out: &mut Vec<MenuItemInfo>) {
    for item in items {
        match item {
            MenuItem::Action { id, label, shortcut, enabled, .. } => {
                out.push(MenuItemInfo {
                    id: id.clone(),
                    label: label.clone(),
                    shortcut: shortcut.as_ref().map(|s| s.label()),
                    enabled: *enabled,
                    menu: menu_label.to_string(),
                });
            }
            MenuItem::Submenu { label, items } => {
                collect_items_recursive(items, label, registry, out);
            }
            MenuItem::Slot { name } => {
                for entry in registry.resolve(name) {
                    match &entry.item {
                        MenuItem::Action { id, label, shortcut, enabled, .. } => {
                            out.push(MenuItemInfo {
                                id: id.clone(),
                                label: label.clone(),
                                shortcut: shortcut.as_ref().map(|s| s.label()),
                                enabled: *enabled,
                                menu: menu_label.to_string(),
                            });
                        }
                        MenuItem::Submenu { label, items } => {
                            collect_items_recursive(items, label, registry, out);
                        }
                        _ => {}
                    }
                }
            }
            MenuItem::Separator => {}
        }
    }
}

fn find_in_items<'a>(items: &'a [MenuItem], id: &str) -> Option<&'a MenuItem> {
    for item in items {
        match item {
            MenuItem::Action { id: item_id, .. } if item_id == id => return Some(item),
            MenuItem::Submenu { items, .. } => {
                if let Some(found) = find_in_items(items, id) {
                    return Some(found);
                }
            }
            _ => {}
        }
    }
    None
}

impl MenuItemInfo {
    /// Serialize to JSON for `menu.list` responses.
    pub fn to_json_value(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "label": self.label,
            "shortcut": self.shortcut,
            "enabled": self.enabled,
            "menu": self.menu,
        })
    }
}

// ── Dynamic menu slots ──────────────────────────────────────────────────

/// An item injected into a named slot by a service.
#[derive(Clone, Debug, PartialEq)]
pub struct SlotEntry {
    /// Unique item ID (must not collide with static menu IDs).
    pub id: String,
    /// Service that injected this item (for cleanup on disconnect).
    pub owner: String,
    /// The menu item to render.
    pub item: MenuItem,
}

/// Registry of dynamically injected menu items, keyed by slot name.
#[derive(Clone, Debug, Default)]
pub struct SlotRegistry {
    pub slots: HashMap<String, Vec<SlotEntry>>,
}

impl SlotRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add an item to a named slot. Rejects duplicate IDs across all slots.
    pub fn add(&mut self, slot_name: &str, owner: &str, id: &str, item: MenuItem) -> Result<(), String> {
        // Check for duplicate ID across all slots
        for entries in self.slots.values() {
            if entries.iter().any(|e| e.id == id) {
                return Err(format!("duplicate slot item ID: {id}"));
            }
        }
        self.slots.entry(slot_name.to_string()).or_default().push(SlotEntry {
            id: id.to_string(),
            owner: owner.to_string(),
            item,
        });
        Ok(())
    }

    /// Remove an item by ID from any slot. Returns true if found.
    pub fn remove(&mut self, id: &str) -> bool {
        for entries in self.slots.values_mut() {
            if let Some(pos) = entries.iter().position(|e| e.id == id) {
                entries.remove(pos);
                return true;
            }
        }
        false
    }

    /// Remove all items in a named slot.
    pub fn clear_slot(&mut self, slot_name: &str) {
        self.slots.remove(slot_name);
    }

    /// Remove all items owned by a given service.
    pub fn clear_owner(&mut self, owner: &str) {
        for entries in self.slots.values_mut() {
            entries.retain(|e| e.owner != owner);
        }
        // Clean up empty slots
        self.slots.retain(|_, v| !v.is_empty());
    }

    /// Get items for a named slot (for rendering).
    pub fn resolve(&self, slot_name: &str) -> &[SlotEntry] {
        self.slots.get(slot_name).map(|v| v.as_slice()).unwrap_or(&[])
    }
}

/// Global slot registry — services inject menu items here via AMP.
pub static SLOT_REGISTRY: GlobalSignal<SlotRegistry> = Signal::global(|| SlotRegistry::new());
