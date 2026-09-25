//! ced E1d (plan §7.2): the mirror driven end to end against the fake editd
//! over the real `cosmix_edit_core::Buffer`.
//!
//! - Every fixture in `tests/fixtures/mirror/` is replayed through a real
//!   [`Mirror`]: client steps drive the mirror, `send` steps compare the exact
//!   request the mirror emitted with the frozen body and hand it to the fake,
//!   reads are answered at once, and every declared client expectation is
//!   checked (convergence, conflicts, detached copy, notices, idle, meta).
//! - Proptest properties P1/P2/P3/P5/P6 over random interleavings of mirror
//!   typing (multibyte, CRLF, multi-item transactions), agent ops (insert,
//!   delete, replace, apply, agent-lane undo), mirror undo, random delivery
//!   orders that respect event order, dropped events/replies, lost requests,
//!   deadlines and dedup eviction.
//!
//! Throughout, the deltas the mirror emits are replayed onto a shadow string:
//! between resyncs the shadow must equal the mirror's text (the `ViewDelta`
//! contract the editor side relies on).

use std::collections::{HashMap, HashSet};
use std::path::Path;

use cosmix_edit_client::fake::{FakeEditd, FakeReply};
use cosmix_edit_client::mirror::{self, LaneArg, Mirror, Phase, ServerOp, Step};
use cosmix_edit_client::types::{DeltaKind, Intent, LocalEdit, Notice, OpIdGen, Outgoing};
use cosmix_edit_core::anchor::Selection;
use cosmix_edit_core::text::Text;
use cosmix_edit_core::wire;
use proptest::prelude::*;
use serde_json::{Value, json};

const EPOCH: &str = "0000e1e1";

fn text_of(t: &Text) -> String {
    let mut s = String::new();
    t.read(0..t.len(), &mut s);
    s
}

/// `{"repeat": [unit, n]}` anywhere in a fixture value → the repeated string.
fn expand(v: &Value) -> Value {
    match v {
        Value::Object(o) => {
            if o.len() == 1
                && let Some(Value::Array(r)) = o.get("repeat")
                && let (Some(u), Some(n)) = (r.first().and_then(Value::as_str), r.get(1).and_then(Value::as_u64))
            {
                return Value::String(u.repeat(n as usize));
            }
            Value::Object(o.iter().map(|(k, v)| (k.clone(), expand(v))).collect())
        }
        Value::Array(a) => Value::Array(a.iter().map(expand).collect()),
        _ => v.clone(),
    }
}

fn to_result(r: &FakeReply) -> Result<Value, wire::Refusal> {
    if r.rc == 0 { Ok(r.body.clone()) } else { Err(serde_json::from_value(r.body.clone()).expect("refusal body")) }
}

fn is_read(verb: &str) -> bool {
    matches!(verb, "edit.get" | "edit.history" | "edit.list" | "edit.open")
}

/// Something the server produced that the client has not seen yet.
#[derive(Debug, Clone)]
enum Pend {
    Event(wire::Event),
    Reply { op_id: String, reply: FakeReply },
}

fn event_seq(e: &wire::Event) -> u64 {
    match e {
        wire::Event::Edit(e) => e.event_seq,
        wire::Event::Cursor(e) => e.event_seq,
        wire::Event::Anchor(e) => e.event_seq,
        wire::Event::Disk(e) => e.event_seq,
        wire::Event::Open(e) => e.event_seq,
        wire::Event::Close(e) => e.event_seq,
        wire::Event::Resync(e) => e.event_seq,
    }
}

/// The client and the server with a scriptable network between them.
struct Net {
    fake: FakeEditd,
    bid: String,
    path: Option<String>,
    mirror: Mirror,
    ids: OpIdGen,
    /// Writes the mirror emitted that have not been "sent" yet.
    outbox: Vec<Outgoing>,
    /// Server output not yet delivered, in server order.
    pending: Vec<Pend>,
    /// Requests lost on the way to the server (they may still arrive).
    lost: Vec<Outgoing>,
    last_seq: Option<u64>,
    notices: Vec<Notice>,
    /// `edit.get snapshot:true` requests after the initial bootstrap.
    snapshots: usize,
    /// The editor-side replay of every delta (`None` right after a resync).
    shadow: Option<String>,
    /// Echo timers the mirror handed out (op ids), not fired yet.
    echo_timers: Vec<String>,
    what: String,
}

impl Net {
    fn new(initial: &str, path: Option<&str>, run_id: u32, fake_cfg: Option<&Value>) -> Net {
        let mut fake = FakeEditd::new(EPOCH);
        if let Some(f) = fake_cfg {
            if let Some(n) = f.get("max_event_insert").and_then(Value::as_u64) {
                fake.max_event_insert = n as usize;
            }
            if let Some(n) = f.get("history_elide_over").and_then(Value::as_u64) {
                fake.history_elide_over = n as usize;
            }
        }
        let bid = fake.create(path, initial);
        let open_args = match path {
            Some(p) => json!({"path": p}),
            None => json!({}),
        };
        let open = if path.is_some() {
            fake.handle("local:ced", "edit.open", &open_args)
        } else {
            // A scratch buffer: build its open reply from the list row.
            let list: wire::ListReply = serde_json::from_value(fake.handle("local:ced", "edit.list", &json!({})).body).unwrap();
            let row = &list.buffers[0];
            FakeReply {
                rc: 0,
                body: serde_json::to_value(wire::OpenReply {
                    buffer: row.buffer.clone(),
                    epoch: list.epoch.clone(),
                    path: None,
                    opened_as: None,
                    name: None,
                    language: row.language.clone(),
                    rev: row.rev,
                    lines: row.lines,
                    bytes: row.bytes,
                    eol: wire::Eol::Lf,
                    bom: false,
                    disk: wire::DiskState::None,
                    reopened: true,
                    created: false,
                    recovery_id: row.recovery_id.clone(),
                    recovered: false,
                    recovered_from: None,
                })
                .unwrap(),
            }
        };
        assert_eq!(open.rc, 0, "open: {}", open.body);
        let open: wire::OpenReply = serde_json::from_value(open.body).unwrap();
        let (mirror, step) = Mirror::bootstrap(&open).expect("bootstrap");
        let mut net = Net {
            fake,
            bid,
            path: path.map(str::to_string),
            mirror,
            ids: OpIdGen::new(run_id),
            outbox: Vec::new(),
            pending: Vec::new(),
            lost: Vec::new(),
            last_seq: Some(0),
            notices: Vec::new(),
            snapshots: 0,
            shadow: Some(String::new()),
            echo_timers: Vec::new(),
            what: "bootstrap".into(),
        };
        net.process(step);
        net.snapshots = 0;
        net.settle_shadow();
        assert!(matches!(net.mirror.phase(), Phase::Live), "bootstrap did not go Live");
        assert_eq!(text_of(net.mirror.text()), initial);
        net
    }

