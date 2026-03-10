use anyhow::Result;
use cosmix_port::{Port, PortHandle};
use mlua::prelude::*;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{error, info, warn};

fn wrappers_dir() -> PathBuf {
    directories::BaseDirs::new()
        .map(|d| d.config_dir().join("cosmix/wrappers"))
        .unwrap_or_else(|| PathBuf::from("wrappers"))
}

/// First pass: get port name and command names from a wrapper script.
fn scan_wrapper(path: &std::path::Path) -> Result<(String, Vec<String>)> {
    let lua = Lua::new();
    let code = std::fs::read_to_string(path)?;
    let table: LuaTable = lua.load(&code).eval()
        .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;

    let port_name: String = table.get("port")
        .map_err(|e| anyhow::anyhow!("{}: missing 'port': {e}", path.display()))?;

    let commands_table: LuaTable = table.get("commands")
        .map_err(|e| anyhow::anyhow!("{}: missing 'commands': {e}", path.display()))?;

    let mut cmd_names = Vec::new();
    for pair in commands_table.pairs::<String, LuaFunction>() {
        let (name, _) = pair.map_err(|e| anyhow::anyhow!("reading commands: {e}"))?;
        cmd_names.push(name);
    }

    Ok((port_name, cmd_names))
}

/// Create a cosmix Port from a wrapper script.
fn start_wrapper_port(path: &std::path::Path) -> Result<(String, PortHandle)> {
    let code = Arc::new(std::fs::read_to_string(path)?);
    let (port_name, cmd_names) = scan_wrapper(path)?;

    let mut port = Port::new(&port_name);

    for cmd_name in &cmd_names {
        let code = code.clone();
        let cmd = cmd_name.clone();

        port = port.command(cmd_name, "Lua wrapper command", move |args| {
            run_lua_command(&code, &cmd, &args)
        });
    }

    let port = port
        .standard_help()
        .standard_info("cosmix-portd", env!("CARGO_PKG_VERSION"));

    let handle = port.start()?;

    info!("port '{}' started with {} commands from {}",
        port_name, cmd_names.len(), path.display());

    Ok((port_name, handle))
}

/// Execute a Lua wrapper command in a fresh Lua state.
fn run_lua_command(code: &str, cmd: &str, args: &serde_json::Value) -> Result<serde_json::Value> {
    let lua = Lua::new();
    inject_builtins(&lua).map_err(|e| anyhow::anyhow!("builtins: {e}"))?;

    let table: LuaTable = lua.load(code).eval()
        .map_err(|e| anyhow::anyhow!("eval: {e}"))?;
    let commands: LuaTable = table.get("commands")
        .map_err(|e| anyhow::anyhow!("commands: {e}"))?;
    let func: LuaFunction = commands.get(cmd)
        .map_err(|e| anyhow::anyhow!("{cmd}: {e}"))?;

    let lua_args = json_to_lua(&lua, args)
        .map_err(|e| anyhow::anyhow!("args: {e}"))?;
    let result: LuaValue = func.call(lua_args)
        .map_err(|e| anyhow::anyhow!("{cmd}: {e}"))?;

    lua_to_json(&result).map_err(|e| anyhow::anyhow!("result: {e}"))
}

