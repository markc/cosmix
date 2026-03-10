//! HTTP client module exposed to Lua as `cosmix.http.*`
//!
//! Provides GET/POST/PUT/PATCH/DELETE with headers, JSON body, and optional
//! SSL verification bypass (for self-signed certs like Proxmox).

use mlua::prelude::*;

/// Register `cosmix.http` table on the given cosmix Lua table.
pub fn register(lua: &Lua, cosmix: &LuaTable) -> Result<(), LuaError> {
    let http = lua.create_table()?;

    // All methods use the same generic handler
    for name in &["get", "post", "put", "patch", "delete"] {
        let method = name.to_uppercase();
        http.set(
            *name,
            lua.create_function(move |lua, (url, opts): (String, Option<LuaTable>)| {
                http_request(lua, &method, &url, &opts)
            })?,
        )?;
    }

    cosmix.set("http", http)?;
    Ok(())
}

fn http_request(
    lua: &Lua,
    method: &str,
    url: &str,
    opts: &Option<LuaTable>,
) -> LuaResult<LuaTable> {
    let ssl_verify = opts
        .as_ref()
        .and_then(|o| o.get::<bool>("ssl_verify").ok())
        .unwrap_or(true);

    // For SSL bypass (self-signed certs), delegate to curl
    if !ssl_verify {
        return do_curl_request(lua, method, url, opts);
    }

    let timeout_secs = opts
        .as_ref()
        .and_then(|o| o.get::<u64>("timeout").ok())
        .unwrap_or(30);

    let config = ureq::config::Config::builder()
        .https_only(false)
        .timeout_global(Some(std::time::Duration::from_secs(timeout_secs)))
        .build();

    let agent = ureq::Agent::new_with_config(config);

    // Extract headers
    let headers = extract_headers(opts)?;
    // Extract body
    let json_body = extract_json_body(opts)?;
    let text_body = if json_body.is_none() {
        opts.as_ref().and_then(|o| o.get::<String>("body").ok())
    } else {
        None
    };

    let mut response = match method {
        "GET" | "DELETE" => {
            let mut req = if method == "GET" {
                agent.get(url)
            } else {
                agent.delete(url)
            };
            for (k, v) in &headers {
                req = req.header(k, v);
            }
            req.call().map_err(LuaError::external)?
        }
        _ => {
            let mut req = match method {
                "POST" => agent.post(url),
                "PUT" => agent.put(url),
                "PATCH" => agent.patch(url),
                _ => unreachable!(),
            };
            for (k, v) in &headers {
                req = req.header(k, v);
            }
            if let Some(json_val) = &json_body {
                req.send_json(json_val).map_err(LuaError::external)?
            } else if let Some(body) = &text_body {
                req.header("Content-Type", "text/plain")
                    .send(body.as_bytes())
                    .map_err(LuaError::external)?
            } else {
                req.send_empty().map_err(LuaError::external)?
            }
        }
    };

    let result = lua.create_table()?;
    result.set("status", response.status().as_u16())?;

    let body = response
        .body_mut()
        .read_to_string()
        .map_err(LuaError::external)?;
    result.set("body", body)?;

    Ok(result)
}

fn extract_headers(opts: &Option<LuaTable>) -> LuaResult<Vec<(String, String)>> {
    let mut result = Vec::new();
    if let Some(o) = opts {
        if let Ok(headers) = o.get::<LuaTable>("headers") {
            for pair in headers.pairs::<String, String>() {
                if let Ok((k, v)) = pair {
                    result.push((k, v));
                }
            }
        }
    }
    Ok(result)
}

fn extract_json_body(opts: &Option<LuaTable>) -> LuaResult<Option<serde_json::Value>> {
    if let Some(o) = opts {
        if let Ok(json_table) = o.get::<LuaValue>("json") {
            if !matches!(json_table, LuaValue::Nil) {
                let json_val = crate::lua::lua_value_to_json(&json_table)?;
                return Ok(Some(json_val));
            }
        }
    }
    Ok(None)
}

/// Fallback to curl for requests needing SSL verification bypass.
fn do_curl_request(
    lua: &Lua,
    method: &str,
    url: &str,
    opts: &Option<LuaTable>,
) -> LuaResult<LuaTable> {
    let mut cmd = std::process::Command::new("curl");
    cmd.args(["-sk", "-X", method, "-w", "\n%{http_code}", url]);

    if let Some(o) = opts {
        // Headers
        if let Ok(headers) = o.get::<LuaTable>("headers") {
            for pair in headers.pairs::<String, String>() {
                if let Ok((k, v)) = pair {
                    cmd.args(["-H", &format!("{k}: {v}")]);
                }
            }
        }

        // JSON body
        if let Ok(json_table) = o.get::<LuaValue>("json") {
            if !matches!(json_table, LuaValue::Nil) {
                let json_val = crate::lua::lua_value_to_json(&json_table)?;
                let json_str =
                    serde_json::to_string(&json_val).map_err(LuaError::external)?;
                cmd.args(["-H", "Content-Type: application/json", "-d", &json_str]);
            }
        } else if let Ok(body) = o.get::<String>("body") {
            cmd.args(["-d", &body]);
        }

        if let Ok(timeout) = o.get::<u64>("timeout") {
            cmd.args(["--max-time", &timeout.to_string()]);
        }
    }

    let output = cmd
        .stderr(std::process::Stdio::null())
        .output()
        .map_err(LuaError::external)?;

    let full = String::from_utf8_lossy(&output.stdout);
    let (body, status_str) = match full.rfind('\n') {
        Some(pos) => (&full[..pos], &full[pos + 1..]),
        None => ("", full.as_ref()),
    };

    let status: u16 = status_str.trim().parse().unwrap_or(0);

    let result = lua.create_table()?;
    result.set("status", status)?;
    result.set("body", body.to_string())?;

    Ok(result)
}
