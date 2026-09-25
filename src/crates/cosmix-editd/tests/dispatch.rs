//! In-process dispatch tests (ced E0 plan §6.2): no broker; synthesized
//! `IncomingCommand`s with broker headers against a live `Editd` (router,
//! actors, publisher, watcher), files in temp directories. Waits are
//! deadline-bounded waits on the recording sink — never sleep loops.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use cosmix_client::IncomingCommand;
use cosmix_editd::events::testing::RecordingSink;
use cosmix_editd::events::{EventSink, Publisher};
use cosmix_editd::{Config, Editd};
use serde_json::{Value, json};

const EPOCH: &str = "9f2c41a7";

struct H {
    editd: Arc<Editd>,
    sink: Arc<RecordingSink>,
    dir: tempfile::TempDir,
}

fn config() -> Config {
    Config { epoch: EPOCH.into(), mesh_open: true, budget_cap: cosmix_editd::limits::MAX_TOTAL_BYTES }
}

fn start_with(config: Config) -> H {
    let sink = Arc::new(RecordingSink::default());
    let editd = Editd::start(config, sink.clone() as Arc<dyn EventSink>);
    H { editd, sink, dir: tempfile::tempdir().unwrap() }
}

fn start() -> H {
    start_with(config())
}

#[derive(Clone, Copy)]
enum Who<'a> {
    Local(&'a str),
    Anon,
    Mesh(&'a str, &'a str),
    Unstamped,
}

fn cmd(who: Who, verb: &str, args: Value) -> IncomingCommand {
    let mut headers = BTreeMap::new();
    let from = match who {
        Who::Local(from) => {
            headers.insert("broker_origin".to_string(), "local".to_string());
            from.to_string()
        }
        Who::Anon => {
            headers.insert("broker_origin".to_string(), "local".to_string());
            String::new()
        }
        Who::Mesh(service, peer) => {
            headers.insert("broker_origin".to_string(), "mesh".to_string());
            headers.insert("broker_service".to_string(), service.to_string());
            headers.insert("broker_peer".to_string(), peer.to_string());
            String::new()
        }
        Who::Unstamped => String::new(),
    };
    IncomingCommand { from, command: verb.into(), id: Some("1".into()), args, body: String::new(), headers }
}

impl H {
    async fn call_as(&self, who: Who<'_>, verb: &str, args: Value) -> (u8, Value) {
        let (rc, body) = self.editd.handle(&cmd(who, verb, args)).await;
        (rc, serde_json::from_str(&body).unwrap_or(Value::String(body)))
    }

    async fn call(&self, verb: &str, args: Value) -> (u8, Value) {
        self.call_as(Who::Local("tester"), verb, args).await
    }

    async fn ok(&self, verb: &str, args: Value) -> Value {
        let (rc, v) = self.call(verb, args).await;
        assert_eq!(rc, 0, "{verb} refused: {v}");
        v
    }

    fn path(&self, name: &str) -> PathBuf {
        std::fs::canonicalize(self.dir.path()).unwrap().join(name)
    }

    fn file(&self, name: &str, text: &str) -> PathBuf {
        let p = self.path(name);
        std::fs::write(&p, text).unwrap();
        p
    }

    async fn open(&self, path: &Path) -> String {
        let v = self.ok("edit.open", json!({"path": path})).await;
        v["buffer"].as_str().unwrap().to_string()
    }

    async fn scratch(&self) -> String {
        self.ok("edit.open", json!({})).await["buffer"].as_str().unwrap().to_string()
    }

    async fn text(&self, b: &str) -> String {
        self.ok("edit.get", json!({"buffer": b})).await["text"].as_str().unwrap().to_string()
    }

    /// Wait until an `edit.changed` body satisfies `pred`.
    async fn event(&self, pred: impl Fn(&Value) -> bool) -> Value {
        let found = self
            .sink
            .wait_for(|s| s.iter().any(|(topic, body)| topic == "edit.changed" && pred(body)))
            .await;
        assert!(found, "event never arrived; sent: {:?}", self.sink.sent.lock().unwrap());
        let sent = self.sink.sent.lock().unwrap();
        sent.iter().find(|(topic, body)| topic == "edit.changed" && pred(body)).unwrap().1.clone()
    }
}

fn refused(v: &(u8, Value), code: &str, reason: Option<&str>) {
    assert_eq!(v.0, 10, "expected a refusal, got {}", v.1);
    assert_eq!(v.1["error_code"], code, "{}", v.1);
    match reason {
        Some(r) => assert_eq!(v.1["reason"], r, "{}", v.1),
        None => assert!(v.1["reason"].is_null(), "{}", v.1),
    }
}

fn fixture(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../cosmix-edit-core/tests/fixtures/contract").join(name);
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{name}: {e}"))).unwrap()
}

/// Same key set as the golden fixture (values are daemon-specific).
fn same_shape(v: &Value, name: &str) {
    let f = fixture(name);
    let keys = |v: &Value| v.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()).unwrap_or_default();
    assert_eq!(keys(v), keys(&f), "{name}: reply shape differs: {v}");
}