/// Inject helper functions available to wrapper scripts.
fn inject_builtins(lua: &Lua) -> LuaResult<()> {
    let globals = lua.globals();

    // exec(cmd) — run a shell command, return { stdout, stderr, code }
    let exec_fn = lua.create_function(|lua, cmd: String| {
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .output()
            .map_err(LuaError::external)?;
        let t = lua.create_table()?;
        t.set("stdout", String::from_utf8_lossy(&output.stdout).to_string())?;
        t.set("stderr", String::from_utf8_lossy(&output.stderr).to_string())?;
        t.set("code", output.status.code().unwrap_or(-1))?;
        Ok(t)
    })?;
    globals.set("exec", exec_fn)?;

    // read_file(path) — read file to string
    let read_fn = lua.create_function(|_, path: String| {
        std::fs::read_to_string(&path).map_err(LuaError::external)
    })?;
    globals.set("read_file", read_fn)?;

    // write_file(path, content) — write string to file
    let write_fn = lua.create_function(|_, (path, content): (String, String)| {
        std::fs::write(&path, &content).map_err(LuaError::external)
    })?;
    globals.set("write_file", write_fn)?;

    // json_encode(value) — Lua value to JSON string
    let encode_fn = lua.create_function(|_, val: LuaValue| {
        let json = lua_to_json(&val).map_err(LuaError::external)?;
        serde_json::to_string(&json).map_err(LuaError::external)
    })?;
    globals.set("json_encode", encode_fn)?;

    // json_decode(string) — JSON string to Lua value
    let decode_fn = lua.create_function(|lua, s: String| {
        let val: serde_json::Value = serde_json::from_str(&s).map_err(LuaError::external)?;
        json_to_lua(lua, &val).map_err(LuaError::external)
    })?;
    globals.set("json_decode", decode_fn)?;

    Ok(())
}

/// Convert serde_json::Value to LuaValue.
fn json_to_lua(lua: &Lua, val: &serde_json::Value) -> LuaResult<LuaValue> {
    match val {
        serde_json::Value::Null => Ok(LuaValue::Nil),
        serde_json::Value::Bool(b) => Ok(LuaValue::Boolean(*b)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(LuaValue::Integer(i))
            } else {
                Ok(LuaValue::Number(n.as_f64().unwrap_or(0.0)))
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

/// Convert LuaValue to serde_json::Value.
fn lua_to_json(val: &LuaValue) -> LuaResult<serde_json::Value> {
    match val {
        LuaValue::Nil => Ok(serde_json::Value::Null),
        LuaValue::Boolean(b) => Ok(serde_json::json!(*b)),
        LuaValue::Integer(n) => Ok(serde_json::json!(*n)),
        LuaValue::Number(n) => Ok(serde_json::json!(*n)),
        LuaValue::String(s) => {
            Ok(serde_json::Value::String(s.to_str()?.to_string()))
        }
        LuaValue::Table(t) => {
            let len = t.raw_len();
            if len > 0 {
                let mut arr = Vec::new();
                for i in 1..=len {
                    let v: LuaValue = t.raw_get(i)?;
                    arr.push(lua_to_json(&v)?);
                }
                Ok(serde_json::Value::Array(arr))
            } else {
                let mut map = serde_json::Map::new();
                for pair in t.clone().pairs::<String, LuaValue>() {
                    let (k, v) = pair?;
                    map.insert(k, lua_to_json(&v)?);
                }
                Ok(serde_json::Value::Object(map))
            }
        }
        _ => Err(LuaError::external(anyhow::anyhow!(
            "unsupported Lua type: {}", val.type_name()
        ))),
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    let dir = wrappers_dir();
    info!("loading wrappers from {}", dir.display());

    if !dir.exists() {
        std::fs::create_dir_all(&dir)?;
        info!("created wrappers directory");
    }

    let pattern = dir.join("*.lua").to_string_lossy().to_string();
    let files: Vec<PathBuf> = glob::glob(&pattern)?
        .filter_map(|e| e.ok())
        .collect();

    if files.is_empty() {
        warn!("no wrapper scripts found in {}", dir.display());
        info!("create .lua files in {} to define ports", dir.display());
        return Ok(());
    }

    let mut handles: Vec<(String, PortHandle)> = Vec::new();

    for path in &files {
        match start_wrapper_port(path) {
            Ok(entry) => handles.push(entry),
            Err(e) => error!("failed to load {}: {e}", path.display()),
        }
    }

    if handles.is_empty() {
        anyhow::bail!("no wrappers loaded successfully");
    }

    info!("{} port(s) active, waiting for commands", handles.len());

    // Block forever — ports run in their own threads.
    // Ctrl+C or SIGTERM will clean up via Drop on PortHandle.
    std::thread::park();

    Ok(())
}
