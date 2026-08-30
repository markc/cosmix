//! Shared process-liveness check: is the process that recorded `(pid,
//! pid_start)` still that same process, or has the pid been reused by
//! something else since, or the process crashed/exited without releasing
//! its hold?
//!
//! Used by the ledger's reservation sweep and the clone lock's stale-holder
//! diagnostics — both record "who took this hold" the same way and need the
//! same answer to "are they still around".

/// Field 22 (starttime) of `/proc/<pid>/stat`, parsed after the last `)` so
/// a bracketed comm cannot shift the fields.
pub fn starttime(pid: i64) -> Option<i64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = stat.rsplit_once(')')?.1;
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// Is the process that recorded `(pid, pid_start)` still that same process —
/// pid alive AND (when recorded) the same `/proc` starttime, so pid reuse by
/// an unrelated long-lived process cannot pin a crashed hold forever.
pub fn owner_alive(pid: Option<i64>, pid_start: Option<i64>) -> bool {
    let Some(p) = pid.filter(|p| *p > 0) else {
        return false;
    };
    if unsafe { libc::kill(p as libc::pid_t, 0) } != 0
        && std::io::Error::last_os_error().raw_os_error() != Some(libc::EPERM)
    {
        return false;
    }
    match (pid_start, starttime(p)) {
        (Some(recorded), Some(current)) => recorded == current,
        // No starttime recorded (older row) or unreadable: err toward alive
        // — a false-dead verdict frees a hold a live run still needs.
        _ => true,
    }
}

/// Whether `ancestor` is this process or appears in `descendant`'s live
/// `/proc` parent chain. Used only for deadlock diagnosis: inability to read
/// one link returns false and leaves the ordinary bounded lock wait in force.
pub fn process_is_ancestor(ancestor: i64, mut descendant: i64) -> bool {
    if ancestor <= 0 || descendant <= 0 {
        return false;
    }
    for _ in 0..256 {
        if descendant == ancestor {
            return true;
        }
        let Some(parent) = parent_pid(descendant) else {
            return false;
        };
        if parent <= 0 || parent == descendant {
            return false;
        }
        descendant = parent;
    }
    false
}

fn parent_pid(pid: i64) -> Option<i64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = stat.rsplit_once(')')?.1;
    // After the command's closing `)`: field 3 is state, field 4 is ppid.
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// PIDs holding an advisory `flock` on `path`, read from `/proc/locks`.
///
/// Strictly diagnostics: it answers "who is this acquire blocked on" when
/// the holder left no stamp in the lock file — which is exactly what
/// `flock(1)` does, since the wrapper takes the lock and then execs, never
/// writing anything. Returns an empty vec on any read/parse failure; a
/// missing hint must never change what the lock itself does.
///
/// Note the answer can legitimately include OUR OWN pid: `flock(1)` execs
/// the command with the locked descriptor still open, so a foreman launched
/// that way already holds the lock it is trying to take.
pub fn flock_holders(path: &std::path::Path) -> Vec<i64> {
    use std::os::unix::fs::MetadataExt;

    let Ok(meta) = std::fs::metadata(path) else {
        return Vec::new();
    };
    let Ok(locks) = std::fs::read_to_string("/proc/locks") else {
        return Vec::new();
    };
    let mut holders = Vec::new();
    for line in locks.lines() {
        // "1: FLOCK  ADVISORY  WRITE 1234 08:02:99 0 EOF". A line for a
        // request that is itself BLOCKED is prefixed "1: -> FLOCK …", and a
        // blocked waiter is not a holder.
        let fields: Vec<&str> = line.split_whitespace().collect();
        let Some(kind) = fields.iter().position(|f| *f == "FLOCK") else {
            continue;
        };
        if fields[..kind].contains(&"->") {
            continue;
        }
        let (Some(pid), Some(target)) = (fields.get(kind + 3), fields.get(kind + 4)) else {
            continue;
        };
        let Ok(pid) = pid.parse::<i64>() else {
            continue;
        };
        if lock_target_matches(target, meta.dev(), meta.ino()) {
            holders.push(pid);
        }
    }
    holders.sort_unstable();
    holders.dedup();
    holders
}