    fn collect(&mut self) {
        for e in self.fake.take_events() {
            self.pending.push(Pend::Event(e));
        }
    }

    fn shadow_apply(&mut self, step: &Step) {
        for d in &step.deltas {
            if d.kind == DeltaKind::Resync {
                self.shadow = None;
                continue;
            }
            if let Some(s) = self.shadow.as_mut() {
                for e in &d.edits {
                    assert!(
                        e.offset + e.delete <= s.len() && s.is_char_boundary(e.offset) && s.is_char_boundary(e.offset + e.delete),
                        "{}: delta edit {e:?} does not fit the shadow ({} bytes)",
                        self.what,
                        s.len()
                    );
                    s.replace_range(e.offset..e.offset + e.delete, &e.insert);
                }
            }
        }
    }

    /// After a top-level action: the shadow must match (or restart after a resync).
    fn settle_shadow(&mut self) {
        let view = text_of(self.mirror.text());
        match &self.shadow {
            Some(s) => assert_eq!(s, &view, "{}: the delta replay diverged from the mirror text", self.what),
            None => self.shadow = Some(view),
        }
    }

    /// Consume a mirror step in call order: replay its deltas, answer reads at
    /// once (recursively, so deltas stay in the order the mirror made them),
    /// queue writes, drain the scheduler.
    fn process(&mut self, step: Step) {
        self.shadow_apply(&step);
        self.notices.extend(step.notices.iter().cloned());
        for o in step.out {
            if is_read(&o.verb) {
                let args: Value = serde_json::from_str(&o.body).unwrap();
                if o.verb == "edit.get" && args.get("snapshot") == Some(&Value::Bool(true)) {
                    self.snapshots += 1;
                }
                let reply = self.fake.handle("local:ced", &o.verb, &args);
                self.collect();
                let id = o.op_id.clone().expect("reads carry a correlation id");
                let s = self.mirror.on_reply(&id, to_result(&reply));
                self.process(s);
            } else {
                self.outbox.push(o);
            }
        }
        if let Some(ms) = self.mirror.take_retry_timer() {
            assert!(ms <= 5_000);
            let s = self.mirror.on_retry();
            self.process(s);
        }
        // Echo timers fire only when a fixture says so (`echo_timeout`).
        if let Some((id, ms)) = self.mirror.take_echo_timer() {
            assert!(ms <= 5_000);
            self.echo_timers.push(id);
        }
        while let Some(o) = self.mirror.next_outgoing() {
            assert!(!is_read(&o.verb), "the scheduler only sends writes");
            self.outbox.push(o);
        }
    }

    /// Hand the oldest unsent write to the server (or lose it).
    fn send_next(&mut self, lose: bool, override_args: Option<Value>) -> Outgoing {
        assert!(!self.outbox.is_empty(), "{}: the mirror has nothing to send", self.what);
        let o = self.outbox.remove(0);
        if lose {
            self.lost.push(o.clone());
            return o;
        }
        let args = override_args.unwrap_or_else(|| serde_json::from_str(&o.body).unwrap());
        let reply = self.fake.handle("local:ced", &o.verb, &args);
        self.collect();
        self.pending.push(Pend::Reply { op_id: o.op_id.clone().unwrap(), reply });
        o
    }

    fn arrive(&mut self, op_id: &str) {
        let i = self.lost.iter().position(|o| o.op_id.as_deref() == Some(op_id)).expect("a lost request");
        let o = self.lost.remove(i);
        let reply = self.fake.handle("local:ced", &o.verb, &serde_json::from_str(&o.body).unwrap());
        self.collect();
        // Nobody is waiting for this copy's reply any more.
        self.pending.push(Pend::Reply { op_id: o.op_id.unwrap(), reply });
    }

    fn agent(&mut self, caller: &str, verb: &str, args: &Value) -> FakeReply {
        let r = self.fake.handle(caller, verb, args);
        self.collect();
        r
    }

    fn deliver_event_seq(&mut self, seq: u64) {
        let i = self
            .pending
            .iter()
            .position(|p| matches!(p, Pend::Event(e) if event_seq(e) == seq))
            .unwrap_or_else(|| panic!("{}: no pending event {seq}", self.what));
        let Pend::Event(ev) = self.pending.remove(i) else { unreachable!() };
        self.deliver_event(ev);
    }

    /// What the controller does with a topic delivery: global `event_seq`
    /// gap → `suspect()`; then route the event to the mirror.
    fn deliver_event(&mut self, ev: wire::Event) {
        let seq = event_seq(&ev);
        if let Some(last) = self.last_seq
            && seq != last + 1
        {
            let s = self.mirror.suspect();
            self.process(s);
        }
        self.last_seq = Some(seq);
        let s = self.mirror.on_event(&ev);
        self.process(s);
    }

    fn drop_event_seq(&mut self, seq: u64) {
        let i = self
            .pending
            .iter()
            .position(|p| matches!(p, Pend::Event(e) if event_seq(e) == seq))
            .unwrap_or_else(|| panic!("{}: no pending event {seq}", self.what));
        self.pending.remove(i);
    }

    fn reply_index(&self, op_id: &str) -> Option<usize> {
        self.pending.iter().position(|p| matches!(p, Pend::Reply { op_id: id, .. } if id == op_id))
    }

