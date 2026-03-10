//! SQLite module exposed to Lua as `cosmix.db.*`
//!
//! Provides database open/query/exec with automatic parameter binding.

use mlua::prelude::*;
use rusqlite::types::Value as SqlValue;
use std::sync::{Arc, Mutex};

fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    path.to_string()
}

/// Register `cosmix.db` table on the given cosmix Lua table.
pub fn register(lua: &Lua, cosmix: &LuaTable) -> Result<(), LuaError> {
    let db = lua.create_table()?;

    // cosmix.db.open(path) -> db handle
    db.set(
        "open",
        lua.create_function(|lua, path: String| {
            let expanded = expand_tilde(&path);
            let conn =
                rusqlite::Connection::open(&expanded).map_err(LuaError::external)?;
            let handle = Arc::new(Mutex::new(Some(conn)));

            let tbl = lua.create_table()?;

            // db:query(sql, params?) -> array of row tables
            let h = handle.clone();
            tbl.set(
                "query",
                lua.create_function(
                    move |lua, (_self, sql, params): (LuaTable, String, Option<LuaTable>)| {
                        let guard = h.lock().unwrap();
                        let conn = guard
                            .as_ref()
                            .ok_or_else(|| LuaError::RuntimeError("database closed".into()))?;

                        let bound = bind_params(&params)?;
                        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                            bound.iter().map(|v| v as &dyn rusqlite::types::ToSql).collect();

                        let mut stmt = conn.prepare(&sql).map_err(LuaError::external)?;
                        let columns: Vec<String> = stmt
                            .column_names()
                            .iter()
                            .map(|s| s.to_string())
                            .collect();

                        let rows = stmt
                            .query_map(param_refs.as_slice(), |row| {
                                let mut vals = Vec::new();
                                for i in 0..columns.len() {
                                    vals.push(row.get::<_, SqlValue>(i)?);
                                }
                                Ok(vals)
                            })
                            .map_err(LuaError::external)?;

                        let result = lua.create_table()?;
                        let mut idx = 1;
                        for row in rows {
                            let vals = row.map_err(LuaError::external)?;
                            let row_tbl = lua.create_table()?;
                            for (i, val) in vals.iter().enumerate() {
                                let lua_val = sql_to_lua(lua, val)?;
                                row_tbl.set(columns[i].as_str(), lua_val)?;
                            }
                            result.set(idx, row_tbl)?;
                            idx += 1;
                        }
                        Ok(result)
                    },
                )?,
            )?;

            // db:exec(sql, params?) -> changes count
            let h = handle.clone();
            tbl.set(
                "exec",
                lua.create_function(
                    move |_, (_self, sql, params): (LuaTable, String, Option<LuaTable>)| {
                        let guard = h.lock().unwrap();
                        let conn = guard
                            .as_ref()
                            .ok_or_else(|| LuaError::RuntimeError("database closed".into()))?;

                        let bound = bind_params(&params)?;
                        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                            bound.iter().map(|v| v as &dyn rusqlite::types::ToSql).collect();

                        let changes = conn
                            .execute(&sql, param_refs.as_slice())
                            .map_err(LuaError::external)?;
                        Ok(changes as u64)
                    },
                )?,
            )?;

            // db:close()
            let h = handle.clone();
            tbl.set(
                "close",
                lua.create_function(move |_, _self: LuaTable| {
                    let mut guard = h.lock().unwrap();
                    *guard = None;
                    Ok(())
                })?,
            )?;

            Ok(tbl)
        })?,
    )?;

    cosmix.set("db", db)?;
    Ok(())
}

fn bind_params(params: &Option<LuaTable>) -> LuaResult<Vec<SqlValue>> {
    let Some(tbl) = params else {
        return Ok(vec![]);
    };
    let mut result = Vec::new();
    let len = tbl.raw_len();
    for i in 1..=len {
        let val: LuaValue = tbl.raw_get(i)?;
        result.push(lua_to_sql(&val)?);
    }
    Ok(result)
}

fn lua_to_sql(val: &LuaValue) -> LuaResult<SqlValue> {
    match val {
        LuaValue::Nil => Ok(SqlValue::Null),
        LuaValue::Boolean(b) => Ok(SqlValue::Integer(if *b { 1 } else { 0 })),
        LuaValue::Integer(n) => Ok(SqlValue::Integer(*n as i64)),
        LuaValue::Number(n) => Ok(SqlValue::Real(*n)),
        LuaValue::String(s) => Ok(SqlValue::Text(s.to_str().map_err(LuaError::external)?.to_string())),
        _ => Err(LuaError::RuntimeError(format!(
            "unsupported SQL parameter type: {}",
            val.type_name()
        ))),
    }
}

fn sql_to_lua<'lua>(lua: &'lua Lua, val: &SqlValue) -> LuaResult<LuaValue> {
    match val {
        SqlValue::Null => Ok(LuaValue::Nil),
        SqlValue::Integer(n) => Ok(LuaValue::Integer(*n as i64)),
        SqlValue::Real(n) => Ok(LuaValue::Number(*n)),
        SqlValue::Text(s) => Ok(LuaValue::String(lua.create_string(s)?)),
        SqlValue::Blob(b) => Ok(LuaValue::String(lua.create_string(b)?)),
    }
}
