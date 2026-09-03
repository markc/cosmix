//! SPEC 18 WS4 — runtime-provided Ch07 L0+ conformance for
//! `mix --serve` citizens.
//!
//! A Mix citizen author writes only its domain `on` handlers. The
//! runtime injects, *pre-dispatch* and unconditionally, the verbs the
//! author does **not** write and **cannot** override (SPEC 18 DECIDED
//! §7-Q4 — *runtime wins*; an author `on HELP do` /
//! `on <svc>.props.get do` would let §9(d)/(e) conformance be silently
//! broken):
//!
//! - **L0** (Ch02 §3): `HELP`, `INFO`, `QUIT`.
//! - **L1** (Ch07 §2): `<svc>.props.{get,list,describe}` over a
//!   runtime-owned lifecycle property tree.
//!
//! The lifecycle tree (`lifecycle.started_at` / `uptime_s` / `mode` /
//! `health` / `props_level`) reuses the exact `cosmix_props` model and
//! `cosmix_props::bus::dispatch_props` encoder the indexd reference L1
//! daemon uses, so a Mix citizen's props surface is byte-consistent
//! with every other cosmix daemon. Finding #3 in the WS plan ("no
//! reusable Ch07-L0 helper") holds only for L0: `cosmix_props` is the
//! L1 helper; `HELP`/`INFO`/`QUIT` have no shared helper and are
//! provided here.
//!
//! Authors *extend* the data surfaced through `INFO`/props via their
//! own domain commands and (future) lifecycle contributions; they do
//! not replace these verbs. The struct is built once per process by
//! `run_serve` and installed via `Evaluator::set_serve_runtime`.

use std::time::Instant;

use cosmix_mix::evaluator::{ReservedOutcome, ServeRuntime};
use cosmix_props::{PropDescribe, PropPath, PropTree, PropType, PropValue, tree::build_snapshot};
use serde_json::{Value as Json, json};

/// The Mix serve-mode runtime surface (SPEC 18 WS4). Implements both
/// [`ServeRuntime`] (the pre-dispatch reserved-verb chokepoint the
/// evaluator consults) and [`PropTree`] (the lifecycle property model
/// `dispatch_props` reads), so one value answers all of L0+L1.
pub struct MixServeRuntime {
    /// Bus service name — the `<svc>` prefix for `<svc>.props.*` and
    /// the `INFO.name`.
    service_name: String,
    /// Monotonic process start, for the live `lifecycle.uptime_s` leaf
    /// (recomputed every `props.get`, never cached).
    started_at: Instant,
    /// Wall-clock start as an RFC 3339 string, for
    /// `lifecycle.started_at` (captured once; matches the indexd
    /// reference's chrono-formatted timestamp).
    started_wall: String,
    /// Precomputed `<svc>.props.` prefix (avoids per-request format!).
    props_prefix: String,
    /// Isolated handler faults recorded via
    /// [`ServeRuntime::record_handler_fault`] (0.63.0). Interior
    /// mutability because the runtime is shared as `Rc<dyn ServeRuntime>`;
    /// single-threaded by construction (the evaluator is `!Send`).
    handler_faults: std::cell::Cell<u64>,
    /// Summary of the most recent fault, for `lifecycle.last_fault`.
    last_fault: std::cell::RefCell<Option<String>>,
}

/// Operating-mode value for `lifecycle.mode`. A Phase-1 serve citizen
/// is always actively serving; the leaf exists so meta-subscribers can
/// branch on it once Phase 2 adds drain/paused modes.
const MODE_SERVING: &str = "serving";
/// Health classification for `lifecycle.health`. Phase 1 has no health
/// degradation path (handler faults are isolated by WS6, not surfaced
/// as daemon-wide health yet), so this is a constant `ok`.
const HEALTH_OK: &str = "ok";
/// `lifecycle.health` once at least one handler fault has been isolated
/// (0.63.0 — the described "ok | degraded | failing" domain gains its
/// first live degradation path).
const HEALTH_DEGRADED: &str = "degraded";
/// SPEC 07 §9 declared conformance level. WS4 ships L0 + L1
/// (props.{get,list,describe}); L2 (`props.watch` + `props.changed`)
/// and L3 (`world.<svc>`) are explicit Phase-1 non-goals.
const PROPS_LEVEL: &str = "L1";

