//! Real inotify and pidfd sources dispatched through Evaluator::run_event_pump.
//! Watchdog deadlines only fail hung tests; all progress is event-driven.
#![cfg(target_os = "linux")]
use cosmix_mix::{
    MixResult,
    evaluator::{Evaluator, ReservedOutcome, ServeRuntime},
    lexer::Lexer,
    parser::Parser,
    value::Value,
};
use std::{
    path::{Path, PathBuf},
    rc::Rc,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

static NEXT: AtomicU64 = AtomicU64::new(1);
struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "mix-native-events-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&p).unwrap();
        Self(p.canonicalize().unwrap())
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Runtime;
impl ServeRuntime for Runtime {
    fn handle_reserved(
        &self,
        command: &str,
        _: Option<&str>,
        _: &str,
        _: &[(&str, Option<&str>)],
        _: bool,
    ) -> Option<ReservedOutcome> {
        (command == "RELOAD").then(|| ReservedOutcome {
            rc: 0,
            body: "{}".into(),
            quit: false,
            reload: true,
        })
    }
}

struct Bus {
    rx: tokio::sync::Mutex<
        tokio::sync::mpsc::UnboundedReceiver<cosmix_mix::evaluator::IncomingEvent>,
    >,
}
impl cosmix_mix::evaluator::BusHandler for Bus {
    fn send<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a Value,
    ) -> cosmix_mix::evaluator::BusFuture<'a, MixResult<(i32, Value)>> {
        Box::pin(async { Ok((0, Value::Nil)) })
    }
    fn emit<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a Value,
    ) -> cosmix_mix::evaluator::BusFuture<'a, MixResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn port_exists<'a>(
        &'a self,
        _: &'a str,
    ) -> cosmix_mix::evaluator::BusFuture<'a, MixResult<bool>> {
        Box::pin(async { Ok(true) })
    }
    fn next_incoming<'a>(
        &'a self,
    ) -> cosmix_mix::evaluator::BusFuture<'a, Option<cosmix_mix::evaluator::IncomingEvent>> {
        Box::pin(async { self.rx.lock().await.recv().await })
    }
}

async fn reload_request(
    e: &mut Evaluator,
    tx: &tokio::sync::mpsc::UnboundedSender<cosmix_mix::evaluator::IncomingEvent>,
) {
    e.set_global("wanted_path", Value::String("unused".into()));
    e.set_global("wanted_kind", Value::String("any".into()));
    tx.send(cosmix_mix::evaluator::IncomingEvent {
        command: "RELOAD".into(),
        body: "{}".into(),
        headers: [
            ("type".into(), "request".into()),
            ("id".into(), "reload-test".into()),
        ]
        .into(),
    })
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), e.run_event_pump())
        .await
        .unwrap()
        .unwrap();
    assert!(e.take_reload_request());
}

async fn exec(eval: &mut Evaluator, source: &str) -> MixResult<Value> {
    let tokens = Lexer::new(source).tokenize()?;
    let stmts = Parser::new(tokens, source).parse_program()?;
    eval.execute(&stmts).await
}

async fn watcher(root: &Path, recursive: bool) -> Evaluator {
    let mut e = Evaluator::new();
    e.set_serve_runtime(Rc::new(Runtime));
    e.set_global("root", Value::String(root.to_str().unwrap().into()));
    e.set_global("recursive", Value::Bool(recursive));
    exec(&mut e, r#"
        $watch = fs_watch($root, {recursive: $recursive})
        $seen = nil
        on fs.changed
            for $change in $event.args.changes
                if $change.path == $wanted_path and ($wanted_kind == "any" or $change.kind == $wanted_kind) then
                    $seen = $event.args
                    quit()
                end
            end
        end
    "#).await.unwrap();
    e
}

async fn delivered(e: &mut Evaluator, path: &Path, kind: &str) -> Value {
    e.set_global("wanted_path", Value::String(path.to_str().unwrap().into()));
    e.set_global("wanted_kind", Value::String(kind.into()));
    tokio::time::timeout(Duration::from_secs(5), e.run_event_pump())
        .await
        .expect("native event deadline")
        .unwrap();
    e.get_global("seen")
        .expect("handler stores the ordinary $event.args envelope")
}

#[tokio::test(flavor = "current_thread")]
async fn serve_atomic_replacement_and_repeated_saves_keep_file_watch_alive() {
    let d = Directory::new();
    let file = d.0.join("scene.mix");
    std::fs::write(&file, "initial").unwrap();
    let mut e = watcher(&file, false).await;
    for n in 0..4 {
        let tmp = d.0.join(format!("save-{n}"));
        std::fs::write(&tmp, format!("revision {n}")).unwrap();
        std::fs::rename(&tmp, &file).unwrap();
        let batch = delivered(&mut e, &file, "moved").await;
        let Value::Map(m) = &batch else {
            panic!("batch")
        };
        assert!(matches!(m.get("watch"), Some(Value::String(_))));
        assert!(matches!(m.get("overflow"), Some(Value::Bool(_))));
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            format!("revision {n}")
        );
    }
    std::fs::write(&file, "close-write").unwrap();
    delivered(&mut e, &file, "modified").await;
    e.close_native_events();
}

