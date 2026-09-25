//! Golden contract fixtures (ced E0 plan §4.2, Stage S).
//!
//! File names: `<verb>.<request|reply|refusal>[.<case>].json` and
//! `event.<kind>[.<case>].json`. Requests must parse into their request type;
//! replies, refusals and events must round-trip exactly (every reply field is
//! serialized, so a fixture lists all of them). An unrecognised file name
//! fails the test, so no fixture goes unchecked.

use std::path::Path;

use cosmix_edit_core::wire::*;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

fn parses<T: DeserializeOwned>(name: &str, v: &Value) {
    if let Err(e) = serde_json::from_value::<T>(v.clone()) {
        panic!("{name}: does not parse as {}: {e}", std::any::type_name::<T>());
    }
}

fn round_trips<T: DeserializeOwned + Serialize>(name: &str, v: &Value) {
    let parsed: T = serde_json::from_value(v.clone())
        .unwrap_or_else(|e| panic!("{name}: does not parse as {}: {e}", std::any::type_name::<T>()));
    let back = serde_json::to_value(&parsed).unwrap();
    assert_eq!(&back, v, "{name}: does not round-trip as {}", std::any::type_name::<T>());
}

fn split(stem: &str) -> (String, &'static str) {
    if let Some(kind) = stem.strip_prefix("event.") {
        let kind = kind.split('.').next().unwrap();
        return (kind.to_string(), "event");
    }
    for kind in ["request", "reply", "refusal"] {
        let marker = format!(".{kind}");
        if let Some(i) = stem.find(&marker) {
            let rest = &stem[i + marker.len()..];
            if rest.is_empty() || rest.starts_with('.') {
                let k: &'static str = match kind {
                    "request" => "request",
                    "reply" => "reply",
                    _ => "refusal",
                };
                return (stem[..i].to_string(), k);
            }
        }
    }
    panic!("{stem}: fixture name has no request/reply/refusal/event part");
}

fn check(name: &str, v: &Value) {
    let (verb, kind) = split(name);
    match (verb.as_str(), kind) {
        (_, "refusal") => round_trips::<Refusal>(name, v),
        (_, "event") => round_trips::<Event>(name, v),
        ("edit.ping" | "edit.info" | "edit.list", "request") => parses::<EmptyReq>(name, v),
        ("edit.open", "request") => parses::<OpenReq>(name, v),
        ("edit.close", "request") => parses::<CloseReq>(name, v),
        ("edit.save", "request") => parses::<SaveReq>(name, v),
        ("edit.reload", "request") => parses::<ReloadReq>(name, v),
        ("edit.get", "request") => parses::<GetReq>(name, v),
        ("edit.insert", "request") => parses::<InsertReq>(name, v),
        ("edit.delete", "request") => parses::<DeleteReq>(name, v),
        ("edit.replace", "request") => parses::<ReplaceReq>(name, v),
        ("edit.apply", "request") => parses::<ApplyReq>(name, v),
        ("edit.find", "request") => parses::<FindReq>(name, v),
        ("edit.select", "request") => parses::<SelectReq>(name, v),
        ("edit.cursor", "request") => parses::<CursorReq>(name, v),
        ("edit.anchor.set", "request") => parses::<AnchorSetReq>(name, v),
        ("edit.anchor.get", "request") => parses::<AnchorGetReq>(name, v),
        ("edit.anchor.clear", "request") => parses::<AnchorClearReq>(name, v),
        ("edit.undo" | "edit.redo", "request") => parses::<UndoReq>(name, v),
        ("edit.history", "request") => parses::<HistoryReq>(name, v),
        ("edit.ping", "reply") => round_trips::<PingReply>(name, v),
        ("edit.info", "reply") => round_trips::<InfoReply>(name, v),
        ("edit.list", "reply") => round_trips::<ListReply>(name, v),
        ("edit.open", "reply") => round_trips::<OpenReply>(name, v),
        ("edit.close", "reply") => round_trips::<CloseReply>(name, v),
        ("edit.save", "reply") => round_trips::<SaveReply>(name, v),
        ("edit.reload", "reply") => round_trips::<ReloadReply>(name, v),
        ("edit.get", "reply") => round_trips::<GetReply>(name, v),
        ("edit.insert" | "edit.delete" | "edit.replace" | "edit.apply", "reply") => {
            round_trips::<MutationReply>(name, v)
        }
        ("edit.find", "reply") => round_trips::<FindReply>(name, v),
        ("edit.select" | "edit.cursor", "reply") => round_trips::<SelectionsReply>(name, v),
        ("edit.anchor.set" | "edit.anchor.get", "reply") => round_trips::<AnchorsReply>(name, v),
        ("edit.anchor.clear", "reply") => round_trips::<ClearedReply>(name, v),
        ("edit.undo" | "edit.redo", "reply") => round_trips::<UndoReply>(name, v),
        ("edit.history", "reply") => round_trips::<HistoryReply>(name, v),
        ("edit.props.watch", "reply") => round_trips::<PropsWatchReply>(name, v),
        _ => panic!("{name}: no DTO mapping for ({verb}, {kind})"),
    }
}

fn fixtures() -> Vec<(String, Value)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/contract");
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("fixture dir") {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let stem = path.file_stem().unwrap().to_str().unwrap().to_string();
        let text = std::fs::read_to_string(&path).unwrap();
        let value: Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{stem}: bad JSON: {e}"));
        out.push((stem, value));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[test]
fn every_fixture_matches_its_dto() {
    let all = fixtures();
    assert!(all.len() >= 100, "expected the full fixture set, found {}", all.len());
    for (name, value) in &all {
        check(name, value);
    }
}

#[test]
fn every_verb_has_a_request_and_a_reply_fixture() {
    let names: Vec<String> = fixtures().into_iter().map(|(n, _)| n).collect();
    for (verb, _) in VERBS {
        if verb.starts_with("edit.props.") && *verb != "edit.props.watch" {
            continue; // props-core's own contract
        }
        let has = |kind: &str| names.iter().any(|n| split(n) == (verb.to_string(), kind_static(kind)));
        if *verb != "edit.props.watch" {
            assert!(has("request"), "{verb}: no request fixture");
        }
        assert!(has("reply"), "{verb}: no reply fixture");
    }
}

fn kind_static(kind: &str) -> &'static str {
    match kind {
        "request" => "request",
        "reply" => "reply",
        _ => "refusal",
    }
}

#[test]
fn every_event_kind_has_a_fixture() {
    let names: Vec<String> = fixtures().into_iter().map(|(n, _)| n).collect();
    for kind in ["edit", "cursor", "anchor", "disk", "open", "close", "resync"] {
        assert!(names.iter().any(|n| split(n) == (kind.to_string(), "event")), "event {kind}: no fixture");
    }
}
