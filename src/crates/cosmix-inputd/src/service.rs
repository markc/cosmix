//! The `input.*` Bus verb surface, dispatched over the shared [`Resolver`].
//!
//! This module needs no keyboard at all — an agent or `inputctl` can query and
//! rebind the keymap whether or not the evdev reader is running. Mutations
//! (`bind`/`unbind`/`mode`/`reload`) are gated to node-local callers for now
//! (the broker stamps `broker_origin: local`); remote mesh rebinds behind a
//! mesh-trust capability are a P3 refinement. Reads (`query`) are open.
//! Key and pointer injection are also mesh-reachable, with no node-local gate.

use std::path::Path;
use std::sync::{Arc, Mutex};

use cosmix_client::IncomingCommand;
use cosmix_input_core::Resolver;
use cosmix_input_schema::{BindingRow, InputMode, KeyInjection, PhysicalStroke, verbs};
use serde_json::{Value, json};

use crate::keymap_file;
use crate::reader::{PointerInjection, PointerInjector};

pub fn verb_manifest() -> Vec<cosmix_bus::VerbDescriptor> {
    use cosmix_bus::VerbDescriptor;
    vec![
        VerbDescriptor::new("HELP", &[], "List all commands this service accepts", true),
        VerbDescriptor::new("input.query", &[], "Read the keymap and input mode", true),
        VerbDescriptor::new(
            "input.bind",
            &["body"],
            "Bind a physical stroke from a BindingRow JSON body",
            false,
        ),
        VerbDescriptor::new(
            "input.unbind",
            &["body"],
            "Unbind a PhysicalStroke JSON body",
            false,
        ),
        VerbDescriptor::new(
            "input.mode",
            &["mode"],
            "Set or toggle the input mode",
            false,
        ),
        VerbDescriptor::new("input.reload", &[], "Reload the keymap file", false),
        VerbDescriptor::new(
            verbs::KEY,
            &["key", "action"],
            "Inject a key press/release/tap by name or decimal Linux code",
            false,
        ),
        VerbDescriptor::new(
            verbs::POINTER_MOVE,
            &["dx", "dy"],
            "Inject relative pointer motion (signed i32 deltas)",
            false,
        ),
        VerbDescriptor::new(
            verbs::POINTER_BUTTON,
            &["button", "action"],
            "Inject left/right/middle button press/release/click",
            false,
        ),
        VerbDescriptor::new(
            verbs::POINTER_SCROLL,
            &["dy", "dx"],
            "Inject wheel steps (signed i32 dy, optional dx)",
            false,
        ),
    ]
}

/// The resolver shared between the Bus service and the (optional) evdev reader.
pub type Shared = Arc<Mutex<Resolver>>;

/// Dispatch one `input.*` command to `(rc, json body)`. `rc` 0 = ok, 10 = error.
/// After a mutation the keymap is persisted to `keymap_path` (if set) so a
/// rebind is remembered across restarts.
pub fn dispatch(
    resolver: &Shared,
    keymap_path: Option<&Path>,
    injector: &mut PointerInjector,
    cmd: &IncomingCommand,
) -> (u8, String) {
    match cmd.command.as_str() {
        verbs::QUERY => query(resolver),
        verbs::BIND => guard_local(cmd, || bind(resolver, keymap_path, &cmd.body)),
        verbs::UNBIND => guard_local(cmd, || unbind(resolver, keymap_path, &cmd.body)),
        verbs::MODE => guard_local(cmd, || mode(resolver, &cmd.body)),
        verbs::RELOAD => guard_local(cmd, || reload(resolver, keymap_path)),
        verbs::KEY => key(injector, &cmd.body),
        verbs::POINTER_MOVE | verbs::POINTER_BUTTON | verbs::POINTER_SCROLL => {
            pointer(injector, &cmd.command, &cmd.body)
        }
        other => error(&format!("unknown input verb: {other}")),
    }
}

fn key(injector: &mut PointerInjector, body: &str) -> (u8, String) {
    let request: KeyInjection = match serde_json::from_str(body) {
        Ok(request) => request,
        Err(err) => return error(&format!("invalid key request: {err}")),
    };
    let code = match crate::reader::key_code(&request.key) {
        Ok(code) => code,
        Err(err) => return error(&err),
    };
    match injector.inject(PointerInjection::Key {
        code,
        action: request.action,
    }) {
        Ok(()) => (0, json!({ "ok": true }).to_string()),
        Err(err) => error(&format!("key injection failed: {err}")),
    }
}