/// `/proc/locks` names the locked file as `MAJOR:MINOR:INODE`, the device
/// numbers in hex and the inode in decimal. The inode must always match;
/// the device is compared only when the pair parses, since an encoding we
/// cannot read should cost us the hint rather than fabricate one.
fn lock_target_matches(target: &str, dev: u64, ino: u64) -> bool {
    let Some((device, inode)) = target.rsplit_once(':') else {
        return false;
    };
    if inode.parse::<u64>() != Ok(ino) {
        return false;
    }
    match parse_dev(device) {
        Some(parsed) => parsed == dev,
        None => true,
    }
}

fn parse_dev(device: &str) -> Option<u64> {
    let (major, minor) = device.split_once(':')?;
    let major = u32::from_str_radix(major, 16).ok()?;
    let minor = u32::from_str_radix(minor, 16).ok()?;
    Some(libc::makedev(major, minor) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_process_is_alive() {
        let pid = std::process::id() as i64;
        assert!(owner_alive(Some(pid), starttime(pid)));
    }

    #[test]
    fn implausible_pid_is_not_alive() {
        assert!(!owner_alive(Some(999_999_999), None));
    }

    #[test]
    fn current_process_is_its_own_ancestor() {
        let pid = std::process::id() as i64;
        assert!(process_is_ancestor(pid, pid));
        assert!(!process_is_ancestor(-1, pid));
    }

    #[test]
    fn no_pid_is_not_alive() {
        assert!(!owner_alive(None, None));
    }

    #[test]
    fn flock_holders_names_this_process_while_it_holds() {
        use std::os::fd::AsRawFd;

        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("held.lock");
        let file = std::fs::File::create(&path).unwrap();

        assert!(
            flock_holders(&path).is_empty(),
            "nobody holds it before we do"
        );

        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(rc, 0, "should have taken a fresh lock");
        assert!(
            flock_holders(&path).contains(&(std::process::id() as i64)),
            "/proc/locks should name us as the holder"
        );

        drop(file);
        // Closing the last descriptor releases the lock — but "the last"
        // is process-wide, and this suite's other tests spawn children.
        // `Command::spawn` forks, and the fork copies our whole fd table;
        // O_CLOEXEC only drops the copy at EXEC, so for the microseconds
        // between a concurrent test's fork and its exec, a child holds a
        // duplicate of THIS descriptor and keeps its open file description
        // — and therefore its flock — alive past our close. `/proc/locks`
        // reports `fl_pid` as recorded at acquisition, so the leftover line
        // still names US, not the child (measured: `us=210826
        // still=[210826]`, ~1 run in 12).
        //
        // So poll rather than sample once. This still tests the property:
        // if closing did not release the lock, the holder never drains and
        // the deadline fails the test.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let holders = flock_holders(&path);
            if holders.is_empty() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "closing the descriptor releases the lock: us={} still={holders:?}",
                std::process::id()
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn flock_holders_on_missing_path_is_empty() {
        assert!(flock_holders(std::path::Path::new("/nonexistent/x.lock")).is_empty());
    }

    #[test]
    fn lock_target_parsing_requires_the_inode_and_tolerates_odd_devices() {
        let dev = libc::makedev(0x08, 0x02) as u64;
        assert!(lock_target_matches("08:02:99", dev, 99));
        assert!(!lock_target_matches("08:02:98", dev, 99));
        assert!(!lock_target_matches("08:03:99", dev, 99));
        // Unparseable device encoding: the inode match alone still gives a
        // hint rather than silently dropping it.
        assert!(lock_target_matches("zz:zz:99", dev, 99));
        assert!(!lock_target_matches("nonsense", dev, 99));
    }

    #[test]
    fn mismatched_starttime_is_not_alive() {
        // Same pid, but a starttime that doesn't match what's recorded —
        // the pid was reused by something else since it was recorded.
        let pid = std::process::id() as i64;
        assert!(!owner_alive(Some(pid), Some(-1)));
    }
}
