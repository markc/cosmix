pub mod events;
pub mod ports;

use anyhow::{Context, Result};
use mlua::prelude::*;
use std::sync::Mutex;

use crate::daemon::state::SharedState;

/// Global handle to daemon shared state, set when running inside the daemon.
/// Lua port resolution reads this to find app sockets from the registry.
pub static DAEMON_STATE: Mutex<Option<SharedState>> = Mutex::new(None);

pub(crate) fn lua_value_to_json(val: &LuaValue) -> Result<serde_json::Value, LuaError> {
    match val {
        LuaValue::Nil => Ok(serde_json::Value::Null),
        LuaValue::Boolean(b) => Ok(serde_json::json!(*b)),
        LuaValue::Integer(n) => Ok(serde_json::json!(*n)),
        LuaValue::Number(n) => Ok(serde_json::json!(*n)),
        LuaValue::String(s) => {
            let s = s.to_str().map_err(LuaError::external)?;
            Ok(serde_json::Value::String(s.to_string()))
        }
        LuaValue::Table(t) => {
            // Check if array (sequential integer keys starting at 1)
            let len = t.raw_len();
            if len > 0 {
                let mut arr = Vec::new();
                for i in 1..=len {
                    let v: LuaValue = t.raw_get(i)?;
                    arr.push(lua_value_to_json(&v)?);
                }
                Ok(serde_json::Value::Array(arr))
            } else {
                let mut map = serde_json::Map::new();
                for pair in t.clone().pairs::<String, LuaValue>() {
                    let (k, v) = pair?;
                    map.insert(k, lua_value_to_json(&v)?);
                }
                Ok(serde_json::Value::Object(map))
            }
        }
        _ => Err(LuaError::external(anyhow::anyhow!("Unsupported Lua type for JSON conversion: {}", val.type_name()))),
    }
}

fn lua_args_to_json(args: Option<LuaValue>) -> Result<Option<Vec<serde_json::Value>>, LuaError> {
    match args {
        None | Some(LuaValue::Nil) => Ok(None),
        Some(LuaValue::Table(t)) => {
            let mut result = Vec::new();
            let len = t.raw_len();
            for i in 1..=len {
                let v: LuaValue = t.raw_get(i)?;
                result.push(lua_value_to_json(&v)?);
            }
            Ok(Some(result))
        }
        Some(other) => {
            Ok(Some(vec![lua_value_to_json(&other)?]))
        }
    }
}

fn project_root() -> std::path::PathBuf {
    // Walk up from CWD looking for _bin/, or fall back to CWD
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let mut dir = cwd.as_path();
    loop {
        if dir.join("_bin").is_dir() {
            return dir.to_path_buf();
        }
        match dir.parent() {
            Some(p) => dir = p,
            None => return cwd,
        }
    }
}

