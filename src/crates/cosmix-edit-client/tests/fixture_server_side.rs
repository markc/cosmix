//! Stage S: every mirror fixture's SERVER side, executed against the fake
//! editd over the real `cosmix_edit_core::Buffer` (codex round-2 closing note
//! — fixtures must be executable, not merely parsed). See
//! `tests/fixtures/mirror/README.md` for the format.

use std::collections::HashMap;
use std::path::Path;

use cosmix_edit_client::fake::FakeEditd;
use serde_json::Value;

const EPOCH: &str = "0000e1e1";
const CLIENT_ONLY: &[&str] =
    &["local", "server_op", "deliver_event", "drop_event", "deliver_reply", "drop_reply", "deadline", "echo_timeout", "action", "deliver_all"];

fn fixtures() -> Vec<(String, Value)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mirror");
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("fixture dir") {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let name = path.file_stem().unwrap().to_string_lossy().into_owned();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap())
            .unwrap_or_else(|e| panic!("{name}: bad JSON: {e}"));
        out.push((name, v));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// `"text"` or `{"repeat": ["unit", n]}`.
fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Object(o) => {
            let r = o["repeat"].as_array().expect("repeat");
            r[0].as_str().unwrap().repeat(r[1].as_u64().unwrap() as usize)
        }
        _ => panic!("bad text {v}"),
    }
}

/// Request args with every `{"repeat": ["unit", n]}` expanded (fixture 31
/// carries 1 MiB texts this way).
fn expand(v: &Value) -> Value {
    match v {
        Value::Object(o) if o.len() == 1 && o.contains_key("repeat") => Value::String(text_of(v)),
        Value::Object(o) => Value::Object(o.iter().map(|(k, v)| (k.clone(), expand(v))).collect()),
        Value::Array(a) => Value::Array(a.iter().map(expand).collect()),
        _ => v.clone(),
    }
}

fn check_expect(name: &str, what: &str, reply: &cosmix_edit_client::fake::FakeReply, expect: &Value) {
    let Some(exp) = expect.as_object() else { return };
    if let Some(rc) = exp.get("rc") {
        assert_eq!(Some(reply.rc as u64), rc.as_u64(), "{name}: {what}: rc (body {})", reply.body);
    }
    for key in ["rev", "reason", "duplicate", "reply_truncated", "unchanged", "saved_rev"] {
        if let Some(want) = exp.get(key) {
            assert_eq!(reply.body.get(key), Some(want), "{name}: {what}: {key} (body {})", reply.body);
        }
    }
}

fn run(name: &str, fx: &Value) {
    let steps = fx["steps"].as_array().unwrap_or_else(|| panic!("{name}: no steps"));
    for s in steps {
        let o = s.as_object().unwrap_or_else(|| panic!("{name}: step is not an object: {s}"));
        assert_eq!(o.len(), 1, "{name}: a step has exactly one key: {s}");
        let kind = o.keys().next().unwrap().as_str();
        let known = CLIENT_ONLY.contains(&kind)
            || ["send", "arrive", "agent", "evict_dedup", "set_disk", "epoch_change"].contains(&kind);
        assert!(known, "{name}: unknown step kind {kind}");
    }
    if fx.get("server_side") == Some(&Value::Bool(false)) {
        return;
    }

    let mut fake = FakeEditd::new(EPOCH);
    if let Some(f) = fx.get("fake") {
        if let Some(n) = f.get("max_event_insert").and_then(Value::as_u64) {
            fake.max_event_insert = n as usize;
        }
        if let Some(n) = f.get("history_elide_over").and_then(Value::as_u64) {
            fake.history_elide_over = n as usize;
        }
    }
    let path = fx.get("path").and_then(Value::as_str);
    let mut bid = fake.create(path, &text_of(&fx["initial"]));
    assert_eq!(bid, format!("b1_{EPOCH}"), "{name}: the fixture buffer id");
    let mut lost: HashMap<String, Value> = HashMap::new();

    for (i, s) in steps.iter().enumerate() {
        let (kind, body) = s.as_object().unwrap().iter().next().unwrap();
        let what = format!("step {i} ({kind})");
        match kind.as_str() {
            "send" => {
                let id = body["id"].as_str().unwrap().to_string();
                if body.get("lost") == Some(&Value::Bool(true)) {
                    lost.insert(id, body.clone());
                    continue;
                }
                if body.get("truncate_reply") == Some(&Value::Bool(true)) {
                    fake.truncate_next_reply = true;
                }
                let args = expand(body.get("server_args").unwrap_or(&body["args"]));
                let reply = fake.handle(body["caller"].as_str().unwrap_or("local:ced"), body["verb"].as_str().unwrap(), &args);
                check_expect(name, &what, &reply, &body["expect"]);
            }
            "arrive" => {
                let id = body["id"].as_str().unwrap();
                let sent = lost.remove(id).unwrap_or_else(|| panic!("{name}: {what}: {id} was not lost"));
                let _ = fake.handle(sent["caller"].as_str().unwrap_or("local:ced"), sent["verb"].as_str().unwrap(), &expand(&sent["args"]));
            }
            "agent" => {
                let reply = fake.handle(body["caller"].as_str().unwrap(), body["verb"].as_str().unwrap(), &expand(&body["args"]));
                check_expect(name, &what, &reply, &body["expect"]);
            }
            "evict_dedup" => fake.evict_dedup(),
            "set_disk" => fake.set_disk_text(&bid, body["text"].as_str().unwrap()),
            "epoch_change" => {
                let new_epoch = body["new_epoch"].as_str().unwrap();
                fake.restart(new_epoch, body.get("text").and_then(Value::as_str));
                bid = fake.buffers().into_iter().next().expect("restored buffer");
                assert!(bid.ends_with(new_epoch), "{name}: {what}: restored id {bid}");
            }
            _ => {} // client-only
        }
        // Events are the fake's business here; the mirror harness replays them.
        let _ = fake.take_events();
    }

    let exp = &fx["expect"];
    if let Some(t) = exp.get("server_text") {
        assert_eq!(fake.text(&bid), text_of(t), "{name}: final server text");
    }
    if let Some(r) = exp.get("server_rev").and_then(Value::as_u64) {
        assert_eq!(fake.rev(&bid), r, "{name}: final server rev");
    }
    if exp.get("op_ids_once") == Some(&Value::Bool(true)) {
        let mut seen = std::collections::HashSet::new();
        for (rev, id) in fake.log_op_ids(&bid) {
            if let Some(id) = id {
                assert!(seen.insert(id.clone()), "{name}: op_id {id} applied twice (again at rev {rev})");
            }
        }
    }
}

#[test]
fn every_mirror_fixture_holds_on_the_server_side() {
    let all = fixtures();
    assert!(all.len() >= 25, "expected the Stage S fixture set, found {}", all.len());
    for (name, fx) in &all {
        run(name, fx);
    }
}
