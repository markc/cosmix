//! Discovery must describe the same shapes the evaluator implements.
use serde_json::Value;
use std::process::Command;

#[test]
fn builtin_json_exposes_watch_shapes_and_managed_spawn_options() {
    let output = Command::new(env!("CARGO_BIN_EXE_mix"))
        .args(["builtins", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let rows: Vec<Value> = serde_json::from_slice(&output.stdout).unwrap();
    let row = |name: &str| rows.iter().find(|r| r["name"] == name).unwrap();
    for name in ["fs_watch", "fs_unwatch", "fs_wait"] {
        assert_eq!(row(name)["capability"], "fs-read");
        assert_eq!(row(name)["arity"]["min"], 1);
        assert_eq!(row(name)["operational_failure"], "raises");
    }
    assert_eq!(row("fs_watch")["arity"]["max"], 2);
    assert_eq!(row("fs_wait")["returns"]["shape"], "fs_watch_batch");
    assert_eq!(row("fs_wait")["effects"]["blocking"], true);
    let variants = row("spawn")["args"][1]["kind"]["any_of"]
        .as_array()
        .unwrap();
    let opts = variants
        .iter()
        .find(|v| v["shape"] == "spawn_options")
        .unwrap();
    let fields = opts["fields"].as_array().unwrap();
    assert!(
        fields
            .iter()
            .any(|f| f["name"] == "exit_event" && f["kind"]["type"] == "bool")
    );
    assert!(
        fields
            .iter()
            .any(|f| f["name"] == "tag" && f["kind"]["type"] == "string")
    );
}

#[test]
fn builtin_json_exposes_net_and_audio_source_shapes() {
    let output = Command::new(env!("CARGO_BIN_EXE_mix"))
        .args(["builtins", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let rows: Vec<Value> = serde_json::from_slice(&output.stdout).unwrap();
    let row = |name: &str| rows.iter().find(|r| r["name"] == name).unwrap();
    for name in ["net_watch", "net_unwatch", "net_state"] {
        assert_eq!(row(name)["capability"], "env", "{name}");
        assert_eq!(row(name)["operational_failure"], "raises", "{name}");
    }
    for name in ["audio_watch", "audio_unwatch"] {
        assert_eq!(row(name)["capability"], "process", "{name}");
        assert_eq!(row(name)["operational_failure"], "raises", "{name}");
    }
    assert_eq!(row("net_watch")["arity"]["min"], 0);
    assert_eq!(row("net_watch")["arity"]["max"], 1);
    assert_eq!(row("net_state")["returns"]["shape"], "net_state");
    let state = row("audio_state");
    assert_eq!(state["capability"], "process");
    assert_eq!(state["operational_failure"], "returns-result");
    assert_eq!(state["effects"]["blocking"], true);
    assert_eq!(state["returns"]["shape"], "audio_state");
    let opts = &row("audio_watch")["args"][0]["kind"];
    assert_eq!(opts["shape"], "audio_watch_options");
    assert!(
        opts["fields"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["name"] == "facilities")
    );
}

#[test]
fn invalid_net_watch_options_raise_wire_shaped_refusal() {
    let output = Command::new(env!("CARGO_BIN_EXE_mix"))
        .args([
            "-c",
            r#"try
            net_watch({events: ["route"]})
        catch $message, $error
            print(json_encode({error_code: $error.error_code, message: $error.message}))
            exit(10)
        end"#,
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(10));
    let refusal: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(refusal["error_code"], "NET_WATCH_OPTIONS");
}

#[test]
fn invalid_watch_options_raise_wire_shaped_refusal_and_nonzero_exit() {
    let output = Command::new(env!("CARGO_BIN_EXE_mix"))
        .args([
            "-c",
            r#"try
            fs_watch(".", {recursive: "not-a-bool"})
        catch $message, $error
            print(json_encode({error_code: $error.error_code, message: $error.message}))
            exit(10)
        end"#,
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(10));
    let refusal: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(refusal["error_code"], "FS_WATCH_OPTIONS");
    assert!(!refusal["message"].as_str().unwrap().is_empty());
}
