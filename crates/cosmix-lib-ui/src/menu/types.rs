use dioxus::prelude::{KeyboardEvent, ModifiersInteraction};

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
}
