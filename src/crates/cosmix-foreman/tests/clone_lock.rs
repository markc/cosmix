//! Clone lock serialization tests.
//!
//! Tests that:
//! - Two concurrent refines serialize (one waits for the other)
//! - Timeout errors cleanly with a clear message
//! - Double acquire in the same process is DENIED — the property that makes
//!   the lane-held handshake necessary rather than an optimisation
//! - The two real compositions: a `flock(1)` wrapper exec'ing foreman, and
//!   a `refine` whose tier-2 verify re-enters the same lane
//!
//! Scenarios that need a particular environment run in owned helper
//! processes. The remaining lock covers only `IN_PROCESS_HOLDS`, the library's
//! genuinely process-wide clone-lane state; tests that acquire in this parent
//! binary must not alter that state under one another.
use std::sync::mpsc;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::Duration;

// No direct imports needed - all items are referenced via full path

/// Upper bound on any cross-thread handshake in this file. Generous, because
/// it is only ever reached when something is already broken; its job is to
/// turn a hang into a failure rather than to time anything.
const SYNC_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a deliberate holder keeps the lock while a waiter blocks on it.
const HOLD: Duration = Duration::from_millis(200);

/// `CloneLock` deliberately exposes an in-process held-lane count to nested
/// callers. Tests that acquire in this parent binary share that real global,
/// so they serialize here; environment-specific scenarios use owned helper
/// processes instead.
fn lane_state_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn lock_lane_state() -> MutexGuard<'static, ()> {
    lane_state_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

const HELPER_ENV: &str = "COSMIX_FOREMAN_CLONE_LOCK_HELPER";

