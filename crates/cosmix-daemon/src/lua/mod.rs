pub mod events;
pub mod ports;

use anyhow::{Context, Result};
use mlua::prelude::*;

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
    ports::register_port_api(&lua)?;

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

    // cosmix.screenshot(save_dir?) — COSMIC native screenshot
    cosmix.set("screenshot", lua.create_function(|_, save_dir: Option<String>| {
        let mut cmd = vec!["cosmic-screenshot".to_string()];
        if let Some(dir) = save_dir {
            cmd.push("-s".into());
            cmd.push(dir);
            cmd.push("--interactive=false".into());
        }
        std::process::Command::new(&cmd[0])
            .args(&cmd[1..])
            .spawn()
            .map_err(LuaError::external)?;
        Ok(())
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
