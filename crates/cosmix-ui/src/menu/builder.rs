use super::types::{MenuAction, MenuItem, MenuBarDef, Shortcut};

// ── Item constructors ──────────────────────────────────────────────────────

pub fn action(id: &str, label: &str) -> MenuItem {
    MenuItem::Action {
        id: id.to_string(),
        label: label.to_string(),
        shortcut: None,
        action: MenuAction::Local(id.to_string()),
        enabled: true,
    }
}

pub fn action_shortcut(id: &str, label: &str, shortcut: Shortcut) -> MenuItem {
    MenuItem::Action {
        id: id.to_string(),
        label: label.to_string(),
        shortcut: Some(shortcut),
        action: MenuAction::Local(id.to_string()),
        enabled: true,
    }
}

/// Menu item that fires an AMP command over the hub WebSocket.
#[cfg(feature = "hub")]
pub fn amp_action(id: &str, label: &str, to: &str, command: &str) -> MenuItem {
    MenuItem::Action {
        id: id.to_string(),
        label: label.to_string(),
        shortcut: None,
        action: MenuAction::Amp {
            to: to.to_string(),
            command: command.to_string(),
            args: serde_json::Value::Null,
        },
        enabled: true,
    }
}

/// Menu item that fires an AMP command with arguments over the hub WebSocket.
#[cfg(feature = "hub")]
pub fn amp_action_args(
    id: &str,
    label: &str,
    to: &str,
    command: &str,
    args: serde_json::Value,
) -> MenuItem {
    MenuItem::Action {
        id: id.to_string(),
        label: label.to_string(),
        shortcut: None,
        action: MenuAction::Amp {
            to: to.to_string(),
            command: command.to_string(),
            args,
        },
        enabled: true,
    }
}

pub fn separator() -> MenuItem {
    MenuItem::Separator
}

pub fn submenu(label: &str, items: Vec<MenuItem>) -> MenuItem {
    MenuItem::Submenu { label: label.to_string(), items }
}

// ── Standard menus ─────────────────────────────────────────────────────────

/// File menu. `extra` items are prepended before the separator + Quit.
pub fn standard_file_menu(extra: Vec<MenuItem>) -> MenuItem {
    let mut items = extra;
    if !items.is_empty() {
        items.push(separator());
    }
    items.push(action_shortcut("quit", "Quit", Shortcut::ctrl('q')));
    submenu("File", items)
}

/// Help menu with an About item.
pub fn standard_help_menu(app_name: &str) -> MenuItem {
    let about_id = "about";
    submenu("Help", vec![
        MenuItem::Action {
            id: about_id.to_string(),
            label: format!("About {app_name}"),
            shortcut: None,
            action: MenuAction::Local(about_id.to_string()),
            enabled: true,
        },
    ])
}

// ── MenuBarDef builder ─────────────────────────────────────────────────────

/// Convenience: start building a MenuBarDef.
pub fn menubar(menus: Vec<MenuItem>) -> MenuBarDef {
    MenuBarDef { menus }
}
