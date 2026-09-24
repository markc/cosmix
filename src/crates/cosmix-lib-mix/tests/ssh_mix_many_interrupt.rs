//! `ssh_mix_many` stops at Ctrl-C: no ssh is spawned once the interrupt
//! flag is set.
//!
//! Its own test binary, with ONE test, because it sets the process-wide
//! interrupt flag and PATH — both global, so they must not be shared with
//! any other test. The fake `ssh` appends to a calls file on every
//! invocation; with the flag already set, the file must never appear.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use cosmix_mix::value::Value;

#[test]
fn a_set_interrupt_flag_spawns_no_ssh_and_marks_every_host_interrupted() {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "mix-many-intr-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let calls = dir.join("calls");
    let ssh = dir.join("ssh");
    std::fs::write(
        &ssh,
        format!("#!/bin/sh\necho \"$@\" >> '{}'\nexit 0\n", calls.display()),
    )
    .unwrap();
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
    // SAFETY: this binary runs exactly one test, so nothing reads the
    // environment concurrently.
    unsafe { std::env::set_var("PATH", &dir) };
    cosmix_mix::interrupt::INTERRUPT_FLAG
        .set(Arc::new(AtomicBool::new(true)))
        .expect("the flag is unset in a fresh test binary");

    let hosts: Vec<Value> = ["h1", "h2", "h3", "h4", "h5", "h6"]
        .iter()
        .map(|h| Value::String((*h).into()))
        .collect();
    let mut opts = indexmap::IndexMap::new();
    opts.insert("max".to_string(), Value::Number(2.0));
    let r = cosmix_mix::builtins::call_builtin(
        "ssh_mix_many",
        vec![
            Value::list(hosts),
            Value::String("print(1)".into()),
            Value::map(opts),
        ],
    )
    .expect("an interrupted fan-out is data, not a raise")
    .expect("a value");

    let Value::Map(m) = &r else {
        panic!("map expected, got {r:?}");
    };
    assert_eq!(m.len(), 6, "{r:?}");
    for (host, res) in m.iter() {
        let Value::Map(res) = res else {
            panic!("{host}: map expected");
        };
        assert!(
            matches!(res.get("interrupted"), Some(Value::Bool(true))),
            "{host}: {res:?}"
        );
        assert!(matches!(res.get("ok"), Some(Value::Bool(false))), "{host}: {res:?}");
    }
    assert!(
        !calls.exists(),
        "ssh was spawned after the interrupt: {:?}",
        std::fs::read_to_string(&calls)
    );
    let _ = std::fs::remove_dir_all(&dir);
}
