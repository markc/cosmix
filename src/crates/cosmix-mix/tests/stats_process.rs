use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn temp_root(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!("mix-stats-process-{}-{label}", std::process::id()))
}

fn command(state: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_mix"));
    command
        .env("XDG_STATE_HOME", state)
        .env("HOME", state)
        .arg("--no-prelude");
    command
}

fn buckets(state: &Path) -> Vec<serde_json::Value> {
    let content = std::fs::read_to_string(state.join("mix/current.json")).unwrap();
    serde_json::from_str::<serde_json::Value>(&content).unwrap()["buckets"]
        .as_array()
        .unwrap()
        .clone()
}

fn current_doc(state: &Path) -> serde_json::Value {
    let content = std::fs::read_to_string(state.join("mix/current.json")).unwrap();
    serde_json::from_str(&content).unwrap()
}

#[test]
fn records_c_script_and_stdin_modes_with_basename_only() {
    let state = temp_root("modes");
    let _ = std::fs::remove_dir_all(&state);
    std::fs::create_dir_all(&state).unwrap();

    let output = command(&state)
        .args(["-c", "print(length([1]))"])
        .output()
        .unwrap();
    assert!(output.status.success());

    let scripts = state.join("private/source");
    std::fs::create_dir_all(&scripts).unwrap();
    let script = scripts.join("mode-fixture.mix");
    std::fs::write(&script, "print(length([1]))\n").unwrap();
    let output = command(&state).arg(&script).output().unwrap();
    assert!(output.status.success());

    let mut child = command(&state)
        .arg("-")
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"print(length([1]))\n")
        .unwrap();
    assert!(child.wait().unwrap().success());

    let mut child = Command::new(env!("CARGO_BIN_EXE_mix"))
        .env("XDG_STATE_HOME", &state)
        .env("HOME", &state)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"print(length([1]))\nexit 0\n")
        .unwrap();
    assert!(child.wait().unwrap().success());

    let buckets = buckets(&state);
    assert!(buckets.iter().any(|b| b["mode"] == "c"));
    assert!(buckets.iter().any(|b| b["mode"] == "stdin"));
    assert!(buckets.iter().any(|b| b["mode"] == "interactive"));
    let script_bucket = buckets.iter().find(|b| b["mode"] == "script").unwrap();
    assert_eq!(script_bucket["script"], "mode-fixture.mix");
    assert!(
        !serde_json::to_string(script_bucket)
            .unwrap()
            .contains("private/source")
    );
    let _ = std::fs::remove_dir_all(&state);
}

#[test]
fn disabled_values_create_no_stats_state() {
    for (index, value) in ["off", "FALSE", "0"].into_iter().enumerate() {
        let state = temp_root(&format!("disabled-{index}"));
        let _ = std::fs::remove_dir_all(&state);
        std::fs::create_dir_all(&state).unwrap();
        let output = command(&state)
            .env("MIX_STATS", value)
            .args(["-c", "print(length([1]))"])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(!state.join("mix").exists(), "MIX_STATS={value}");
        let _ = std::fs::remove_dir_all(&state);
    }
}

#[test]
fn disabled_repl_stats_command_prints_the_disabled_message() {
    let state = temp_root("disabled-repl");
    let _ = std::fs::remove_dir_all(&state);
    std::fs::create_dir_all(&state).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_mix"))
        .env("XDG_STATE_HOME", &state)
        .env("HOME", &state)
        .env("MIX_STATS", "off")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"mix stats\nexit\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("Usage statistics — disabled (MIX_STATS)")
    );
    assert!(!state.join("mix").exists());
    let _ = std::fs::remove_dir_all(&state);
}

#[test]
fn records_optimised_builtin_expression_keywords_aliases_and_command_lists() {
    let state = temp_root("tracking-gaps");
    let _ = std::fs::remove_dir_all(&state);
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(state.join(".mixrc"), "alias ll = \"printf alias\"\n").unwrap();
    let code = "$m = {a: []}\n\
                $m.a = push($m.a, 1)\n\
                $f = function($x) = $x\n\
                $out = sh \"true\"\n\
                $reply = send \"missing\" ping";
    assert!(
        command(&state)
            .args(["-c", code])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        command(&state)
            .args(["-i", "-c", "ll"])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        command(&state)
            .args(["-c", "printf x | cat"])
            .status()
            .unwrap()
            .success()
    );
    let chain = command(&state)
        .args(["-c", "true && false"])
        .status()
        .unwrap();
    assert_eq!(chain.code(), Some(1));

    let doc = current_doc(&state);
    assert_eq!(doc["builtins"]["push"], 1);
    assert_eq!(doc["keywords"]["function"], 1);
    assert_eq!(doc["keywords"]["sh"], 1);
    assert_eq!(doc["keywords"]["send"], 1);
    assert_eq!(doc["aliases"]["ll"], 1);
    assert!(doc["commands"]["printf"].as_u64().unwrap() >= 2);
    assert_eq!(doc["commands"]["true"], 1);
    assert_eq!(doc["commands"]["false"], 1);
    let _ = std::fs::remove_dir_all(&state);
}