#[tokio::test(flavor = "current_thread")]
async fn serve_recursive_directory_create_move_delete_and_root_recreation() {
    let d = Directory::new();
    let root = d.0.join("scenes");
    std::fs::create_dir(&root).unwrap();
    let existing = root.join("existing");
    std::fs::create_dir(&existing).unwrap();
    let mut e = watcher(&root, true).await;
    let file = existing.join("a.mix");
    std::fs::write(&file, "a").unwrap();
    delivered(&mut e, &file, "created").await;
    let fresh = root.join("fresh");
    std::fs::create_dir(&fresh).unwrap();
    delivered(&mut e, &fresh, "created").await;
    let nested = fresh.join("b.mix");
    std::fs::write(&nested, "b").unwrap();
    delivered(&mut e, &nested, "created").await;
    let moved = root.join("moved");
    std::fs::rename(&fresh, &moved).unwrap();
    delivered(&mut e, &moved, "moved").await;
    let nested = moved.join("b.mix");
    std::fs::write(&nested, "b2").unwrap();
    delivered(&mut e, &nested, "modified").await;
    std::fs::remove_dir_all(&moved).unwrap();
    delivered(&mut e, &moved, "deleted").await;
    std::fs::remove_dir_all(&root).unwrap();
    delivered(&mut e, &root, "deleted").await;
    std::fs::create_dir(&root).unwrap();
    delivered(&mut e, &root, "created").await;
    let file = root.join("after.mix");
    std::fs::write(&file, "after").unwrap();
    delivered(&mut e, &file, "created").await;
}

#[tokio::test(flavor = "current_thread")]
async fn nonrecursive_and_symlink_targets_do_not_deliver_nested_changes() {
    let d = Directory::new();
    let outside = Directory::new();
    let sub = d.0.join("sub");
    std::fs::create_dir(&sub).unwrap();
    std::os::unix::fs::symlink(&outside.0, d.0.join("link")).unwrap();
    for recursive in [false, true] {
        let mut e = watcher(&d.0, recursive).await;
        if !recursive {
            std::fs::write(sub.join("hidden"), "x").unwrap();
        }
        std::fs::write(outside.0.join("hidden"), "x").unwrap();
        let sentinel = d.0.join("sentinel");
        std::fs::write(&sentinel, "ready").unwrap();
        let batch = delivered(&mut e, &sentinel, "any").await;
        assert!(!batch.to_mix_string().contains("hidden"));
    }
}