fn create_lua() -> std::result::Result<Lua, LuaError> {
    let lua = Lua::new();

    // Set up package.path: _lib/ (project) + ~/.config/cosmix/lib/ (user)
    let root = project_root();
    let lib_path = root.join("_lib");
    let user_lib = dirs().join("lib");
    let package: LuaTable = lua.globals().get("package")?;
    let existing_path: String = package.get("path")?;
    let new_path = format!(
        "{}/?.lua;{}/?/init.lua;{}/?.lua;{}/?/init.lua;{existing_path}",
        lib_path.display(), lib_path.display(),
        user_lib.display(), user_lib.display(),
    );
    package.set("path", new_path)?;

    lua.globals().set("cosmix", lua.create_table()?)?;

    let cosmix: LuaTable = lua.globals().get("cosmix")?;

    // cosmix.windows() -> table of window info
    cosmix.set("windows", lua.create_function(|lua, ()| {
        let (_conn, _eq, state) = crate::wayland::connect()
            .map_err(LuaError::external)?;
        let tbl = lua.create_table()?;
        let mut entries: Vec<_> = state.toplevels.values().collect();
        entries.sort_by(|a, b| a.app_id.cmp(&b.app_id));
        for (i, w) in entries.iter().enumerate() {
            if w.app_id.is_empty() && w.title.is_empty() {
                continue;
            }
            let entry = lua.create_table()?;
            entry.set("app_id", w.app_id.as_str())?;
            entry.set("title", w.title.as_str())?;
            entry.set("activated", w.activated)?;
            entry.set("maximized", w.maximized)?;
            entry.set("minimized", w.minimized)?;
            entry.set("fullscreen", w.fullscreen)?;
            entry.set("sticky", w.sticky)?;
            if let Some(ref g) = w.geometry {
                let geo = lua.create_table()?;
                geo.set("x", g.x)?;
                geo.set("y", g.y)?;
                geo.set("width", g.width)?;
                geo.set("height", g.height)?;
                entry.set("geometry", geo)?;
            }
            tbl.set(i + 1, entry)?;
        }
        Ok(tbl)
    })?)?;

    // cosmix.workspaces() -> table of workspace info
    cosmix.set("workspaces", lua.create_function(|lua, ()| {
        let (_conn, _eq, state) = crate::wayland::connect()
            .map_err(LuaError::external)?;
        let tbl = lua.create_table()?;
        let mut entries: Vec<_> = state.workspaces.values().collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        for (i, ws) in entries.iter().enumerate() {
            let entry = lua.create_table()?;
            entry.set("name", ws.name.as_str())?;
            entry.set("active", ws.active)?;
            entry.set("urgent", ws.urgent)?;
            entry.set("hidden", ws.hidden)?;
            if !ws.coordinates.is_empty() {
                let coords = lua.create_table()?;
                for (j, c) in ws.coordinates.iter().enumerate() {
                    coords.set(j + 1, *c)?;
                }
                entry.set("coordinates", coords)?;
            }
            tbl.set(i + 1, entry)?;
        }
        Ok(tbl)
    })?)?;

    // cosmix.activate(query)
    cosmix.set("activate", lua.create_function(|_, query: String| {
        crate::wayland::toplevel::activate_window(&query)
            .map_err(LuaError::external)
    })?)?;

    // cosmix.close(query)
    cosmix.set("close", lua.create_function(|_, query: String| {
        crate::wayland::toplevel::close_window(&query)
            .map_err(LuaError::external)
    })?)?;

    // cosmix.minimize(query)
    cosmix.set("minimize", lua.create_function(|_, query: String| {
        crate::wayland::toplevel::minimize_window(&query)
            .map_err(LuaError::external)
    })?)?;

    // cosmix.maximize(query)
    cosmix.set("maximize", lua.create_function(|_, query: String| {
        crate::wayland::toplevel::maximize_window(&query)
            .map_err(LuaError::external)
    })?)?;

    // cosmix.clipboard() -> string
    cosmix.set("clipboard", lua.create_function(|_, ()| {
        crate::dbus::clipboard::get_clipboard()
            .map_err(LuaError::external)
    })?)?;

    // cosmix.set_clipboard(text)
    cosmix.set("set_clipboard", lua.create_function(|_, text: String| {
        crate::dbus::clipboard::set_clipboard(&text)
            .map_err(LuaError::external)
    })?)?;

    // cosmix.apps() -> table of desktop entries
    cosmix.set("apps", lua.create_function(|lua, ()| {
        let apps = crate::desktop::list_apps()
            .map_err(LuaError::external)?;
        let tbl = lua.create_table()?;
        for (i, app) in apps.iter().enumerate() {
            let entry = lua.create_table()?;
            entry.set("name", app.name.as_str())?;
            entry.set("exec", app.exec.as_str())?;
            entry.set("icon", app.icon.as_str())?;
            entry.set("comment", app.comment.as_str())?;
            entry.set("categories", app.categories.as_str())?;
            entry.set("terminal", app.terminal)?;
            tbl.set(i + 1, entry)?;
        }
        Ok(tbl)
    })?)?;

    // cosmix.screenshot(save_path?) — native Wayland full-screen screenshot
    cosmix.set("screenshot", lua.create_function(|_, save_path: Option<String>| {
        let path = match save_path {
            Some(p) => std::path::PathBuf::from(p),
            None => {
                let dir = crate::wayland::screenshot::screenshots_dir();
                let ts = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S");
                dir.join(format!("Screenshot_{ts}.png"))
            }
        };
        let result = crate::wayland::screenshot::capture_screenshot(&path)
            .map_err(LuaError::external)?;
        Ok(result.to_string_lossy().to_string())
    })?)?;

    // cosmix.fullscreen(query)
    cosmix.set("fullscreen", lua.create_function(|_, query: String| {
        crate::wayland::toplevel::fullscreen_window(&query)
            .map_err(LuaError::external)
    })?)?;

    // cosmix.sticky(query)
    cosmix.set("sticky", lua.create_function(|_, query: String| {
        crate::wayland::toplevel::sticky_window(&query)
            .map_err(LuaError::external)
    })?)?;

    // cosmix.notify(summary, body?)
    cosmix.set("notify", lua.create_function(|_, (summary, body): (String, Option<String>)| {
        let body = body.unwrap_or_default();
        let rt = tokio::runtime::Runtime::new().map_err(LuaError::external)?;
        rt.block_on(crate::dbus::notify::send_notification(&summary, &body))
            .map_err(LuaError::external)?;
        Ok(())
    })?)?;

    // cosmix.exec(cmd) -> stdout string
    cosmix.set("exec", lua.create_function(|_, cmd: String| {
        let output = std::process::Command::new("sh")
            .args(["-c", &cmd])
            .stderr(std::process::Stdio::null())
            .output()
            .map_err(LuaError::external)?;
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    })?)?;

    // cosmix.spawn(cmd) -> pid
    cosmix.set("spawn", lua.create_function(|_, cmd: String| {
        let child = std::process::Command::new("sh")
            .args(["-c", &cmd])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(LuaError::external)?;
        Ok(child.id())
    })?)?;

    // cosmix.sleep(ms)
    cosmix.set("sleep", lua.create_function(|_, ms: u64| {
        std::thread::sleep(std::time::Duration::from_millis(ms));
        Ok(())
    })?)?;

    // cosmix.env(name) -> string or nil
    cosmix.set("env", lua.create_function(|_, name: String| {
        Ok(std::env::var(&name).ok())
    })?)?;

    // cosmix.hostname() -> string
    cosmix.set("hostname", lua.create_function(|_, ()| {
        let name = std::fs::read_to_string("/etc/hostname")
            .unwrap_or_else(|_| "unknown".into());
        Ok(name.trim().to_string())
    })?)?;

    // cosmix.read_file(path) -> string
    cosmix.set("read_file", lua.create_function(|_, path: String| {
        std::fs::read_to_string(&path).map_err(LuaError::external)
    })?)?;

    // cosmix.write_file(path, content)
    cosmix.set("write_file", lua.create_function(|_, (path, content): (String, String)| {
        std::fs::write(&path, &content).map_err(LuaError::external)
    })?)?;

    // cosmix.glob(pattern) -> table of paths
    cosmix.set("glob", lua.create_function(|lua, pattern: String| {
        let tbl = lua.create_table()?;
        let entries = glob::glob(&pattern).map_err(LuaError::external)?;
        let mut i = 1;
        for entry in entries {
            if let Ok(path) = entry {
                tbl.set(i, path.display().to_string())?;
                i += 1;
            }
        }
        Ok(tbl)
    })?)?;

    // cosmix.type_text(text, delay_us?) — inject text via virtual keyboard
    cosmix.set("type_text", lua.create_function(|_, (text, delay_us): (String, Option<u64>)| {
        let delay = delay_us.unwrap_or(5000);
        crate::wayland::virtual_keyboard::type_text(&text, delay)
            .map_err(LuaError::external)
    })?)?;

    // cosmix.send_key(combo, delay_us?) — send key combo via virtual keyboard
    cosmix.set("send_key", lua.create_function(|_, (combo, delay_us): (String, Option<u64>)| {
        let delay = delay_us.unwrap_or(5000);
        crate::wayland::virtual_keyboard::send_key(&combo, delay)
            .map_err(LuaError::external)
    })?)?;

    // cosmix.midi table
    {
        let midi = lua.create_table()?;

        midi.set("list_ports", lua.create_function(|lua, ()| {
            let (outputs, inputs) = crate::pipewire::list_ports().map_err(LuaError::external)?;
            let tbl = lua.create_table()?;
            let out_tbl = lua.create_table()?;
            for (i, p) in outputs.iter().enumerate() {
                out_tbl.set(i + 1, p.name.as_str())?;
            }
            let in_tbl = lua.create_table()?;
            for (i, p) in inputs.iter().enumerate() {
                in_tbl.set(i + 1, p.name.as_str())?;
            }
            tbl.set("outputs", out_tbl)?;
            tbl.set("inputs", in_tbl)?;
            Ok(tbl)
        })?)?;

        midi.set("list_connections", lua.create_function(|lua, ()| {
            let connections = crate::pipewire::list_connections().map_err(LuaError::external)?;
            let tbl = lua.create_table()?;
            for (i, (out, inp)) in connections.iter().enumerate() {
                let pair = lua.create_table()?;
                pair.set("output", out.as_str())?;
                pair.set("input", inp.as_str())?;
                tbl.set(i + 1, pair)?;
            }
            Ok(tbl)
        })?)?;

        midi.set("connect", lua.create_function(|_, (output, input): (String, String)| {
            crate::pipewire::connect(&output, &input).map_err(LuaError::external)?;
            Ok(true)
        })?)?;

        midi.set("disconnect", lua.create_function(|_, (output, input): (String, String)| {
            crate::pipewire::disconnect(&output, &input).map_err(LuaError::external)?;
            Ok(true)
        })?)?;

        cosmix.set("midi", midi)?;
    }

    // cosmix.mail table
    {
        let mail = lua.create_table()?;

        mail.set("connect", lua.create_function(|_, (url, user, pass): (String, String, String)| {
            crate::mail::connect(&url, &user, &pass).map_err(LuaError::external)
        })?)?;

        mail.set("mailboxes", lua.create_function(|lua, ()| {
            let result = crate::mail::mailboxes().map_err(LuaError::external)?;
            lua.to_value(&result).map_err(LuaError::external)
        })?)?;

        mail.set("query", lua.create_function(|lua, (mailbox, limit): (Option<String>, Option<u32>)| {
            let result = crate::mail::query(mailbox.as_deref(), limit).map_err(LuaError::external)?;
            lua.to_value(&result).map_err(LuaError::external)
        })?)?;

        mail.set("read", lua.create_function(|lua, id: String| {
            let result = crate::mail::read(&id).map_err(LuaError::external)?;
            lua.to_value(&result).map_err(LuaError::external)
        })?)?;

        mail.set("send", lua.create_function(|lua, (to, subject, body): (String, String, String)| {
            let result = crate::mail::send(&to, &subject, &body).map_err(LuaError::external)?;
            lua.to_value(&result).map_err(LuaError::external)
        })?)?;

        mail.set("reply", lua.create_function(|lua, (id, body): (String, String)| {
            let result = crate::mail::reply(&id, &body).map_err(LuaError::external)?;
            lua.to_value(&result).map_err(LuaError::external)
        })?)?;

        cosmix.set("mail", mail)?;
    }

    // cosmix.dbus(service, path, interface, method, args?) -> result
    cosmix.set("dbus", lua.create_function(|lua, (service, path, interface, method, args): (String, String, String, String, Option<LuaValue>)| {
        let json_args = lua_args_to_json(args)?;
        let rt = tokio::runtime::Runtime::new().map_err(LuaError::external)?;
        let result = rt.block_on(crate::dbus::generic::dbus_call(&service, &path, &interface, &method, json_args.as_deref(), false))
            .map_err(LuaError::external)?;
        lua.to_value(&result).map_err(LuaError::external)
    })?)?;

    // cosmix.dbus_system(service, path, interface, method, args?) -> result
    cosmix.set("dbus_system", lua.create_function(|lua, (service, path, interface, method, args): (String, String, String, String, Option<LuaValue>)| {
        let json_args = lua_args_to_json(args)?;
        let rt = tokio::runtime::Runtime::new().map_err(LuaError::external)?;
        let result = rt.block_on(crate::dbus::generic::dbus_call(&service, &path, &interface, &method, json_args.as_deref(), true))
            .map_err(LuaError::external)?;
        lua.to_value(&result).map_err(LuaError::external)
    })?)?;

    // cosmix.dbus_list(service, path?) -> introspection XML
    cosmix.set("dbus_list", lua.create_function(|_, (service, path): (String, Option<String>)| {
        let path = path.unwrap_or_else(|| "/".into());
        let rt = tokio::runtime::Runtime::new().map_err(LuaError::external)?;
        rt.block_on(crate::dbus::generic::dbus_introspect(&service, &path, false))
            .map_err(LuaError::external)
    })?)?;

    // cosmix.config_list() -> table of component names
    cosmix.set("config_list", lua.create_function(|lua, ()| {
        let components = crate::cosmic_config::list_components()
            .map_err(LuaError::external)?;
        let tbl = lua.create_table()?;
        for (i, c) in components.iter().enumerate() {
            tbl.set(i + 1, c.as_str())?;
        }
        Ok(tbl)
    })?)?;

    // cosmix.config_keys(component) -> table of key names
    cosmix.set("config_keys", lua.create_function(|lua, component: String| {
        let keys = crate::cosmic_config::list_keys(&component)
            .map_err(LuaError::external)?;
        let tbl = lua.create_table()?;
        for (i, k) in keys.iter().enumerate() {
            tbl.set(i + 1, k.as_str())?;
        }
        Ok(tbl)
    })?)?;

    // cosmix.config_read(component, key) -> string
    cosmix.set("config_read", lua.create_function(|_, (component, key): (String, String)| {
        crate::cosmic_config::read_key(&component, &key)
            .map_err(LuaError::external)
    })?)?;

    // cosmix.config_write(component, key, value)
    cosmix.set("config_write", lua.create_function(|_, (component, key, value): (String, String, String)| {
        crate::cosmic_config::write_key(&component, &key, &value)
            .map_err(LuaError::external)
    })?)?;

    // cosmix.json_encode(table) -> string
    cosmix.set("json_encode", lua.create_function(|_, val: LuaValue| {
        let json = serde_json::to_string(&val).map_err(LuaError::external)?;
        Ok(json)
    })?)?;

    // cosmix.json_decode(string) -> table
    cosmix.set("json_decode", lua.create_function(|lua, s: String| {
        let val: serde_json::Value = serde_json::from_str(&s).map_err(LuaError::external)?;
        lua.to_value(&val).map_err(LuaError::external)
    })?)?;

    // Register port system (must be after all cosmix.* functions)
    ports::register_port_api(&lua)?;

    Ok(lua)
}

