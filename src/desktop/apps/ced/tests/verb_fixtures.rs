//! `ced.v1` golden fixtures (ced E1 plan §4.8, Stage S): every verb has a
//! request that parses and a reply that round-trips exactly; refusals
//! round-trip. An unmapped file fails the test.

use std::path::Path;

use cosmix_ced::verbs::*;
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
    assert_eq!(&serde_json::to_value(&parsed).unwrap(), v, "{name}: does not round-trip");
}

fn check(name: &str, v: &Value) {
    if name.starts_with("refusal.") {
        return round_trips::<Refusal>(name, v);
    }
    let (verb, kind) = name.rsplit_once('.').unwrap_or_else(|| panic!("{name}: bad fixture name"));
    match (verb, kind) {
        ("ced.ping" | "ced.info" | "ced.new" | "ced.tabs" | "ced.actions" | "ced.stats" | "app.describe" | "app.quit", "request") => {
            parses::<EmptyReq>(name, v)
        }
        ("ced.open", "request") => parses::<OpenReq>(name, v),
        ("ced.focus" | "ced.layout", "request") => parses::<TabSel>(name, v),
        ("ced.state", "request") => parses::<StateReq>(name, v),
        ("ced.type", "request") => parses::<TypeReq>(name, v),
        ("ced.select", "request") => parses::<SelectReq>(name, v),
        ("ced.action", "request") => parses::<ActionReq>(name, v),
        ("ced.wait", "request") => parses::<WaitReq>(name, v),
        ("ced.ping", "reply") => round_trips::<PingReply>(name, v),
        ("ced.info", "reply") => round_trips::<InfoReply>(name, v),
        ("ced.open", "reply") => round_trips::<OpenReply>(name, v),
        ("ced.new", "reply") => round_trips::<NewReply>(name, v),
        ("ced.tabs", "reply") => round_trips::<TabsReply>(name, v),
        ("ced.focus", "reply") => round_trips::<FocusReply>(name, v),
        ("ced.state", "reply") => round_trips::<StateReply>(name, v),
        ("ced.type", "reply") => round_trips::<TypeReply>(name, v),
        ("ced.select", "reply") => round_trips::<SelectReply>(name, v),
        ("ced.action", "reply") => round_trips::<ActionReply>(name, v),
        ("ced.actions", "reply") => round_trips::<ActionsReply>(name, v),
        ("ced.wait", "reply") => round_trips::<WaitReply>(name, v),
        ("ced.layout", "reply") => round_trips::<LayoutReply>(name, v),
        ("ced.stats", "reply") => round_trips::<StatsReply>(name, v),
        ("app.describe", "reply") => round_trips::<DescribeReply>(name, v),
        ("app.quit", "reply") => round_trips::<QuitReply>(name, v),
        _ => panic!("{name}: no DTO mapping"),
    }
}

fn fixtures() -> Vec<(String, Value)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/verbs");
    let mut out: Vec<(String, Value)> = std::fs::read_dir(&dir)
        .expect("fixture dir")
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
        .map(|p| {
            let name = p.file_stem().unwrap().to_string_lossy().into_owned();
            let v = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap_or_else(|e| panic!("{name}: {e}"));
            (name, v)
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[test]
fn every_fixture_matches_its_dto() {
    for (name, v) in fixtures() {
        check(&name, &v);
    }
}

#[test]
fn every_verb_has_a_request_and_a_reply() {
    let names: Vec<String> = fixtures().into_iter().map(|(n, _)| n).collect();
    for (verb, _) in VERBS {
        for kind in ["request", "reply"] {
            assert!(names.contains(&format!("{verb}.{kind}")), "{verb}: no {kind} fixture");
        }
    }
}