#[tokio::test(flavor = "current_thread")]
async fn nonserve_wait_and_serve_refusal_use_structured_errors() {
    let d = Directory::new();
    let mut e = Evaluator::new();
    e.set_global("root", Value::String(d.0.to_str().unwrap().into()));
    exec(&mut e, "$h = fs_watch($root)").await.unwrap();
    std::fs::write(d.0.join("ready"), "x").unwrap();
    let v = tokio::time::timeout(Duration::from_secs(5), exec(&mut e, "fs_wait($h)"))
        .await
        .unwrap()
        .unwrap();
    assert!(v.to_mix_string().contains("ready"));
    e.set_serve_runtime(Rc::new(Runtime));
    let err = exec(&mut e, "fs_wait($h)").await.unwrap_err();
    assert!(matches!(err, cosmix_mix::MixError::Structured(info) if info.code == "FS_WAIT_SERVE"));
    exec(&mut e, "fs_unwatch($h)").await.unwrap();
    let error = exec(&mut e, "fs_unwatch($h)").await.unwrap_err();
    assert!(
        matches!(error, cosmix_mix::MixError::Structured(info) if info.code == "FS_WATCH_HANDLE")
    );
    let caught = exec(
        &mut e,
        r#"try
        fs_watch($root, {recursive: 42})
    catch $message, $info
        $refusal = $info
    end
    $refusal"#,
    )
    .await
    .unwrap();
    assert!(
        caught
            .to_mix_string()
            .contains("error_code: FS_WATCH_OPTIONS")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn failed_candidate_cleanup_preserves_old_generation_then_success_retires_it() {
    let d = Directory::new();
    let mut old = watcher(&d.0, false).await;
    let old_pid = blocked_child(&mut old).await;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let bus = Rc::new(Bus {
        rx: tokio::sync::Mutex::new(rx),
    });
    old.set_bus_handler(bus.clone());
    reload_request(&mut old, &tx).await;
    let mut candidate = watcher(&d.0, true).await;
    candidate.set_bus_handler(bus.clone());
    let candidate_pid = blocked_child(&mut candidate).await;
    assert!(
        exec(&mut candidate, "die(\"reject candidate\")")
            .await
            .is_err()
    );
    candidate
        .drain_class_c_for_shutdown(Duration::ZERO, false)
        .await;
    candidate.close_native_events();
    let mut status = 0;
    assert_eq!(
        unsafe { libc::waitpid(candidate_pid, &mut status, libc::WNOHANG) },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
    assert_eq!(
        exec(&mut old, "process_alive($pid)").await.unwrap(),
        Value::Bool(true)
    );
    let a = d.0.join("old-still-live");
    std::fs::write(&a, "a").unwrap();
    delivered(&mut old, &a, "created").await;
    reload_request(&mut old, &tx).await;
    let mut replacement = watcher(&d.0, true).await;
    replacement.set_bus_handler(bus);
    old.drain_class_c_for_shutdown(Duration::ZERO, false).await;
    old.close_native_events();
    assert_eq!(
        unsafe { libc::waitpid(old_pid, &mut status, libc::WNOHANG) },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
    let b = d.0.join("new-live");
    std::fs::write(&b, "b").unwrap();
    delivered(&mut replacement, &b, "created").await;
    assert!(exec(&mut old, "fs_unwatch($watch)").await.is_err());
    replacement
        .drain_class_c_for_shutdown(Duration::ZERO, false)
        .await;
    replacement.close_native_events();
    let error = exec(&mut replacement, "fs_watch($root)").await.unwrap_err();
    assert!(
        matches!(error, cosmix_mix::MixError::Structured(info) if info.code == "NATIVE_CLOSED")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn serve_managed_child_exits_are_reaped_and_generation_tagged() {
    let mut e = Evaluator::new();
    e.set_serve_runtime(Rc::new(Runtime));
    exec(
        &mut e,
        r#"
        $exit = nil
        on proc.exited
            $exit = $event.args
            quit()
        end
        $pid = spawn(["/bin/true"], {exit_event: true, tag: "scene:7"})
    "#,
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), e.run_event_pump())
        .await
        .unwrap()
        .unwrap();
    let exit_value = e.get_global("exit").unwrap();
    let Value::Map(exit) = &exit_value else {
        panic!("exit map")
    };
    assert_eq!(exit["tag"].to_mix_string(), "scene:7");
    assert_eq!(exit["exit_code"].to_number(), Some(0.0));
    assert!(matches!(exit["signal"], Value::Nil));
    let pid = exit["pid"].to_number().unwrap() as i32;
    let mut status = 0;
    assert_eq!(
        unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
}

#[test]
fn lint_contracts_capabilities_and_expression_wait_denial() {
    let source = "$h = fs_watch(\".\", {recursive: true})\nfs_unwatch($h)\nfs_wait($h)";
    let stmts = Parser::new(Lexer::new(source).tokenize().unwrap(), source)
        .parse_program()
        .unwrap();
    let report = cosmix_mix::analyzer::analyze(&stmts, None, &Default::default());
    assert!(!report.diagnostics.iter().any(|d| d.code == "MIX-E1102"));
    assert!(report.capabilities.contains(&"fs-read"));
    for name in ["fs_watch", "fs_unwatch", "fs_wait"] {
        let info = cosmix_mix::builtins::builtin_info_of(name).unwrap();
        assert_eq!(info.capability, cosmix_mix::CapabilityClass::FsRead);
        assert!(!info.contract.accepts_arity(0));
        assert!(info.contract.accepts_arity(1));
    }
    assert!(cosmix_mix::evaluator::EXPR_MODE_DENIED_BUILTINS.contains(&"fs_wait"));
    assert!(cosmix_mix::expr_mode_check("fs_wait(\"handle\")").is_err());
    let bad = "fs_watch()\nfs_wait(1, 2)";
    let stmts = Parser::new(Lexer::new(bad).tokenize().unwrap(), bad)
        .parse_program()
        .unwrap();
    let report = cosmix_mix::analyzer::analyze(&stmts, None, &Default::default());
    assert_eq!(
        report
            .diagnostics
            .iter()
            .filter(|d| d.code == "MIX-E1201")
            .count(),
        2
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn idle_serve_pump_has_no_clock_wakeups() {
    use std::{
        future::Future,
        pin::pin,
        sync::{Arc, atomic::AtomicUsize},
        task::{Context, Wake, Waker},
    };
    struct Counter(AtomicUsize);
    impl Wake for Counter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
    let d = Directory::new();
    let mut e = watcher(&d.0, true).await;
    let count = Arc::new(Counter(AtomicUsize::new(0)));
    let waker = Waker::from(count.clone());
    {
        let mut pump = pin!(e.run_event_pump());
        assert!(
            pump.as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
        tokio::time::advance(Duration::from_secs(86400)).await;
        assert_eq!(count.0.load(Ordering::Relaxed), 0);
    }
    e.close_native_events();
}

/// A real child blocked on a native thread wake, without sleeps or polling.
#[test]
fn managed_child_fixture() {
    if std::env::var_os("MIX_NATIVE_CHILD_FIXTURE").is_some() {
        if let Some(path) = std::env::var_os("MIX_NATIVE_CHILD_ESCAPE") {
            use std::io::Write;
            // Join the parent's group after exec, escaping the spawn-time one.
            assert_eq!(
                unsafe { libc::setpgid(0, libc::getpgid(libc::getppid())) },
                0
            );
            let mut ready = std::os::unix::net::UnixStream::connect(path).unwrap();
            ready.write_all(b"ready").unwrap();
        }
        std::thread::park();
    }
}

#[test]
fn shutdown_reaps_a_managed_leader_that_left_its_process_group() {
    let d = Directory::new();
    let socket = d.0.join("ready.sock");
    let (pid_tx, pid_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let listener = tokio::net::UnixListener::bind(&socket).unwrap();
            let mut e = Evaluator::new();
            e.set_global("exe", Value::String(std::env::current_exe().unwrap().to_str().unwrap().into()));
            e.set_global("socket", Value::String(socket.to_str().unwrap().into()));
            exec(&mut e, r#"$pid = spawn([$exe, "--exact", "managed_child_fixture"], {
                exit_event: true, env: {MIX_NATIVE_CHILD_FIXTURE: "1", MIX_NATIVE_CHILD_ESCAPE: $socket}
            })"#).await.unwrap();
            let pid = e.get_global("pid").unwrap().to_number().unwrap() as i32;
            pid_tx.send(pid).unwrap();
            let (mut ready, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept()).await.unwrap().unwrap();
            let mut bytes = [0; 5];
            use tokio::io::AsyncReadExt;
            tokio::time::timeout(Duration::from_secs(5), ready.read_exact(&mut bytes)).await.unwrap().unwrap();
            assert_eq!(&bytes, b"ready");
            assert_ne!(unsafe { libc::getpgid(pid) }, pid);
            e.close_native_events();
            let mut status = 0;
            assert_eq!(unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) }, -1);
            assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::ECHILD));
            done_tx.send(()).unwrap();
        });
    });
    let pid = pid_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let done = done_rx.recv_timeout(Duration::from_secs(10));
    if done.is_err() {
        // Watchdog cleanup also makes the unfixed regression fail without
        // leaving a blocked monitor or an orphan behind in the test process.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }
    let joined = worker.join();
    assert!(done.is_ok(), "shutdown waited for an escaped, live leader");
    joined.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn plain_pump_finishes_after_the_last_managed_exit_without_quit() {
    let mut e = Evaluator::new();
    exec(
        &mut e,
        r#"
        $exits = 0
        on proc.exited
            $exits = $exits + 1
        end
        spawn(["/bin/true"], {exit_event: true})
    "#,
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), e.run_event_pump())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(e.get_global("exits").unwrap().to_number(), Some(1.0));
}

#[tokio::test(flavor = "current_thread")]
async fn sleep_with_child_handler_preserves_batches_for_fs_wait() {
    let d = Directory::new();
    let mut e = Evaluator::new();
    e.set_global("root", Value::String(d.0.to_str().unwrap().into()));
    exec(
        &mut e,
        r#"
        $exits = 0
        on proc.exited
            $exits = $exits + 1
        end
        $h = fs_watch($root, {events: ["created"]})
    "#,
    )
    .await
    .unwrap();
    std::fs::write(d.0.join("kept"), "x").unwrap();
    exec(
        &mut e,
        "spawn([\"/bin/true\"], {exit_event: true})\nsleep(0.1)",
    )
    .await
    .unwrap();
    let batch = tokio::time::timeout(Duration::from_secs(5), exec(&mut e, "fs_wait($h)"))
        .await
        .unwrap()
        .unwrap();
    assert!(batch.to_mix_string().contains("kept"));
    // If the child was delayed, finish through the native pump without a timer.
    if e.get_global("exits").unwrap().to_number() != Some(1.0) {
        tokio::time::timeout(Duration::from_secs(5), exec(&mut e, "fs_unwatch($h)"))
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), e.run_event_pump())
            .await
            .unwrap()
            .unwrap();
    }
    assert_eq!(e.get_global("exits").unwrap().to_number(), Some(1.0));
}

