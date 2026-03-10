//! Formatting module exposed to Lua as `cosmix.fmt.*`
//!
//! Provides table rendering, coloured messages, and unit formatting.

use comfy_table::{ContentArrangement, Table};
use mlua::prelude::*;

/// Register `cosmix.fmt` table on the given cosmix Lua table.
pub fn register(lua: &Lua, cosmix: &LuaTable) -> Result<(), LuaError> {
    let fmt = lua.create_table()?;

    // cosmix.fmt.table(headers, rows)
    // headers: {"Name", "Type", "TTL"}
    // rows: {{name, type, ttl}, {name2, type2, ttl2}, ...}
    fmt.set(
        "table",
        lua.create_function(|_, (headers, rows): (LuaTable, LuaTable)| {
            let mut table = Table::new();
            table.set_content_arrangement(ContentArrangement::Dynamic);

            // Header row
            let mut hdr: Vec<String> = Vec::new();
            for i in 1..=headers.raw_len() {
                let val: String = headers.raw_get(i)?;
                hdr.push(val);
            }
            table.set_header(&hdr);

            // Data rows
            for i in 1..=rows.raw_len() {
                let row: LuaTable = rows.raw_get(i)?;
                let mut cells: Vec<String> = Vec::new();
                for j in 1..=row.raw_len() {
                    let val: LuaValue = row.raw_get(j)?;
                    cells.push(lua_val_to_string(&val));
                }
                table.add_row(cells);
            }

            println!("{table}");
            Ok(())
        })?,
    )?;

    // cosmix.fmt.details(pairs) — key-value display
    // pairs: {{"Status", "Running"}, {"Memory", "512MB"}}
    fmt.set(
        "details",
        lua.create_function(|_, pairs: LuaTable| {
            let mut max_key = 0usize;
            let mut items: Vec<(String, String)> = Vec::new();
            for i in 1..=pairs.raw_len() {
                let pair: LuaTable = pairs.raw_get(i)?;
                let key: String = pair.raw_get(1)?;
                let val: LuaValue = pair.raw_get(2)?;
                max_key = max_key.max(key.len());
                items.push((key, lua_val_to_string(&val)));
            }
            for (k, v) in &items {
                println!("  {k:>width$}: {v}", width = max_key);
            }
            Ok(())
        })?,
    )?;

    // cosmix.fmt.info(msg)
    fmt.set(
        "info",
        lua.create_function(|_, msg: String| {
            eprintln!("\x1b[34m  INFO\x1b[0m  {msg}");
            Ok(())
        })?,
    )?;

    // cosmix.fmt.error(msg)
    fmt.set(
        "error",
        lua.create_function(|_, msg: String| {
            eprintln!("\x1b[31m  ERROR\x1b[0m  {msg}");
            Ok(())
        })?,
    )?;

    // cosmix.fmt.success(msg)
    fmt.set(
        "success",
        lua.create_function(|_, msg: String| {
            eprintln!("\x1b[32m  SUCCESS\x1b[0m  {msg}");
            Ok(())
        })?,
    )?;

    // cosmix.fmt.warn(msg)
    fmt.set(
        "warn",
        lua.create_function(|_, msg: String| {
            eprintln!("\x1b[33m  WARN\x1b[0m  {msg}");
            Ok(())
        })?,
    )?;

    // cosmix.fmt.memory(bytes) -> "512 MB"
    fmt.set(
        "memory",
        lua.create_function(|_, bytes: f64| {
            let result = if bytes >= 1_073_741_824.0 {
                format!("{:.1} GB", bytes / 1_073_741_824.0)
            } else if bytes >= 1_048_576.0 {
                format!("{:.0} MB", bytes / 1_048_576.0)
            } else if bytes >= 1024.0 {
                format!("{:.0} KB", bytes / 1024.0)
            } else {
                format!("{:.0} B", bytes)
            };
            Ok(result)
        })?,
    )?;

    // cosmix.fmt.uptime(seconds) -> "2d 3h 15m"
    fmt.set(
        "uptime",
        lua.create_function(|_, secs: u64| {
            let days = secs / 86400;
            let hours = (secs % 86400) / 3600;
            let mins = (secs % 3600) / 60;
            let result = if days > 0 {
                format!("{days}d {hours}h {mins}m")
            } else if hours > 0 {
                format!("{hours}h {mins}m")
            } else {
                format!("{mins}m")
            };
            Ok(result)
        })?,
    )?;

    cosmix.set("fmt", fmt)?;
    Ok(())
}

fn lua_val_to_string(val: &LuaValue) -> String {
    match val {
        LuaValue::Nil => String::new(),
        LuaValue::Boolean(b) => b.to_string(),
        LuaValue::Integer(n) => n.to_string(),
        LuaValue::Number(n) => n.to_string(),
        LuaValue::String(s) => match s.to_str() {
            Ok(s) => s.to_string(),
            Err(_) => "???".to_string(),
        },
        _ => format!("<{}>", val.type_name()),
    }
}