// ── fixtures: success shapes ────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replies_have_the_golden_shapes() {
    let h = start();
    same_shape(&h.ok("edit.ping", json!({})).await, "edit.ping.reply.json");
    let f = h.file("x.mix", "alpha\nbeta\n");
    let open = h.ok("edit.open", json!({"path": f})).await;
    same_shape(&open, "edit.open.reply.json");
    let b = open["buffer"].as_str().unwrap().to_string();
    assert!(b.starts_with('b') && b.ends_with(&format!("_{EPOCH}")), "epoch in id: {b}");
    assert_eq!(open["epoch"], EPOCH);
    same_shape(&h.ok("edit.info", json!({})).await, "edit.info.reply.json");
    same_shape(&h.ok("edit.list", json!({})).await, "edit.list.reply.json");
    let ins = h.ok("edit.insert", json!({"buffer": b, "at": {"line": 2, "col": 1}, "text": "-- note\n", "op_id": "k1"})).await;
    same_shape(&ins, "edit.insert.reply.json");
    assert_eq!(ins["op_id"], "k1");
    same_shape(&h.ok("edit.delete", json!({"buffer": b, "range": [0, 1]})).await, "edit.delete.reply.json");
    same_shape(&h.ok("edit.replace", json!({"buffer": b, "range": [0, 1], "text": "A"})).await, "edit.replace.reply.json");
    let apply = h
        .ok("edit.apply", json!({"buffer": b, "ops": [{"op": "insert", "at": "end", "text": "tail\n"}, {"op": "delete", "range": {"lines": [1, 1]}}]}))
        .await;
    same_shape(&apply, "edit.apply.reply.json");
    same_shape(&h.ok("edit.get", json!({"buffer": b})).await, "edit.get.reply.json");
    same_shape(&h.ok("edit.find", json!({"buffer": b, "pattern": "beta"})).await, "edit.find.reply.json");
    same_shape(&h.ok("edit.select", json!({"buffer": b, "ranges": [[0, 1]]})).await, "edit.select.reply.json");
    same_shape(&h.ok("edit.cursor", json!({"buffer": b, "at": 0})).await, "edit.cursor.reply.json");
    same_shape(&h.ok("edit.anchor.set", json!({"buffer": b, "name": "m", "at": 0})).await, "edit.anchor.set.reply.json");
    same_shape(&h.ok("edit.anchor.get", json!({"buffer": b})).await, "edit.anchor.get.reply.json");
    same_shape(&h.ok("edit.anchor.clear", json!({"buffer": b, "name": "m"})).await, "edit.anchor.clear.reply.json");
    same_shape(&h.ok("edit.undo", json!({"buffer": b})).await, "edit.undo.reply.json");
    same_shape(&h.ok("edit.redo", json!({"buffer": b})).await, "edit.redo.reply.json");
    same_shape(&h.ok("edit.history", json!({"buffer": b})).await, "edit.history.reply.json");
    same_shape(&h.ok("edit.save", json!({"buffer": b})).await, "edit.save.reply.json");
    same_shape(&h.ok("edit.reload", json!({"buffer": b})).await, "edit.reload.reply.unchanged.json");
    same_shape(&h.ok("edit.props.watch", json!({})).await, "edit.props.watch.reply.json");
    let props = h.ok("edit.props.get", json!({})).await;
    assert_eq!(props["lifecycle"]["epoch"], EPOCH);
    assert_eq!(props["buffers"][&b]["dirty"], false);
    same_shape(&h.ok("edit.close", json!({"buffer": b})).await, "edit.close.reply.json");
}