impl MixServeRuntime {
    /// Build the runtime surface for a citizen registered as
    /// `service_name`. Captures the start instants now.
    pub fn new(service_name: impl Into<String>) -> Self {
        let service_name = service_name.into();
        let props_prefix = format!("{service_name}.props.");
        Self {
            service_name,
            started_at: Instant::now(),
            started_wall: chrono::Utc::now().to_rfc3339(),
            props_prefix,
            handler_faults: std::cell::Cell::new(0),
            last_fault: std::cell::RefCell::new(None),
        }
    }

    /// The seven lifecycle leaf paths, in stable declaration order.
    fn leaf_paths() -> [&'static str; 7] {
        [
            "lifecycle.started_at",
            "lifecycle.uptime_s",
            "lifecycle.mode",
            "lifecycle.health",
            "lifecycle.props_level",
            "lifecycle.handler_faults",
            "lifecycle.last_fault",
        ]
    }

    /// Build the HELP body: the runtime-reserved verbs (fixed canonical
    /// order) followed by the citizen's author commands (sorted, so the
    /// payload is byte-deterministic for a given handler set). SPEC 02
    /// §3 shape: `[{name, description, args}]`.
    fn help_body(&self, handler_commands: &[&str]) -> String {
        let svc = &self.service_name;
        let mut cmds = vec![
            json!({
                "name": "HELP",
                "description": "List all commands this service accepts",
                "args": [],
            }),
            json!({
                "name": "INFO",
                "description": "Service identity and capabilities",
                "args": [],
            }),
            json!({
                "name": "QUIT",
                "description": "Graceful shutdown: deregister, then exit 0 (SPEC 18 §3.5)",
                "args": [],
            }),
            json!({
                "name": format!("{svc}.props.get"),
                "description": "Property snapshot at an optional path (root if absent)",
                "args": ["path?"],
            }),
            json!({
                "name": format!("{svc}.props.list"),
                "description": "All defined property paths",
                "args": [],
            }),
            json!({
                "name": format!("{svc}.props.describe"),
                "description": "Schema entry (type, mutability, sensitivity) for a path",
                "args": ["path"],
            }),
        ];
        // Drop any authored command that collides with a reserved verb:
        // it is intercepted pre-dispatch and unreachable, so advertising
        // it would publish a duplicate name with a misleading
        // "author-defined" description for a handler that never fires.
        let mut authored: Vec<&str> = handler_commands
            .iter()
            .copied()
            .filter(|c| !self.is_reserved(c))
            .collect();
        authored.sort_unstable();
        authored.dedup();
        for c in authored {
            cmds.push(json!({
                "name": c,
                "description": "Author-defined handler",
                "args": [],
            }));
        }
        Json::Array(cmds).to_string()
    }

    /// Build the INFO body: exactly the SPEC 02 §3 `{name, version,
    /// description}` triple. `version` is the `mix` runtime version
    /// (the citizen has no version of its own — its identity is the
    /// runtime plus its script).
    fn info_body(&self) -> String {
        json!({
            "name": self.service_name,
            "version": env!("CARGO_PKG_VERSION"),
            "description": "Mix supervised Bus citizen (SPEC 18 Phase 1 runtime)",
        })
        .to_string()
    }

