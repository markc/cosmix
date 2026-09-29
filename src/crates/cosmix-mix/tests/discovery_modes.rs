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

#[test]
fn agent_profile_adds_rules_and_promotes() {
    use std::io::Write;
    let dir = std::env::temp_dir().join(format!("mix-agent-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("tmp dir");
    let file = dir.join("agent.mix");
    let mut f = std::fs::File::create(&file).expect("write probe");
    writeln!(f, "$x = 1").unwrap();
    writeln!(f, "function f($a)").unwrap();
    writeln!(f, "  $x = 2").unwrap();
    writeln!(f, "  return $a").unwrap();
    writeln!(f, "end").unwrap();
    writeln!(f, "$l = [1]").unwrap();
    writeln!(f, "$n = push($l, 2)").unwrap();
    writeln!(f, "$f = f").unwrap();
    writeln!(f, "if \"false\" then").unwrap();
    writeln!(f, "  print(\"never\")").unwrap();
    writeln!(f, "end").unwrap();
    writeln!(f, "while true").unwrap();
    writeln!(f, "  break").unwrap();
    writeln!(f, "end").unwrap();
    writeln!(f, "print(\"$x\")").unwrap();
    drop(f);
    let path = file.to_str().unwrap();

    // Ordinary profile: no agent-only rules; D3015 stays a note.
    let (out, _) = mix(&["lint", path]);
    for code in ["MIX-E1503", "MIX-E1504", "MIX-E1505", "MIX-E1506"] {
        assert!(!out.contains(code), "ordinary profile must not emit {code}: {out}");
    }

    // Agent profile: all four rules fire as errors, `while true` stays
    // clean (the canonical event-pump idiom), and D3015 promotes from
    // note to error.
    let (out, _) = mix(&["lint", "--agent", "--json", path]);
    for code in ["MIX-E1503", "MIX-E1504", "MIX-E1505", "MIX-E1506"] {
        assert!(out.contains(code), "--agent must emit {code}: {out}");
    }
    // Exactly ONE E1505 (the if-condition), never the while-true loop.
    assert_eq!(
        out.matches("MIX-E1505").count(),
        1,
        "E1505 must fire once (the string condition), not on while true: {out}"
    );
    assert!(
        out.contains("MIX-D3015") && out.contains("\"severity\": \"error\""),
        "D3015 must promote to error under --agent: {out}"
    );
    std::fs::remove_dir_all(&dir).ok();
}
