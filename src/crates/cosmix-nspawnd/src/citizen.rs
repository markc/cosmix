//! SPEC-10 daemon identity legibility surface.

pub const BUS_SERVICE: &str = "nspawnd";
const SPEC10_VERSION: &str = "1.4.5";
const DAEMON_NAME: &str = "cosmix-nspawnd";
const DAEMON_UID: u32 = 518;
const DAEMON_GID: u32 = 518;

pub fn report_spec10_identity() {
    tracing::info!(
        cosmix.spec = 10,
        cosmix.spec.version = SPEC10_VERSION,
        cosmix.daemon = DAEMON_NAME,
        cosmix.uid = DAEMON_UID,
        cosmix.gid = DAEMON_GID,
        cosmix.bus_service = BUS_SERVICE,
        "SPEC-10 daemon identity; systemd User=/Group= is the enforcement point"
    );
    if let Some((uid, gid)) = effective_ids() {
        if uid == DAEMON_UID && gid == DAEMON_GID {
            tracing::info!(
                running.euid = uid,
                running.egid = gid,
                "effective identity matches SPEC-10"
            );
        } else {
            tracing::warn!(
                running.euid = uid,
                running.egid = gid,
                expected.uid = DAEMON_UID,
                expected.gid = DAEMON_GID,
                "effective identity differs from SPEC-10 (advisory for root-only admin/once mode)"
            );
        }
    }
}

fn effective_ids() -> Option<(u32, u32)> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let mut uid = None;
    let mut gid = None;
    for line in status.lines() {
        if let Some(value) = line.strip_prefix("Uid:") {
            uid = value.split_whitespace().nth(1)?.parse().ok();
        } else if let Some(value) = line.strip_prefix("Gid:") {
            gid = value.split_whitespace().nth(1)?.parse().ok();
        }
    }
    Some((uid?, gid?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run-time manifest directory rather than the `env!`-baked one: cargo
    /// exports `CARGO_MANIFEST_DIR` into the test process, and that names
    /// the tree cargo is actually running in, whereas `env!` records
    /// whichever tree last *compiled* the binary. The two diverge when one
    /// `CARGO_TARGET_DIR` is shared across several git worktrees of this
    /// repo. Falls back to the compile-time value when run outside cargo.
    fn manifest_dir() -> std::path::PathBuf {
        std::env::var_os("CARGO_MANIFEST_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")))
    }

    #[test]
    fn spec10_identity_matches_generated_sysusers_projection() {
        let conf = std::fs::read_to_string(manifest_dir().join("../../_etc/sysusers/cosmix.conf"))
            .unwrap();
        let version = conf
            .lines()
            .find_map(|line| line.split("cosmix-daemon-identity v").nth(1))
            .map(|value| value.trim().trim_end_matches('.'))
            .unwrap();
        assert_eq!(version, SPEC10_VERSION);
        let row = conf
            .lines()
            .map(|line| line.split_whitespace().collect::<Vec<_>>())
            .find(|fields| fields.first() == Some(&"u") && fields.get(1) == Some(&DAEMON_NAME))
            .unwrap();
        assert_eq!(row[2].parse::<u32>().unwrap(), DAEMON_UID);
        assert_eq!(DAEMON_GID, DAEMON_UID);
        assert_eq!(DAEMON_NAME.strip_prefix("cosmix-"), Some(BUS_SERVICE));
    }
}