    /// Reconstruct the `props.*` args JSON the way the indexd reference
    /// does (`cosmix-indexd::props::parse_args(header).or_else(body)`):
    /// ANY successfully-parsed `args` header JSON wins — not only
    /// objects — and the request body is consulted only when the header
    /// is absent or unparseable. Gating the header on `is_object()`
    /// would feed a different value into `dispatch_props` than indexd
    /// does for a non-object header, breaking L1 byte-consistency.
    fn props_args(args_header: Option<&str>, req_body: &str) -> Option<Json> {
        if let Some(v) = args_header.and_then(|raw| serde_json::from_str::<Json>(raw).ok()) {
            return Some(v);
        }
        if req_body.trim().is_empty() {
            return None;
        }
        serde_json::from_str::<Json>(req_body).ok()
    }

    /// Is `command` a runtime-reserved verb — intercepted pre-dispatch
    /// and never delivered to an author handler? Single source of truth
    /// for the [`ServeRuntime::handle_reserved`] dispatch arms and the
    /// [`Self::help_body`] author-list filter, so the two cannot drift.
    ///
    /// `props.watch` (L2) and `props.set`/`delete` (SPEC 12 L4+) are
    /// deliberately NOT reserved: an author may implement them, so they
    /// must remain advertisable in HELP and must fall through here.
    fn is_reserved(&self, command: &str) -> bool {
        matches!(command, "HELP" | "INFO" | "QUIT")
            || command
                .strip_prefix(&self.props_prefix)
                .is_some_and(|s| matches!(s, "get" | "list" | "describe"))
    }
}

impl PropTree for MixServeRuntime {
    fn snapshot(&self) -> PropValue {
        // uptime_s is live: recomputed from the monotonic clock on
        // every snapshot (props.get), never a stale cached field.
        let uptime_s = self.started_at.elapsed().as_secs();
        // 0.63.0 — isolated handler faults degrade health instead of
        // hiding: a citizen that swallowed a raise used to look exactly
        // like a healthy one (the blind-but-healthy failure shape).
        let faults = self.handler_faults.get();
        let health = if faults > 0 { HEALTH_DEGRADED } else { HEALTH_OK };
        let last_fault = self
            .last_fault
            .borrow()
            .clone()
            .unwrap_or_default();
        build_snapshot([
            (
                PropPath::new("lifecycle.started_at").unwrap(),
                PropValue::from(self.started_wall.clone()),
            ),
            (
                PropPath::new("lifecycle.uptime_s").unwrap(),
                PropValue::from(uptime_s),
            ),
            (
                PropPath::new("lifecycle.mode").unwrap(),
                PropValue::from(MODE_SERVING),
            ),
            (
                PropPath::new("lifecycle.health").unwrap(),
                PropValue::from(health),
            ),
            (
                PropPath::new("lifecycle.props_level").unwrap(),
                PropValue::from(PROPS_LEVEL),
            ),
            (
                PropPath::new("lifecycle.handler_faults").unwrap(),
                PropValue::from(faults),
            ),
            (
                PropPath::new("lifecycle.last_fault").unwrap(),
                PropValue::from(last_fault),
            ),
        ])
    }

    fn list(&self) -> Vec<PropPath> {
        Self::leaf_paths()
            .into_iter()
            .map(|s| PropPath::new(s).unwrap())
            .collect()
    }

    fn describe(&self, path: &PropPath) -> Option<PropDescribe> {
        use PropType::*;
        match path.as_str() {
            "lifecycle.started_at" => Some(
                PropDescribe::leaf(path.clone(), String, "RFC 3339 timestamp of process start.")
                    .with_format("rfc3339"),
            ),
            "lifecycle.uptime_s" => Some(
                PropDescribe::leaf(path.clone(), Number, "Seconds since process start.")
                    .with_transient(true),
            ),
            "lifecycle.mode" => Some(PropDescribe::leaf(
                path.clone(),
                String,
                "Operating mode (serving). Phase 2 adds drain | paused.",
            )),
            "lifecycle.health" => Some(PropDescribe::leaf(
                path.clone(),
                String,
                "Coarse health classification (ok | degraded | failing).",
            )),
            "lifecycle.props_level" => Some(PropDescribe::leaf(
                path.clone(),
                String,
                "SPEC 07 conformance level (L0 | L1 | L2 | L3).",
            )),
            "lifecycle.handler_faults" => Some(
                PropDescribe::leaf(
                    path.clone(),
                    Number,
                    "Isolated handler faults (errors + panics) since start; \
                     > 0 flips health to degraded.",
                )
                .with_transient(true),
            ),
            "lifecycle.last_fault" => Some(
                PropDescribe::leaf(
                    path.clone(),
                    String,
                    "Summary of the most recent isolated handler fault \
                     (empty when none).",
                )
                .with_transient(true),
            ),
            _ => None,
        }
    }
}

