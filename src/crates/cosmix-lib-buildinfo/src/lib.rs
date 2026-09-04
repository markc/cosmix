//! Compile-time build provenance for the cosmix daemon family.
//!
//! # Why a shared crate
//!
//! The 2026-06-01 stale-binary incident (a `cosmix-mcp` built before a
//! config-format migration kept running, with no fleet-level signal it
//! was stale) showed that a daemon's semver alone is too weak a "what
//! build is this?" signal — a forgotten bump hides a real change. The
//! truthful fingerprint is `git_sha` + `build_time`. This crate makes
//! both available to every daemon uniformly.
//!
//! # The cross-repo subtlety
//!
//! The daemons live in the `cos` repo; mix in `mix`; this crate in
//! `bus` (the bottom of `bus ← mix ← cos`). The git sha embedded in a
//! daemon must be **that daemon's repo HEAD**, captured at the daemon's
//! own compile. So the capture runs in the *consumer's* `build.rs` (via
//! [`emit`]), and the values surface through env vars that
//! [`build_info!`] reads at the consumer's compile — never bus's.
//!
//! # Usage
//!
//! In the consumer's `Cargo.toml`:
//! ```toml
//! [dependencies]
//! cosmix-lib-buildinfo = { path = "..." }
//! [build-dependencies]
//! cosmix-lib-buildinfo = { path = "..." }
//! ```
//! In `build.rs`:
//! ```ignore
//! fn main() { cosmix_lib_buildinfo::emit(); }
//! ```
//! In the crate:
//! ```ignore
//! let bi = cosmix_lib_buildinfo::build_info!();
//! println!("{} {} ({}{}, built {})", bi.pkg, bi.version, bi.git_sha,
//!          if bi.git_dirty { "-dirty" } else { "" }, bi.build_time);
//! ```

/// Compile-time build provenance for a single crate.
///
/// All fields are `'static` — captured at compile time. Construct via
/// [`build_info!`] (do not build by hand; the macro wires the right
/// `env!`/`option_env!` sources).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildInfo {
    /// `CARGO_PKG_NAME` of the crate the macro expanded in.
    pub pkg: &'static str,
    /// `CARGO_PKG_VERSION` (semver).
    pub version: &'static str,
    /// Short git sha of the consumer repo's HEAD, or `"unknown"` when
    /// the consumer has no `build.rs` calling [`emit`] / no git.
    pub git_sha: &'static str,
    /// FULL 40-hex object id of the consumer repo's HEAD (0.63.0 — the
    /// release-B provenance gate compares a recorded `source_commit`
    /// against this; equality with the short form can never hold).
    /// `"unknown"` under the same conditions as `git_sha`.
    pub git_sha_full: &'static str,
    /// Whether the consumer repo's working tree was dirty at build.
    pub git_dirty: bool,
    /// RFC3339 UTC build timestamp (honours `SOURCE_DATE_EPOCH`).
    pub build_time: &'static str,
}

impl BuildInfo {
    /// Human one-line form: `"<pkg> <version> (<sha>[-dirty], built <time>)"`.
    pub fn line(&self) -> String {
        format!(
            "{} {} ({}{}, built {})",
            self.pkg,
            self.version,
            self.git_sha,
            if self.git_dirty { "-dirty" } else { "" },
            self.build_time,
        )
    }
}

/// Construct a [`BuildInfo`] for the **calling** crate.
///
/// `env!("CARGO_PKG_*")` and `option_env!("COSMIX_*")` resolve at the
/// expansion site, so the values describe the crate that invokes the
/// macro, not this one. `option_env!` (not `env!`) is used for the
/// build.rs-set vars so a crate WITHOUT [`emit`] in its `build.rs`
/// still compiles — its provenance just degrades to `"unknown"` /
/// `false` (itself the "no build.rs wired" signal).
#[macro_export]
macro_rules! build_info {
    () => {
        $crate::BuildInfo {
            pkg: env!("CARGO_PKG_NAME"),
            version: env!("CARGO_PKG_VERSION"),
            git_sha: option_env!("COSMIX_GIT_SHA").unwrap_or("unknown"),
            git_sha_full: option_env!("COSMIX_GIT_SHA_FULL").unwrap_or("unknown"),
            git_dirty: matches!(option_env!("COSMIX_GIT_DIRTY"), Some("1") | Some("true")),
            build_time: option_env!("COSMIX_BUILD_TIME").unwrap_or("unknown"),
        }
    };
}

/// Current wall-clock time as RFC3339 UTC — a runtime helper for a
/// citizen's `started_at` provenance, so daemons need no `chrono` dep.
/// Unlike the build-time stamp this ignores `SOURCE_DATE_EPOCH` (it is a
/// live timestamp, not a reproducible build artifact).
pub fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    rfc3339_utc(secs)
}

// ── build.rs helper ──────────────────────────────────────────────────

