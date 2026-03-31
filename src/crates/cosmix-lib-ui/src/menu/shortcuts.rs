use dioxus::prelude::*;

use super::types::{MenuAction, MenuBarDef, MenuItem};

#[cfg(feature = "hub")]
use std::sync::Arc;
#[cfg(feature = "hub")]
use cosmix_client::HubClient;

/// Collect all (Shortcut, MenuAction) pairs recursively from a menu tree.
fn collect_shortcuts(items: &[MenuItem], out: &mut Vec<(super::types::Shortcut, MenuAction)>) {
    for item in items {
        match item {
            MenuItem::Action { shortcut: Some(sc), action, .. } => {
                out.push((sc.clone(), action.clone()));
            }
            MenuItem::Submenu { items, .. } => {
                collect_shortcuts(items, out);
            }
            _ => {}
        }
    }
}

/// Returns an `onkeydown` event handler that fires menu shortcuts.
///
/// Mount this on the app's root container div:
/// ```ignore
/// div { onkeydown: use_menu_shortcuts(&menu, on_action), ... }
/// ```
///
/// # Parameters
/// - `menu` — the MenuBarDef to extract shortcuts from
/// - `on_action` — callback for `Local` actions (receives action ID)
/// - `hub` — optional hub client signal for `Amp` actions
#[cfg(feature = "hub")]
pub fn use_menu_shortcuts(
    menu: &MenuBarDef,
    on_action: EventHandler<String>,
    hub: Option<Signal<Option<Arc<HubClient>>>>,
) -> EventHandler<KeyboardEvent> {
    let mut pairs: Vec<(super::types::Shortcut, MenuAction)> = Vec::new();
    collect_shortcuts(&menu.menus, &mut pairs);

    use_callback(move |e: KeyboardEvent| {
        for (shortcut, action) in &pairs {
            if shortcut.matches(&e) {
                dispatch_action(action, &on_action, &hub);
                break;
            }
        }
    })
}

/// Shortcut handler without hub support.
#[cfg(not(feature = "hub"))]
pub fn use_menu_shortcuts(
    menu: &MenuBarDef,
    on_action: EventHandler<String>,
) -> EventHandler<KeyboardEvent> {
    let mut pairs: Vec<(super::types::Shortcut, MenuAction)> = Vec::new();
    collect_shortcuts(&menu.menus, &mut pairs);

    use_callback(move |e: KeyboardEvent| {
        for (shortcut, action) in &pairs {
            if shortcut.matches(&e) {
                dispatch_action_local(action, &on_action);
                break;
            }
        }
    })
}

/// Dispatch a MenuAction — local variant only.
#[cfg(not(feature = "hub"))]
pub fn dispatch_action_local(action: &MenuAction, on_action: &EventHandler<String>) {
    match action {
        MenuAction::Local(id) => on_action.call(id.clone()),
        MenuAction::None => {}
    }
}

/// Dispatch a MenuAction — with hub support.
#[cfg(feature = "hub")]
pub fn dispatch_action(
    action: &MenuAction,
    on_action: &EventHandler<String>,
    hub: &Option<Signal<Option<Arc<HubClient>>>>,
) {
    match action {
        MenuAction::Local(id) => on_action.call(id.clone()),
        MenuAction::Amp { to, command, args } => {
            if let Some(hub_sig) = hub {
                if let Some(client) = hub_sig.read().as_ref() {
                    let client = client.clone();
                    let to = to.clone();
                    let command = command.clone();
                    let args = args.clone();
                    spawn(async move {
                        if let Err(e) = client.call(&to, &command, args).await {
                            tracing::warn!("Menu AMP shortcut failed ({command}): {e}");
                        }
                    });
                } else {
                    tracing::warn!("Menu AMP shortcut: hub not connected");
                }
            }
        }
        MenuAction::None => {}
    }
}