#[tokio::test(flavor = "current_thread")]
async fn overlapping_watches_keep_independent_filters_and_survive_peer_unwatch() {
    let d = Directory::new();
    let mut e = Evaluator::new();
    e.set_global("root", Value::String(d.0.to_str().unwrap().into()));
    exec(
        &mut e,
        r#"
        $a = fs_watch($root, {events: ["created"]})
        $b = fs_watch($root, {events: ["deleted"]})
    "#,
    )
    .await
    .unwrap();
    let file = d.0.join("shared");
    std::fs::write(&file, "x").unwrap();
    let created = tokio::time::timeout(Duration::from_secs(5), exec(&mut e, "fs_wait($a)"))
        .await
        .unwrap()
        .unwrap();
    assert!(created.to_mix_string().contains("created"));
    exec(&mut e, "fs_unwatch($a)").await.unwrap();
    std::fs::remove_file(file).unwrap();
    let deleted = tokio::time::timeout(Duration::from_secs(5), exec(&mut e, "fs_wait($b)"))
        .await
        .unwrap()
        .unwrap();
    assert!(deleted.to_mix_string().contains("deleted"));
    assert!(!deleted.to_mix_string().contains("created"));
}

#[test]
fn shared_fs_registry_fixture() {
    if std::env::var_os("MIX_SHARED_FS_FIXTURE").is_none() {
        return;
    }
    fn inotify_count() -> usize {
        std::fs::read_dir("/proc/self/fd")
            .unwrap()
            .flatten()
            .filter_map(|entry| std::fs::read_link(entry.path()).ok())
            .filter(|target| target.as_os_str() == "anon_inode:inotify")
            .count()
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let d = Directory::new();
        let mut e = Evaluator::new();
        e.set_global("root", Value::String(d.0.to_str().unwrap().into()));
        let instances = inotify_count();
        let threads = std::fs::read_dir("/proc/self/task").unwrap().count();
        for _ in 0..128 {
            exec(&mut e, "fs_watch($root)").await.unwrap();
        }
        assert_eq!(inotify_count(), instances + 1);
        // notify owns one event-loop thread; reconciliation owns one worker.
        // Both are per registry, regardless of the number of watch handles.
        assert_eq!(
            std::fs::read_dir("/proc/self/task").unwrap().count(),
            threads + 2
        );
        let error = exec(&mut e, "fs_watch($root)").await.unwrap_err();
        let cosmix_mix::MixError::Structured(info) = error else {
            panic!("structured limit")
        };
        assert_eq!(info.code, "FS_WATCH_LIMIT");
        e.close_native_events();
    });
}

