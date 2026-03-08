use mlua::prelude::*;

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
                return Err(LuaError::RuntimeError(format!(
                    "Unknown port: '{name}'. Available: clipboard, windows, screenshot, dbus, config, mail, midi, notify, input"
                )));
            }
        };

        // Cache it
        lua.set_named_registry_value(&cache_key, port_table.clone())?;
        Ok(LuaValue::Table(port_table))
    })?)?;

    // cosmix.ports() -> list of registered port names
    cosmix.set("ports", lua.create_function(|lua, ()| {
        let tbl = lua.create_table()?;
        let names = ["clipboard", "windows", "screenshot", "dbus", "config", "mail", "midi", "notify", "input"];
        for (i, name) in names.iter().enumerate() {
            tbl.set(i + 1, *name)?;
        }
        Ok(tbl)
    })?)?;

    Ok(())
}

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
