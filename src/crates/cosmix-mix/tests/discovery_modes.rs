//! The discovery surface an agent meets: bus statements resolve like
//! builtins (D3), `mix config` reports the modes that gate a run (B14),
//! `mix status` reports real uptime in one-shot mode, and a discarded
//! pure transform is a visible warning (D3's must_use half).

use std::process::Command;

fn mix(args: &[&str]) -> (String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_mix"))
        .args(args)
        .env("MIX_STATS", "off")
        .env_remove("MIXRC")
        .output()
        .expect("run mix");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn bus_statements_resolve_in_the_discovery_surface() {
    for name in ["send", "emit", "address", "on", "reply", "subscribe", "unsubscribe", "port_exists", "bus_reconnect", "noded_register"] {
        let (out, err) = mix(&["builtins", name]);
        assert!(
            !out.contains("unknown builtin") && !err.contains("unknown builtin"),
            "mix builtins {name} must resolve (D3); got: {out}{err}"
        );
    }
    // send carries the rc bands, so an agent reading one line learns the
    // whole reply contract.
    let (out, _) = mix(&["builtins", "send"]);
    assert!(out.contains("$rc"), "send's description must name the rc bands: {out}");
}

#[test]
fn config_reports_the_agent_modes() {
    let (out, _) = mix(&["config"]);
    assert!(out.contains("arity:"), "config must report the arity mode: {out}");
    assert!(out.contains("login shell:"), "config must report login-shell mode: {out}");
    assert!(
        out.contains("(exists)") || out.contains("(missing)"),
        "config must say whether the rc file exists: {out}"
    );
}

#[test]
fn status_reports_uptime_not_a_question_mark() {
    let (out, _) = mix(&["status"]);
    assert!(
        !out.contains("uptime:    ?"),
        "one-shot status must report real uptime (START_TIME init): {out}"
    );
}

#[test]
fn discarded_pure_transform_is_a_visible_warning() {
    use std::io::Write;
    let dir = std::env::temp_dir().join(format!("mix-w2201-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("tmp dir");
    let file = dir.join("discard.mix");
    let mut f = std::fs::File::create(&file).expect("write probe");
    writeln!(f, "upper(\"a\")").unwrap();
    writeln!(f, "$s = upper(\"b\")").unwrap();
    drop(f);
    let (out, _) = mix(&["lint", file.to_str().unwrap()]);
    assert!(
        out.contains("MIX-W2201") && out.contains("pure transform"),
        "a discarded pure transform must warn (D3 must_use half): {out}"
    );
    std::fs::remove_dir_all(&dir).ok();
}
