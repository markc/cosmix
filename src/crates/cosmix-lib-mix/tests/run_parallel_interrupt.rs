//! One test in its own binary: the interrupt flag is process-wide.
#![cfg(unix)]

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use cosmix_mix::value::Value;

#[test]
fn preset_interrupt_spawns_no_jobs_and_marks_every_result_interrupted() {
    let dir = std::env::temp_dir().join(format!(
        "mix-parallel-interrupt-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&dir).unwrap();
    let marker = dir.join("spawned");
    cosmix_mix::interrupt::INTERRUPT_FLAG
        .set(Arc::new(AtomicBool::new(true)))
        .expect("fresh test binary");
    // An attempted spawn of the missing executable must return PROCESS_SPAWN,
    // even if a spawned `touch` could be killed before creating its marker.
    // The second interrupt check (between fetch_add and spawn) cannot be
    // exercised deterministically by this preset-interrupt test.
    let jobs = (0..6)
        .map(|i| if i % 2 == 0 {
            Value::list(vec![Value::String("/nonexistent/cosmix-test-bin".into())])
        } else {
            Value::list(vec![
                Value::String("touch".into()),
                Value::String(marker.to_str().unwrap().into()),
            ])
        })
        .collect();
    let mut opts = indexmap::IndexMap::new();
    opts.insert("max".into(), Value::Number(2.0));
    let result = cosmix_mix::builtins::call_builtin(
        "run_parallel",
        vec![Value::list(jobs), Value::map(opts)],
    )
    .expect("interruption is data")
    .expect("result");
    let Value::List(results) = &result else { panic!("expected list") };
    assert_eq!(results.len(), 6);
    for result in results.iter() {
        let Value::Map(result) = result else { panic!("expected map") };
        assert!(matches!(result.get("ok"), Some(Value::Bool(false))));
        assert!(matches!(result.get("interrupted"), Some(Value::Bool(true))));
        assert!(matches!(result.get("error_code"), None | Some(Value::Nil)),
            "skipped jobs must not report a spawn error: {result:?}");
    }
    assert!(!marker.exists(), "a job spawned despite the preset interrupt");
    std::fs::remove_dir(&dir).unwrap();
}