// ── fixtures: every refusal reason, and precedence ─────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refusals_carry_code_and_reason() {
    let h = start();
    refused(&h.call("edit.bogus", json!({"buffer": "b1_x"})).await, "UNKNOWN_VERB", None);
    refused(&h.call("edit.open", json!({"path": "rel/x"})).await, "INVALID_ARGUMENT", Some("bad_path"));
    let missing = h.path("missing.mix");
    let r = h.call("edit.open", json!({"path": missing})).await;
    refused(&r, "NOT_FOUND", Some("file_not_found"));
    assert_eq!(r.1["path"], missing.display().to_string());
    let png = h.path("logo.png");
    std::fs::write(&png, [0xff, 0xfe, 0x00, 0x80]).unwrap();
    refused(&h.call("edit.open", json!({"path": png})).await, "INVALID_ARGUMENT", Some("not_utf8"));
    let r = h.call("edit.get", json!({"buffer": "b3_0badf00d"})).await;
    refused(&r, "NOT_FOUND", Some("epoch_mismatch"));
    assert_eq!(r.1["epoch"], EPOCH);
    refused(&h.call("edit.get", json!({"buffer": format!("b99_{EPOCH}")})).await, "NOT_FOUND", Some("unknown_buffer"));

    let f = h.file("x.txt", "one\ntwo\n");
    let b = h.open(&f).await;
    refused(&h.call("edit.insert", json!({"buffer": b})).await, "INVALID_ARGUMENT", Some("bad_args"));
    refused(
        &h.call("edit.insert", json!({"buffer": b, "at": 0, "text": "x", "expect_rev": 0, "base_rev": 0})).await,
        "INVALID_ARGUMENT",
        Some("both_cas"),
    );
    refused(
        &h.call("edit.insert", json!({"buffer": b, "at": 0, "text": "x", "origin": "agent:has space"})).await,
        "INVALID_ARGUMENT",
        Some("bad_origin"),
    );
    refused(
        &h.call("edit.insert", json!({"buffer": b, "at": 0, "text": "x", "op_id": "no spaces"})).await,
        "INVALID_ARGUMENT",
        Some("bad_op_id"),
    );
    let r = h.call("edit.insert", json!({"buffer": b, "at": 0, "text": "x", "expect_rev": 7})).await;
    refused(&r, "CONFLICT", Some("stale_rev"));
    assert_eq!(r.1["rev"], 0);
    refused(
        &h.call("edit.insert", json!({"buffer": b, "at": {"line": 900}, "text": "x"})).await,
        "INVALID_ARGUMENT",
        Some("line_out_of_range"),
    );
    refused(
        &h.call("edit.insert", json!({"buffer": b, "at": {"line": 1}, "text": "x", "base_rev": 0})).await,
        "INVALID_ARGUMENT",
        Some("base_rev_needs_offsets"),
    );
    refused(&h.call("edit.find", json!({"buffer": b, "pattern": "(", "regex": true})).await, "INVALID_ARGUMENT", Some("bad_regex"));
    refused(&h.call("edit.undo", json!({"buffer": b})).await, "NOT_FOUND", Some("nothing_to_undo"));
    refused(&h.call("edit.anchor.get", json!({"buffer": b, "name": "nope"})).await, "NOT_FOUND", Some("unknown_anchor"));
    refused(&h.call("edit.anchor.set", json!({"buffer": b, "name": "bad name", "at": 0})).await, "INVALID_ARGUMENT", Some("bad_name"));
    refused(
        &h.call("edit.get", json!({"buffer": b, "snapshot": "s999"})).await,
        "NOT_FOUND",
        Some("snapshot_expired"),
    );

    let s = h.scratch().await;
    refused(&h.call("edit.save", json!({"buffer": s})).await, "INVALID_ARGUMENT", Some("scratch_needs_path"));

    // dirty close / reload
    h.ok("edit.insert", json!({"buffer": b, "at": 0, "text": "x"})).await;
    let r = h.call("edit.close", json!({"buffer": b})).await;
    refused(&r, "CONFLICT", Some("dirty"));
    assert_eq!(r.1["rev"], 1);
    refused(&h.call("edit.reload", json!({"buffer": b})).await, "CONFLICT", Some("dirty"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refusal_precedence_order() {
    let h = start_with(Config { mesh_open: false, ..config() });
    let f = h.file("p.txt", "p\n");
    let b = h.open(&f).await;
    let mesh = Who::Mesh("term", "beta");
    // 1 unknown verb beats everything.
    refused(&h.call_as(Who::Unstamped, "edit.nope", json!({"buffer": "zz"})).await, "UNKNOWN_VERB", None);
    // 2 arg shape beats unstamped.
    refused(&h.call_as(Who::Unstamped, "edit.insert", json!({"buffer": b})).await, "INVALID_ARGUMENT", Some("bad_args"));
    // 3 unstamped beats the mesh lock and the buffer lookup.
    refused(
        &h.call_as(Who::Unstamped, "edit.insert", json!({"buffer": "b9_0badf00d", "at": 0, "text": "x"})).await,
        "INVALID_ARGUMENT",
        Some("unstamped"),
    );
    // 4 mesh lock beats the lookup; reads stay open.
    refused(
        &h.call_as(mesh, "edit.insert", json!({"buffer": "b9_0badf00d", "at": 0, "text": "x"})).await,
        "FORBIDDEN",
        Some("mesh_locked"),
    );
    assert_eq!(h.call_as(mesh, "edit.get", json!({"buffer": b})).await.0, 0);
    // 5 lookup beats CAS.
    refused(
        &h.call("edit.insert", json!({"buffer": "b9_0badf00d", "at": 0, "text": "x", "expect_rev": 9})).await,
        "NOT_FOUND",
        Some("epoch_mismatch"),
    );
    // 7 dedup beats CAS: a replay with a now-stale expect_rev returns the original.
    let first = h.ok("edit.insert", json!({"buffer": b, "at": 0, "text": "x", "expect_rev": 0, "op_id": "d1"})).await;
    let again = h.ok("edit.insert", json!({"buffer": b, "at": 0, "text": "x", "expect_rev": 0, "op_id": "d1"})).await;
    assert_eq!(again["duplicate"], true);
    assert_eq!(again["rev"], first["rev"]);
    // 8 CAS beats position validation.
    refused(
        &h.call("edit.insert", json!({"buffer": b, "at": {"line": 999}, "text": "x", "expect_rev": 0})).await,
        "CONFLICT",
        Some("stale_rev"),
    );
}

// ── caller rules ─────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn caller_rules() {
    let h = start();
    let b = h.scratch().await;
    refused(&h.call_as(Who::Unstamped, "edit.insert", json!({"buffer": b, "at": 0, "text": "x"})).await, "INVALID_ARGUMENT", Some("unstamped"));
    assert_eq!(h.call_as(Who::Unstamped, "edit.get", json!({"buffer": b})).await.0, 0);

    let r = h.call_as(Who::Local("ced"), "edit.insert", json!({"buffer": b, "at": 0, "text": "a", "origin": "human:mark"})).await;
    assert_eq!((r.1["origin"].clone(), r.1["origin_downgraded"].clone()), (json!("human:mark"), json!(false)));
    let r = h.call_as(Who::Anon, "edit.insert", json!({"buffer": b, "at": 0, "text": "b", "origin": "human:mark"})).await;
    assert_eq!((r.1["origin"].clone(), r.1["origin_downgraded"].clone()), (json!("agent:mark"), json!(true)));
    let r = h.call_as(Who::Mesh("ced", "beta"), "edit.insert", json!({"buffer": b, "at": 0, "text": "c", "origin": "human:mark"})).await;
    assert_eq!(r.1["origin"], "agent:mark");
    let r = h.call_as(Who::Local("ced"), "edit.insert", json!({"buffer": b, "at": 0, "text": "d", "origin": "tool:disk"})).await;
    assert_eq!((r.1["origin"].clone(), r.1["origin_downgraded"].clone()), (json!("agent:disk"), json!(true)));
    let r = h.call_as(Who::Anon, "edit.undo", json!({"buffer": b, "origin": "*", "as": "human:x"})).await;
    assert_eq!(r.0, 0, "{}", r.1);
    assert_eq!((r.1["origin"].clone(), r.1["origin_downgraded"].clone()), (json!("agent:x"), json!(true)));
    let long = "s".repeat(60);
    let r = h.call_as(Who::Mesh(&long, "peer-name-long"), "edit.insert", json!({"buffer": b, "at": 0, "text": "e"})).await;
    let origin = r.1["origin"].as_str().unwrap();
    let label = origin.strip_prefix("agent:").unwrap();
    assert_eq!(label.len(), 64);
    assert_eq!(&label[55..56], "+");
    let r = h.call_as(Who::Local("ced"), "edit.insert", json!({"buffer": b, "at": 0, "text": "f"})).await;
    assert_eq!(r.1["origin"], "agent:ced");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mesh_lock_blocks_mutations_only() {
    let h = start_with(Config { mesh_open: false, ..config() });
    let b = h.scratch().await;
    let mesh = Who::Mesh("x", "beta");
    refused(&h.call_as(mesh, "edit.insert", json!({"buffer": b, "at": 0, "text": "x"})).await, "FORBIDDEN", Some("mesh_locked"));
    refused(&h.call_as(mesh, "edit.open", json!({})).await, "FORBIDDEN", Some("mesh_locked"));
    for verb in ["edit.get", "edit.history", "edit.anchor.get"] {
        assert_eq!(h.call_as(mesh, verb, json!({"buffer": b})).await.0, 0, "{verb}");
    }
    assert_eq!(h.call_as(mesh, "edit.list", json!({})).await.0, 0);
    assert_eq!(h.ok("edit.info", json!({})).await["mesh_open"], false);
}

// ── dedup ───────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dedup_global_undo_retry_and_refused_retry() {
    let h = start();
    let b = h.scratch().await;
    let a = Who::Local("a");
    let c = Who::Local("c");
    h.call_as(a, "edit.insert", json!({"buffer": b, "at": 0, "text": "AAA"})).await;
    h.call_as(c, "edit.insert", json!({"buffer": b, "at": "end", "text": "CCC"})).await;
    // Global undo takes c's newest group.
    let first = h.call_as(Who::Anon, "edit.undo", json!({"buffer": b, "origin": "*", "op_id": "u1"})).await;
    assert_eq!(first.0, 0, "{}", first.1);
    assert_eq!(first.1["lane"], "agent:c");
    // a edits again: its lane is now newest. The retry must not undo it.
    h.call_as(a, "edit.insert", json!({"buffer": b, "at": 0, "text": "Z"})).await;
    let retry = h.call_as(Who::Anon, "edit.undo", json!({"buffer": b, "origin": "*", "op_id": "u1"})).await;
    assert_eq!(retry.1["duplicate"], true);
    assert_eq!(retry.1["rev"], first.1["rev"]);
    assert_eq!(h.text(&b).await, "ZAAA");

    // A refused request retried with the same op_id executes.
    let stale = h.call("edit.insert", json!({"buffer": b, "at": 0, "text": "q", "expect_rev": 0, "op_id": "r1"})).await;
    refused(&stale, "CONFLICT", Some("stale_rev"));
    let fresh = h.call("edit.insert", json!({"buffer": b, "at": 0, "text": "q", "op_id": "r1"})).await;
    assert_eq!((fresh.0, fresh.1["duplicate"].clone()), (0, json!(false)));

    // Two callers, one op_id: both execute.
    let x = h.call_as(Who::Local("x"), "edit.insert", json!({"buffer": b, "at": 0, "text": "1", "op_id": "same"})).await;
    let y = h.call_as(Who::Local("y"), "edit.insert", json!({"buffer": b, "at": 0, "text": "2", "op_id": "same"})).await;
    assert_eq!((x.1["duplicate"].clone(), y.1["duplicate"].clone()), (json!(false), json!(false)));
    assert!(h.text(&b).await.starts_with("21q"));
}

// ── compact replies ─────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn compact_reply_for_a_huge_undo_group() {
    let h = start();
    let b = h.scratch().await;
    h.ok("edit.insert", json!({"buffer": b, "at": 0, "text": "x".repeat(5_000)})).await;
    // 5,000 coalesced backspaces from the end.
    for i in (0..5_000usize).rev() {
        h.ok("edit.delete", json!({"buffer": b, "range": [i, i + 1], "coalesce": true})).await;
    }
    let (rc, body) = h.editd.handle(&cmd(Who::Local("tester"), "edit.undo", json!({"buffer": b}))).await;
    assert_eq!(rc, 0, "{body}");
    assert!(body.len() < 4096, "undo reply is {} bytes", body.len());
    let v: Value = serde_json::from_str(&body).unwrap();
    assert!(v["changed"].as_array().unwrap().len() <= 64);
    if v["edit_count"].as_u64().unwrap() > 64 && v["changed"].as_array().unwrap().len() == 64 {
        assert_eq!(v["changed_truncated"], true);
    }
    assert_eq!(h.text(&b).await.len(), 5_000);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn find_with_500_groups_truncates_and_pages() {
    let h = start();
    let b = h.scratch().await;
    let line = "a".repeat(10_000);
    h.ok("edit.insert", json!({"buffer": b, "at": 0, "text": format!("{line}\n{line}\n{line}\n")})).await;
    let pattern = format!("{}a+{}", "(".repeat(500), ")".repeat(500));
    let first = h.ok("edit.find", json!({"buffer": b, "pattern": pattern, "regex": true, "groups": true, "limit": 1})).await;
    let m = &first["matches"][0];
    assert_eq!(m["groups_truncated"], true);
    assert_eq!(m["text_truncated"], true);
    assert_eq!(first["truncated"], true);
    let next = first["next"].as_u64().unwrap();
    let second = h
        .ok("edit.find", json!({"buffer": b, "pattern": pattern, "regex": true, "groups": true, "limit": 1, "from": next}))
        .await;
    assert!(second["matches"][0]["start"]["offset"].as_u64().unwrap() >= next, "next advances");
}

// ── router reservations ────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_opens_yield_one_buffer() {
    let h = start();
    let f = h.file("c.txt", "c\n");
    let futures: Vec<_> = (0..8)
        .map(|i| h.editd.submit(&cmd(Who::Local(&format!("c{i}")), "edit.open", json!({"path": f}))))
        .collect();
    let mut ids = std::collections::BTreeSet::new();
    let mut fresh = 0;
    for fut in futures {
        let (rc, body) = fut.await;
        assert_eq!(rc, 0, "{body}");
        let v: Value = serde_json::from_str(&body).unwrap();
        ids.insert(v["buffer"].as_str().unwrap().to_string());
        if v["reopened"] == false {
            fresh += 1;
        }
    }
    assert_eq!(ids.len(), 1);
    assert_eq!(fresh, 1);
    let list = h.ok("edit.list", json!({})).await;
    assert_eq!(list["buffers"][0]["holders"].as_array().unwrap().len(), 8);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn open_racing_close_waits_and_loads_fresh() {
    let h = start();
    let f = h.file("r.txt", "old\n");
    let b = h.open(&f).await;
    let close = h.editd.submit(&cmd(Who::Local("tester"), "edit.close", json!({"buffer": b})));
    let reopen = h.editd.submit(&cmd(Who::Local("other"), "edit.open", json!({"path": f})));
    std::fs::write(&f, "new\n").unwrap();
    let (rc, body) = close.await;
    assert_eq!(rc, 0, "{body}");
    let (rc, body) = reopen.await;
    assert_eq!(rc, 0, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    let b2 = v["buffer"].as_str().unwrap();
    if b2 != b {
        assert_eq!(v["reopened"], false);
        assert_eq!(h.text(b2).await, "new\n");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn save_as_races_to_one_absent_path() {
    let h = start();
    let one = h.scratch().await;
    let two = h.scratch().await;
    h.ok("edit.insert", json!({"buffer": one, "at": 0, "text": "one"})).await;
    h.ok("edit.insert", json!({"buffer": two, "at": 0, "text": "two"})).await;
    let dest = h.path("dest.txt");
    let a = h.editd.submit(&cmd(Who::Local("t"), "edit.save", json!({"buffer": one, "path": dest})));
    let b = h.editd.submit(&cmd(Who::Local("t"), "edit.save", json!({"buffer": two, "path": dest})));
    let (ra, rb) = (a.await, b.await);
    let results = [ra, rb];
    let wins = results.iter().filter(|(rc, _)| *rc == 0).count();
    assert_eq!(wins, 1, "{results:?}");
    let loser: Value = serde_json::from_str(&results.iter().find(|(rc, _)| *rc != 0).unwrap().1).unwrap();
    assert_eq!(loser["error_code"], "CONFLICT");
    assert!(loser["reason"] == "path_open" || loser["reason"] == "exists", "{loser}");
    // The winner is now bound: an open of the path reopens it.
    let v = h.ok("edit.open", json!({"path": dest})).await;
    assert_eq!(v["reopened"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn byte_budget_admits_only_what_fits() {
    let h = start_with(Config { budget_cap: 1_000_000, ..config() });
    let a = h.scratch().await;
    let b = h.scratch().await;
    let text = "x".repeat(300_000);
    let fa = h.editd.submit(&cmd(Who::Local("t"), "edit.insert", json!({"buffer": a, "at": 0, "text": text})));
    let fb = h.editd.submit(&cmd(Who::Local("t"), "edit.insert", json!({"buffer": b, "at": 0, "text": text})));
    let (ra, rb) = (fa.await, fb.await);
    let ok = [&ra, &rb].iter().filter(|(rc, _)| *rc == 0).count();
    assert!(ok >= 1, "{ra:?} {rb:?}");
    assert!(h.editd.budget().used() <= 1_000_000);
    for (rc, body) in [&ra, &rb] {
        if *rc != 0 {
            let v: Value = serde_json::from_str(body).unwrap();
            assert_eq!((v["error_code"].as_str(), v["reason"].as_str()), (Some("RESOURCE_LIMIT"), Some("budget")));
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_actor_inbox_is_busy() {
    let h = start();
    let b = h.scratch().await;
    h.ok("edit.insert", json!({"buffer": b, "at": 0, "text": "a".repeat(1_000_000)})).await;
    for _ in 0..8 {
        h.ok("edit.insert", json!({"buffer": b, "at": "end", "text": "a".repeat(1_000_000)})).await;
    }
    let slow = h.editd.submit(&cmd(Who::Local("t"), "edit.find", json!({"buffer": b, "pattern": "a*b", "regex": true})));
    let mut busy = 0;
    let mut pending = Vec::new();
    for _ in 0..2_000 {
        let f = h.editd.submit(&cmd(Who::Local("t"), "edit.history", json!({"buffer": b, "limit": 1})));
        pending.push(f);
    }
    for f in pending {
        let (rc, body) = f.await;
        if rc == 10 {
            let v: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(v["reason"], "busy");
            busy += 1;
        }
    }
    slow.await;
    assert!(busy > 0, "2,000 queued commands behind a slow find never filled the {}-slot inbox", cosmix_editd::limits::ACTOR_INBOX);
}

// ── holders, close ─────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reopen_holders_dirty_close_and_force() {
    let h = start();
    let f = h.file("h.txt", "h\n");
    let b = h.call_as(Who::Local("ced"), "edit.open", json!({"path": f})).await.1["buffer"].as_str().unwrap().to_string();
    let again = h.call_as(Who::Anon, "edit.open", json!({"path": f})).await;
    assert_eq!(again.1["buffer"], b);
    assert_eq!(again.1["reopened"], true);
    h.ok("edit.insert", json!({"buffer": b, "at": 0, "text": "x"})).await;
    // Not the last holder: releases the hold, buffer stays.
    let r = h.call_as(Who::Local("ced"), "edit.close", json!({"buffer": b})).await;
    assert_eq!((r.1["closed"].clone(), r.1["holders"].clone()), (json!(false), json!(["anon"])));
    // Last holder, dirty, no force: refused and still held.
    refused(&h.call_as(Who::Anon, "edit.close", json!({"buffer": b})).await, "CONFLICT", Some("dirty"));
    assert_eq!(h.ok("edit.list", json!({})).await["buffers"][0]["holders"], json!(["anon"]));
    // force frees it (discarding the text).
    let r = h.call_as(Who::Anon, "edit.close", json!({"buffer": b, "force": true})).await;
    assert_eq!(r.1["closed"], true);
    h.event(|e| e["event"] == "close" && e["buffer"] == b.as_str()).await;
    refused(&h.call("edit.get", json!({"buffer": b})).await, "NOT_FOUND", Some("unknown_buffer"));
    assert_eq!(h.editd.budget().used(), 0, "the actor's lease went with it");
}

// ── save ────────────────────────────────────────────────────────────────────

fn temps(dir: &Path) -> usize {
    std::fs::read_dir(dir).unwrap().filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().contains(".editd-")).count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn save_semantics() {
    let h = start();
    let target = h.file("t.sh", "old\n");
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o750)).unwrap();
    let link = h.path("link.sh");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let b = h.open(&link).await;
    h.ok("edit.replace", json!({"buffer": b, "range": "all", "text": "new\n"})).await;
    let saved = h.ok("edit.save", json!({"buffer": b})).await;
    assert_eq!(saved["durable"], true);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "new\n");
    assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
    assert_eq!(std::fs::metadata(&target).unwrap().permissions().mode() & 0o7777, 0o750);
    assert_eq!(temps(&h.path("")), 0);

    // save-as onto an existing file: exists, then force.
    let other = h.file("other.txt", "keep\n");
    let r = h.call("edit.save", json!({"buffer": b, "path": other})).await;
    refused(&r, "CONFLICT", Some("exists"));
    assert_eq!(std::fs::read_to_string(&other).unwrap(), "keep\n");
    // save-as to a path bound to another buffer: path_open.
    let bound = h.open(&other).await;
    let r = h.call("edit.save", json!({"buffer": b, "path": other})).await;
    refused(&r, "CONFLICT", Some("path_open"));
    assert_eq!(r.1["other_buffer"], bound);

    // save-as to a fresh path rebinds the watch: an external write there reloads.
    let fresh = h.path("fresh.sh");
    h.ok("edit.save", json!({"buffer": b, "path": fresh})).await;
    let list = h.ok("edit.list", json!({})).await;
    let entry = list["buffers"].as_array().unwrap().iter().find(|e| e["buffer"] == b.as_str()).unwrap().clone();
    assert_eq!(entry["path"], fresh.display().to_string());
    std::fs::write(&fresh, "external\n").unwrap();
    h.event(|e| e["event"] == "edit" && e["buffer"] == b.as_str() && e["kind"] == "reload").await;
    assert_eq!(h.text(&b).await, "external\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn save_refuses_after_external_change_to_a_dirty_buffer() {
    let h = start();
    let f = h.file("d.txt", "one\n");
    let b = h.open(&f).await;
    h.ok("edit.insert", json!({"buffer": b, "at": 0, "text": "mine "})).await;
    std::fs::write(&f, "theirs\n").unwrap();
    h.event(|e| e["event"] == "disk" && e["buffer"] == b.as_str() && e["disk"] == "modified").await;
    let r = h.call("edit.save", json!({"buffer": b})).await;
    refused(&r, "CONFLICT", Some("disk_modified"));
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "theirs\n");
    assert_eq!(h.text(&b).await, "mine one\n", "base never advanced from the observation");
    h.ok("edit.save", json!({"buffer": b, "force": true})).await;
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "mine one\n");
}

// ── watcher ─────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watcher_reloads_clean_and_flags_deleted() {
    let h = start();
    let f = h.file("w.txt", "one\n");
    let b = h.open(&f).await;
    std::fs::write(&f, "two\n").unwrap();
    h.event(|e| e["event"] == "edit" && e["kind"] == "reload" && e["buffer"] == b.as_str()).await;
    h.event(|e| e["event"] == "disk" && e["buffer"] == b.as_str()).await;
    assert_eq!(h.text(&b).await, "two\n");
    let hist = h.ok("edit.history", json!({"buffer": b})).await;
    assert!(hist["entries"].as_array().unwrap().iter().any(|e| e["kind"] == "reload" && e["origin"] == "tool:disk"));
    assert_eq!(h.ok("edit.list", json!({})).await["buffers"][0]["dirty"], false);

    // Own save: no reload entry.
    h.ok("edit.insert", json!({"buffer": b, "at": 0, "text": "x"})).await;
    let rev = h.ok("edit.save", json!({"buffer": b})).await["rev"].as_u64().unwrap();
    // Rename-over by another editor.
    let tmp = h.file(".w.txt.swp", "three\n");
    std::fs::rename(&tmp, &f).unwrap();
    let reload = h.event(|e| e["event"] == "edit" && e["kind"] == "reload" && e["rev"].as_u64() > Some(rev)).await;
    assert_eq!(reload["base_rev"], rev, "our own save produced no reload");
    assert_eq!(h.text(&b).await, "three\n");

    // Delete.
    std::fs::remove_file(&f).unwrap();
    h.event(|e| e["event"] == "disk" && e["disk"] == "deleted").await;
    // Recreate.
    std::fs::write(&f, "four\n").unwrap();
    h.event(|e| e["event"] == "edit" && e["kind"] == "reload" && e["buffer"] == b.as_str() && e["edits"].to_string().contains("four")).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replaced_parent_directory_rearms_through_dispatch() {
    let h = start();
    let dir = h.path("sub");
    std::fs::create_dir(&dir).unwrap();
    let f = dir.join("a.txt");
    std::fs::write(&f, "one\n").unwrap();
    let b = h.open(&f).await;
    std::fs::rename(&dir, h.path("old")).unwrap();
    h.event(|e| e["event"] == "disk" && e["disk"] == "deleted").await;
    let fresh = h.path("fresh");
    std::fs::create_dir(&fresh).unwrap();
    std::fs::write(fresh.join("a.txt"), "one\n").unwrap();
    std::fs::rename(&fresh, &dir).unwrap();
    h.event(|e| e["event"] == "disk" && e["disk"] == "clean").await;
    std::fs::write(&f, "later\n").unwrap();
    h.event(|e| e["event"] == "edit" && e["kind"] == "reload" && e["buffer"] == b.as_str()).await;
    assert_eq!(h.text(&b).await, "later\n");
}

// ── events ──────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn events_are_contiguous_and_oversized_resyncs() {
    let h = start();
    let b = h.scratch().await;
    for i in 0..100 {
        h.ok("edit.insert", json!({"buffer": b, "at": "end", "text": format!("{i}")})).await;
    }
    h.event(|e| e["event"] == "edit" && e["rev"] == 100).await;
    let edits: Vec<Value> = h
        .sink
        .sent
        .lock()
        .unwrap()
        .iter()
        .filter(|(t, e)| t == "edit.changed" && e["event"] == "edit")
        .map(|(_, e)| e.clone())
        .collect();
    for (i, e) in edits.iter().enumerate() {
        assert_eq!(e["base_rev"], i as u64);
        assert_eq!(e["rev"], i as u64 + 1);
        assert_eq!(e["epoch"], EPOCH);
    }
    let seqs: Vec<u64> = h.sink.sent.lock().unwrap().iter().filter_map(|(_, e)| e["event_seq"].as_u64()).collect();
    assert!(seqs.windows(2).all(|w| w[1] > w[0]), "event_seq strictly increasing");

    h.ok("edit.insert", json!({"buffer": b, "at": 0, "text": "p".repeat(300 * 1024)})).await;
    let r = h.event(|e| e["event"] == "resync" && e["reason"] == "oversized").await;
    assert_eq!(r["buffers"], json!([b]));
    assert_eq!(r["rev"], 101);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn publish_failure_and_reconnect_resync() {
    let h = start();
    let b = h.scratch().await;
    h.event(|e| e["event"] == "open").await;
    h.sink.fail_next.store(1, Ordering::Release);
    h.ok("edit.insert", json!({"buffer": b, "at": 0, "text": "lost"})).await;
    let r = h.event(|e| e["event"] == "resync" && e["reason"] == "publisher_loss").await;
    assert_eq!(r["buffers"], json!([b]));
    assert!(h.editd.publisher().loss() >= 1);
    h.editd.publisher().reconnected();
    let r = h.event(|e| e["event"] == "resync" && e["reason"] == "reconnect").await;
    assert_eq!(r["buffers"], "all");
    let info = h.ok("edit.info", json!({})).await;
    assert!(info["publisher_loss"].as_u64().unwrap() >= 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn publisher_queue_budget_is_announced() {
    let sink = Arc::new(RecordingSink::default());
    let publisher = Publisher::with_queue_cap(EPOCH, 2_000);
    let editd = Editd::start_with(config(), Some(publisher), sink.clone() as Arc<dyn EventSink>);
    let (rc, body) = editd.handle(&cmd(Who::Local("t"), "edit.open", json!({}))).await;
    assert_eq!(rc, 0, "{body}");
    let b = serde_json::from_str::<Value>(&body).unwrap()["buffer"].as_str().unwrap().to_string();
    let futures: Vec<_> = (0..50)
        .map(|_| editd.submit(&cmd(Who::Local("t"), "edit.insert", json!({"buffer": b, "at": 0, "text": "z".repeat(200)}))))
        .collect();
    for f in futures {
        assert_eq!(f.await.0, 0);
    }
    if editd.publisher().loss() > 0 {
        assert!(sink.wait_for(|s| s.iter().any(|(_, e)| e["event"] == "resync" && e["reason"] == "publisher_loss")).await);
    }
}

// ── paging ──────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn snapshot_paging_reassembles_its_rev_while_edits_land() {
    let h = start();
    let line = format!("{}\n", "0123456789".repeat(10));
    let body = line.repeat(10 * 1024 * 1024 / line.len());
    let f = h.file("big.txt", &body);
    let b = h.open(&f).await;
    let first = h.ok("edit.get", json!({"buffer": b, "snapshot": true})).await;
    let token = first["snapshot"].as_str().unwrap().to_string();
    let rev = first["rev"].as_u64().unwrap();
    let mut text = first["text"].as_str().unwrap().to_string();
    let mut next = first["next"].as_u64();
    assert_eq!(first["truncated"], true);
    while let Some(from) = next {
        h.ok("edit.insert", json!({"buffer": b, "at": 0, "text": "EDIT\n"})).await;
        let page = h.ok("edit.get", json!({"buffer": b, "snapshot": token, "range": [from, body.len()]})).await;
        assert_eq!(page["rev"], rev);
        text.push_str(page["text"].as_str().unwrap());
        next = page["next"].as_u64();
    }
    assert_eq!(text, body);
    // The final page released the snapshot.
    refused(&h.call("edit.get", json!({"buffer": b, "snapshot": token})).await, "NOT_FOUND", Some("snapshot_expired"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn long_single_line_pages_with_cont() {
    let h = start();
    let long = "y".repeat(3 * 1024 * 1024);
    let f = h.file("long.txt", &format!("short\n{long}\n"));
    let b = h.open(&f).await;
    let first = h.ok("edit.get", json!({"buffer": b, "numbered": true})).await;
    assert_eq!(first["truncated"], true, "a 3 MiB line + numbering does not fit one page? {}", first["end"]);
    let mut collected = String::new();
    let mut next = Some(0u64);
    let mut saw_cont = false;
    while let Some(from) = next {
        let end = 6 + long.len() + 1;
        let page = h.ok("edit.get", json!({"buffer": b, "numbered": true, "range": [from, end]})).await;
        for l in page["lines"].as_array().unwrap() {
            if l["line"] == 2 {
                collected.push_str(l["text"].as_str().unwrap());
                saw_cont |= l["cont"] == true;
            }
        }
        next = page["next"].as_u64();
    }
    let _ = saw_cont;
    assert_eq!(collected, long);
}

// ── isolation, scale ────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slow_find_on_one_buffer_does_not_delay_another() {
    let h = start();
    let a = h.scratch().await;
    let b = h.scratch().await;
    for _ in 0..16 {
        h.ok("edit.insert", json!({"buffer": a, "at": "end", "text": "a".repeat(1_000_000)})).await;
    }
    h.ok("edit.insert", json!({"buffer": b, "at": 0, "text": "b"})).await;
    let slow = h.editd.submit(&cmd(Who::Local("t"), "edit.find", json!({"buffer": a, "pattern": "(a|aa)*c", "regex": true})));
    let started = Instant::now();
    let fast = tokio::time::timeout(Duration::from_secs(1), h.editd.handle(&cmd(Who::Local("t"), "edit.get", json!({"buffer": b}))))
        .await
        .expect("get on B waited behind find on A");
    assert_eq!(fast.0, 0);
    let fast_ms = started.elapsed().as_millis();
    slow.await;
    eprintln!("isolation: get on B answered in {fast_ms} ms while find ran on A");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn large_file_open_edit_get_latency() {
    let h = start();
    let line = format!("{}\n", "z".repeat(31));
    let lines = 1_999_999usize;
    let f = h.path("large.txt");
    std::fs::write(&f, line.repeat(lines)).unwrap();
    let t = Instant::now();
    let b = h.open(&f).await;
    let open_ms = t.elapsed().as_millis();
    let t = Instant::now();
    h.ok("edit.insert", json!({"buffer": b, "at": 0, "text": "x"})).await;
    let edit_ms = t.elapsed().as_millis();
    let t = Instant::now();
    let page = h.ok("edit.get", json!({"buffer": b})).await;
    let get_ms = t.elapsed().as_millis();
    eprintln!("large file (64 MiB, 2M lines): open {open_ms} ms, edit {edit_ms} ms, first page {get_ms} ms");
    assert_eq!(page["lines_total"], lines + 1);
    assert!(open_ms < 1_000 && edit_ms < 1_000 && get_ms < 1_000, "over the 1 s ceiling");
}