    fn deliver_reply(&mut self, op_id: &str) {
        let i = self.reply_index(op_id).unwrap_or_else(|| panic!("{}: no pending reply for {op_id}", self.what));
        let Pend::Reply { op_id, reply } = self.pending.remove(i) else { unreachable!() };
        let s = self.mirror.on_reply(&op_id, to_result(&reply));
        self.process(s);
    }

    fn drop_reply(&mut self, op_id: &str) {
        let i = self.reply_index(op_id).unwrap_or_else(|| panic!("{}: no pending reply for {op_id}", self.what));
        self.pending.remove(i);
    }

    fn deadline(&mut self, op_id: &str) {
        let s = self.mirror.on_deadline(op_id);
        self.process(s);
    }

    /// The echo timer the mirror handed out for `op_id` fires.
    fn echo_timeout(&mut self, op_id: &str) {
        let i = self.echo_timers.iter().position(|t| t == op_id);
        let i = i.unwrap_or_else(|| panic!("{}: no echo timer for {op_id} (have {:?})", self.what, self.echo_timers));
        self.echo_timers.remove(i);
        let s = self.mirror.on_echo_timeout(op_id);
        self.process(s);
    }

    /// Deliver everything pending, in server order.
    fn deliver_all(&mut self) {
        while !self.pending.is_empty() {
            match self.pending.remove(0) {
                Pend::Event(ev) => self.deliver_event(ev),
                Pend::Reply { op_id, reply } => {
                    let s = self.mirror.on_reply(&op_id, to_result(&reply));
                    self.process(s);
                }
            }
        }
    }

    /// The daemon restarted with recovery; the controller reopens the path.
    fn epoch_change(&mut self, new_epoch: &str, text: Option<&str>) {
        self.fake.restart(new_epoch, text);
        self.bid = self.fake.buffers().into_iter().next().expect("restored buffer");
        self.pending.clear();
        self.last_seq = None;
        let s = self.mirror.epoch_changed();
        self.process(s);
        let path = self.path.clone().expect("reattach fixtures carry a path");
        let open = self.fake.handle("local:ced", "edit.open", &json!({"path": path}));
        assert_eq!(open.rc, 0, "reopen: {}", open.body);
        let open: wire::OpenReply = serde_json::from_value(open.body).unwrap();
        let s = self.mirror.reattach(&open);
        self.process(s);
    }

    fn server_text(&self) -> String {
        self.fake.text(&self.bid)
    }

    fn has_message(&self, text: &str) -> bool {
        self.notices.iter().any(|n| matches!(n, Notice::Message { text: t, .. } if t == text))
    }
}

// ── fixtures ────────────────────────────────────────────────────────────────

fn fixtures() -> Vec<(String, Value)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mirror");
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("fixture dir") {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let name = path.file_stem().unwrap().to_string_lossy().into_owned();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        out.push((name, v));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn intent_of(v: Option<&Value>) -> Intent {
    match v {
        None => Intent::ui(1),
        Some(Value::String(s)) if s == "ui" => Intent::ui(1),
        Some(v) => Intent::bus(1, v.get("bus").and_then(Value::as_str).expect("intent {bus: caller_key}")),
    }
}

fn lane_of(v: &Value) -> LaneArg {
    match v.get("lane").and_then(Value::as_str).unwrap_or("own") {
        "own" => LaneArg::Own,
        "*" => LaneArg::Any,
        l => LaneArg::Lane(l.to_string()),
    }
}

fn server_op_of(v: &Value) -> ServerOp {
    let (k, a) = v.as_object().and_then(|o| o.iter().next()).expect("op");
    match k.as_str() {
        "undo" => ServerOp::Undo { lane: lane_of(a) },
        "redo" => ServerOp::Redo { lane: lane_of(a) },
        "save" => ServerOp::Save { path: a.get("path").and_then(Value::as_str).map(str::to_string), force: false },
        "reload" => ServerOp::Reload { force: a.get("force").and_then(Value::as_bool).unwrap_or(false) },
        other => panic!("unknown server op {other}"),
    }
}

fn notice_text(code: &str) -> &'static str {
    match code {
        "undo_incomplete" => mirror::MSG_UNDO_INCOMPLETE,
        "redo_incomplete" => mirror::MSG_REDO_INCOMPLETE,
        "replace_incomplete" => mirror::MSG_REPLACE_INCOMPLETE,
        "save_uncertain" => mirror::MSG_SAVE_UNCERTAIN,
        "double_commit" => mirror::MSG_DOUBLE_COMMIT,
        other => panic!("unknown notice code {other}"),
    }
}