#[test]
fn sourced_shell_tracks_expanded_heads_for_each_executed_branch() {
    let state = temp_root("sourced-command-tracking");
    let _ = std::fs::remove_dir_all(&state);
    std::fs::create_dir_all(&state).unwrap();
    let sourced = state.join("commands.mix");
    std::fs::write(
        &sourced,
        "alias sx = \"printf %s\"\nsx x; true && false || printf recovered\n",
    )
    .unwrap();
    let outer = state.join("outer.mix");
    std::fs::write(
        &outer,
        format!("source \"{}\"\n", sourced.to_string_lossy()),
    )
    .unwrap();

    assert!(command(&state).arg(&outer).status().unwrap().success());

    let doc = current_doc(&state);
    assert_eq!(doc["commands"]["printf"], 2);
    assert_eq!(doc["commands"]["true"], 1);
    assert_eq!(doc["commands"]["false"], 1);
    assert!(doc["commands"].get("sx").is_none());
    let _ = std::fs::remove_dir_all(&state);
}

#[test]
fn report_windows_reject_noncanonical_arguments() {
    let state = temp_root("invalid-windows");
    let _ = std::fs::remove_dir_all(&state);
    std::fs::create_dir_all(&state).unwrap();
    let since = command(&state)
        .args(["stats", "since", "2026-8-1"])
        .output()
        .unwrap();
    assert!(!since.status.success());
    let week = command(&state)
        .args(["stats", "week", "definitely-not-a-week"])
        .output()
        .unwrap();
    assert!(!week.status.success());
    let _ = std::fs::remove_dir_all(&state);
}

#[test]
fn hostile_persisted_timestamp_report_finishes_within_two_seconds() {
    let state = temp_root("hostile-timestamp");
    let _ = std::fs::remove_dir_all(&state);
    std::fs::create_dir_all(state.join("mix")).unwrap();
    std::fs::write(
        state.join("mix/current.json"),
        serde_json::json!({
            "schema_version": 2,
            "week": "2026-W34",
            "last_date": "",
            "sessions": [{
                "id": "hostile",
                "started": u64::MAX,
                "duration_secs": 0,
                "commands": 0,
                "peak_memory_kb": 0,
                "mode": "script",
                "script": "hostile.mix"
            }]
        })
        .to_string(),
    )
    .unwrap();

    let mut child = command(&state)
        .args(["stats", "all"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("stats report hung on sessions[].started = u64::MAX");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let output = child.wait_with_output().unwrap();

    assert_eq!(output.status, status);
    assert!(
        status.success() || !String::from_utf8_lossy(&output.stderr).trim().is_empty(),
        "report must succeed or fail with a clean diagnostic"
    );
    assert!(
        std::fs::read_dir(state.join("mix"))
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.ends_with(".corrupt")))
    );
    let _ = std::fs::remove_dir_all(&state);
}

#[test]
fn reports_legacy_store_migration_advisory() {
    let root = temp_root("legacy-advisory");
    let state = root.join("state");
    let source = root.join("source");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(source.join("_stats")).unwrap();
    std::fs::write(source.join("_stats/current.json"), "{}").unwrap();
    let output = command(&state)
        .env("COSMIX_SRC", &source)
        .arg("stats")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("legacy stats found"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn exit_request_flushes_and_preserves_status() {
    let state = temp_root("exit");
    let _ = std::fs::remove_dir_all(&state);
    std::fs::create_dir_all(&state).unwrap();
    let output = command(&state).args(["-c", "exit(7)"]).output().unwrap();
    assert_eq!(output.status.code(), Some(7));
    let buckets = buckets(&state);
    assert_eq!(buckets[0]["builtins"]["exit"], 1);
    let _ = std::fs::remove_dir_all(&state);
}

#[test]
fn sourced_shell_exit_flushes_and_preserves_status() {
    let state = temp_root("sourced-exit");
    let _ = std::fs::remove_dir_all(&state);
    std::fs::create_dir_all(&state).unwrap();
    let sourced = state.join("sourced.mix");
    std::fs::write(&sourced, "true\nexit 7\n").unwrap();
    let outer = state.join("outer.mix");
    std::fs::write(
        &outer,
        format!("source \"{}\"\n", sourced.to_string_lossy()),
    )
    .unwrap();
    let output = command(&state).arg(&outer).output().unwrap();
    assert_eq!(output.status.code(), Some(7));
    assert!(state.join("mix/current.json").exists());
    let _ = std::fs::remove_dir_all(&state);
}

#[test]
fn stats_failure_never_changes_script_status() {
    let root = temp_root("invalid-state");
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_file(&root);
    std::fs::write(&root, "not a directory").unwrap();
    let output = command(&root).args(["-c", "print(42)"]).output().unwrap();
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "42");
    let _ = std::fs::remove_file(&root);
}