#[test]
fn all_watch_handles_share_one_inotify_instance_and_worker_pair() {
    // Isolate /proc counts from the other concurrently running native tests.
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "shared_fs_registry_fixture", "--nocapture"])
        .env("MIX_SHARED_FS_FIXTURE", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

async fn blocked_child(e: &mut Evaluator) -> i32 {
    e.set_global(
        "exe",
        Value::String(std::env::current_exe().unwrap().to_str().unwrap().into()),
    );
    exec(
        e,
        r#"$pid = spawn([$exe, "--exact", "managed_child_fixture"], {
        exit_event: true, tag: "child:9", env: {MIX_NATIVE_CHILD_FIXTURE: "1"}
    })"#,
    )
    .await
    .unwrap();
    e.get_global("pid").unwrap().to_number().unwrap() as i32
}

#[tokio::test(flavor = "current_thread")]
async fn managed_signal_delivery_and_shutdown_have_one_reaper() {
    let mut e = Evaluator::new();
    e.set_serve_runtime(Rc::new(Runtime));
    exec(&mut e, "on proc.exited\n$exit = $event.args\nquit()\nend")
        .await
        .unwrap();
    blocked_child(&mut e).await;
    exec(&mut e, "kill($pid, 15)").await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), e.run_event_pump())
        .await
        .unwrap()
        .unwrap();
    let exit_value = e.get_global("exit").unwrap();
    let Value::Map(exit) = &exit_value else {
        panic!("exit map")
    };
    assert_eq!(exit["signal"].to_number(), Some(15.0));
    assert!(matches!(exit["exit_code"], Value::Nil));
    let pid = blocked_child(&mut e).await;
    e.drain_class_c_for_shutdown(Duration::ZERO, false).await;
    e.close_native_events();
    let mut status = 0;
    assert_eq!(
        unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn managed_exit_async_handler_uses_existing_scheduler_and_drain() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut e = Evaluator::new();
            e.set_serve_runtime(Rc::new(Runtime));
            exec(
                &mut e,
                r#"
            $exit = nil
            on proc.exited async
                $exit = $event.args
                quit()
            end
            spawn(["/bin/true"], {exit_event: true, tag: "async:1"})
        "#,
            )
            .await
            .unwrap();
            tokio::time::timeout(Duration::from_secs(5), e.run_event_pump())
                .await
                .unwrap()
                .unwrap();
            e.drain_class_c_for_shutdown(Duration::from_secs(1), false)
                .await;
            assert_eq!(e.class_c_task_count(), 0);
            assert!(
                e.get_global("exit")
                    .unwrap()
                    .to_mix_string()
                    .contains("async:1")
            );
            e.close_native_events();
        })
        .await;
}

