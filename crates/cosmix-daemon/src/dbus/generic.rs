use anyhow::{Context, Result};
use zbus::Connection;
use zbus::zvariant::{self, OwnedValue, Value};

fn json_to_zvariant(val: &serde_json::Value) -> Value<'static> {
    match val {
        serde_json::Value::Null => Value::new(""),
        serde_json::Value::Bool(b) => Value::new(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::new(i)
            } else if let Some(f) = n.as_f64() {
                Value::new(f)
            } else {
                Value::new(0i64)
            }
        }
        serde_json::Value::String(s) => Value::new(s.clone()),
        serde_json::Value::Array(arr) => {
            // Check for type-hint object: {"type": "u32", "value": 42}
            // (handled at object level below)
            let items: Vec<Value<'static>> = arr.iter().map(json_to_zvariant).collect();
            Value::new(items)
        }
        serde_json::Value::Object(obj) => {
            // Type hint: {"type": "u32", "value": 42}
            if let (Some(serde_json::Value::String(ty)), Some(val)) = (obj.get("type"), obj.get("value")) {
                return match ty.as_str() {
                    "u8" => Value::new(val.as_u64().unwrap_or(0) as u8),
                    "i16" => Value::new(val.as_i64().unwrap_or(0) as i16),
                    "u16" => Value::new(val.as_u64().unwrap_or(0) as u16),
                    "i32" => Value::new(val.as_i64().unwrap_or(0) as i32),
                    "u32" => Value::new(val.as_u64().unwrap_or(0) as u32),
                    "i64" => Value::new(val.as_i64().unwrap_or(0)),
                    "u64" => Value::new(val.as_u64().unwrap_or(0)),
                    "f64" => Value::new(val.as_f64().unwrap_or(0.0)),
                    "bool" => Value::new(val.as_bool().unwrap_or(false)),
                    "string" => Value::new(val.as_str().unwrap_or("").to_string()),
                    _ => json_to_zvariant(val),
                };
            }
            // Generic dict
            let items: Vec<(String, Value<'static>)> = obj.iter()
                .map(|(k, v)| (k.clone(), json_to_zvariant(v)))
                .collect();
            Value::new(items)
        }
    }
}

fn owned_to_json(val: &OwnedValue) -> serde_json::Value {
    // Use serde serialization since OwnedValue may come from a different zvariant version
    match serde_json::to_value(val) {
        Ok(v) => v,
        Err(_) => serde_json::json!(format!("{val:?}")),
    }
}

pub async fn dbus_call(
    service: &str,
    path: &str,
    interface: &str,
    method: &str,
    args: Option<&[serde_json::Value]>,
    system: bool,
) -> Result<serde_json::Value> {
    let conn = if system {
        Connection::system().await?
    } else {
        Connection::session().await?
    };

    let reply = if let Some(args) = args {
        if args.is_empty() {
            conn.call_method(Some(service), path, Some(interface), method, &()).await?
        } else if args.len() == 1 {
            let val = json_to_zvariant(&args[0]);
            conn.call_method(Some(service), path, Some(interface), method, &val).await?
        } else {
            // Build a Structure from the args
            let mut builder = zvariant::StructureBuilder::new();
            for arg in args {
                builder = builder.add_field(json_to_zvariant(arg));
            }
            let body = builder.build()
                .map_err(|e| anyhow::anyhow!("Failed to build D-Bus structure: {e}"))?;
            conn.call_method(Some(service), path, Some(interface), method, &body).await?
        }
    } else {
        conn.call_method(Some(service), path, Some(interface), method, &()).await?
    };

    let body = reply.body();
    let sig = body.signature();
    let sig_str = format!("{sig}");

    if sig_str.is_empty() {
        return Ok(serde_json::Value::Null);
    }

    // Try to deserialize as OwnedValue
    match body.deserialize::<OwnedValue>() {
        Ok(val) => Ok(owned_to_json(&val)),
        Err(_) => {
            // Try as structure of multiple return values
            match body.deserialize::<Vec<OwnedValue>>() {
                Ok(vals) => {
                    let items: Vec<serde_json::Value> = vals.iter().map(owned_to_json).collect();
                    Ok(serde_json::Value::Array(items))
                }
                Err(e) => Err(anyhow::anyhow!("Failed to deserialize D-Bus response (sig={sig_str}): {e}")),
            }
        }
    }
}

pub async fn dbus_introspect(service: &str, path: &str, system: bool) -> Result<String> {
    let conn = if system {
        Connection::system().await?
    } else {
        Connection::session().await?
    };

    let reply = conn.call_method(
        Some(service),
        path,
        Some("org.freedesktop.DBus.Introspectable"),
        "Introspect",
        &(),
    ).await.context("Introspection call failed")?;

    let xml: String = reply.body().deserialize()?;
    Ok(xml)
}
