use super::fixture::write_executable;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

#[test]
fn writes_executable_and_removes_the_stage() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fixture");
    write_executable(&path, "#!/bin/sh\nexit 3\n");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "#!/bin/sh\nexit 3\n"
    );
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(&path).unwrap().permissions().mode()
    };
    assert_eq!(mode & 0o777, 0o755);
    assert!(
        std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .all(|e| e.file_name().to_string_lossy() == "fixture")
    );
}

/// Replay the fleet's failure shape: sibling threads continuously fork while
/// this thread writes a fresh fake and immediately execs it. An in-process
/// destination writer lets those children inherit the writable descriptor;
/// the child-copy writer never puts that descriptor in this process's table.
#[test]
fn exec_survives_a_concurrent_fork_storm() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fixture");
    let stop = Arc::new(AtomicBool::new(false));
    let mut churn = Vec::new();
    for _ in 0..2 {
        let stop = Arc::clone(&stop);
        churn.push(thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                Command::new("true").status().unwrap();
                thread::yield_now();
            }
        }));
    }
    for round in 0..32u32 {
        write_executable(&path, format!("#!/bin/sh\nexit {}\n", round % 100));
        let status = Command::new(&path).status().unwrap();
        assert_eq!(
            status.code(),
            Some((round % 100) as i32),
            "round {round}: fixture exec failed — {status}"
        );
    }
    stop.store(true, Ordering::Relaxed);
    for handle in churn {
        handle.join().unwrap();
    }
}