fn run_owned_helper(name: &str, configure: impl FnOnce(&mut std::process::Command)) {
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            name,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(HELPER_ENV, name);
    configure(&mut command);
    let out = command.output().expect("spawn owned clone-lock helper");
    assert!(
        out.status.success(),
        "owned helper {name} failed\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn assert_owned_helper(name: &str) {
    assert_eq!(std::env::var(HELPER_ENV).as_deref(), Ok(name));
}

#[test]
fn double_acquire_in_same_process_blocks() {
    run_owned_helper(
        "double_acquire_in_same_process_blocks_owned_process",
        |command| {
            command
                .env("FOREMAN_CLONE_LOCK_WAIT_SECS", "0")
                .env_remove("FOREMAN_CLONE_LANE_HELD");
        },
    );
}

#[test]
#[ignore = "run only in the owned helper process"]
fn double_acquire_in_same_process_blocks_owned_process() {
    assert_owned_helper("double_acquire_in_same_process_blocks_owned_process");
    // flock(LOCK_EX | LOCK_NB) does not grant a second exclusive to a
    // process that already holds one through another descriptor — flock(2)
    // says it may be denied, and on Linux it is.
    //
    // This is the LOAD-BEARING fact, not a curiosity: it is why the systemd
    // `flock(1)` wrappers are NOT safely composable with a binary that also
    // locks. flock(1) execs foreman with its locked descriptor still open,
    // so foreman IS the holder and re-acquiring would wait out its whole
    // timeout and fail, every tick. Hence the lane-held handshake — see
    // `wrapper_held_lock_is_joined_via_the_marker_not_re_acquired` below.
    let temp = tempfile::TempDir::new().unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();

    // First acquire succeeds.
    let _lock1 = cosmix_foreman::clone_lock::CloneLock::acquire(&repo).unwrap();

    // Second acquire in the same process fails (LOCK_NB behavior).
    let lock2 = cosmix_foreman::clone_lock::CloneLock::acquire(&repo);
    assert!(lock2.is_err(), "re-acquiring in same process should fail");
    let msg = lock2.unwrap_err().to_string();
    // The error message should contain "another process holds the clone lock"
    assert!(
        msg.contains("another process holds the clone lock"),
        "error should mention 'another process holds the clone lock', got: {msg}"
    );
    // And it should name who — our own pid, since lock1 is held in this
    // same process (the holder stamp records whoever won the flock).
    let my_pid = std::process::id();
    assert!(
        msg.contains(&format!("pid {my_pid}")),
        "error should name the holder's pid ({my_pid}), got: {msg}"
    );
    assert!(
        msg.contains("(alive)"),
        "the holder is this same live process, error should say so: {msg}"
    );
}

#[test]
fn concurrent_refines_serialize() {
    // Both acquisitions contribute to the library's process-wide lane count.
    let _guard = lock_lane_state();

    // Create a fixture repo.
    let temp = tempfile::TempDir::new().unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();

    // The holder announces that it actually HOLDS the lock, rather than the
    // waiter sleeping and hoping. A fixed "give the holder time to acquire"
    // sleep is a race: under the load of a full-workspace `cargo test` the
    // holder thread can fail to be scheduled inside the window, the waiter
    // then wins the lock uncontended, and the elapsed-time assertion below
    // fails for a reason that has nothing to do with the lock.
    let (acquired_tx, acquired_rx) = mpsc::channel();
    let repo_clone = repo.clone();
    let holder = thread::spawn(move || {
        let lock = cosmix_foreman::clone_lock::CloneLock::acquire(&repo_clone).unwrap();
        acquired_tx.send(()).unwrap();
        // Hold long enough that the waiter measurably blocks, then release
        // by dropping. Bounded, and joined below — never a stray holder.
        thread::sleep(HOLD);
        drop(lock);
    });

    acquired_rx
        .recv_timeout(SYNC_TIMEOUT)
        .expect("holder should acquire the lock");

    // Now that the lock is provably held, a second acquire must block until
    // the holder releases.
    let repo_clone2 = repo.clone();
    let waiter = thread::spawn(move || {
        let start = std::time::Instant::now();
        let _lock = cosmix_foreman::clone_lock::CloneLock::acquire(&repo_clone2).unwrap();
        start.elapsed()
    });

    holder.join().unwrap();
    let elapsed = waiter.join().unwrap();
    // The waiter started after the holder had the lock and the holder held it
    // for HOLD, so any honest wait is a large fraction of HOLD. Assert well
    // under it to stay robust against scheduler jitter while still proving
    // the waiter did not sail straight through.
    assert!(
        elapsed >= HOLD / 2,
        "waiter should have blocked on the holder, but returned after {elapsed:?}"
    );
}

#[test]
fn timeout_errors_cleanly() {
    run_owned_helper("timeout_errors_cleanly_owned_process", |command| {
        command
            .env("FOREMAN_CLONE_LOCK_WAIT_SECS", "1")
            .env_remove("FOREMAN_CLONE_LANE_HELD");
    });
}

#[test]
#[ignore = "run only in the owned helper process"]
fn timeout_errors_cleanly_owned_process() {
    assert_owned_helper("timeout_errors_cleanly_owned_process");
    // Create a fixture repo.
    let temp = tempfile::TempDir::new().unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();

    // The holder signals that it holds the lock, and then waits to be told to
    // let go, rather than sleeping for a guessed interval. The waiter below
    // must observe a real timeout, so the holder has to still be holding when
    // the waiter's 1s budget expires — a fixed sleep either races the waiter's
    // start (holder not yet in) or has to be padded long enough to slow the
    // suite down. Both signals are bounded and the thread is joined, so no
    // holder can outlive the test: an orphaned holder on a lock file is
    // precisely the failure that wedged the live merge queue.
    let (acquired_tx, acquired_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let repo_clone = repo.clone();
    let holder = thread::spawn(move || {
        let lock = cosmix_foreman::clone_lock::CloneLock::acquire(&repo_clone).unwrap();
        acquired_tx.send(()).unwrap();
        // Wait to be released; the recv_timeout is a backstop so this thread
        // can never hang the suite even if the waiter panics.
        let _ = release_rx.recv_timeout(SYNC_TIMEOUT);
        drop(lock);
    });

    acquired_rx
        .recv_timeout(SYNC_TIMEOUT)
        .expect("holder should acquire the lock");

    // With the lock provably held, this acquire must exhaust its 1s budget
    // and fail with a clear, attributable error.
    let repo_clone2 = repo.clone();
    let waiter = thread::spawn(move || {
        let result = cosmix_foreman::clone_lock::CloneLock::acquire(&repo_clone2);
        assert!(result.is_err());
        let err = result.unwrap_err();
        let msg = err.to_string();
        // Error message should mention the timeout.
        assert!(
            msg.contains("timed out"),
            "error should mention timeout: {msg}"
        );
        // Error message should mention the lock file.
        assert!(
            msg.contains("clone.lock"),
            "error should mention clone.lock: {msg}"
        );
        // And name who it's blocked on — the holder thread stamped its own
        // (this same process's) pid into the lock file on acquire.
        let my_pid = std::process::id();
        assert!(
            msg.contains(&format!("blocked on pid {my_pid} (alive)")),
            "error should name the live holder: {msg}"
        );
    });

    // Collect the waiter's outcome BEFORE re-raising any panic, so the holder
    // is always released and joined even when an assertion above failed. A
    // test that panics its way out while still holding a lock is the shape of
    // bug this whole module exists to prevent.
    let outcome = waiter.join();
    let _ = release_tx.send(());
    holder.join().unwrap();
    outcome.unwrap();
}

#[test]
fn fail_fast_exits_without_blocking() {
    run_owned_helper(
        "fail_fast_exits_without_blocking_owned_process",
        |command| {
            command
                .env("FOREMAN_CLONE_LOCK_WAIT_SECS", "0")
                .env_remove("FOREMAN_CLONE_LANE_HELD");
        },
    );
}

#[test]
#[ignore = "run only in the owned helper process"]
fn fail_fast_exits_without_blocking_owned_process() {
    assert_owned_helper("fail_fast_exits_without_blocking_owned_process");
    // Test that FOREMAN_CLONE_LOCK_WAIT_SECS=0 causes acquire() to return
    // immediately (success or failure) without blocking. We don't assert
    // on the result because it's racy - another test might have touched
    // the same tempdir. We just verify it doesn't hang.
    let temp = tempfile::TempDir::new().unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();

    let start = std::time::Instant::now();
    let _result = cosmix_foreman::clone_lock::CloneLock::acquire(&repo);
    let elapsed = start.elapsed();

    // Should return immediately (well under 1 second).
    assert!(
        elapsed < Duration::from_secs(1),
        "fail-fast should return without blocking, took {elapsed:?}"
    );
}

#[test]
fn lock_path_is_sibling_of_repo() {
    let _serial = lock_lane_state();
    // For a repo at /tmp/testXXX/repo, the lock should be at /tmp/testXXX/clone.lock
    let temp = tempfile::TempDir::new().unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();

    // The lock should be created at temp.path()/clone.lock
    let _lock = cosmix_foreman::clone_lock::CloneLock::acquire(&repo).unwrap();
    let lock_path = temp.path().join("clone.lock");

    assert!(
        lock_path.exists(),
        "lock file should exist at sibling of repo"
    );
}

#[test]
fn inspect_reports_no_holder_before_any_acquire() {
    let temp = tempfile::TempDir::new().unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();

    let holder = cosmix_foreman::clone_lock::inspect(&repo).unwrap();
    assert!(holder.pid.is_none(), "nobody has acquired yet: {holder:?}");
}

#[test]
fn inspect_reports_the_live_holder_while_held() {
    let _serial = lock_lane_state();
    let temp = tempfile::TempDir::new().unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();

    let repo_clone = repo.clone();
    let holder_thread = thread::spawn(move || {
        let _lock = cosmix_foreman::clone_lock::CloneLock::acquire(&repo_clone).unwrap();
        thread::sleep(Duration::from_millis(150));
    });

    // Give the holder time to acquire and stamp its identity.
    thread::sleep(Duration::from_millis(30));

    let holder = cosmix_foreman::clone_lock::inspect(&repo).unwrap();
    assert_eq!(holder.pid, Some(std::process::id() as i64));
    assert!(holder.is_alive(), "we're still running: {holder:?}");
    assert!(holder.acquired_at.is_some());

    holder_thread.join().unwrap();
}

#[test]
fn dead_holder_stamp_does_not_block_reacquire() {
    let _serial = lock_lane_state();
    // Simulate the aftermath of a crashed/killed holder: a lock file whose
    // last stamp names a pid that no longer exists, but that (like any
    // exited process) is NOT actually flock-holding anything anymore — the
    // kernel released that the instant it exited. A fresh acquire must not
    // wait on stale metadata; it should succeed immediately.
    let temp = tempfile::TempDir::new().unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let lock_path = temp.path().join("clone.lock");
    std::fs::write(
        &lock_path,
        "pid=999999999\npid_start=1\nacquired_at=1970-01-01T00:00:00Z\n",
    )
    .unwrap();

    let holder = cosmix_foreman::clone_lock::inspect(&repo).unwrap();
    assert_eq!(holder.pid, Some(999_999_999));
    assert!(!holder.is_alive(), "the stamped pid should not exist");

    let start = std::time::Instant::now();
    let lock = cosmix_foreman::clone_lock::CloneLock::acquire(&repo).unwrap();
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(1),
        "acquiring over a stale dead-holder stamp should be immediate, took {elapsed:?}"
    );

    // The fresh acquire re-stamps its own (live) identity.
    drop(lock);
    let holder = cosmix_foreman::clone_lock::inspect(&repo).unwrap();
    assert_eq!(holder.pid, Some(std::process::id() as i64));
}

#[test]
fn repo_detection_works() {
    run_owned_helper("repo_detection_works_owned_process", |command| {
        command
            .env_remove("FOREMAN_CLONE_LANE_HELD")
            .env_remove("FOREMAN_CLONE_LOCK_WAIT_SECS");
    });
}

#[test]
#[ignore = "run only in the owned helper process"]
fn repo_detection_works_owned_process() {
    assert_owned_helper("repo_detection_works_owned_process");
    // Be hermetic to FOREMAN_CLONE_LANE_HELD: if the operator has set it in
    // the tier-1 unit (which runs this test), acquire_if_in_repo would skip
    // the lock and the assertions would fail.
    let temp = tempfile::TempDir::new().unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();

    // Create .git to make it a real repo.
    std::fs::create_dir(repo.join(".git")).unwrap();

    // A repo with no sibling clone.lock has no lane to join, so verify does
    // NOT invent one — it must not scatter clone.lock files into unrelated
    // checkouts just because a `foreman verify --dir` ran there.
    let lock = cosmix_foreman::clone_lock::acquire_if_in_repo(&repo).unwrap();
    assert!(
        lock.is_none(),
        "no sibling clone.lock means no lane to join"
    );
    assert!(
        !temp.path().join("clone.lock").exists(),
        "verify must not create the lock file it only means to join"
    );

    // Once the lane exists — created by refine or the systemd wrapper — a
    // verify inside the repo joins it.
    std::fs::write(temp.path().join("clone.lock"), "").unwrap();
    let lock = cosmix_foreman::clone_lock::acquire_if_in_repo(&repo).unwrap();
    assert!(lock.is_some(), "should join an existing lane");
    drop(lock);

    // Outside of a repo, should return None.
    let outside = temp.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    let lock = cosmix_foreman::clone_lock::acquire_if_in_repo(&outside).unwrap();
    assert!(
        lock.is_none(),
        "should not acquire lock when outside a repo"
    );
}

#[test]
fn refine_creates_the_lane_that_verify_joins() {
    run_owned_helper(
        "refine_creates_the_lane_that_verify_joins_owned_process",
        |command| {
            command
                .env_remove("FOREMAN_CLONE_LANE_HELD")
                .env_remove("FOREMAN_CLONE_LOCK_WAIT_SECS");
        },
    );
}

#[test]
#[ignore = "run only in the owned helper process"]
fn refine_creates_the_lane_that_verify_joins_owned_process() {
    assert_owned_helper("refine_creates_the_lane_that_verify_joins_owned_process");
    // Be hermetic to FOREMAN_CLONE_LANE_HELD: if the operator has set it in
    // the tier-1 unit (which runs this test), acquire_if_in_repo would skip
    // the lock and the assertion would fail.
    // The asymmetry between the two entry points, pinned: refine OWNS the
    // lane and so creates <repo>/../clone.lock on demand; verify only JOINS
    // one. Without this, a fresh checkout's first refine would have no lock
    // file and nothing to serialize against.
    let temp = tempfile::TempDir::new().unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    std::fs::create_dir(repo.join(".git")).unwrap();
    let lock_path = temp.path().join("clone.lock");

    assert!(!lock_path.exists(), "fixture starts with no lane");

    let lock = cosmix_foreman::clone_lock::CloneLock::acquire(&repo).unwrap();
    assert!(lock_path.exists(), "refine's acquire creates the lane");
    drop(lock);

    // And now verify finds it.
    assert!(
        cosmix_foreman::clone_lock::acquire_if_in_repo(&repo)
            .unwrap()
            .is_some(),
        "verify joins the lane refine created"
    );
}

/// The composition the suite used to miss entirely: a `flock(1)` wrapper
/// that takes the lock and execs foreman, exactly as
/// `foreman-refine.service` and `foreman-tier2.service` do.
///
/// Without the marker the exec'd binary is blocked on a descriptor it
/// already owns and can only time out; with the marker it joins the lane and
/// runs. Both arms are asserted because both are live deployment states —
/// the wrapper cannot be removed from the units in the same instant this
/// binary lands.
///
/// `verify --profile none --tier 2` is the cheapest command that reaches the
/// clone lane: tier 2 is the arm that joins it, and the `none` profile runs
/// no verifier steps at all. Everything is under a tempdir, including the
/// ledger path — nothing here may touch live fleet state.
#[test]
fn wrapper_held_lock_is_joined_via_the_marker_not_re_acquired() {
    let Some(flock_bin) = ["/usr/bin/flock", "/bin/flock"]
        .into_iter()
        .find(|p| std::path::Path::new(p).exists())
    else {
        eprintln!("flock(1) not installed — skipping wrapper composition test");
        return;
    };

    let temp = tempfile::TempDir::new().unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    std::fs::create_dir(repo.join(".git")).unwrap();
    let lock_path = temp.path().join("clone.lock");
    std::fs::write(&lock_path, "").unwrap();

    // The wrapper: flock holds `clone.lock` and execs foreman with that
    // descriptor still open. `-w 5` bounds the WRAPPER itself, so this test
    // can never leave a process holding the lock.
    let wrapper = |marker: Option<&str>| {
        let mut cmd = std::process::Command::new(flock_bin);
        cmd.arg("-w")
            .arg("5")
            .arg(&lock_path)
            .arg(env!("CARGO_BIN_EXE_foreman"))
            .arg("--db")
            .arg(temp.path().join("ledger.db"))
            .arg("verify")
            .arg("--dir")
            .arg(&repo)
            .arg("--tier")
            .arg("2")
            .arg("--profile")
            .arg("none")
            .env("FOREMAN_VERIFY_LANE", temp.path().join("verify.lock"))
            .env("FOREMAN_VERIFY_LANE_WAIT_SECS", "30")
            // A real timeout, not the 900s default: if the handshake
            // regresses, this test fails in a second instead of wedging the
            // suite for a quarter of an hour.
            .env("FOREMAN_CLONE_LOCK_WAIT_SECS", "1")
            .env_remove("FOREMAN_CLONE_LANE_HELD");
        if let Some(value) = marker {
            cmd.env("FOREMAN_CLONE_LANE_HELD", value);
        }
        cmd.output().expect("running the flock-wrapped foreman")
    };

    // Arm 1 — wrapper, no marker: the binary must NOT silently succeed by
    // pretending it locked something. It is genuinely blocked, and it says
    // so, naming the inherited-descriptor cause it cannot otherwise detect.
    let out = wrapper(None);
    assert!(
        !out.status.success(),
        "an unmarked wrapper cannot be joined; foreman must fail rather than run unserialized"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("FOREMAN_CLONE_LANE_HELD"),
        "the timeout must name the marker that fixes it, got: {err}"
    );
    assert!(
        err.contains("flock(1)"),
        "the timeout must name the inherited-descriptor cause, got: {err}"
    );

    // Arm 2 — wrapper plus marker: joined, so the run completes.
    let out = wrapper(Some("1"));
    assert!(
        out.status.success(),
        "with the marker set the wrapped run must join the lane and complete; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The same binary with no wrapper at all — the post-migration state, where
/// the units have dropped both the `flock` prefix and the marker. It must
/// take the lock itself.
#[test]
fn unwrapped_run_still_takes_the_lock_itself() {
    let _guard = lock_lane_state();
    let temp = tempfile::TempDir::new().unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    std::fs::create_dir(repo.join(".git")).unwrap();
    let lock_path = temp.path().join("clone.lock");
    std::fs::write(&lock_path, "").unwrap();

    // Hold the lane from a bounded child so the run below has something real
    // to contend with. `flock -w`/`-c` is not used here: a plain in-process
    // hold is enough, and it cannot outlive the test.
    let held = cosmix_foreman::clone_lock::CloneLock::acquire(&repo).unwrap();

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_foreman"))
        .arg("--db")
        .arg(temp.path().join("ledger.db"))
        .args(["verify", "--tier", "2", "--profile", "none", "--dir"])
        .arg(&repo)
        .env("FOREMAN_VERIFY_LANE", temp.path().join("verify.lock"))
        .env("FOREMAN_VERIFY_LANE_WAIT_SECS", "30")
        .env("FOREMAN_CLONE_LOCK_WAIT_SECS", "1")
        .env_remove("FOREMAN_CLONE_LANE_HELD")
        .output()
        .expect("running foreman");
    assert!(
        !out.status.success(),
        "an unwrapped, unmarked run must contend for the lock like anyone else"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    let my_pid = std::process::id();
    assert!(
        err.contains(&format!("blocked on pid {my_pid} (alive)")),
        "and it must name this test process as the holder, got: {err}"
    );

    drop(held);
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_foreman"))
        .arg("--db")
        .arg(temp.path().join("ledger.db"))
        .args(["verify", "--tier", "2", "--profile", "none", "--dir"])
        .arg(&repo)
        .env("FOREMAN_VERIFY_LANE", temp.path().join("verify.lock"))
        .env("FOREMAN_VERIFY_LANE_WAIT_SECS", "30")
        .env("FOREMAN_CLONE_LOCK_WAIT_SECS", "1")
        .env_remove("FOREMAN_CLONE_LANE_HELD")
        .output()
        .expect("running foreman");
    assert!(
        out.status.success(),
        "once the lane is free the same run succeeds; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn lane_marker_turns_both_entry_points_into_joins() {
    run_owned_helper(
        "lane_marker_turns_both_entry_points_into_joins_owned_process",
        |command| {
            command.env("FOREMAN_CLONE_LANE_HELD", "1");
        },
    );
}

#[test]
#[ignore = "run only in the owned helper process"]
fn lane_marker_turns_both_entry_points_into_joins_owned_process() {
    assert_owned_helper("lane_marker_turns_both_entry_points_into_joins_owned_process");
    // The in-library half of the handshake, without spawning anything: with
    // an ancestor's marker set, neither entry point touches the lock file —
    // not even to create it.
    let temp = tempfile::TempDir::new().unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    std::fs::create_dir(repo.join(".git")).unwrap();

    assert!(
        cosmix_foreman::clone_lock::lane_held(),
        "the marker is the whole signal"
    );
    assert!(
        cosmix_foreman::clone_lock::acquire_lane(&repo)
            .unwrap()
            .is_none(),
        "refine's entry point joins rather than re-acquires"
    );
    assert!(
        cosmix_foreman::clone_lock::acquire_if_in_repo(&repo)
            .unwrap()
            .is_none(),
        "verify's entry point joins rather than re-acquires"
    );
    assert!(
        !temp.path().join("clone.lock").exists(),
        "joining a lane must not create a lock file"
    );
}

#[test]
fn an_in_process_hold_is_itself_the_lane_for_nested_callers() {
    run_owned_helper(
        "an_in_process_hold_is_itself_the_lane_for_nested_callers_owned_process",
        |command| {
            command
                .env_remove("FOREMAN_CLONE_LANE_HELD")
                .env_remove("FOREMAN_CLONE_LOCK_WAIT_SECS");
        },
    );
}

#[test]
#[ignore = "run only in the owned helper process"]
fn an_in_process_hold_is_itself_the_lane_for_nested_callers_owned_process() {
    assert_owned_helper("an_in_process_hold_is_itself_the_lane_for_nested_callers_owned_process");
    // The other half: no environment involved. `refine` holds the lane and
    // calls straight into the tier-2 verify, in-process, on a worktree whose
    // ../clone.lock is the same file — the self-deadlock this replaces.
    //
    // The test asserts "nothing held yet" — we must be hermetic to the
    // FOREMAN_CLONE_LANE_HELD environment variable, because when this runs
    // under the tier-1 unit (which the operator configures with the marker),
    // the test binary inherits it and the assertion would fail.
    let temp = tempfile::TempDir::new().unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    std::fs::create_dir(repo.join(".git")).unwrap();

    assert!(!cosmix_foreman::clone_lock::lane_held(), "nothing held yet");
    let held = cosmix_foreman::clone_lock::acquire_lane(&repo).unwrap();
    assert!(held.is_some(), "first acquire takes the lane for real");
    assert!(
        cosmix_foreman::clone_lock::lane_held(),
        "and announces it in-process"
    );
    assert!(
        cosmix_foreman::clone_lock::acquire_if_in_repo(&repo)
            .unwrap()
            .is_none(),
        "a nested tier-2 verify joins instead of deadlocking on its own parent"
    );

    drop(held);
    assert!(
        !cosmix_foreman::clone_lock::lane_held(),
        "and the lane is released on drop"
    );
}
