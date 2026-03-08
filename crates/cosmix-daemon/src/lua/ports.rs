use mlua::prelude::*;

/// Built-in port names handled internally by the daemon.
const BUILTIN_PORTS: &[&str] = &[
    "clipboard", "windows", "screenshot", "dbus", "config",
    "mail", "midi", "notify", "input",
];

pub fn register_port_api(lua: &Lua) -> LuaResult<()> {
    let cosmix: LuaTable = lua.globals().get("cosmix")?;

    // cosmix.port(name) -> table with port's methods (cached in registry)
    cosmix.set("port", lua.create_function(|lua, name: String| {
        // Check cache first
        let cache_key = format!("_port_cache_{name}");
        let registry = lua.named_registry_value::<LuaValue>(&cache_key)?;
        if let LuaValue::Table(_) = registry {
            return Ok(registry);
        }

        let cosmix: LuaTable = lua.globals().get("cosmix")?;

        // Try built-in ports first
        let port_table = match name.as_str() {
            "clipboard" => build_clipboard_port(lua, &cosmix)?,
            "windows" => build_windows_port(lua, &cosmix)?,
            "screenshot" => build_screenshot_port(lua, &cosmix)?,
            "dbus" => build_dbus_port(lua, &cosmix)?,
            "config" => build_config_port(lua, &cosmix)?,
            "mail" => build_mail_port(lua, &cosmix)?,
            "midi" => build_midi_port(lua, &cosmix)?,
            "notify" => build_notify_port(lua, &cosmix)?,
            "input" => build_input_port(lua, &cosmix)?,
            _ => {
                // Try app port from registry — resolve socket path
                let socket_path = resolve_app_port(&name)?;
                build_app_port(lua, &name, &socket_path)?
            }
        };

        // Cache it
        lua.set_named_registry_value(&cache_key, port_table.clone())?;
        Ok(LuaValue::Table(port_table))
    })?)?;

    // cosmix.ports() -> list of all port names (built-in + app ports)
    cosmix.set("ports", lua.create_function(|lua, ()| {
        let tbl = lua.create_table()?;
        let mut i = 1;
        // Built-in ports
        for name in BUILTIN_PORTS {
            tbl.set(i, *name)?;
            i += 1;
        }
        // App ports from registry (read via daemon state)
        if let Some(app_ports) = get_app_port_names() {
            for name in app_ports {
                tbl.set(i, name)?;
                i += 1;
            }
        }
        Ok(tbl)
    })?)?;

    // cosmix.port_exists(name) -> bool
    cosmix.set("port_exists", lua.create_function(|_, name: String| {
        if BUILTIN_PORTS.contains(&name.to_lowercase().as_str()) {
            return Ok(true);
        }
        Ok(resolve_app_port(&name).is_ok())
    })?)?;

    // cosmix.list_ports() -> table of port info
    cosmix.set("list_ports", lua.create_function(|lua, ()| {
        let tbl = lua.create_table()?;
        let mut i = 1;

        // Built-in ports
        for name in BUILTIN_PORTS {
            let entry = lua.create_table()?;
            entry.set("name", *name)?;
            entry.set("type", "builtin")?;
            tbl.set(i, entry)?;
            i += 1;
        }

        // App ports from registry
        if let Some(infos) = get_app_port_infos() {
            for (name, commands) in infos {
                let entry = lua.create_table()?;
                entry.set("name", name)?;
                entry.set("type", "app")?;
                let cmd_tbl = lua.create_table()?;
                for (j, cmd) in commands.iter().enumerate() {
                    cmd_tbl.set(j + 1, cmd.as_str())?;
                }
                entry.set("commands", cmd_tbl)?;
                tbl.set(i, entry)?;
                i += 1;
            }
        }

        Ok(tbl)
    })?)?;

    // cosmix.wait_for_port(name, timeout_ms) -> bool
    cosmix.set("wait_for_port", lua.create_function(|_, (name, timeout_ms): (String, Option<u64>)| {
        let timeout = timeout_ms.unwrap_or(5000);
        let start = std::time::Instant::now();
        let interval = std::time::Duration::from_millis(100);
        let deadline = std::time::Duration::from_millis(timeout);

        loop {
            if resolve_app_port(&name).is_ok() {
                return Ok(true);
            }
            if start.elapsed() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(interval);
        }
    })?)?;

    Ok(())
}

// ── App port resolution (reads daemon shared state) ──