fn pointer(injector: &mut PointerInjector, verb: &str, body: &str) -> (u8, String) {
    let request = match verb {
        verbs::POINTER_MOVE => serde_json::from_str(body).map(PointerInjection::Move),
        verbs::POINTER_BUTTON => serde_json::from_str(body).map(PointerInjection::Button),
        verbs::POINTER_SCROLL => serde_json::from_str(body).map(PointerInjection::Scroll),
        _ => unreachable!("only pointer verbs reach this handler"),
    };
    let request = match request {
        Ok(request) => request,
        Err(err) => return error(&format!("invalid pointer request: {err}")),
    };
    match injector.inject(request) {
        Ok(()) => (0, json!({ "ok": true }).to_string()),
        Err(err) => error(&format!("pointer injection failed: {err}")),
    }
}

/// Persist the current physical rows to the keymap file, logging on failure. A
/// save failure does not fail the verb — the in-memory rebind still took.
fn persist(resolver: &Shared, keymap_path: Option<&Path>) {
    let Some(path) = keymap_path else { return };
    let rows = resolver
        .lock()
        .expect("resolver poisoned")
        .physical_rows()
        .to_vec();
    if let Err(error) = keymap_file::save(path, &rows) {
        eprintln!(
            "cosmix-inputd: keymap save to {} failed: {error}",
            path.display()
        );
    }
}

/// The broker stamps `broker_origin` from the source socket; a node-local
/// caller (loopback/same-node) is stamped `local`. Keymap mutations require it.
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
    let result = resolver
        .lock()
        .expect("resolver poisoned")
        .bind_physical(binding);
    match result {
        Ok(generation) => {
            persist(resolver, keymap_path);
            (
                0,
                json!({ "ok": true, "generation": generation }).to_string(),
            )
        }
        Err(err) => error(&format!("rebind refused: {err:?}")),
    }
}