/// Emit `cargo:rustc-env=COSMIX_{GIT_SHA,GIT_DIRTY,BUILD_TIME}` for the
/// consumer crate. Call from the consumer's `build.rs` `main()`.
///
/// - `git_sha`: `git rev-parse --short=12 HEAD` in the consumer repo, or
///   `"unknown"` when git is unavailable / there is no repo (e.g. a
///   vendored/tarball build). Never fails the build.
/// - `git_dirty`: `git status --porcelain` non-empty (tracked
///   modifications or untracked non-ignored files) — **repo-wide**, so a
///   dirty *dependency* crate also flags the binary (which is correct: it
///   was built against uncommitted code). Best-effort on freshness: the
///   bit recomputes when the consumer package's own `src/`/`Cargo.toml`
///   change (the rerun watches below), so between rebuilds it can lag an
///   uncommitted edit elsewhere in the repo. Production deploys build
///   from a clean checkout, where it is reliably `false`.
/// - `build_time`: RFC3339 UTC. Honours `SOURCE_DATE_EPOCH` (so a
///   reproducible build can pin it); falls back to wall-clock `now()`.
///
/// Re-run triggers are emitted so the sha tracks new commits and the
/// timestamp follows the epoch override; see the source for the
/// branch-ref watch.
pub fn emit() {
    let sha = git(&["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    let sha_full = git(&["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    let dirty = git(&["status", "--porcelain"])
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);

    println!("cargo:rustc-env=COSMIX_GIT_SHA={sha}");
    println!("cargo:rustc-env=COSMIX_GIT_SHA_FULL={sha_full}");
    println!(
        "cargo:rustc-env=COSMIX_GIT_DIRTY={}",
        if dirty { 1 } else { 0 }
    );
    println!("cargo:rustc-env=COSMIX_BUILD_TIME={}", build_time());

    // Emitting ANY `rerun-if-changed` disables Cargo's default "rerun on
    // any package-file change" scan, so we must re-add the source watch
    // ourselves — otherwise `git_dirty`/`build_time` go stale on an
    // uncommitted edit (Codex P0 MAJOR) — AND watch git metadata, which
    // Cargo's package scan never sees, so the sha tracks new commits.
    if let Ok(manifest) = std::env::var("CARGO_MANIFEST_DIR") {
        println!("cargo:rerun-if-changed={manifest}/src");
        println!("cargo:rerun-if-changed={manifest}/Cargo.toml");
    }
    // HEAD lives in the per-worktree git dir; branch refs + packed-refs
    // live in the COMMON dir — they differ under a linked worktree
    // (`claude --worktree`), so watch both. Watching only the
    // per-worktree dir would embed a stale sha after a same-branch
    // commit in a worktree (Codex P0 MAJOR). Best-effort: absent git →
    // no watches, fixed "unknown" sha.
    if let Some(git_dir) = git(&["rev-parse", "--git-dir"]) {
        println!("cargo:rerun-if-changed={}/HEAD", git_dir.trim());
    }
    if let Some(common) = git(&["rev-parse", "--git-common-dir"]) {
        let c = common.trim();
        println!("cargo:rerun-if-changed={c}/packed-refs");
        if let Some(reff) = git(&["symbolic-ref", "--quiet", "HEAD"]) {
            println!("cargo:rerun-if-changed={c}/{}", reff.trim());
        }
    }
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
}

fn git(args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

fn build_time() -> String {
    let epoch = match std::env::var("SOURCE_DATE_EPOCH") {
        // Set + valid → pin (reproducible build).
        Ok(raw) if raw.trim().parse::<i64>().is_ok() => raw.trim().parse::<i64>().unwrap(),
        // Set + garbage → the caller WANTS a pinned time; silently
        // falling back to wall-clock would defeat reproducibility
        // (Codex P0 MAJOR). Warn and use a deterministic 0 instead.
        Ok(_) => {
            println!(
                "cargo:warning=SOURCE_DATE_EPOCH is set but not a valid integer; \
                 using epoch 0 for COSMIX_BUILD_TIME (deterministic, not wall-clock)"
            );
            0
        }
        // Unset → ordinary build; wall-clock is the freshness signal.
        Err(_) => std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
    };
    rfc3339_utc(epoch)
}

/// Format a Unix epoch (seconds) as RFC3339 UTC, dep-free.
///
/// Uses Howard Hinnant's `civil_from_days` algorithm (public domain) so
/// no `chrono`/`time` dependency is pulled into every daemon's build.
fn rfc3339_utc(epoch_secs: i64) -> String {
    let days = epoch_secs.div_euclid(86_400);
    let rem = epoch_secs.rem_euclid(86_400);
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    // civil_from_days: days since 1970-01-01 → (year, month, day).
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if month <= 2 { y + 1 } else { y };

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_zero_is_unix_epoch() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn known_timestamp_formats_correctly() {
        // 2026-06-01T00:00:00Z = 1_780_272_000 (verified externally).
        assert_eq!(rfc3339_utc(1_780_272_000), "2026-06-01T00:00:00Z");
        // A mid-day, mid-month value with all fields non-zero.
        // 2021-07-15T12:34:56Z = 1_626_352_496.
        assert_eq!(rfc3339_utc(1_626_352_496), "2021-07-15T12:34:56Z");
    }

    #[test]
    fn leap_day_handled() {
        // 2020-02-29T00:00:00Z = 1_582_934_400.
        assert_eq!(rfc3339_utc(1_582_934_400), "2020-02-29T00:00:00Z");
    }

    #[test]
    fn build_info_line_includes_dirty_marker() {
        let bi = BuildInfo {
            pkg: "cosmix-demo",
            version: "1.2.3",
            git_sha: "abc123def456",
            git_sha_full: "abc123def456abc123def456abc123def456abc1",
            git_dirty: true,
            build_time: "2026-06-01T00:00:00Z",
        };
        assert_eq!(
            bi.line(),
            "cosmix-demo 1.2.3 (abc123def456-dirty, built 2026-06-01T00:00:00Z)"
        );
    }

    #[test]
    fn build_info_macro_expands_in_this_crate() {
        // Without a build.rs setting COSMIX_*, the macro degrades to
        // "unknown"/false but still yields this crate's pkg/version.
        let bi = build_info!();
        assert_eq!(bi.pkg, "cosmix-lib-buildinfo");
        assert!(!bi.version.is_empty());
    }
}
