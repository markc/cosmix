//! SPEC-10 daemon-identity legibility surface.
//!
//! `cosmix-wgd` takes a SPEC-10 daemon identity (UID/GID 515, Bus service
//! `wgd`) exactly like `cosmix-dnsd`/`-maild`/`-webd`/`-noded`. Identity is
//! *enforced* by the shared sysusers fragment + the systemd unit's
//! `User=`/`Group=` + the registry-generic `spec10_postcheck.mix` — never by
//! bespoke Rust. This module adds only a **legibility surface**: a structured
//! startup log stating the registered identity, plus an advisory euid/egid
//! self-report. It deliberately does NOT refuse to start on mismatch (the
//! established maild/webd/dnsd posture — systemd `User=` is the enforcement
//! point; inventing a stricter in-process gate would diverge from the pattern).
//!
//! wgd is UID/GID **515** — the next free daemon-identity slot after the
//! obs-tier band (500–514 used; the append-only frontier advances to 515).

/// SPEC-10 v1.4.5 registered daemon-identity facts for `cosmix-wgd`. These MUST
/// track SPEC-10 Appendix A / §2.2 / §9.3. Drift is caught two ways: the
/// registry-generic `spec10_postcheck.mix` lint out of process, and the unit
/// test below cross-checking these constants against the checked-in
/// `src/_etc/sysusers/cosmix.conf` projection (a renumber/rename trips
/// `cargo test`, not only the Mix gate).
const SPEC10_VERSION: &str = "1.4.5";
const DAEMON_NAME: &str = "cosmix-wgd";
const DAEMON_UID: u32 = 515;
/// SPEC-10 §2.2: every daemon-identity row has GID == UID.
const DAEMON_GID: u32 = 515;
/// SPEC-10 R6: Bus service name is the daemon name minus `cosmix-`.
pub const BUS_SERVICE: &str = "wgd";

/// Emit the SPEC-10 daemon-identity legibility surface at startup: one
/// structured identity log line plus an advisory euid/egid self-report. Never
/// refuses — enforcement is sysusers + systemd `User=` + the registry-generic
/// `spec10_postcheck.mix` (the maild/webd/dnsd posture).
pub fn report_spec10_identity() {
    tracing::info!(
        cosmix.spec = 10,
        cosmix.spec.version = SPEC10_VERSION,
        cosmix.daemon = DAEMON_NAME,
        cosmix.uid = DAEMON_UID,
        cosmix.gid = DAEMON_GID,
        cosmix.bus_service = BUS_SERVICE,
        "SPEC-10 daemon identity (citizen build): enforced by sysusers + systemd User= + registry-generic spec10 postcheck; this line is the legibility surface"
    );

    match effective_ids() {
        Some((euid, egid)) => {
            if euid != DAEMON_UID || egid != DAEMON_GID {
                tracing::warn!(
                    running.euid = euid,
                    running.egid = egid,
                    expected.uid = DAEMON_UID,
                    expected.gid = DAEMON_GID,
                    "running euid/egid does NOT match the SPEC-10 cosmix-wgd slot (515/515) — ADVISORY only, not refusing (systemd User= is the enforcement point, matching the maild/webd/dnsd posture)"
                );
            } else {
                tracing::info!(
                    running.euid = euid,
                    running.egid = egid,
                    "effective uid/gid match the SPEC-10 cosmix-wgd slot"
                );
            }
        }
        None => {
            tracing::warn!(
                "could not read /proc/self/status for the advisory euid/egid self-report (non-fatal — advisory surface only)"
            );
        }
    }
}

/// Read effective uid/gid from `/proc/self/status`. The `Uid:` / `Gid:` lines
/// are tab-separated `real effective saved fs`; field index 1 (0-based, after
/// the label) is the *effective* id. Linux-only, zero new dependency. Returns
/// `None` on any read/parse miss (advisory surface — a miss is a soft WARN).
fn effective_ids() -> Option<(u32, u32)> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let mut euid = None;
    let mut egid = None;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("Uid:") {
            euid = rest.split_whitespace().nth(1).and_then(|v| v.parse().ok());
        } else if let Some(rest) = line.strip_prefix("Gid:") {
            egid = rest.split_whitespace().nth(1).and_then(|v| v.parse().ok());
        }
    }
    Some((euid?, egid?))
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
    fn spec10_identity_matches_checked_in_sysusers_fragment() {
        // Non-circular: cross-check the in-code SPEC-10 facts against the
        // checked-in `src/_etc/sysusers/cosmix.conf` projection of Appendix A
        // (the same fragment `spec10_postcheck.mix` validates). A registry edit
        // that renumbers or renames cosmix-wgd trips THIS unit test, not just
        // the out-of-process Mix lint.
        let conf = std::fs::read_to_string(manifest_dir().join("../../_etc/sysusers/cosmix.conf"))
            .expect("checked-in sysusers fragment must be readable from the workspace");

        // Header line pins the registry version: `... cosmix-daemon-identity v1.4.5.`
        let version = conf
            .lines()
            .find_map(|l| l.split("cosmix-daemon-identity v").nth(1))
            .map(|v| v.trim().trim_end_matches('.'))
            .expect("sysusers header must name the cosmix-daemon-identity version");
        assert_eq!(
            version, SPEC10_VERSION,
            "sysusers fragment was generated from a different SPEC-10 version than the daemon pins"
        );

        // The `u cosmix-wgd <uid> "GECOS ..." ...` row.
        let row = conf
            .lines()
            .map(|l| l.split_whitespace().collect::<Vec<_>>())
            .find(|f| f.first() == Some(&"u") && f.get(1) == Some(&DAEMON_NAME))
            .expect("sysusers fragment must carry a `u cosmix-wgd` daemon-identity row");
        let uid: u32 = row[2]
            .parse()
            .expect("sysusers cosmix-wgd uid column must be numeric");
        assert_eq!(
            uid, DAEMON_UID,
            "sysusers fragment assigns cosmix-wgd a different UID than the daemon pins"
        );

        // SPEC-10 §2.2: GID == UID.
        assert_eq!(DAEMON_GID, DAEMON_UID, "SPEC-10 §2.2: GID == UID");

        // SPEC-10 R6: Bus service name is the daemon name minus `cosmix-`.
        assert_eq!(
            DAEMON_NAME.strip_prefix("cosmix-"),
            Some(BUS_SERVICE),
            "SPEC-10 R6: Bus service name must be the daemon name minus the cosmix- prefix"
        );
    }

    #[test]
    fn effective_ids_reads_self() {
        assert!(
            effective_ids().is_some(),
            "expected to parse /proc/self/status Uid:/Gid:"
        );
    }
}