fn unbind(resolver: &Shared, keymap_path: Option<&Path>, body: &str) -> (u8, String) {
    let stroke: PhysicalStroke = match serde_json::from_str(body) {
        Ok(stroke) => stroke,
        Err(err) => return error(&format!("invalid stroke: {err}")),
    };
    let removed = resolver
        .lock()
        .expect("resolver poisoned")
        .unbind_physical(&stroke);
    match removed {
        Some(generation) => {
            persist(resolver, keymap_path);
            (
                0,
                json!({ "ok": true, "generation": generation }).to_string(),
            )
        }
        None => {
            let generation = resolver.lock().expect("resolver poisoned").generation();
            (
                0,
                json!({ "ok": true, "generation": generation, "removed": false }).to_string(),
            )
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
    (
        0,
        json!({ "mode": next, "generation": generation }).to_string(),
    )
}

fn reload(resolver: &Shared, keymap_path: Option<&Path>) -> (u8, String) {
    let Some(path) = keymap_path else {
        let generation = resolver.lock().expect("resolver poisoned").generation();
        return (
            0,
            json!({ "ok": true, "generation": generation, "note": "no keymap file configured" })
                .to_string(),
        );
    };
    match keymap_file::load(path) {
        Some(rows) => {
            let generation = resolver
                .lock()
                .expect("resolver poisoned")
                .replace_physical(rows);
            (
                0,
                json!({ "ok": true, "generation": generation }).to_string(),
            )
        }
        None => error(&format!("keymap file {} could not be read", path.display())),
    }
}

fn error(message: &str) -> (u8, String) {
    (10, json!({ "error": message }).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Read;
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    fn command(verb: &str, body: Value, origin: Option<&str>) -> IncomingCommand {
        IncomingCommand {
            from: "agent@beta".into(),
            command: verb.into(),
            id: Some("pointer-test".into()),
            args: json!({}),
            body: body.to_string(),
            headers: origin
                .map(|value| ("broker_origin".into(), value.into()))
                .into_iter()
                .collect(),
        }
    }

    fn resolver() -> Shared {
        Arc::new(Mutex::new(Resolver::new(
            cosmix_input_core::default_keymap(),
        )))
    }

    #[test]
    fn mesh_injection_dispatch_emits_exact_frames_without_a_reader() {
        let (writer, mut reader) = UnixStream::pair().unwrap();
        reader
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut injector = PointerInjector::with_test_file(File::from(OwnedFd::from(writer)));
        let resolver = resolver();
        let mut cases = vec![
            (
                verbs::POINTER_MOVE,
                json!({"dx": -17, "dy": 23}),
                vec![(2, 0, -17), (2, 1, 23), (0, 0, 0)],
            ),
            (
                verbs::POINTER_MOVE,
                json!({"dx": i32::MIN, "dy": i32::MAX}),
                vec![(2, 0, i32::MIN), (2, 1, i32::MAX), (0, 0, 0)],
            ),
            (
                verbs::POINTER_SCROLL,
                json!({"dy": -2}),
                vec![(2, 8, -2), (0, 0, 0)],
            ),
            (
                verbs::POINTER_SCROLL,
                json!({"dy": 3, "dx": -1}),
                vec![(2, 8, 3), (2, 6, -1), (0, 0, 0)],
            ),
            (
                verbs::POINTER_SCROLL,
                json!({"dy": 0, "dx": 0}),
                vec![(2, 8, 0), (2, 6, 0), (0, 0, 0)],
            ),
        ];
        for (button, code) in [("left", 0x110), ("right", 0x111), ("middle", 0x112)] {
            for (action, events) in [
                ("press", vec![(1, code, 1), (0, 0, 0)]),
                ("release", vec![(1, code, 0), (0, 0, 0)]),
                (
                    "click",
                    vec![(1, code, 1), (0, 0, 0), (1, code, 0), (0, 0, 0)],
                ),
            ] {
                cases.push((
                    verbs::POINTER_BUTTON,
                    json!({"button": button, "action": action}),
                    events,
                ));
            }
        }
        for (key, code) in [("F9", 67), ("f9", 67), ("67", 67), ("0", 0), ("767", 767)] {
            for (action, events) in [
                ("press", vec![(1, code, 1), (0, 0, 0)]),
                ("release", vec![(1, code, 0), (0, 0, 0)]),
                (
                    "tap",
                    vec![(1, code, 1), (0, 0, 0), (1, code, 0), (0, 0, 0)],
                ),
            ] {
                cases.push((verbs::KEY, json!({"key": key, "action": action}), events));
            }
        }
        // Reuse one injector across calls, for mesh, local and unstamped callers.
        for origin in [Some("mesh"), Some("local"), None] {
            for (verb, body, expected) in &cases {
                let cmd = command(verb, body.clone(), origin);
                let (rc, reply) = dispatch(&resolver, None, &mut injector, &cmd);
                assert_eq!(rc, 0, "{reply}");
                assert_eq!(
                    serde_json::from_str::<Value>(&reply).unwrap(),
                    json!({"ok": true})
                );
                for &(kind, code, value) in expected {
                    let mut event = [0; 24];
                    reader.read_exact(&mut event).unwrap();
                    assert_eq!(&event[..16], &[0; 16]);
                    assert_eq!(u16::from_ne_bytes(event[16..18].try_into().unwrap()), kind);
                    assert_eq!(u16::from_ne_bytes(event[18..20].try_into().unwrap()), code);
                    assert_eq!(i32::from_ne_bytes(event[20..24].try_into().unwrap()), value);
                }
            }
        }
        // No extra frames, and dropping the injector closes the owned fd.
        drop(injector);
        assert_eq!(reader.read(&mut [0; 24]).unwrap(), 0);
    }

    #[test]
    fn invalid_pointer_requests_are_rejected_before_device_access() {
        let mut injector = PointerInjector::default();
        for (verb, body) in [
            (verbs::POINTER_MOVE, json!({"dx": 1})),
            (verbs::POINTER_MOVE, json!({"dx": 2147483648_i64, "dy": 0})),
            (verbs::POINTER_MOVE, json!({"dx": -2147483649_i64, "dy": 0})),
            (verbs::POINTER_MOVE, json!({"dx": 1.5, "dy": 0})),
            (verbs::POINTER_MOVE, json!({"dx": "1", "dy": 0})),
            (verbs::POINTER_SCROLL, json!({"dx": 1})),
            (verbs::POINTER_SCROLL, json!({"dy": 0, "dx": false})),
            (
                verbs::POINTER_SCROLL,
                json!({"dy": 0, "dx": 2147483648_i64}),
            ),
            (
                verbs::POINTER_BUTTON,
                json!({"button": "side", "action": "click"}),
            ),
            (
                verbs::POINTER_BUTTON,
                json!({"button": "left", "action": "toggle"}),
            ),
            (verbs::POINTER_BUTTON, json!({"button": "left"})),
            (verbs::POINTER_BUTTON, json!(null)),
            (verbs::POINTER_MOVE, json!([])),
        ] {
            let cmd = command(verb, body, Some("mesh"));
            let (rc, reply) = dispatch(&resolver(), None, &mut injector, &cmd);
            assert_eq!(rc, 10);
            assert!(reply.contains("invalid pointer request"), "{reply}");
        }
        let mut cmd = command(verbs::POINTER_MOVE, json!({}), None);
        cmd.body = "{".into();
        let (rc, reply) = dispatch(&resolver(), None, &mut injector, &cmd);
        assert_eq!(rc, 10);
        assert!(reply.contains("invalid pointer request"));
    }

    #[test]
    fn invalid_key_requests_are_rejected_before_device_access() {
        let mut injector = PointerInjector::default();
        for key in [
            "NotAKey",
            "768",
            "65536",
            "999999999999999999999",
            "-1",
            "+67",
            "1.5",
            "",
        ] {
            let cmd = command(
                verbs::KEY,
                json!({"key": key, "action": "tap"}),
                Some("mesh"),
            );
            let (rc, reply) = dispatch(&resolver(), None, &mut injector, &cmd);
            assert_eq!(rc, 10);
            let reply: Value = serde_json::from_str(&reply).unwrap();
            let error = reply["error"].as_str().unwrap();
            assert!(error.contains(key), "{error}");
            assert!(!error.contains("injection failed"), "{error}");
        }
        for body in [
            json!({"key": "F9"}),
            json!({"action": "tap"}),
            json!({"key": 67, "action": "tap"}),
            json!({"key": "F9", "action": "click"}),
            json!({"key": "F9", "action": "TAP"}),
            json!(null),
            json!([]),
        ] {
            let cmd = command(verbs::KEY, body, Some("mesh"));
            let (rc, reply) = dispatch(&resolver(), None, &mut injector, &cmd);
            assert_eq!(rc, 10);
            assert!(reply.contains("invalid key request"), "{reply}");
        }
        let mut cmd = command(verbs::KEY, json!({}), None);
        cmd.body = "{".into();
        let (rc, reply) = dispatch(&resolver(), None, &mut injector, &cmd);
        assert_eq!(rc, 10);
        assert!(reply.contains("invalid key request"));
    }

    #[test]
    fn pointer_write_errors_are_reported_and_keymap_gate_is_preserved() {
        let mut injector =
            PointerInjector::with_test_file(File::options().write(true).open("/dev/full").unwrap());
        let cmd = command(verbs::POINTER_MOVE, json!({"dx": 1, "dy": 0}), Some("mesh"));
        let (rc, reply) = dispatch(&resolver(), None, &mut injector, &cmd);
        assert_eq!(rc, 10);
        assert!(reply.contains("pointer injection failed"));
        let cmd = command(
            verbs::KEY,
            json!({"key": "F9", "action": "tap"}),
            Some("mesh"),
        );
        let (rc, reply) = dispatch(&resolver(), None, &mut injector, &cmd);
        assert_eq!(rc, 10);
        assert!(reply.contains("key injection failed"));
        let cmd = command(verbs::MODE, json!({"mode": "transparent"}), Some("mesh"));
        let (rc, reply) = dispatch(&resolver(), None, &mut injector, &cmd);
        assert_eq!(rc, 10);
        assert!(reply.contains("node-local caller"));
    }

    #[test]
    fn bind_admits_a_service_target_and_query_shows_it() {
        let mut injector = PointerInjector::default();
        let resolver = resolver();
        let row = |service: &str| {
            json!({
                "layer": "physical",
                "stroke": {"code": 63, "modifiers": {}},
                "action": "desktop.clipboard.menu",
                "service": service,
            })
        };
        for bad in ["", "Desktop", "desk.vt1"] {
            let cmd = command(verbs::BIND, row(bad), Some("local"));
            let (rc, reply) = dispatch(&resolver, None, &mut injector, &cmd);
            assert_eq!(rc, 10, "{bad:?}: {reply}");
            assert!(reply.contains("InvalidService"), "{reply}");
        }
        let cmd = command(verbs::BIND, row("desktop-vt1"), Some("local"));
        let (rc, reply) = dispatch(&resolver, None, &mut injector, &cmd);
        assert_eq!(rc, 0, "{reply}");
        let (rc, reply) = dispatch(
            &resolver,
            None,
            &mut injector,
            &command(verbs::QUERY, json!({}), None),
        );
        assert_eq!(rc, 0);
        let reply: Value = serde_json::from_str(&reply).unwrap();
        let rows = reply["physical"].as_array().unwrap();
        let f5 = rows.iter().find(|r| r["stroke"]["code"] == 63).unwrap();
        assert_eq!(f5["service"], "desktop-vt1");
        assert_eq!(f5["action"], "desktop.clipboard.menu");
        // Rows without a target keep the old wire shape: no `service` key.
        let next = rows
            .iter()
            .find(|r| r["action"] == "desktop.workspace.next")
            .unwrap();
        assert!(next.get("service").is_none(), "{next}");
    }

    #[test]
    fn injection_manifest_lists_writable_verbs_and_arguments() {
        let manifest = serde_json::to_value(verb_manifest()).unwrap();
        for (verb, args) in [
            (verbs::KEY, json!(["key", "action"])),
            (verbs::POINTER_MOVE, json!(["dx", "dy"])),
            (verbs::POINTER_BUTTON, json!(["button", "action"])),
            (verbs::POINTER_SCROLL, json!(["dy", "dx"])),
        ] {
            let entry = manifest
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry["name"] == verb)
                .unwrap();
            assert_eq!(entry["args"], args);
            assert_eq!(entry["read_only"], false);
        }
    }
}
