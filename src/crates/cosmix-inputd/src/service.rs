//! The `input.*` Bus verb surface, dispatched over the shared [`Resolver`].
//!
//! This module needs no keyboard at all — an agent or `inputctl` can query and
//! rebind the keymap whether or not the evdev reader is running. Mutations
//! (`bind`/`unbind`/`mode`/`reload`) are gated to node-local callers for now
//! (the broker stamps `broker_origin: local`); remote mesh rebinds behind a
//! mesh-trust capability are a P3 refinement. Reads (`query`) are open.
//! Key and pointer injection are also mesh-reachable, with no node-local gate.

use std::path::PathBuf;
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

/// Where the keymap lives and whether this process may write it.
///
/// The CONFIGURED path is kept even when writing is disabled, so `input.reload`
/// can still read it (and re-enable writing once the operator has fixed it) and
/// replies can say truthfully why a rebind was not persisted.
#[derive(Default)]
pub struct Store {
    path: Option<PathBuf>,
    /// Where THIS process moved an unusable keymap document at startup.
    recovered_from: Option<PathBuf>,
    /// `Some("<path>: <reason>")` while the path must not be written.
    persist_disabled: Mutex<Option<String>>,
}

impl Store {
    pub fn new(
        path: Option<PathBuf>,
        recovered_from: Option<PathBuf>,
        persist_disabled: Option<String>,
    ) -> Self {
        Store {
            path,
            recovered_from,
            persist_disabled: Mutex::new(persist_disabled),
        }
    }

    fn disabled(&self) -> Option<String> {
        self.persist_disabled.lock().expect("store poisoned").clone()
    }
}