fn run_fixture(name: &str, fx: &Value) {
    let initial = expand(&fx["initial"]);
    let mut net = Net::new(
        initial.as_str().unwrap(),
        fx.get("path").and_then(Value::as_str),
        fx.get("run_id").and_then(Value::as_u64).unwrap_or(1) as u32,
        fx.get("fake"),
    );
    // Fixture id → op_id.
    let mut ids: HashMap<String, String> = HashMap::new();
    for (i, s) in fx["steps"].as_array().unwrap().iter().enumerate() {
        let (kind, body) = s.as_object().unwrap().iter().next().unwrap();
        net.what = format!("{name}: step {i} ({kind})");
        match kind.as_str() {
            "local" => {
                let items = body["edit"]["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|it| {
                        let a = it.as_array().unwrap();
                        let s = a[0].as_u64().unwrap() as usize;
                        let e = a[1].as_u64().unwrap() as usize;
                        (s..e, expand(&a[2]).as_str().unwrap().to_string())
                    })
                    .collect();
                let coalesce = body["edit"]["coalesce"].as_bool().unwrap_or(false);
                ids.insert(body["id"].as_str().unwrap().to_string(), net.ids.clone().next_id());
                let e = LocalEdit { items, coalesce, caret_after: Selection { anchor: 0, head: 0 } };
                let step = net.mirror.local_edit(e, intent_of(body.get("intent")), &mut net.ids).unwrap_or_else(|e| panic!("{}: {e:?}", net.what));
                net.process(step);
            }
            "server_op" => {
                ids.insert(body["id"].as_str().unwrap().to_string(), net.ids.clone().next_id());
                let (step, _) = net.mirror.server_op(server_op_of(&body["op"]), intent_of(body.get("intent")), &mut net.ids);
                net.process(step);
            }
            "send" => {
                let id = body["id"].as_str().unwrap();
                let args = expand(&body["args"]);
                let want_op = args["op_id"].as_str().unwrap().to_string();
                if let Some(known) = ids.get(id) {
                    assert_eq!(known, &want_op, "{}: fixture op_id for {id}", net.what);
                } else {
                    ids.insert(id.to_string(), want_op.clone());
                }
                let got = net.outbox.first().cloned().unwrap_or_else(|| panic!("{}: the mirror sent nothing for {id}", net.what));
                assert_eq!(got.verb, body["verb"].as_str().unwrap(), "{}: verb", net.what);
                let got_body: Value = serde_json::from_str(&got.body).unwrap();
                assert_eq!(got_body, args, "{}: request body", net.what);
                assert_eq!(got.op_id.as_deref(), Some(want_op.as_str()), "{}: correlation op_id", net.what);
                let lose = body.get("lost") == Some(&Value::Bool(true));
                if body.get("truncate_reply") == Some(&Value::Bool(true)) {
                    net.fake.truncate_next_reply = true;
                }
                let over = body.get("server_args").map(expand);
                net.send_next(lose, over);
                if !lose {
                    let exp = &body["expect"];
                    let Some(Pend::Reply { reply, .. }) = net.pending.iter().rev().find(|p| matches!(p, Pend::Reply { .. })) else {
                        unreachable!()
                    };
                    if let Some(rc) = exp.get("rc").and_then(Value::as_u64) {
                        assert_eq!(reply.rc as u64, rc, "{}: rc ({})", net.what, reply.body);
                    }
                }
            }
            "arrive" => {
                let op = ids[body["id"].as_str().unwrap()].clone();
                net.arrive(&op);
            }
            "agent" => {
                let r = net.agent(body["caller"].as_str().unwrap(), body["verb"].as_str().unwrap(), &expand(&body["args"]));
                if let Some(rc) = body["expect"].get("rc").and_then(Value::as_u64) {
                    assert_eq!(r.rc as u64, rc, "{}: agent rc ({})", net.what, r.body);
                }
            }
            "evict_dedup" => net.fake.evict_dedup(),
            "set_disk" => {
                let bid = net.bid.clone();
                net.fake.set_disk_text(&bid, body["text"].as_str().unwrap());
            }
            "epoch_change" => {
                let text = body.get("text").map(expand);
                net.epoch_change(body["new_epoch"].as_str().unwrap(), text.as_ref().and_then(Value::as_str));
            }
            "deliver_event" => net.deliver_event_seq(body["seq"].as_u64().unwrap()),
            "drop_event" => net.drop_event_seq(body["seq"].as_u64().unwrap()),
            "deliver_reply" => {
                let op = ids[body.as_str().unwrap()].clone();
                net.deliver_reply(&op);
            }
            "drop_reply" => {
                let op = ids[body.as_str().unwrap()].clone();
                net.drop_reply(&op);
            }
            "deadline" => {
                let op = ids[body.as_str().unwrap()].clone();
                net.deadline(&op);
            }
            "echo_timeout" => {
                let op = ids[body.as_str().unwrap()].clone();
                net.echo_timeout(&op);
            }
            "action" => {
                let (a, _) = body.as_object().unwrap().iter().next().unwrap();
                let step = match a.as_str() {
                    "keep_mine" => net.mirror.keep_mine(Intent::ui(1), &mut net.ids),
                    "take_theirs" => net.mirror.take_theirs(),
                    other => panic!("{}: unknown action {other}", net.what),
                };
                net.process(step);
            }
            "deliver_all" => net.deliver_all(),
            other => panic!("{name}: unknown step {other}"),
        }
        net.settle_shadow();
    }

    net.what = format!("{name}: final");
    let exp = &fx["expect"];
    let view = text_of(net.mirror.text());
    let server = net.server_text();
    if let Some(t) = exp.get("server_text") {
        assert_eq!(server, expand(t).as_str().unwrap(), "{name}: server text");
    }
    if let Some(r) = exp.get("server_rev").and_then(Value::as_u64) {
        assert_eq!(net.fake.rev(&net.bid), r, "{name}: server rev");
    }
    if exp.get("converged") == Some(&Value::Bool(true)) {
        assert!(matches!(net.mirror.phase(), Phase::Live), "{name}: phase {:?}", net.mirror.phase());
        assert_eq!(view, server, "{name}: mirror text vs server text");
        assert_eq!(net.mirror.rev(), net.fake.rev(&net.bid), "{name}: mirror rev vs server rev");
        assert_eq!(net.mirror.pending(), 0, "{name}: local ops still pending");
    }
    if let Some(n) = exp.get("conflicts").and_then(Value::as_u64) {
        assert_eq!(net.mirror.conflicts().len() as u64, n, "{name}: conflicts {:?}", net.mirror.conflicts());
    }
    if let Some(texts) = exp.get("conflict_texts").and_then(Value::as_array) {
        let got: Vec<Vec<String>> = net.mirror.conflicts().iter().map(|c| c.texts.clone()).collect();
        let want: Vec<Vec<String>> = serde_json::from_value(Value::Array(texts.clone())).unwrap();
        assert_eq!(got, want, "{name}: conflict texts");
    }
    if let Some(d) = exp.get("detached").and_then(Value::as_bool) {
        assert_eq!(net.mirror.detached_copy().is_some(), d, "{name}: detached copy");
    }
    if let Some(t) = exp.get("detached_text") {
        assert_eq!(net.mirror.detached_copy().map(|d| d.text.as_str()), expand(t).as_str(), "{name}: detached text");
    }
    if let Some(codes) = exp.get("notices").and_then(Value::as_array) {
        for c in codes {
            let text = notice_text(c.as_str().unwrap());
            assert!(net.has_message(text), "{name}: notice {c} missing from {:?}", net.notices);
        }
    }
    if let Some(i) = exp.get("idle").and_then(Value::as_bool) {
        assert_eq!(net.mirror.is_idle(), i, "{name}: idle");
    }
    if let Some(d) = exp.get("meta_dirty").and_then(Value::as_bool) {
        assert_eq!(net.mirror.meta().dirty, d, "{name}: meta.dirty");
    }
    if let Some(n) = exp.get("snapshots").and_then(Value::as_u64) {
        assert_eq!(net.snapshots as u64, n, "{name}: snapshot recoveries");
    }
    if exp.get("op_ids_once") == Some(&Value::Bool(true)) {
        let mut seen = HashSet::new();
        for (rev, id) in net.fake.log_op_ids(&net.bid) {
            if let Some(id) = id {
                assert!(seen.insert(id.clone()), "{name}: op_id {id} applied twice (again at rev {rev})");
            }
        }
    }
    assert!(net.outbox.is_empty(), "{name}: unscripted requests left: {:?}", net.outbox);
}

