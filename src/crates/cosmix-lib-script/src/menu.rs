//! Menu generation — builds a "User" submenu from discovered scripts.
//!
//! Behind the `menu` feature flag (requires cosmix-lib-ui).

use std::collections::HashMap;
use std::sync::Arc;

use cosmix_ui::menu::{action, action_shortcut, separator, submenu, MenuItem, Shortcut};

use crate::discovery::discover_scripts;
use crate::executor;
use crate::types::{ScriptContext, ScriptResult};

/// Build a "User" submenu from script files for the given service.
///
/// Scans `~/.config/cosmix/scripts/global/` and `~/.config/cosmix/scripts/{service_name}/`.
/// Returns an empty submenu if no scripts are found.
pub fn user_menu(service_name: &str) -> MenuItem {
    let scripts = discover_scripts(service_name);

    let mut items: Vec<MenuItem> = Vec::new();

    for (id, script) in &scripts {
        let meta = script.meta();
        let action_id = format!("script:{id}");
        if let Some(ref shortcut_str) = meta.shortcut {
            if let Some(shortcut) = parse_shortcut(shortcut_str) {
                items.push(action_shortcut(&action_id, &meta.name, shortcut));
            } else {
                items.push(action(&action_id, &meta.name));
            }
        } else {
            items.push(action(&action_id, &meta.name));
        }
    }

    if !items.is_empty() {
        items.push(separator());
    }
    items.push(action("script:reload", "Reload Scripts"));
    items.push(action("script:open-folder", "Open Scripts Folder"));

    submenu("User", items)
}

/// Handle a script menu action. Returns `Some(result)` if the action was a script,
/// `None` if it wasn't (caller should handle other actions).
pub async fn handle_script_action(
    action_id: &str,
    service_name: &str,
    hub: Arc<cosmix_client::HubClient>,
    vars: &HashMap<String, String>,
) -> Option<ScriptResult> {
    let script_id = action_id.strip_prefix("script:")?;

    // Re-discover to find the script (could cache, but scripts dir is small)
    let scripts = discover_scripts(service_name);
    let (_, script) = scripts.iter().find(|(id, _)| id == script_id)?;

    let mut ctx = ScriptContext::new(service_name, vars.clone());
    let result = executor::execute_script(script, &mut ctx, hub).await;

    if let Some(ref err) = result.error {
        tracing::warn!("Script '{script_id}' error: {err}");
    } else {
        tracing::info!("Script '{script_id}' completed (rc={})", result.rc);
    }

    Some(result)
}

/// Parse a shortcut string like "Ctrl+Shift+V" into a `Shortcut`.
fn parse_shortcut(s: &str) -> Option<Shortcut> {
    let parts: Vec<&str> = s.split('+').map(|p| p.trim()).collect();
    let mut ctrl = false;
    let mut shift = false;
    let mut alt = false;
    let mut key = None;

    for part in &parts {
        match part.to_lowercase().as_str() {
            "ctrl" => ctrl = true,
            "shift" => shift = true,
            "alt" => alt = true,
            k if k.len() == 1 => key = k.chars().next(),
            _ => return None,
        }
    }

    Some(Shortcut {
        ctrl,
        shift,
        alt,
        key: key?,
    })
}