/// Dispatch one `input.*` command to `(rc, json body)`. `rc` 0 = ok, 10 = error.
/// After a mutation the keymap is persisted to the store's path (if set and
/// writable) so a rebind is remembered across restarts.
pub fn dispatch(
    resolver: &Shared,
    store: &Store,
    injector: &mut PointerInjector,
    cmd: &IncomingCommand,
) -> (u8, String) {
    match cmd.command.as_str() {
        verbs::QUERY => query(resolver, store),
        verbs::BIND => guard_local(cmd, || bind(resolver, store, &cmd.body)),
        verbs::UNBIND => guard_local(cmd, || unbind(resolver, store, &cmd.body)),
        verbs::MODE => guard_local(cmd, || mode(resolver, &cmd.body)),
        verbs::RELOAD => guard_local(cmd, || reload(resolver, store)),
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

/// Persist the current physical rows to the keymap file and annotate a
/// mutation's reply with the outcome. A persistence failure does not fail the
/// verb (rc stays 0 — the live table DID change), but the reply says so:
/// `persisted:false` plus `persist_disabled` (writing is off this run) or
/// `persist_error` (the save itself failed). A successful save, or no
/// configured path, leaves the reply unchanged.
fn persist(resolver: &Shared, store: &Store, reply: &mut Value) {
    let Some(path) = store.path.as_deref() else { return };
    if let Some(reason) = store.disabled() {
        eprintln!("cosmix-inputd: rebind not persisted: {reason}");
        reply["persisted"] = json!(false);
        reply["persist_disabled"] = json!(reason);
        return;
    }
    let rows = resolver
        .lock()
        .expect("resolver poisoned")
        .physical_rows()
        .to_vec();
    if let Err(error) = keymap_file::save(path, &rows) {
        let reason = format!("keymap save to {} failed: {error}", path.display());
        eprintln!("cosmix-inputd: {reason}");
        reply["persisted"] = json!(false);
        reply["persist_error"] = json!(reason);
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

/// `input.query`'s body. Two additive fields, each absent unless it applies:
/// `recovered_from` when this process moved an unusable keymap aside at
/// startup (it stays for the life of the process, even after a later
/// successful reload), and `persist_disabled` while rebinds cannot be written.
fn query(resolver: &Shared, store: &Store) -> (u8, String) {
    let resolver = resolver.lock().expect("resolver poisoned");
    let mut body = json!({
        "mode": resolver.mode(),
        "generation": resolver.generation(),
        "physical": resolver.physical_rows(),
    });
    if let Some(backup) = store.recovered_from.as_deref() {
        body["recovered_from"] = json!(backup.to_string_lossy());
    }
    if let Some(reason) = store.disabled() {
        body["persist_disabled"] = json!(reason);
    }
    (0, body.to_string())
}

fn bind(resolver: &Shared, store: &Store, body: &str) -> (u8, String) {
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
            let mut reply = json!({ "ok": true, "generation": generation });
            persist(resolver, store, &mut reply);
            (0, reply.to_string())
        }
        Err(err) => error(&format!("rebind refused: {err:?}")),
    }
}

fn unbind(resolver: &Shared, store: &Store, body: &str) -> (u8, String) {
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
            let mut reply = json!({ "ok": true, "generation": generation });
            persist(resolver, store, &mut reply);
            (0, reply.to_string())
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

fn reload(resolver: &Shared, store: &Store) -> (u8, String) {
    let Some(path) = store.path.as_deref() else {
        let generation = resolver.lock().expect("resolver poisoned").generation();
        return (
            0,
            json!({ "ok": true, "generation": generation, "note": "no keymap file configured" })
                .to_string(),
        );
    };
    match keymap_file::load(path) {
        Ok(loaded) => {
            let generation = resolver
                .lock()
                .expect("resolver poisoned")
                .replace_physical(loaded.rows);
            // The configured file is usable again (the operator fixed it, or
            // the file that reappeared at startup is valid): rebinds may be
            // written from here on.
            let was = store.persist_disabled.lock().expect("store poisoned").take();
            let mut reply =
                json!({ "ok": true, "generation": generation, "dropped": loaded.dropped });
            if let Some(reason) = was {
                eprintln!("cosmix-inputd: keymap persistence re-enabled (was: {reason})");
                reply["persist_reenabled"] = json!(reason);
            }
            (0, reply.to_string())
        }
        // Reload never rewrites or moves the file, so an unusable one is left
        // in place and the live keymap is unchanged. The reply names the real
        // state: configured but unusable, plus why writing is off if it is.
        Err(load_error) => {
            let mut reply = json!({
                "error": format!(
                    "keymap file {} could not be read: {}",
                    path.display(),
                    load_error.reason()
                ),
            });
            if let Some(reason) = store.disabled() {
                reply["persist_disabled"] = json!(reason);
            }
            (10, reply.to_string())
        }
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
                let (rc, reply) = dispatch(&resolver, &Store::default(), &mut injector, &cmd);
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
            let (rc, reply) = dispatch(&resolver(), &Store::default(), &mut injector, &cmd);
            assert_eq!(rc, 10);
            assert!(reply.contains("invalid pointer request"), "{reply}");
        }
        let mut cmd = command(verbs::POINTER_MOVE, json!({}), None);
        cmd.body = "{".into();
        let (rc, reply) = dispatch(&resolver(), &Store::default(), &mut injector, &cmd);
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
            let (rc, reply) = dispatch(&resolver(), &Store::default(), &mut injector, &cmd);
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
            let (rc, reply) = dispatch(&resolver(), &Store::default(), &mut injector, &cmd);
            assert_eq!(rc, 10);
            assert!(reply.contains("invalid key request"), "{reply}");
        }
        let mut cmd = command(verbs::KEY, json!({}), None);
        cmd.body = "{".into();
        let (rc, reply) = dispatch(&resolver(), &Store::default(), &mut injector, &cmd);
        assert_eq!(rc, 10);
        assert!(reply.contains("invalid key request"));
    }

    #[test]
    fn pointer_write_errors_are_reported_and_keymap_gate_is_preserved() {
        let mut injector =
            PointerInjector::with_test_file(File::options().write(true).open("/dev/full").unwrap());
        let cmd = command(verbs::POINTER_MOVE, json!({"dx": 1, "dy": 0}), Some("mesh"));
        let (rc, reply) = dispatch(&resolver(), &Store::default(), &mut injector, &cmd);
        assert_eq!(rc, 10);
        assert!(reply.contains("pointer injection failed"));
        let cmd = command(
            verbs::KEY,
            json!({"key": "F9", "action": "tap"}),
            Some("mesh"),
        );
        let (rc, reply) = dispatch(&resolver(), &Store::default(), &mut injector, &cmd);
        assert_eq!(rc, 10);
        assert!(reply.contains("key injection failed"));
        let cmd = command(verbs::MODE, json!({"mode": "transparent"}), Some("mesh"));
        let (rc, reply) = dispatch(&resolver(), &Store::default(), &mut injector, &cmd);
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
            let (rc, reply) = dispatch(&resolver, &Store::default(), &mut injector, &cmd);
            assert_eq!(rc, 10, "{bad:?}: {reply}");
            assert!(reply.contains("InvalidService"), "{reply}");
        }
        let cmd = command(verbs::BIND, row("desktop-vt1"), Some("local"));
        let (rc, reply) = dispatch(&resolver, &Store::default(), &mut injector, &cmd);
        assert_eq!(rc, 0, "{reply}");
        let (rc, reply) = dispatch(
            &resolver,
            &Store::default(),
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
    fn reload_reports_dropped_rows_and_keeps_the_good_ones() {
        let dir =
            std::env::temp_dir().join(format!("inputd-reload-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("keymap.json");
        std::fs::write(
            &path,
            r#"{"version":1,"physical":[
                {"stroke":{"code":108,"modifiers":{"right_ctrl":true}},
                 "action":"desktop.clipboard.menu","service":7},
                {"stroke":{"code":106,"modifiers":{"right_ctrl":true}},
                 "action":"desktop.workspace.next"}]}"#,
        )
        .unwrap();
        let mut injector = PointerInjector::default();
        let resolver = resolver();
        let cmd = command(verbs::RELOAD, json!({}), Some("local"));
        let (rc, reply) = dispatch(&resolver, &Store::new(Some(path.clone()), None, None), &mut injector, &cmd);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(rc, 0, "{reply}");
        let reply: Value = serde_json::from_str(&reply).unwrap();
        let dropped = reply["dropped"].as_array().expect("dropped list");
        assert_eq!(dropped.len(), 1, "{reply}");
        assert_eq!(dropped[0]["code"], 108);
        assert_eq!(dropped[0]["action"], "desktop.clipboard.menu");
        assert_eq!(dropped[0]["service"], 7);
        assert_eq!(dropped[0]["modifiers"]["right_ctrl"], true);
        let rows = resolver.lock().unwrap().physical_rows().to_vec();
        assert_eq!(rows.len(), 1, "only the good row is live");
        assert_eq!(rows[0].action.as_str(), "desktop.workspace.next");
    }

    fn run(resolver: &Shared, store: &Store, verb: &str, body: Value) -> (u8, Value) {
        let mut injector = PointerInjector::default();
        let cmd = command(verb, body, Some("local"));
        let (rc, reply) = dispatch(resolver, store, &mut injector, &cmd);
        (rc, serde_json::from_str(&reply).unwrap())
    }

    fn bind_row() -> Value {
        json!({"layer":"physical","stroke":{"code":63},"action":"user.f05"})
    }

    /// A fresh directory holding `keymap.json` with `text`.
    fn keymap_dir(tag: &str, text: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("inputd-store-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("keymap.json");
        std::fs::write(&path, text).unwrap();
        (dir, path)
    }

    #[test]
    fn query_reports_recovery_and_disabled_persistence_only_when_they_apply() {
        let resolver = resolver();
        let (rc, plain) = run(&resolver, &Store::default(), verbs::QUERY, json!({}));
        assert_eq!(rc, 0);
        assert!(plain.get("recovered_from").is_none(), "{plain}");
        assert!(plain.get("persist_disabled").is_none(), "{plain}");
        let backup = PathBuf::from("/var/lib/example/keymap.json.bad-20260924-101112");
        let store = Store::new(
            Some(PathBuf::from("/var/lib/example/keymap.json")),
            Some(backup.clone()),
            Some("/var/lib/example/keymap.json: file reappeared".to_string()),
        );
        let (rc, flagged) = run(&resolver, &store, verbs::QUERY, json!({}));
        assert_eq!(rc, 0);
        assert_eq!(flagged["recovered_from"], backup.to_str().unwrap());
        assert_eq!(
            flagged["persist_disabled"],
            "/var/lib/example/keymap.json: file reappeared"
        );
        // Additive: every other field is unchanged.
        for key in ["mode", "generation", "physical"] {
            assert_eq!(flagged[key], plain[key], "{key}");
        }
    }

    #[test]
    fn a_disabled_store_never_writes_and_says_so() {
        // main hands the startup outcome straight to the Store: the path stays
        // configured and persistence is off. bind/unbind still change the live
        // table (rc 0) but must not touch the file, and must say why.
        let (dir, path) = keymap_dir("disabled", "not json");
        let reason = format!("{}: unusable and could not be moved aside", path.display());
        let store = Store::new(Some(path.clone()), None, Some(reason.clone()));
        let resolver = resolver();
        let (rc, reply) = run(&resolver, &store, verbs::BIND, bind_row());
        assert_eq!(rc, 0, "{reply}");
        assert_eq!(reply["ok"], true);
        assert_eq!(reply["persisted"], false);
        assert_eq!(reply["persist_disabled"], reason.as_str());
        let (rc, reply) = run(&resolver, &store, verbs::UNBIND, json!({"code":63}));
        assert_eq!(rc, 0, "{reply}");
        assert_eq!(reply["persisted"], false);
        assert_eq!(reply["persist_disabled"], reason.as_str());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not json");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_enabled_store_writes_and_leaves_the_reply_shape_unchanged() {
        let (dir, path) = keymap_dir("enabled", "{}");
        let store = Store::new(Some(path.clone()), None, None);
        let resolver = resolver();
        let (rc, reply) = run(&resolver, &store, verbs::BIND, bind_row());
        assert_eq!(rc, 0, "{reply}");
        assert!(reply.get("persisted").is_none(), "{reply}");
        let written = keymap_file::load(&path).expect("bind persisted a loadable file");
        assert!(written.rows.iter().any(|r| r.action.as_str() == "user.f05"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_save_is_reported_as_persist_error() {
        // The path's parent is a regular FILE, so create_dir_all fails (as
        // root too): persistence is enabled but the save itself errors.
        let (dir, blocker) = keymap_dir("save-error", "x");
        let store = Store::new(Some(blocker.join("keymap.json")), None, None);
        let (rc, reply) = run(&resolver(), &store, verbs::BIND, bind_row());
        assert_eq!(rc, 0, "{reply}");
        assert_eq!(reply["persisted"], false);
        assert!(reply["persist_error"].as_str().unwrap().contains("save"), "{reply}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reload_reports_the_real_state_and_reenables_once_the_file_is_fixed() {
        let (dir, path) = keymap_dir("reenable", "not json");
        let reason = format!("{}: unusable and could not be moved aside", path.display());
        let store = Store::new(Some(path.clone()), None, Some(reason.clone()));
        let resolver = resolver();
        // Still broken: rc 10, names the configured file, the parse reason and
        // why writing is off — never "no keymap file configured".
        let (rc, reply) = run(&resolver, &store, verbs::RELOAD, json!({}));
        assert_eq!(rc, 10, "{reply}");
        let error = reply["error"].as_str().unwrap();
        assert!(error.contains(&path.display().to_string()), "{error}");
        assert!(error.contains("not JSON"), "{error}");
        assert_eq!(reply["persist_disabled"], reason.as_str());
        // The operator fixes the file: reload loads it and re-enables writes.
        std::fs::write(
            &path,
            r#"{"version":1,"physical":[{"stroke":{"code":106},"action":"desktop.workspace.next"}]}"#,
        )
        .unwrap();
        let (rc, reply) = run(&resolver, &store, verbs::RELOAD, json!({}));
        assert_eq!(rc, 0, "{reply}");
        assert_eq!(reply["persist_reenabled"], reason.as_str());
        let (_, query) = run(&resolver, &store, verbs::QUERY, json!({}));
        assert!(query.get("persist_disabled").is_none(), "{query}");
        // And a rebind now reaches the disk.
        let (rc, reply) = run(&resolver, &store, verbs::BIND, bind_row());
        assert_eq!(rc, 0);
        assert!(reply.get("persisted").is_none(), "{reply}");
        let written = keymap_file::load(&path).unwrap();
        assert_eq!(written.rows.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recovered_from_survives_a_later_successful_reload() {
        let (dir, path) = keymap_dir("survives", r#"{"version":1,"physical":[]}"#);
        let backup = dir.join("keymap.json.bad-20260924-101112");
        let store = Store::new(Some(path), Some(backup.clone()), None);
        let resolver = resolver();
        let (rc, _) = run(&resolver, &store, verbs::RELOAD, json!({}));
        assert_eq!(rc, 0);
        let (_, query) = run(&resolver, &store, verbs::QUERY, json!({}));
        assert_eq!(query["recovered_from"], backup.to_str().unwrap());
        let _ = std::fs::remove_dir_all(&dir);
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