#[test]
fn every_mirror_fixture_converges() {
    let all = fixtures();
    assert!(all.len() >= 35, "expected the full fixture set, found {}", all.len());
    for (name, fx) in &all {
        run_fixture(name, fx);
    }
}

// ── unit checks of the frozen helpers ───────────────────────────────────────

#[test]
fn pending_seq_and_inverse_round_trip() {
    use cosmix_edit_client::mirror::{Item, Pending};
    let base = "0123456789abcdefghijXYZ\n";
    let p = Pending {
        op_id: "c1-000001".into(),
        intent: Intent::ui(1),
        items: vec![
            Item { range: 10..20, text: "R".into(), deleted: "abcdefghij".into() },
            Item { range: 10..10, text: "A".into(), deleted: String::new() },
            Item { range: 0..2, text: String::new(), deleted: "01".into() },
        ],
        coalesce: false,
    };
    let mut s = base.to_string();
    for (_, e) in p.seq() {
        s.replace_range(e.offset..e.offset + e.delete, &e.insert);
    }
    assert_eq!(s, "23456789ARXYZ\n");
    for e in p.inverse() {
        s.replace_range(e.offset..e.offset + e.delete, &e.insert);
    }
    assert_eq!(s, base);
}

// ── Stage R fixes, driven directly ──────────────────────────────────────────

const DOC: &str = "/fixture/doc.txt";

/// Type "!" at 5 and take the request the scheduler sent (not yet delivered).
fn typed(net: &mut Net) -> Outgoing {
    let e = LocalEdit { items: vec![(5..5, "!".into())], coalesce: true, caret_after: Selection { anchor: 0, head: 0 } };
    let s = net.mirror.local_edit(e, Intent::ui(1), &mut net.ids).unwrap();
    net.process(s);
    net.outbox.remove(0)
}

fn refused(code: wire::ErrorCode, reason: &str) -> wire::Refusal {
    wire::Refusal { error_code: code, message: format!("refused: {reason}"), reason: Some(reason.into()), buffer: None, rev: None, context: Default::default() }
}

/// Queue a server op and hand its request to the fake; the reply is pending.
fn server(net: &mut Net, op: ServerOp) -> String {
    let (s, id) = net.mirror.server_op(op, Intent::ui(1), &mut net.ids);
    net.process(s);
    let sent = net.send_next(false, None).op_id.unwrap();
    assert_eq!(sent, id, "server_op returns the id it sends");
    id
}

#[test]
fn busy_is_given_up_after_eight_refusals() {
    // GLM M6: the identical request goes again after each backoff, but not
    // forever — then it is reverted and stashed like any other refusal.
    let mut net = Net::new("hello world\n", Some(DOC), 1, None);
    let op = typed(&mut net).op_id.unwrap();
    for k in 0..8 {
        let s = net.mirror.on_reply(&op, Err(refused(wire::ErrorCode::ResourceLimit, "busy")));
        net.process(s);
        let o = net.outbox.remove(0);
        assert_eq!(o.op_id.as_deref(), Some(op.as_str()), "busy #{k}: the identical request again");
    }
    let s = net.mirror.on_reply(&op, Err(refused(wire::ErrorCode::ResourceLimit, "busy")));
    net.process(s);
    assert!(net.outbox.is_empty(), "the ninth busy is not retried");
    assert_eq!(text_of(net.mirror.text()), "hello world\n", "the edit is reverted");
    assert_eq!(net.mirror.conflicts().len(), 1, "and stashed as a conflict");
    assert!(net.mirror.is_idle());
}

#[test]
fn a_reply_without_rev_reconciles_through_history() {
    // Opus n1: a rev-less rc 0 reply must not ack the op at the current rev.
    let mut net = Net::new("hello world\n", Some(DOC), 1, None);
    let o = typed(&mut net);
    let args: Value = serde_json::from_str(&o.body).unwrap();
    assert_eq!(net.fake.handle("local:ced", &o.verb, &args).rc, 0);
    net.collect();
    let s = net.mirror.on_reply(o.op_id.as_deref().unwrap(), Ok(json!({"buffer": net.bid})));
    net.process(s);
    assert_eq!((net.mirror.rev(), net.mirror.pending()), (1, 0), "acked through its history entry at rev 1");
    net.deliver_all();
    assert_eq!(text_of(net.mirror.text()), net.server_text());
    assert!(net.mirror.is_idle());
}

#[test]
fn a_silent_service_is_reported_once_while_reads_keep_retrying() {
    // GLM M5.
    let mut fake = FakeEditd::new(EPOCH);
    fake.create(Some(DOC), "hello\n");
    let open: wire::OpenReply = serde_json::from_value(fake.handle("local:ced", "edit.open", &json!({"path": DOC})).body).unwrap();
    let (mut m, step) = Mirror::bootstrap(&open).unwrap();
    let id = step.out[0].op_id.clone().unwrap();
    let mut silent = 0;
    for _ in 0..6 {
        let s = m.on_deadline(&id);
        assert_eq!(s.out.len(), 1, "the read goes again");
        assert_eq!(s.out[0].op_id.as_deref(), Some(id.as_str()));
        silent += s.notices.iter().filter(|n| matches!(n, Notice::Message { text, .. } if text == mirror::MSG_SERVICE_SILENT)).count();
    }
    assert_eq!(silent, 1, "one notice, not one per deadline");
}

