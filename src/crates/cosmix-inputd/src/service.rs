//! The `input.*` Bus verb surface, dispatched over the shared [`Resolver`].
//!
//! This module needs no keyboard at all — an agent or `inputctl` can query and
//! rebind the keymap whether or not the evdev reader is running. Mutations
//! (`bind`/`unbind`/`mode`/`reload`) are gated to node-local callers for now
//! (the broker stamps `broker_origin: local`); remote mesh rebinds behind a
//! mesh-trust capability are a P3 refinement. Reads (`query`) are open.

use std::path::Path;
use std::sync::{Arc, Mutex};

use cosmix_client::IncomingCommand;
use cosmix_input_core::Resolver;
use cosmix_input_schema::{BindingRow, InputMode, PhysicalStroke, verbs};
use serde_json::{Value, json};

use crate::keymap_file;

/// The resolver shared between the Bus service and the (optional) evdev reader.
pub type Shared = Arc<Mutex<Resolver>>;

/// Dispatch one `input.*` command to `(rc, json body)`. `rc` 0 = ok, 10 = error.
/// After a mutation the keymap is persisted to `keymap_path` (if set) so a
/// rebind is remembered across restarts.
pub fn dispatch(
    resolver: &Shared,
    keymap_path: Option<&Path>,
    cmd: &IncomingCommand,
) -> (u8, String) {
    match cmd.command.as_str() {
        verbs::QUERY => query(resolver),
        verbs::BIND => guard_local(cmd, || bind(resolver, keymap_path, &cmd.body)),
        verbs::UNBIND => guard_local(cmd, || unbind(resolver, keymap_path, &cmd.body)),
        verbs::MODE => guard_local(cmd, || mode(resolver, &cmd.body)),
        verbs::RELOAD => guard_local(cmd, || reload(resolver, keymap_path)),
        other => error(&format!("unknown input verb: {other}")),
    }
}

/// Persist the current physical rows to the keymap file, logging on failure. A
/// save failure does not fail the verb — the in-memory rebind still took.
fn persist(resolver: &Shared, keymap_path: Option<&Path>) {
    let Some(path) = keymap_path else { return };
    let rows = resolver.lock().expect("resolver poisoned").physical_rows().to_vec();
    if let Err(error) = keymap_file::save(path, &rows) {
        eprintln!("cosmix-inputd: keymap save to {} failed: {error}", path.display());
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

fn bind(resolver: &Shared, keymap_path: Option<&Path>, body: &str) -> (u8, String) {
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
    let result = resolver.lock().expect("resolver poisoned").bind_physical(binding);
    match result {
        Ok(generation) => {
            persist(resolver, keymap_path);
            (0, json!({ "ok": true, "generation": generation }).to_string())
        }
        Err(err) => error(&format!("rebind refused: {err:?}")),
    }
}

fn unbind(resolver: &Shared, keymap_path: Option<&Path>, body: &str) -> (u8, String) {
    let stroke: PhysicalStroke = match serde_json::from_str(body) {
        Ok(stroke) => stroke,
        Err(err) => return error(&format!("invalid stroke: {err}")),
    };
    let removed = resolver.lock().expect("resolver poisoned").unbind_physical(&stroke);
    match removed {
        Some(generation) => {
            persist(resolver, keymap_path);
            (0, json!({ "ok": true, "generation": generation }).to_string())
        }
        None => {
            let generation = resolver.lock().expect("resolver poisoned").generation();
            (0, json!({ "ok": true, "generation": generation, "removed": false }).to_string())
        }
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

fn reload(resolver: &Shared, keymap_path: Option<&Path>) -> (u8, String) {
    let Some(path) = keymap_path else {
        let generation = resolver.lock().expect("resolver poisoned").generation();
        return (0, json!({ "ok": true, "generation": generation, "note": "no keymap file configured" }).to_string());
    };
    match keymap_file::load(path) {
        Some(rows) => {
            let generation = resolver.lock().expect("resolver poisoned").replace_physical(rows);
            (0, json!({ "ok": true, "generation": generation }).to_string())
        }
        None => error(&format!("keymap file {} could not be read", path.display())),
    }
}

fn error(message: &str) -> (u8, String) {
    (10, json!({ "error": message }).to_string())
}