impl ServeRuntime for MixServeRuntime {
    fn record_handler_fault(&self, summary: &str) {
        self.handler_faults.set(self.handler_faults.get() + 1);
        // Bound the stored summary: the fault detail can carry request
        // data; the props surface is a health signal, not a log.
        let mut s = summary.to_string();
        if s.chars().count() > 200 {
            s = s.chars().take(200).collect::<String>() + "…";
        }
        *self.last_fault.borrow_mut() = Some(s);
    }

    fn handle_reserved(
        &self,
        command: &str,
        args_header: Option<&str>,
        req_body: &str,
        handler_commands: &[&str],
    ) -> Option<ReservedOutcome> {
        // L0 — bare Ch02 universals (routed by `to:`, never prefixed).
        match command {
            "HELP" => {
                return Some(ReservedOutcome {
                    rc: 0,
                    body: self.help_body(handler_commands),
                    quit: false,
                });
            }
            "INFO" => {
                return Some(ReservedOutcome {
                    rc: 0,
                    body: self.info_body(),
                    quit: false,
                });
            }
            "QUIT" => {
                // §3.5: not a no-op. Reply rc:0, then the pump breaks
                // and the serve entrypoint runs the shutdown path
                // (WS5 wires deregister-before-exit onto that break).
                return Some(ReservedOutcome {
                    rc: 0,
                    body: "{}".to_string(),
                    quit: true,
                });
            }
            _ => {}
        }

        // L1 — `<svc>.props.{get,list,describe}` via the shared
        // cosmix_props encoder (byte-consistent with indexd). Only
        // these three suffixes are reserved; `props.watch` (L2),
        // `props.set`/`delete` (SPEC 12 L4+) are out of Phase-1 scope
        // and fall through to author handlers (return None). The
        // membership predicate here MUST stay in lock-step with
        // [`Self::is_reserved`] (the HELP author-list filter).
        let suffix = command.strip_prefix(&self.props_prefix)?;
        if !matches!(suffix, "get" | "list" | "describe") {
            return None;
        }
        let args = Self::props_args(args_header, req_body);
        let resp = cosmix_props::bus::dispatch_props(
            self,
            suffix,
            args.as_ref(),
            /* redact_sensitive = */ true,
        );
        Some(ReservedOutcome {
            rc: resp.rc.clamp(0, 255) as u8,
            body: resp.body,
            quit: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt() -> MixServeRuntime {
        MixServeRuntime::new("statecache")
    }

    #[test]
    fn help_lists_reserved_verbs_then_sorted_author_commands() {
        let r = rt();
        let out = r
            .handle_reserved("HELP", None, "", &["statecache.get", "alpha.cmd"])
            .expect("HELP is reserved");
        assert_eq!(out.rc, 0);
        assert!(!out.quit);
        let v: Json = serde_json::from_str(&out.body).unwrap();
        let arr = v.as_array().unwrap();
        let names: Vec<&str> = arr.iter().map(|e| e["name"].as_str().unwrap()).collect();
        // Reserved verbs first, fixed order.
        assert_eq!(
            &names[..6],
            &[
                "HELP",
                "INFO",
                "QUIT",
                "statecache.props.get",
                "statecache.props.list",
                "statecache.props.describe",
            ]
        );
        // Author commands appended, sorted+deduped.
        assert_eq!(&names[6..], &["alpha.cmd", "statecache.get"]);
    }

    #[test]
    fn info_is_exactly_the_spec02_triple() {
        let r = rt();
        let out = r.handle_reserved("INFO", None, "", &[]).unwrap();
        let v: Json = serde_json::from_str(&out.body).unwrap();
        let obj = v.as_object().unwrap();
        let mut keys: Vec<&str> = obj.keys().map(|s| s.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["description", "name", "version"]);
        assert_eq!(obj["name"], "statecache");
        assert_eq!(obj["version"], env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn quit_replies_rc0_and_signals_shutdown() {
        let r = rt();
        let out = r.handle_reserved("QUIT", None, "", &[]).unwrap();
        assert_eq!(out.rc, 0);
        assert!(out.quit, "QUIT must signal the graceful shutdown path");
    }

    #[test]
    fn props_get_root_is_the_lifecycle_tree() {
        let r = rt();
        let out = r
            .handle_reserved("statecache.props.get", None, "", &[])
            .unwrap();
        assert_eq!(out.rc, 0);
        let v: Json = serde_json::from_str(&out.body).unwrap();
        let lc = &v["lifecycle"];
        assert_eq!(lc["mode"], MODE_SERVING);
        assert_eq!(lc["health"], HEALTH_OK);
        assert_eq!(lc["props_level"], PROPS_LEVEL);
        assert!(lc["started_at"].is_string());
        assert!(lc["uptime_s"].is_number());
        // 0.63.0 fault surface, quiescent state.
        assert_eq!(lc["handler_faults"], 0);
        assert_eq!(lc["last_fault"], "");
    }

    #[test]
    fn recorded_fault_degrades_health_and_surfaces_summary() {
        // The observable half of SPEC 18 fault isolation (0.63.0): a
        // citizen that swallowed a raise must stop LOOKING healthy.
        let r = rt();
        r.record_handler_fault("handler fault: play.note[0]: boom");
        r.record_handler_fault("handler fault: play.note[0]: boom again");
        let out = r
            .handle_reserved("statecache.props.get", None, "", &[])
            .unwrap();
        let v: Json = serde_json::from_str(&out.body).unwrap();
        let lc = &v["lifecycle"];
        assert_eq!(lc["health"], HEALTH_DEGRADED);
        assert_eq!(lc["handler_faults"], 2);
        assert_eq!(lc["last_fault"], "handler fault: play.note[0]: boom again");
    }

    #[test]
    fn fault_summary_is_bounded() {
        // The props surface is a health signal, not a log — a fault
        // detail carrying request data is truncated.
        let r = rt();
        r.record_handler_fault(&"x".repeat(500));
        let stored = r.last_fault.borrow().clone().unwrap();
        assert!(stored.chars().count() <= 201, "200 + ellipsis");
        assert!(stored.ends_with('…'));
    }

    #[test]
    fn props_get_leaf_path_via_args_header() {
        let r = rt();
        let out = r
            .handle_reserved(
                "statecache.props.get",
                Some(r#"{"path":"lifecycle.props_level"}"#),
                "",
                &[],
            )
            .unwrap();
        let v: Json = serde_json::from_str(&out.body).unwrap();
        assert_eq!(v, json!("L1"));
    }

    #[test]
    fn props_get_leaf_path_falls_back_to_body() {
        let r = rt();
        let out = r
            .handle_reserved(
                "statecache.props.get",
                None,
                r#"{"path":"lifecycle.mode"}"#,
                &[],
            )
            .unwrap();
        let v: Json = serde_json::from_str(&out.body).unwrap();
        assert_eq!(v, json!("serving"));
    }

    #[test]
    fn args_header_wins_even_when_not_an_object() {
        // indexd's parse_args accepts ANY parsed header JSON; a
        // non-object header must still win over the body (it then
        // carries no `path`, so dispatch_props returns the root tree)
        // — proving the body was NOT consulted as a fallback.
        let r = rt();
        let out = r
            .handle_reserved(
                "statecache.props.get",
                Some("42"),
                r#"{"path":"lifecycle.mode"}"#,
                &[],
            )
            .unwrap();
        assert_eq!(out.rc, 0);
        let v: Json = serde_json::from_str(&out.body).unwrap();
        // Root snapshot (header wins, no path) — NOT the body's
        // `"serving"` leaf.
        assert!(v["lifecycle"].is_object());
        assert_ne!(v, json!("serving"));
    }

    #[test]
    fn help_filters_authored_commands_that_collide_with_reserved_verbs() {
        let r = rt();
        let out = r
            .handle_reserved(
                "HELP",
                None,
                "",
                &[
                    "HELP",
                    "QUIT",
                    "statecache.props.get",
                    "statecache.props.watch",
                    "alpha.cmd",
                ],
            )
            .unwrap();
        let v: Json = serde_json::from_str(&out.body).unwrap();
        let names: Vec<&str> = v
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        // Reserved prefix unchanged.
        assert_eq!(
            &names[..6],
            &[
                "HELP",
                "INFO",
                "QUIT",
                "statecache.props.get",
                "statecache.props.list",
                "statecache.props.describe",
            ]
        );
        // Authored HELP/QUIT/props.get are reserved → filtered out.
        // props.watch (L2, NOT reserved) and alpha.cmd survive.
        assert_eq!(&names[6..], &["alpha.cmd", "statecache.props.watch"]);
        // HELP/QUIT/props.get appear exactly once (the reserved entry),
        // never duplicated by an authored shadow.
        assert_eq!(names.iter().filter(|n| **n == "HELP").count(), 1);
        assert_eq!(names.iter().filter(|n| **n == "QUIT").count(), 1);
        assert_eq!(
            names
                .iter()
                .filter(|n| **n == "statecache.props.get")
                .count(),
            1
        );
    }

    #[test]
    fn props_list_enumerates_the_seven_leaves() {
        let r = rt();
        let out = r
            .handle_reserved("statecache.props.list", None, "", &[])
            .unwrap();
        let v: Json = serde_json::from_str(&out.body).unwrap();
        let mut paths: Vec<&str> = v
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p.as_str().unwrap())
            .collect();
        paths.sort_unstable();
        assert_eq!(
            paths,
            [
                "lifecycle.handler_faults",
                "lifecycle.health",
                "lifecycle.last_fault",
                "lifecycle.mode",
                "lifecycle.props_level",
                "lifecycle.started_at",
                "lifecycle.uptime_s",
            ]
        );
    }

    #[test]
    fn props_describe_uptime_is_transient_number() {
        let r = rt();
        let out = r
            .handle_reserved(
                "statecache.props.describe",
                Some(r#"{"path":"lifecycle.uptime_s"}"#),
                "",
                &[],
            )
            .unwrap();
        let v: Json = serde_json::from_str(&out.body).unwrap();
        assert_eq!(v["type"], "number");
        assert_eq!(v["transient"], true);
    }

    #[test]
    fn non_reserved_command_falls_through_to_author() {
        let r = rt();
        // A domain command is the author's — not reserved.
        assert!(r.handle_reserved("statecache.get", None, "", &[]).is_none());
        // props.watch (L2) / props.set (SPEC 12) are out of WS4 scope:
        // not reserved, so the author may (not) implement them.
        assert!(
            r.handle_reserved("statecache.props.watch", None, "", &[])
                .is_none()
        );
        assert!(
            r.handle_reserved("statecache.props.set", None, "", &[])
                .is_none()
        );
    }

    #[test]
    fn props_prefix_is_service_scoped() {
        let r = MixServeRuntime::new("statecache");
        // A different service's props verb is NOT this citizen's
        // reserved surface (it would never be routed here anyway, but
        // the matcher must not claim it).
        assert!(
            r.handle_reserved("other.props.get", None, "", &[])
                .is_none()
        );
    }
}