/// Try to find an app port's socket path from the daemon's port registry.
///
/// This reads from DAEMON_STATE if the daemon is running (set during daemon startup).
/// Falls back to scanning the port directory directly for CLI mode.
fn resolve_app_port(name: &str) -> Result<String, LuaError> {
    // Try daemon shared state first
    if let Some(state) = crate::lua::DAEMON_STATE.lock().unwrap().as_ref() {
        let s = state.read().unwrap();
        if let Some(info) = s.port_registry.find(name) {
            return Ok(info.socket.to_string_lossy().into_owned());
        }
    }

    // Fallback: try the conventional socket path directly
    let lower = name.to_lowercase();
    // Strip ".N" suffix for path lookup
    let base = if let Some(dot_pos) = lower.rfind('.') {
        let suffix = &lower[dot_pos + 1..];
        if suffix.chars().all(|c| c.is_ascii_digit()) {
            &lower[..dot_pos]
        } else {
            &lower
        }
    } else {
        &lower
    };

    let socket_path = cosmix_port::Port::socket_path(base);
    if socket_path.exists() {
        return Ok(socket_path.to_string_lossy().into_owned());
    }

    Err(LuaError::RuntimeError(format!(
        "Unknown port: '{name}'. Not a built-in port and no app socket found.\n\
         Built-in: {}\n\
         Tip: Is the app running with cosmix-port enabled?",
        BUILTIN_PORTS.join(", ")
    )))
}

/// Get app port names from daemon state (if running).
fn get_app_port_names() -> Option<Vec<String>> {
    let lock = crate::lua::DAEMON_STATE.lock().unwrap();
    let state = lock.as_ref()?;
    let s = state.read().unwrap();
    Some(s.port_registry.ports.values()
        .map(|p| p.name.clone())
        .collect())
}

/// Get app port infos (name + commands) from daemon state.
fn get_app_port_infos() -> Option<Vec<(String, Vec<String>)>> {
    let lock = crate::lua::DAEMON_STATE.lock().unwrap();
    let state = lock.as_ref()?;
    let s = state.read().unwrap();
    Some(s.port_registry.ports.values()
        .map(|p| (p.name.clone(), p.commands.clone()))
        .collect())
}

// ── Build app port table with :send() method ──

fn build_app_port(lua: &Lua, name: &str, socket_path: &str) -> LuaResult<LuaTable> {
    let t = lua.create_table()?;
    t.set("_name", name.to_string())?;
    t.set("_socket", socket_path.to_string())?;

    // port:send(command, args?) -> response table
    t.set("send", lua.create_function(|lua, (this, command, args): (LuaTable, String, Option<LuaValue>)| {
        let socket: String = this.get("_socket")?;
        let port_name: String = this.get("_name")?;

        // Convert Lua args to JSON
        let json_args = match args {
            None | Some(LuaValue::Nil) => serde_json::Value::Null,
            Some(val) => super::lua_value_to_json(&val)?,
        };

        // Call the port synchronously (Lua is single-threaded)
        let rt = tokio::runtime::Runtime::new().map_err(LuaError::external)?;
        let result = rt.block_on(cosmix_port::call_port(&socket, &command, json_args));

        match result {
            Ok(data) => {
                // Convert JSON response to Lua table
                let response = lua.create_table()?;
                response.set("ok", true)?;
                let lua_data = json_to_lua(lua, &data)?;
                response.set("data", lua_data)?;
                Ok(response)
            }
            Err(e) => {
                let response = lua.create_table()?;
                response.set("ok", false)?;
                response.set("error", e.to_string())?;
                response.set("port", port_name)?;
                response.set("command", command)?;
                Ok(response)
            }
        }
    })?)?;

    Ok(t)
}

/// Convert serde_json::Value to LuaValue.
fn json_to_lua(lua: &Lua, val: &serde_json::Value) -> LuaResult<LuaValue> {
    match val {
        serde_json::Value::Null => Ok(LuaValue::Nil),
        serde_json::Value::Bool(b) => Ok(LuaValue::Boolean(*b)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(LuaValue::Integer(i))
            } else if let Some(f) = n.as_f64() {
                Ok(LuaValue::Number(f))
            } else {
                Ok(LuaValue::Nil)
            }
        }
        serde_json::Value::String(s) => {
            Ok(LuaValue::String(lua.create_string(s)?))
        }
        serde_json::Value::Array(arr) => {
            let t = lua.create_table()?;
            for (i, v) in arr.iter().enumerate() {
                t.set(i + 1, json_to_lua(lua, v)?)?;
            }
            Ok(LuaValue::Table(t))
        }
        serde_json::Value::Object(map) => {
            let t = lua.create_table()?;
            for (k, v) in map {
                t.set(k.as_str(), json_to_lua(lua, v)?)?;
            }
            Ok(LuaValue::Table(t))
        }
    }
}

// ── Built-in port builders (unchanged) ──

