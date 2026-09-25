//! net_watch/audio_watch through Evaluator::run_event_pump. Its own test
//! binary because the audio test puts a pactl stand-in first on PATH, which
//! would race with any other test in the same process.
//! Watchdog deadlines only fail hung tests; all progress is event-driven.
#![cfg(target_os = "linux")]
use cosmix_mix::{
    MixResult,
    evaluator::{Evaluator, ReservedOutcome, ServeRuntime},
    lexer::Lexer,
    parser::Parser,
    value::Value,
};
use std::{rc::Rc, time::Duration};

struct Runtime;
impl ServeRuntime for Runtime {
    fn handle_reserved(
        &self,
        _: &str,
        _: Option<&str>,
        _: &str,
        _: &[(&str, Option<&str>)],
        _: bool,
    ) -> Option<ReservedOutcome> {
        None
    }
}

async fn exec(eval: &mut Evaluator, source: &str) -> MixResult<Value> {
    let tokens = Lexer::new(source).tokenize()?;
    let stmts = Parser::new(tokens, source).parse_program()?;
    eval.execute(&stmts).await
}


const FAKE_PACTL: &str = r#"#!/bin/sh
# Stand-in for `pactl subscribe`, checking what audio_watch hands it.
[ "$1" = subscribe ] || exit 64
[ "$LC_ALL" = C ] || exit 65
[ "$XDG_RUNTIME_DIR" = /nonexistent-mix-audio-runtime ] || exit 66
printf "Event 'new' on client #9\nEvent 'change' on sink #5\n"
[ -n "$MIX_FAKE_PACTL_EXIT" ] && exit "$MIX_FAKE_PACTL_EXIT"
exec sleep 3600
"#;

async fn next_batch(e: &mut Evaluator) -> Value {
    e.set_global("seen", Value::Nil);
    tokio::time::timeout(Duration::from_secs(5), e.run_event_pump())
        .await
        .expect("audio event deadline")
        .unwrap();
    e.get_global("seen").unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn audio_watch_owns_a_subscription_child_and_reports_its_exit() {
    let dir = std::env::temp_dir().join(format!("mix-fake-pactl-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let pactl = dir.join("pactl");
    std::fs::write(&pactl, FAKE_PACTL).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&pactl, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    // SAFETY: the only test in this binary that reads or writes the
    // environment, and no other thread is running yet.
    unsafe { std::env::set_var("PATH", path) };

    let mut e = Evaluator::new();
    e.set_serve_runtime(Rc::new(Runtime));
    exec(
        &mut e,
        r#"
        $seen = nil
        on audio.changed
            $seen = $event.args
            quit()
        end
        $h = audio_watch({runtime_dir: "/nonexistent-mix-audio-runtime"})
    "#,
    )
    .await
    .unwrap();
    let batch = next_batch(&mut e).await.to_mix_string();
    assert!(!batch.contains("closed"), "stand-in refused its setup: {batch}");
    assert!(batch.contains("facility: sink"), "{batch}");
    assert!(!batch.contains("client"), "client events are filtered: {batch}");
    exec(&mut e, "audio_unwatch($h)").await.unwrap();

    // A stream that ends by itself is reported once, as closed.
    unsafe { std::env::set_var("MIX_FAKE_PACTL_EXIT", "3") };
    exec(
        &mut e,
        "$h = audio_watch({runtime_dir: \"/nonexistent-mix-audio-runtime\"})",
    )
    .await
    .unwrap();
    let mut closed = String::new();
    for _ in 0..3 {
        let batch = next_batch(&mut e).await.to_mix_string();
        if batch.contains("closed") {
            closed = batch;
            break;
        }
    }
    assert!(closed.contains("AUDIO_SOURCE_EXITED"), "{closed}");
    assert!(closed.contains("exit_code: 3"), "{closed}");
    exec(&mut e, "audio_unwatch($h)").await.unwrap();
    e.close_native_events();
    let _ = std::fs::remove_dir_all(dir);
}
