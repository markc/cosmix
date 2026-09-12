//! The `input.*` Bus verb surface, dispatched over the shared [`Resolver`].
//!
//! This module needs no keyboard at all — an agent or `inputctl` can query and
//! rebind the keymap whether or not the evdev reader is running. Mutations
//! (`bind`/`unbind`/`mode`/`reload`) are gated to node-local callers for now
//! (the broker stamps `broker_origin: local`); remote mesh rebinds behind a
//! mesh-trust capability are a P3 refinement. Reads (`query`) are open.

use std::sync::{Arc, Mutex};

use cosmix_client::IncomingCommand;
use cosmix_input_core::Resolver;
use cosmix_input_schema::{BindingRow, InputMode, PhysicalStroke, verbs};
use serde_json::{Value, json};

/// The resolver shared between the Bus service and the (optional) evdev reader.
pub type Shared = Arc<Mutex<Resolver>>;

/// Dispatch one `input.*` command to `(rc, json body)`. `rc` 0 = ok, 10 = error.
pub fn dispatch(resolver: &Shared, cmd: &IncomingCommand) -> (u8, String) {
    match cmd.command.as_str() {
        verbs::QUERY => query(resolver),
        verbs::BIND => guard_local(cmd, || bind(resolver, &cmd.body)),
        verbs::UNBIND => guard_local(cmd, || unbind(resolver, &cmd.body)),
        verbs::MODE => guard_local(cmd, || mode(resolver, &cmd.body)),
        verbs::RELOAD => guard_local(cmd, || reload(resolver)),
        other => error(&format!("unknown input verb: {other}")),
    }
}

/// The broker stamps `broker_origin` from the source socket; a node-local
/// caller (loopback/same-node) is stamped `local`. Mutations require it.
fn guard_local(cmd: &IncomingCommand, apply: impl FnOnce() -> (u8, String)) -> (u8, String) {
    let local = cmd
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("broker_origin"))
        .map(|(_, value)| value == "local")
        .unwrap_or(false);
    if local {
        apply()
    } else {
        error("input mutations require a node-local caller (remote rebind is gated, P3)")
    }
}

fn query(resolver: &Shared) -> (u8, String) {
    let resolver = resolver.lock().expect("resolver poisoned");
    let body = json!({
        "mode": resolver.mode(),
        "generation": resolver.generation(),
        "physical": resolver.physical_rows(),
    });
    (0, body.to_string())
}

fn bind(resolver: &Shared, body: &str) -> (u8, String) {
    let row: BindingRow = match serde_json::from_str(body) {
        Ok(row) => row,
        Err(err) => return error(&format!("invalid binding row: {err}")),
    };
    let binding = match row {
        BindingRow::Physical(binding) => binding,
        BindingRow::Semantic(_) => {
            return error("semantic (keysym) rows are not yet resolved; use a physical row");
        }
    };
    let mut resolver = resolver.lock().expect("resolver poisoned");
    match resolver.bind_physical(binding) {
        Ok(generation) => (0, json!({ "ok": true, "generation": generation }).to_string()),
        Err(err) => error(&format!("rebind refused: {err:?}")),
    }
}

fn unbind(resolver: &Shared, body: &str) -> (u8, String) {
    let stroke: PhysicalStroke = match serde_json::from_str(body) {
        Ok(stroke) => stroke,
        Err(err) => return error(&format!("invalid stroke: {err}")),
    };
    let mut resolver = resolver.lock().expect("resolver poisoned");
    match resolver.unbind_physical(&stroke) {
        Some(generation) => (0, json!({ "ok": true, "generation": generation }).to_string()),
        None => (0, json!({ "ok": true, "generation": resolver.generation(), "removed": false }).to_string()),
    }
}

fn mode(resolver: &Shared, body: &str) -> (u8, String) {
    // `{"mode":"transparent"|"normal"}`, or `{}`/empty to toggle.
    let requested = if body.trim().is_empty() {
        None
    } else {
        match serde_json::from_str::<Value>(body).ok().and_then(|v| {
            v.get("mode")
                .and_then(Value::as_str)
                .map(str::to_ascii_lowercase)
        }) {
            Some(m) if m == "transparent" => Some(InputMode::Transparent),
            Some(m) if m == "normal" => Some(InputMode::Normal),
            Some(other) => return error(&format!("unknown mode: {other}")),
            None => None,
        }
    };
    let mut resolver = resolver.lock().expect("resolver poisoned");
    let next = requested.unwrap_or(match resolver.mode() {
        InputMode::Normal => InputMode::Transparent,
        InputMode::Transparent => InputMode::Normal,
    });
    let generation = resolver.set_mode(next);
    (0, json!({ "mode": next, "generation": generation }).to_string())
}

fn reload(resolver: &Shared) -> (u8, String) {
    // Write-through + file reload land with the .mix persistence (P3). For now a
    // reload is a no-op ack over the in-memory keymap so the verb contract is
    // stable for callers.
    let resolver = resolver.lock().expect("resolver poisoned");
    (
        0,
        json!({ "ok": true, "generation": resolver.generation(), "note": "in-memory keymap; file persistence is P3" })
            .to_string(),
    )
}

fn error(message: &str) -> (u8, String) {
    (10, json!({ "error": message }).to_string())
}