#[test]
fn server_ops_report_their_outcome_by_op_id() {
    // Opus m4: the controller answers a Bus caller from these.
    let mut net = Net::new("hello world\n", Some(DOC), 1, None);
    let undo = server(&mut net, ServerOp::Undo { lane: LaneArg::Own });
    net.deliver_all();
    match net.mirror.take_outcomes().as_slice() {
        [(id, mirror::Outcome::Refused(r))] => {
            assert_eq!((id, r.reason.as_deref()), (&undo, Some("nothing_to_undo")));
        }
        other => panic!("undo outcome: {other:?}"),
    }
    let save = server(&mut net, ServerOp::Save { path: None, force: false });
    net.deliver_all();
    assert_eq!(net.mirror.take_outcomes(), vec![(save, mirror::Outcome::Done)]);
    // Queued behind nothing but detached before it went out: refused.
    let (s, _) = net.mirror.server_op(ServerOp::Undo { lane: LaneArg::Own }, Intent::ui(1), &mut net.ids);
    net.process(s);
    let queued = net.outbox.remove(0).op_id.unwrap();
    let _ = net.mirror.epoch_changed();
    match net.mirror.take_outcomes().as_slice() {
        [(id, mirror::Outcome::Refused(r))] => assert_eq!((id, r.reason.as_deref()), (&queued, Some("detached"))),
        other => panic!("detached outcome: {other:?}"),
    }
}

#[test]
fn saved_state_follows_the_list_row_and_never_goes_back() {
    // Opus m2: the open reply's clean guess is replaced by the service's row;
    // a row older than what ced knows is ignored.
    let mut net = Net::new("hello world\n", Some(DOC), 1, None);
    assert!(!net.mirror.meta().dirty, "the open-time guess");
    net.mirror.note_saved(None, false);
    assert!(net.mirror.meta().dirty, "another holder's unsaved edits");
    net.mirror.note_saved(Some(0), false);
    assert!(!net.mirror.meta().dirty);
    net.mirror.note_saved(None, false);
    assert!(!net.mirror.meta().dirty, "a stale row does not undo a later save");
}

#[test]
fn cursor_events_from_another_session_are_ignored() {
    // GLM NIT 4.
    let mut net = Net::new("hello world\n", Some(DOC), 1, None);
    let cursor = |epoch: &str| {
        wire::Event::Cursor(wire::CursorEvent {
            epoch: epoch.into(),
            buffer: net.bid.clone(),
            rev: 0,
            origin: "agent:x".into(),
            selections: vec![wire::OffsetSelection { anchor: 1, head: 1 }],
            event_seq: 1,
        })
    };
    let (stale, live) = (cursor("0000dead"), cursor(EPOCH));
    let _ = net.mirror.on_event(&stale);
    assert!(net.mirror.remote_cursors().is_empty());
    let _ = net.mirror.on_event(&live);
    assert_eq!(net.mirror.remote_cursors().len(), 1);
}

// ── properties (plan §7.2) ──────────────────────────────────────────────────

#[derive(Debug, Clone)]
enum Act {
    /// Mirror typing at a fraction of the view (grapheme-ish text).
    Type(u16, u8),
    /// Mirror delete of up to n chars at a fraction.
    Delete(u16, u8),
    /// Mirror multi-item "indent": insert at the start of up to 3 lines.
    Indent(u16),
    /// Mirror undo of its own lane.
    MirrorUndo,
    AgentInsert(u16, u8),
    AgentDelete(u16, u8),
    AgentReplace(u16, u8, u8),
    AgentApply(u16, u16),
    AgentUndo,
    /// Send the oldest mirror write (lost when the flag is set).
    Send(bool),
    /// Deliver the next event in order (dropped when the flag is set).
    Event(bool),
    /// Deliver the k-th pending reply (dropped when the flag is set).
    Reply(u8, bool),
    EvictDedup,
}

const SNIPPETS: &[&str] = &["a", "é", "日本", "\r\n", "👍🏽", "x\ny", "  ", "Ω", "e\u{301}", "ab"];

fn act() -> impl Strategy<Value = Act> {
    prop_oneof![
        4 => (any::<u16>(), any::<u8>()).prop_map(|(p, t)| Act::Type(p, t)),
        2 => (any::<u16>(), 1u8..4).prop_map(|(p, n)| Act::Delete(p, n)),
        1 => any::<u16>().prop_map(Act::Indent),
        1 => Just(Act::MirrorUndo),
        2 => (any::<u16>(), any::<u8>()).prop_map(|(p, t)| Act::AgentInsert(p, t)),
        1 => (any::<u16>(), 1u8..6).prop_map(|(p, n)| Act::AgentDelete(p, n)),
        1 => (any::<u16>(), 1u8..6, any::<u8>()).prop_map(|(p, n, t)| Act::AgentReplace(p, n, t)),
        1 => (any::<u16>(), any::<u16>()).prop_map(|(a, b)| Act::AgentApply(a, b)),
        1 => Just(Act::AgentUndo),
        5 => prop_oneof![9 => Just(false), 1 => Just(true)].prop_map(Act::Send),
        5 => prop_oneof![14 => Just(false), 1 => Just(true)].prop_map(Act::Event),
        4 => (any::<u8>(), prop_oneof![9 => Just(false), 1 => Just(true)]).prop_map(|(k, d)| Act::Reply(k, d)),
        1 => Just(Act::EvictDedup),
    ]
}

/// Char boundaries of `s`, including the end.
fn boundaries(s: &str) -> Vec<usize> {
    s.char_indices().map(|(i, _)| i).chain(std::iter::once(s.len())).collect()
}

fn pick(b: &[usize], frac: u16) -> usize {
    b[(frac as usize) % b.len()]
}

/// `n` chars from boundary index `i` (clamped).
fn span(b: &[usize], frac: u16, n: u8) -> std::ops::Range<usize> {
    let i = (frac as usize) % b.len();
    let j = (i + n as usize).min(b.len() - 1);
    b[i]..b[j]
}