fn resolve_script(name: &str) -> Result<std::path::PathBuf> {
    let path = std::path::Path::new(name);

    // 1. Literal path (absolute or relative with extension)
    if path.exists() {
        return Ok(path.to_path_buf());
    }

    // 2. Try adding .lua extension
    let with_ext = path.with_extension("lua");
    if with_ext.exists() {
        return Ok(with_ext);
    }

    // 3. User scripts: ~/.config/cosmix/
    if let Ok(config) = std::env::var("XDG_CONFIG_HOME") {
        let user_path = std::path::Path::new(&config).join("cosmix").join(name);
        if user_path.exists() { return Ok(user_path); }
        let user_lua = user_path.with_extension("lua");
        if user_lua.exists() { return Ok(user_lua); }
    }
    let home_config = dirs();
    let user_path = home_config.join(name);
    if user_path.exists() { return Ok(user_path); }
    let user_lua = user_path.with_extension("lua");
    if user_lua.exists() { return Ok(user_lua); }

    // 4. Project _bin/
    let root = project_root();
    let builtin = root.join("_bin").join(name);
    if builtin.exists() { return Ok(builtin); }
    let builtin_lua = builtin.with_extension("lua");
    if builtin_lua.exists() { return Ok(builtin_lua); }

    anyhow::bail!("Script not found: {name}\nSearched: ./, ~/.config/cosmix/, _bin/")
}