fn text(e: &Evaluator, name: &str) -> String {
    e.get_global(name)
        .map(|v| v.to_mix_string())
        .unwrap_or_default()
}

#[tokio::test(flavor = "current_thread")]
async fn net_handles_snapshot_and_refusals() {
    let mut e = Evaluator::new();
    exec(
        &mut e,
        r#"
        $h = net_watch({events: ["link"]})
        $loopback = false
        for $link in net_state().links
            if $link.loopback then $loopback = true end
        end
        net_unwatch($h)
        try
            net_unwatch($h)
        catch $message, $info
            $again = $info.error_code
        end
        try
            net_watch({recursive: true})
        catch $message, $info
            $options = $info.error_code
        end
        $other = net_watch()
        try
            audio_unwatch($other)
        catch $message, $info
            $cross = $info.error_code
        end
        net_unwatch($other)
        -- No PipeWire at this path: an ordinary ok:false state, not an error.
        $vol = audio_state({runtime_dir: "/nonexistent-mix-audio-test"})
    "#,
    )
    .await
    .unwrap();
    assert_eq!(e.get_global("loopback"), Some(Value::Bool(true)));
    assert_eq!(text(&e, "again"), "NET_WATCH_HANDLE");
    assert_eq!(text(&e, "options"), "NET_WATCH_OPTIONS");
    assert_eq!(text(&e, "cross"), "AUDIO_WATCH_HANDLE");
    let vol_value = e.get_global("vol").expect("vol is set");
    let Value::Map(vol) = &vol_value else {
        panic!("audio_state returns a map")
    };
    // PIPEWIRE_RUNTIME_DIR in the test environment would still reach a
    // server; either way the answer is a state map, never a raise.
    match vol.get("ok") {
        Some(Value::Bool(false)) => {
            assert!(matches!(vol.get("reason"), Some(Value::String(s)) if !s.is_empty()))
        }
        Some(Value::Bool(true)) => assert!(matches!(vol.get("level"), Some(Value::Number(_)))),
        other => panic!("audio_state ok must be a bool, got {other:?}"),
    }
    e.close_native_events();
    let err = exec(&mut e, "net_watch()").await.unwrap_err();
    assert!(
        matches!(err, cosmix_mix::MixError::Structured(info) if info.code == "NATIVE_CLOSED")
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn idle_net_watch_pump_has_no_clock_wakeups() {
    use std::{
        future::Future,
        pin::pin,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        task::{Context, Wake, Waker},
    };
    struct Counter(AtomicUsize);
    impl Wake for Counter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
    let mut e = Evaluator::new();
    e.set_serve_runtime(Rc::new(Runtime));
    exec(
        &mut e,
        "$h = net_watch({events: [\"link\"]})\non net.changed\n    quit()\nend",
    )
    .await
    .unwrap();
    let count = Arc::new(Counter(AtomicUsize::new(0)));
    let waker = Waker::from(count.clone());
    {
        let mut pump = pin!(e.run_event_pump());
        assert!(
            pump.as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
        tokio::time::advance(Duration::from_secs(86400)).await;
        assert_eq!(count.0.load(Ordering::Relaxed), 0);
    }
    e.close_native_events();
}

#[test]
fn desktop_source_contracts_capabilities_and_expression_denial() {
    use cosmix_mix::CapabilityClass::{Env, Process};
    for (name, capability, arities) in [
        ("net_watch", Env, &[0, 1][..]),
        ("net_unwatch", Env, &[1][..]),
        ("net_state", Env, &[0][..]),
        ("audio_watch", Process, &[0, 1][..]),
        ("audio_unwatch", Process, &[1][..]),
        ("audio_state", Process, &[0, 1][..]),
    ] {
        let info = cosmix_mix::builtins::builtin_info_of(name).unwrap();
        assert_eq!(info.capability, capability, "{name}");
        for n in 0..3 {
            assert_eq!(info.contract.accepts_arity(n), arities.contains(&n), "{name}/{n}");
        }
        assert!(cosmix_mix::evaluator::EXPR_MODE_DENIED_BUILTINS.contains(&name));
        assert!(cosmix_mix::expr_mode_check(&format!("{name}()")).is_err());
    }
    let source = "$n = net_watch({events: [\"link\"]})\nnet_unwatch($n)\n$a = audio_watch()\naudio_unwatch($a)\n$s = net_state()\n$v = audio_state()";
    let stmts = Parser::new(Lexer::new(source).tokenize().unwrap(), source)
        .parse_program()
        .unwrap();
    let report = cosmix_mix::analyzer::analyze(&stmts, None, &Default::default());
    assert!(!report.diagnostics.iter().any(|d| d.code == "MIX-E1102"));
    assert!(report.capabilities.contains(&"env"));
    assert!(report.capabilities.contains(&"process"));
}