fn run_script(initial: &str, script: &[Act]) -> Result<(), TestCaseError> {
    let mut net = Net::new(initial, Some("/prop/doc.txt"), 7, None);
    let evicted = script.iter().any(|a| matches!(a, Act::EvictDedup));
    let agent = |net: &mut Net, verb: &str, mut args: Value| {
        args["buffer"] = json!(net.bid);
        args["origin"] = json!("agent:ctl-90");
        net.agent("local:agent", verb, &args);
    };
    for (i, a) in script.iter().enumerate() {
        net.what = format!("prop step {i} {a:?}");
        let live = matches!(net.mirror.phase(), Phase::Live);
        match *a {
            Act::Type(p, t) if live => {
                let view = text_of(net.mirror.text());
                let at = pick(&boundaries(&view), p);
                let text = SNIPPETS[t as usize % SNIPPETS.len()].to_string();
                let e = LocalEdit { items: vec![(at..at, text)], coalesce: true, caret_after: Selection { anchor: 0, head: 0 } };
                let s = net.mirror.local_edit(e, Intent::ui(1), &mut net.ids).expect("typing");
                net.process(s);
            }
            Act::Delete(p, n) if live => {
                let view = text_of(net.mirror.text());
                let r = span(&boundaries(&view), p, n);
                if !r.is_empty() {
                    let e = LocalEdit { items: vec![(r, String::new())], coalesce: true, caret_after: Selection { anchor: 0, head: 0 } };
                    let s = net.mirror.local_edit(e, Intent::ui(1), &mut net.ids).expect("delete");
                    net.process(s);
                }
            }
            Act::Indent(p) if live => {
                let view = text_of(net.mirror.text());
                let starts: Vec<usize> =
                    std::iter::once(0).chain(view.match_indices('\n').map(|(i, _)| i + 1)).filter(|&i| i <= view.len()).collect();
                let first = (p as usize) % starts.len();
                let items: Vec<_> = starts[first..].iter().take(3).map(|&s| (s..s, "  ".to_string())).collect();
                let e = LocalEdit { items, coalesce: false, caret_after: Selection { anchor: 0, head: 0 } };
                let s = net.mirror.local_edit(e, Intent::ui(1), &mut net.ids).expect("indent");
                net.process(s);
            }
            Act::MirrorUndo if live => {
                let (s, _) = net.mirror.server_op(ServerOp::Undo { lane: LaneArg::Own }, Intent::ui(1), &mut net.ids);
                net.process(s);
            }
            Act::AgentInsert(p, t) => {
                let at = pick(&boundaries(&net.server_text()), p);
                agent(&mut net, "edit.insert", json!({"at": at, "text": SNIPPETS[t as usize % SNIPPETS.len()]}));
            }
            Act::AgentDelete(p, n) => {
                let r = span(&boundaries(&net.server_text()), p, n);
                if !r.is_empty() {
                    agent(&mut net, "edit.delete", json!({"range": [r.start, r.end]}));
                }
            }
            Act::AgentReplace(p, n, t) => {
                let r = span(&boundaries(&net.server_text()), p, n);
                agent(&mut net, "edit.replace", json!({"range": [r.start, r.end], "text": SNIPPETS[t as usize % SNIPPETS.len()]}));
            }
            Act::AgentApply(a, b) => {
                let bs = boundaries(&net.server_text());
                let (x, y) = (pick(&bs, a), pick(&bs, b));
                let ops = if x == y {
                    json!([{"op": "insert", "at": x, "text": "P"}, {"op": "insert", "at": x, "text": "Q"}])
                } else {
                    let (lo, hi) = (x.min(y), x.max(y));
                    json!([{"op": "insert", "at": hi, "text": "]"}, {"op": "insert", "at": lo, "text": "["}])
                };
                agent(&mut net, "edit.apply", json!({"ops": ops}));
            }
            Act::AgentUndo => {
                let bid = net.bid.clone();
                net.agent("local:agent", "edit.undo", &json!({"buffer": bid, "as": "agent:ctl-90"}));
            }
            Act::Send(lose) if !net.outbox.is_empty() => {
                net.send_next(lose, None);
            }
            Act::Event(drop) => {
                if let Some(i) = net.pending.iter().position(|p| matches!(p, Pend::Event(_))) {
                    let Pend::Event(ev) = net.pending.remove(i) else { unreachable!() };
                    if !drop {
                        net.deliver_event(ev);
                    }
                }
            }
            Act::Reply(k, drop) => {
                let replies: Vec<String> = net
                    .pending
                    .iter()
                    .filter_map(|p| match p {
                        Pend::Reply { op_id, .. } => Some(op_id.clone()),
                        _ => None,
                    })
                    .collect();
                if !replies.is_empty() {
                    let id = replies[k as usize % replies.len()].clone();
                    if drop { net.drop_reply(&id) } else { net.deliver_reply(&id) }
                }
            }
            Act::EvictDedup => net.fake.evict_dedup(),
            _ => {}
        }
        net.settle_shadow();
    }

    // Quiesce: send, deliver, then deadlines for anything whose reply or
    // request was lost, then a reconnect-style suspect for lost events.
    net.what = "quiesce".into();
    for round in 0..10_000 {
        net.what = format!("quiesce round {round}");
        let live = matches!(net.mirror.phase(), Phase::Live);
        let inflight = net.mirror.inflight().map(|i| match i {
            mirror::Inflight::Local { wire, .. } | mirror::Inflight::Server { wire, .. } => wire.op_id.clone(),
        });
        if !net.outbox.is_empty() {
            net.send_next(false, None);
        } else if !net.pending.is_empty() {
            net.deliver_all();
        } else if live && net.mirror.rev() < net.fake.rev(&net.bid) {
            // Lost events with nothing after them: the reconnect edge.
            let s = net.mirror.suspect();
            net.process(s);
        } else if let Some(op) = inflight {
            // Its reply was dropped or its request lost (nothing else is pending).
            if round % 2 == 1 && net.lost.iter().any(|o| o.op_id.as_deref() == Some(op.as_str())) {
                // Sometimes the lost copy turns up late, before the deadline.
                net.arrive(&op);
                continue;
            }
            net.deadline(&op);
        } else if !live {
            prop_assert!(false, "stuck in {:?}", net.mirror.phase());
        } else {
            break;
        }
        net.settle_shadow();
    }
    // Late copies of lost requests that a resend already answered. Dedup makes
    // them harmless — unless the fake's evict-everything hook ran in between,
    // which models more than 1,024 other mutations landing between the two
    // copies: the plan's accepted limit (§3.5), so they stay lost then.
    if evicted {
        net.lost.clear();
    }
    while let Some(o) = net.lost.pop() {
        let reply = net.fake.handle("local:ced", &o.verb, &serde_json::from_str(&o.body).unwrap());
        net.collect();
        net.pending.push(Pend::Reply { op_id: o.op_id.unwrap(), reply });
        net.deliver_all();
        while !net.outbox.is_empty() {
            net.send_next(false, None);
            net.deliver_all();
        }
        if net.mirror.rev() < net.fake.rev(&net.bid) {
            let s = net.mirror.suspect();
            net.process(s);
        }
    }
    net.settle_shadow();

    // P1 convergence.
    prop_assert!(matches!(net.mirror.phase(), Phase::Live), "phase {:?}", net.mirror.phase());
    prop_assert_eq!(text_of(net.mirror.text()), net.server_text(), "text");
    prop_assert_eq!(net.mirror.rev(), net.fake.rev(&net.bid), "rev");
    prop_assert!(net.mirror.is_idle(), "not idle");
    // P3: a doomed op is never accepted by a correct server (no desync copy).
    prop_assert!(
        !net.notices.iter().any(|n| matches!(n, Notice::DetachedCopy { .. })),
        "a desync detached copy was made: {:?}",
        net.notices
    );
    // P6 exactly once: every op_id at most once in the server log.
    let mut seen = HashSet::new();
    for (rev, id) in net.fake.log_op_ids(&net.bid) {
        if let Some(id) = id {
            prop_assert!(seen.insert(id.clone()), "op_id {} applied twice (again at rev {})", id, rev);
        }
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, max_shrink_iters: 2000, ..ProptestConfig::default() })]

    /// P1/P3/P5/P6: random interleavings with faults converge, exactly once.
    #[test]
    fn random_interleavings_converge(
        initial in prop_oneof![Just("hello world\n"), Just("é日本\r\nline two\nthree 👍🏽\n"), Just("")],
        script in proptest::collection::vec(act(), 1..60),
    ) {
        run_script(initial, &script)?;
    }

    /// P2: with no agent edits at all, typing never conflicts.
    #[test]
    fn no_spurious_conflicts_without_overlap(
        script in proptest::collection::vec(
            prop_oneof![
                3 => (any::<u16>(), any::<u8>()).prop_map(|(p, t)| Act::Type(p, t)),
                1 => (any::<u16>(), 1u8..4).prop_map(|(p, n)| Act::Delete(p, n)),
                1 => any::<u16>().prop_map(Act::Indent),
                3 => Just(Act::Send(false)),
                3 => Just(Act::Event(false)),
                2 => any::<u8>().prop_map(|k| Act::Reply(k, false)),
            ],
            1..80,
        ),
    ) {
        let initial = "one\ntwo\nthree\n";
        let mut net = Net::new(initial, Some("/prop/p2.txt"), 9, None);
        for a in &script {
            match *a {
                Act::Type(p, t) => {
                    let view = text_of(net.mirror.text());
                    let at = pick(&boundaries(&view), p);
                    let e = LocalEdit { items: vec![(at..at, SNIPPETS[t as usize % SNIPPETS.len()].into())], coalesce: true,
                                        caret_after: Selection { anchor: 0, head: 0 } };
                    let s = net.mirror.local_edit(e, Intent::ui(1), &mut net.ids).unwrap();
                    net.process(s);
                }
                Act::Delete(p, n) => {
                    let view = text_of(net.mirror.text());
                    let r = span(&boundaries(&view), p, n);
                    if !r.is_empty() {
                        let e = LocalEdit { items: vec![(r, String::new())], coalesce: true, caret_after: Selection { anchor: 0, head: 0 } };
                        let s = net.mirror.local_edit(e, Intent::ui(1), &mut net.ids).unwrap();
                        net.process(s);
                    }
                }
                Act::Indent(p) => {
                    let view = text_of(net.mirror.text());
                    let starts: Vec<usize> = std::iter::once(0).chain(view.match_indices('\n').map(|(i, _)| i + 1)).collect();
                    let first = (p as usize) % starts.len();
                    let items: Vec<_> = starts[first..].iter().take(3).map(|&s| (s..s, "  ".to_string())).collect();
                    let e = LocalEdit { items, coalesce: false, caret_after: Selection { anchor: 0, head: 0 } };
                    let s = net.mirror.local_edit(e, Intent::ui(1), &mut net.ids).unwrap();
                    net.process(s);
                }
                Act::Send(_) if !net.outbox.is_empty() => { net.send_next(false, None); }
                Act::Event(_) => {
                    if let Some(i) = net.pending.iter().position(|p| matches!(p, Pend::Event(_))) {
                        let Pend::Event(ev) = net.pending.remove(i) else { unreachable!() };
                        net.deliver_event(ev);
                    }
                }
                Act::Reply(k, _) => {
                    let replies: Vec<String> = net.pending.iter().filter_map(|p| match p {
                        Pend::Reply { op_id, .. } => Some(op_id.clone()), _ => None }).collect();
                    if !replies.is_empty() {
                        let id = replies[k as usize % replies.len()].clone();
                        net.deliver_reply(&id);
                    }
                }
                _ => {}
            }
            net.settle_shadow();
            prop_assert_eq!(net.mirror.conflicts().len(), 0);
        }
        while !net.outbox.is_empty() || !net.pending.is_empty() {
            if !net.outbox.is_empty() { net.send_next(false, None); } else { net.deliver_all(); }
        }
        prop_assert_eq!(net.mirror.conflicts().len(), 0);
        prop_assert_eq!(text_of(net.mirror.text()), net.server_text());
        prop_assert_eq!(net.mirror.rev(), net.fake.rev(&net.bid));
    }
}