fn dirs() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    std::path::PathBuf::from(home).join(".config").join("cosmix")
}

pub fn run_file(name: &str, script_args: &[String]) -> Result<()> {
    let path = resolve_script(name)?;
    let lua = create_lua().map_err(|e| anyhow::anyhow!("{e}"))?;

    // Set Lua's arg table (arg[0] = script, arg[1..] = arguments)
    let arg_table = lua.create_table().map_err(|e| anyhow::anyhow!("{e}"))?;
    arg_table.set(0, path.to_string_lossy().to_string()).map_err(|e| anyhow::anyhow!("{e}"))?;
    for (i, a) in script_args.iter().enumerate() {
        arg_table.set((i + 1) as i64, a.as_str()).map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    lua.globals().set("arg", arg_table).map_err(|e| anyhow::anyhow!("{e}"))?;

    let code = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    lua.load(&code).set_name(path.to_string_lossy()).exec()
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}

pub fn run_shell() -> Result<()> {
    let lua = create_lua().map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut rl = std::io::stdin().lines();

    println!("cosmix lua shell — type Lua expressions or statements");
    println!("  cosmix.windows()  cosmix.clipboard()  cosmix.notify(\"hi\")");
    println!("  Ctrl-D to exit");
    println!();

    loop {
        eprint!(">> ");
        let line = match rl.next() {
            Some(Ok(line)) => line,
            _ => break,
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Try as expression first (return value), fall back to statement
        let result = lua.load(&format!("return {line}")).eval::<LuaMultiValue>();
        match result {
            Ok(vals) => {
                for val in &vals {
                    print_lua_value(val, 0);
                    println!();
                }
            }
            Err(_) => {
                if let Err(e) = lua.load(line).exec() {
                    eprintln!("Error: {e}");
                }
            }
        }
    }

    println!();
    Ok(())
}

fn print_lua_value(val: &LuaValue, depth: usize) {
    let indent = "  ".repeat(depth);
    match val {
        LuaValue::Nil => print!("nil"),
        LuaValue::Boolean(b) => print!("{b}"),
        LuaValue::Integer(n) => print!("{n}"),
        LuaValue::Number(n) => print!("{n}"),
        LuaValue::String(s) => {
            match s.to_str() {
                Ok(s) => print!("\"{s}\""),
                Err(_) => print!("\"???\""),
            }
        }
        LuaValue::Table(t) => {
            println!("{{");
            for pair in t.clone().pairs::<LuaValue, LuaValue>() {
                if let Ok((k, v)) = pair {
                    print!("{indent}  ");
                    print_lua_value(&k, 0);
                    print!(" = ");
                    print_lua_value(&v, depth + 1);
                    println!(",");
                }
            }
            print!("{indent}}}");
        }
        _ => print!("<{}>", val.type_name()),
    }
}