fn delegate(_lua: &Lua, cosmix: &LuaTable, key: &str) -> LuaResult<LuaFunction> {
    let val: LuaValue = cosmix.get(key)?;
    match val {
        LuaValue::Function(f) => Ok(f),
        _ => Err(LuaError::RuntimeError(format!("cosmix.{key} is not a function"))),
    }
}

fn delegate_sub(_lua: &Lua, cosmix: &LuaTable, table_key: &str, method: &str) -> LuaResult<LuaFunction> {
    let sub: LuaTable = cosmix.get(table_key)?;
    let val: LuaValue = sub.get(method)?;
    match val {
        LuaValue::Function(f) => Ok(f),
        _ => Err(LuaError::RuntimeError(format!("cosmix.{table_key}.{method} is not a function"))),
    }
}

fn build_clipboard_port(lua: &Lua, cosmix: &LuaTable) -> LuaResult<LuaTable> {
    let t = lua.create_table()?;
    t.set("get", delegate(lua, cosmix, "clipboard")?)?;
    t.set("set", delegate(lua, cosmix, "set_clipboard")?)?;
    Ok(t)
}

fn build_windows_port(lua: &Lua, cosmix: &LuaTable) -> LuaResult<LuaTable> {
    let t = lua.create_table()?;
    t.set("list", delegate(lua, cosmix, "windows")?)?;
    t.set("activate", delegate(lua, cosmix, "activate")?)?;
    t.set("close", delegate(lua, cosmix, "close")?)?;
    t.set("minimize", delegate(lua, cosmix, "minimize")?)?;
    t.set("maximize", delegate(lua, cosmix, "maximize")?)?;
    t.set("fullscreen", delegate(lua, cosmix, "fullscreen")?)?;
    t.set("sticky", delegate(lua, cosmix, "sticky")?)?;
    Ok(t)
}

fn build_screenshot_port(lua: &Lua, cosmix: &LuaTable) -> LuaResult<LuaTable> {
    let t = lua.create_table()?;
    t.set("take", delegate(lua, cosmix, "screenshot")?)?;
    Ok(t)
}

fn build_dbus_port(lua: &Lua, cosmix: &LuaTable) -> LuaResult<LuaTable> {
    let t = lua.create_table()?;
    t.set("call", delegate(lua, cosmix, "dbus")?)?;
    t.set("call_system", delegate(lua, cosmix, "dbus_system")?)?;
    t.set("list", delegate(lua, cosmix, "dbus_list")?)?;
    Ok(t)
}

fn build_config_port(lua: &Lua, cosmix: &LuaTable) -> LuaResult<LuaTable> {
    let t = lua.create_table()?;
    t.set("list", delegate(lua, cosmix, "config_list")?)?;
    t.set("keys", delegate(lua, cosmix, "config_keys")?)?;
    t.set("read", delegate(lua, cosmix, "config_read")?)?;
    t.set("write", delegate(lua, cosmix, "config_write")?)?;
    Ok(t)
}

fn build_mail_port(lua: &Lua, cosmix: &LuaTable) -> LuaResult<LuaTable> {
    let t = lua.create_table()?;
    t.set("connect", delegate_sub(lua, cosmix, "mail", "connect")?)?;
    t.set("mailboxes", delegate_sub(lua, cosmix, "mail", "mailboxes")?)?;
    t.set("query", delegate_sub(lua, cosmix, "mail", "query")?)?;
    t.set("read", delegate_sub(lua, cosmix, "mail", "read")?)?;
    t.set("send", delegate_sub(lua, cosmix, "mail", "send")?)?;
    t.set("reply", delegate_sub(lua, cosmix, "mail", "reply")?)?;
    Ok(t)
}

fn build_midi_port(lua: &Lua, cosmix: &LuaTable) -> LuaResult<LuaTable> {
    let t = lua.create_table()?;
    t.set("list_ports", delegate_sub(lua, cosmix, "midi", "list_ports")?)?;
    t.set("list_connections", delegate_sub(lua, cosmix, "midi", "list_connections")?)?;
    t.set("connect", delegate_sub(lua, cosmix, "midi", "connect")?)?;
    t.set("disconnect", delegate_sub(lua, cosmix, "midi", "disconnect")?)?;
    Ok(t)
}

fn build_notify_port(lua: &Lua, cosmix: &LuaTable) -> LuaResult<LuaTable> {
    let t = lua.create_table()?;
    t.set("send", delegate(lua, cosmix, "notify")?)?;
    Ok(t)
}

fn build_input_port(lua: &Lua, cosmix: &LuaTable) -> LuaResult<LuaTable> {
    let t = lua.create_table()?;
    t.set("type", delegate(lua, cosmix, "type_text")?)?;
    t.set("key", delegate(lua, cosmix, "send_key")?)?;
    Ok(t)
}
