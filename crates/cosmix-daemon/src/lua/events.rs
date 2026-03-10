use mlua::prelude::*;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::daemon::events::DaemonEvent;

/// Stores Lua callback functions registered for named events and timers.
pub struct LuaEventRegistry {
    /// Maps event names (e.g. "window_opened") to lists of registered Lua callbacks.
    pub handlers: HashMap<String, Vec<mlua::RegistryKey>>,
    /// Timer callbacks: (interval_ms, callback).
    pub timers: Vec<(u64, mlua::RegistryKey)>,
}

impl LuaEventRegistry {
    pub fn new() -> Self {
        Self {
            handlers: HashMap::new(),
            timers: Vec::new(),
        }
    }
}

impl Default for LuaEventRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Registers `cosmix.on(event_name, handler_fn)` and `cosmix.every(ms, handler_fn)`
/// on the global `cosmix` table.
pub fn register_event_api(lua: &Lua, registry: Arc<Mutex<LuaEventRegistry>>) -> LuaResult<()> {
    let cosmix: LuaTable = lua.globals().get("cosmix")?;

    // cosmix.on(event_name, handler_fn)
    let reg = Arc::clone(&registry);
    cosmix.set(
        "on",
        lua.create_function(move |lua, (event_name, handler): (String, LuaFunction)| {
            let key = lua.create_registry_value(handler)?;
            let mut guard = reg.lock().map_err(|e| LuaError::external(format!("lock poisoned: {e}")))?;
            guard.handlers.entry(event_name).or_default().push(key);
            Ok(())
        })?,
    )?;

    // cosmix.every(ms, handler_fn)
    let reg = Arc::clone(&registry);
    cosmix.set(
        "every",
        lua.create_function(move |lua, (ms, handler): (u64, LuaFunction)| {
            let key = lua.create_registry_value(handler)?;
            let mut guard = reg.lock().map_err(|e| LuaError::external(format!("lock poisoned: {e}")))?;
            guard.timers.push((ms, key));
            Ok(())
        })?,
    )?;

    Ok(())
}

/// Dispatches an event to all Lua handlers registered for `event_name`.
///
/// Errors from individual handlers are logged to stderr but do not stop
/// dispatch to remaining handlers.
pub fn dispatch_event(
    lua: &Lua,
    registry: &LuaEventRegistry,
    event_name: &str,
    event_data: LuaValue,
) {
    let Some(handlers) = registry.handlers.get(event_name) else {
        return;
    };

    for key in handlers {
        let Ok(handler) = lua.registry_value::<LuaFunction>(key) else {
            eprintln!("[cosmix] warning: stale registry key for event '{event_name}'");
            continue;
        };
        if let Err(e) = handler.call::<()>(event_data.clone()) {
            eprintln!("[cosmix] error in '{event_name}' handler: {e}");
        }
    }
}

/// Converts a `DaemonEvent` into a (event_name, lua_value) pair suitable for
/// [`dispatch_event`]. Returns `None` for events that should not be dispatched
/// to Lua (e.g. `Timer`, `Shutdown`).
pub fn daemon_event_to_lua(
    lua: &Lua,
    event: &DaemonEvent,
) -> Option<(&'static str, LuaValue)> {
    match event {
        DaemonEvent::WindowOpened { app_id, title } => {
            let tbl = lua.create_table().ok()?;
            tbl.set("app_id", app_id.as_str()).ok()?;
            tbl.set("title", title.as_str()).ok()?;
            Some(("window_opened", LuaValue::Table(tbl)))
        }
        DaemonEvent::WindowClosed { app_id } => {
            let tbl = lua.create_table().ok()?;
            tbl.set("app_id", app_id.as_str()).ok()?;
            Some(("window_closed", LuaValue::Table(tbl)))
        }
        DaemonEvent::WindowFocused { app_id } => {
            let tbl = lua.create_table().ok()?;
            tbl.set("app_id", app_id.as_str()).ok()?;
            Some(("window_focused", LuaValue::Table(tbl)))
        }
        DaemonEvent::WorkspaceChanged { name } => {
            let tbl = lua.create_table().ok()?;
            tbl.set("name", name.as_str()).ok()?;
            Some(("workspace_changed", LuaValue::Table(tbl)))
        }
        DaemonEvent::ClipboardChanged => {
            Some(("clipboard_changed", LuaValue::Nil))
        }
        DaemonEvent::PortMessage { from, command, body } => {
            let tbl = lua.create_table().ok()?;
            tbl.set("from", from.as_str()).ok()?;
            tbl.set("command", command.as_str()).ok()?;
            match body {
                Some(b) => tbl.set("body", b.as_str()).ok()?,
                None => tbl.set("body", LuaValue::Nil).ok()?,
            };
            Some(("port_message", LuaValue::Table(tbl)))
        }
        DaemonEvent::MeshPeerConnected { node } => {
            let tbl = lua.create_table().ok()?;
            tbl.set("node", node.as_str()).ok()?;
            Some(("mesh_peer_connected", LuaValue::Table(tbl)))
        }
        DaemonEvent::MeshPeerDisconnected { node } => {
            let tbl = lua.create_table().ok()?;
            tbl.set("node", node.as_str()).ok()?;
            Some(("mesh_peer_disconnected", LuaValue::Table(tbl)))
        }
        // Timer and Shutdown are internal daemon events, not dispatched to Lua scripts.
        DaemonEvent::Timer { .. } | DaemonEvent::Shutdown => None,
    }
}
