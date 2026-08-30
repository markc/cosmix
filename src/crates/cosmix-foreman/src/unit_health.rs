//! Unit health monitoring for fleet visibility.
//!
//! Tracks systemd unit health for the fleet's own units — dispatch, refine,
//! tier-2 verify, and the wake notifier — because none of that is visible
//! in the ledger: a unit stuck in `failed` leaves tasks piling up in `done`
//! with no landings and nothing says why (2026-08-19: an orphaned holder
//! wedged `foreman-refine` behind the clone lock for two hours, invisible
//! because nothing but `systemctl --user status` ever looked).
//!
//! A second, related blind spot, measured the same day: a dispatch sweep
//! that is still running an OLD binary after an operator installs a new
//! one. A sweep started at 16:43 with `--max-tasks 3` kept the 16:43 code
//! in memory across a 16:51 install, so two attempts produced findings from
//! pre-fix code while the operator reasonably believed the fix was live.
//! The installer's version probe asserts the ARTIFACT is new; nothing
//! asserted that RUNNING WORK was using it — the same class as `enable
//! --now` not restarting an active unit. So this module also reports
//! whether an active unit started BEFORE the installed `foreman` binary's
//! mtime.
//!
//! Nothing here kills anything: an agent mid-task is paid work in flight,
//! and the operator decides whether to wait or interrupt. Saying it plainly
//! is the whole fix.
//!
//! Everything here is advisory: a systemctl failure (missing binary, no
//! systemd user session, …) degrades to a note, never an error that could
//! take the rest of `foreman status` down with it.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use chrono::{Local, TimeZone};
use serde::Serialize;

/// The fleet's units, checked for `failed` state.
pub const FLEET_UNITS: &[&str] = &[
    "foreman-dispatch.service",
    "foreman-refine.service",
    "foreman-tier2.service",
    "foreman-wake.service",
];

/// The subset of [`FLEET_UNITS`] worth an active/deploy-age report for.
/// `foreman-wake` is a short-lived notifier — it rings an ABP verb and
/// exits, so it never carries in-flight work across a binary swap and an
/// "it predates the deploy" line about it would be noise.
const ACTIVITY_CHECK_UNITS: &[&str] = &[
    "foreman-dispatch.service",
    "foreman-refine.service",
    "foreman-tier2.service",
];

/// Health/activity snapshot for a single systemd unit.
#[derive(Debug, Clone, Serialize)]
pub struct UnitHealth {
    pub name: String,
    pub active_state: String,
    pub sub_state: String,
    /// Exit code of the last run of the unit's main process.
    pub exit_code: Option<i32>,
    /// Local-time RFC 3339 stamp of the last exit, when the unit is failed.
    pub failed_at: Option<String>,
    /// Local-time RFC 3339 stamp of an active unit's start.
    pub started_at: Option<String>,
    /// How long an active unit has been running, in seconds.
    pub running_secs: Option<i64>,
    /// Set for an active unit whose `ExecMainStartTimestamp` is before the
    /// installed `foreman` binary's mtime — its in-flight work predates the
    /// current deploy and is running the old code.
    pub predates_binary: bool,
}

/// Unit health summary for the fleet.
#[derive(Debug, Clone, Serialize, Default)]
pub struct FleetHealth {
    pub failed: Vec<UnitHealth>,
    pub active: Vec<UnitHealth>,
    /// Set when systemctl itself could not be run at all — not the same as
    /// an individual unit being absent, which is a normal "not-found" reply.
    pub systemd_unavailable: bool,
}

impl FleetHealth {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether this snapshot says anything at all — the gate for the
    /// MACHINE surface (`status --json`). An active sweep is reportable
    /// even when it is perfectly healthy: the amendment asks for "any
    /// fleet unit currently ACTIVE, with how long it has been running",
    /// and a consumer that has to infer activity from an omitted key is
    /// exactly the blind spot this module exists to close.
    pub fn has_report(&self) -> bool {
        !self.failed.is_empty() || !self.active.is_empty() || self.systemd_unavailable
    }

    /// Whether there is anything here worth an operator's attention — the
    /// gate for the HUMAN surface, which must print nothing extra for a
    /// healthy fleet.
    pub fn has_issues(&self) -> bool {
        !self.failed.is_empty()
            || self.systemd_unavailable
            || self.active.iter().any(|u| u.predates_binary)
    }
}

/// The mtime (Unix epoch seconds) of `path`, or `None` if it can't be
/// determined — never fatal, since the deploy-age check is advisory.
pub fn binary_mtime(path: &Path) -> Option<i64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let secs = modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    i64::try_from(secs).ok()
}

/// The mtime of the currently-running `foreman` binary. `None` when
/// `current_exe()` is unavailable — advisory, so never an error.
pub fn current_binary_mtime() -> Option<i64> {
    binary_mtime(&std::env::current_exe().ok()?)
}

/// Which `systemctl` to run. Production resolves the optional override once,
/// before entering the path-injected probe used by tests.
fn systemctl_bin() -> String {
    std::env::var("FOREMAN_SYSTEMCTL_BIN").unwrap_or_else(|_| "systemctl".to_string())
}

struct RawUnit {
    active_state: String,
    sub_state: String,
    exit_code: Option<i32>,
    start_ts: Option<i64>,
    exit_ts: Option<i64>,
}

/// Query one unit's state. `Err(())` means systemctl couldn't be run or
/// answered unsuccessfully — the caller treats that as "systemd
/// unavailable", never as a hard failure.
fn query_unit_with(systemctl: &Path, unit_name: &str) -> Result<RawUnit, ()> {
    // `--timestamp=unix` asks systemd to print `@<epoch>` instead of a
    // localized "Mon 2026-08-18 16:43:12 AEST" string. That matters: the
    // epoch is unambiguous and needs no date/timezone arithmetic to
    // consume, and hand-rolled date maths over the localized form is
    // exactly what made an earlier cut of this module report a start time
    // ~34h in the FUTURE, so no unit was ever flagged.
    let output = Command::new(systemctl)
        .args(["--user", "--timestamp=unix", "show", unit_name])
        .args([
            "--property=ActiveState",
            "--property=SubState",
            "--property=ExecMainStatus",
            "--property=ExecMainStartTimestamp",
            "--property=ExecMainExitTimestamp",
        ])
        .output()
        .map_err(|_| ())?;

    if !output.status.success() {
        return Err(());
    }

    let props = parse_systemd_show(&output.stdout);
    Ok(RawUnit {
        active_state: props.get("ActiveState").cloned().unwrap_or_default(),
        sub_state: props.get("SubState").cloned().unwrap_or_default(),
        exit_code: props.get("ExecMainStatus").and_then(|v| v.parse().ok()),
        start_ts: props
            .get("ExecMainStartTimestamp")
            .and_then(|v| parse_unix_ts(v)),
        exit_ts: props
            .get("ExecMainExitTimestamp")
            .and_then(|v| parse_unix_ts(v)),
    })
}

/// Check the health of the fleet's units against `bin_mtime`, the epoch
/// mtime of the foreman binary a deploy-age comparison should be made
/// against (see [`current_binary_mtime`] / [`binary_mtime`]). Never fails:
/// a systemctl problem is reported via `systemd_unavailable`, not returned
/// as an error.
pub fn check_fleet_health(bin_mtime: Option<i64>) -> FleetHealth {
    let systemctl = systemctl_bin();
    check_fleet_health_with(Path::new(&systemctl), bin_mtime)
}

/// Parameterized version of [`check_fleet_health`] that accepts an explicit
/// systemctl path. This path never reads process environment, so concurrent
/// callers can safely use different fixtures.
pub fn check_fleet_health_with(systemctl: &Path, bin_mtime: Option<i64>) -> FleetHealth {
    let mut health = FleetHealth::new();
    let now = now_epoch();

    for &unit in FLEET_UNITS {
        match query_unit_with(systemctl, unit) {
            Ok(raw) if raw.active_state == "failed" => {
                health.failed.push(UnitHealth {
                    name: unit.to_string(),
                    active_state: raw.active_state,
                    sub_state: raw.sub_state,
                    exit_code: raw.exit_code,
                    failed_at: raw.exit_ts.map(format_ts),
                    started_at: raw.start_ts.map(format_ts),
                    running_secs: None,
                    predates_binary: false,
                });
            }
            Ok(raw)
                if is_running(&raw.active_state, &raw.sub_state)
                    && ACTIVITY_CHECK_UNITS.contains(&unit) =>
            {
                let running_secs = raw.start_ts.map(|start| (now - start).max(0));
                // Strictly before: a unit started in the same second as the
                // install is not evidence of stale code, and claiming it
                // would train the operator to ignore the line.
                let predates_binary = match (raw.start_ts, bin_mtime) {
                    (Some(start), Some(mtime)) => start < mtime,
                    _ => false,
                };
                health.active.push(UnitHealth {
                    name: unit.to_string(),
                    active_state: raw.active_state,
                    sub_state: raw.sub_state,
                    exit_code: raw.exit_code,
                    failed_at: None,
                    started_at: raw.start_ts.map(format_ts),
                    running_secs,
                    predates_binary,
                });
            }
            Ok(_) => {}
            Err(()) => health.systemd_unavailable = true,
        }
    }

    health
}

/// Is this unit's work actually in flight right now?
///
/// `activating` matters as much as `active`, and getting this wrong makes
/// the whole deploy-age report silent: all three sweep units are
/// `Type=oneshot`, and systemd holds a oneshot in `activating`/`start` for
/// the ENTIRE run of its ExecStart, only ever reaching `active` if it has
/// `RemainAfterExit`. Probed live 2026-08-21 — a dispatch sweep mid-task
/// reports `ActiveState=activating`, so an `== "active"` test reports
/// nothing about the exact case the amendment was written for. (The one
/// unit that does sit in `active` is `foreman-wake`, a `Type=simple`
/// citizen, and it is excluded from this report by design.)
///
/// `deactivating` is deliberately excluded: a unit being torn down is not
/// work an operator has to decide about. So is the `active`/`exited` pair
/// — a `RemainAfterExit=yes` oneshot parks there after its ExecStart has
/// FINISHED, and reporting a finished sweep as "still running the old
/// code" forever is exactly the kind of false alarm that teaches an
/// operator to skip the line. (None of the three units sets
/// `RemainAfterExit` today, checked 2026-08-21; this costs one comparison
/// and survives a unit-file edit that does.)
fn is_running(active_state: &str, sub_state: &str) -> bool {
    match active_state {
        "activating" => true,
        "active" => sub_state != "exited",
        _ => false,
    }
}

/// What a failed unit means for the fleet, in plain terms — the whole
/// point of surfacing this at all, per the 2026-08-19 incident: a failed
/// state with no explanation is as silent as no report.
fn failure_meaning(unit: &str) -> &'static str {
    match unit {
        "foreman-dispatch.service" => "no new tasks are being dispatched",
        "foreman-refine.service" => "nothing is landing",
        "foreman-tier2.service" => "tier-2 verification is not running",
        "foreman-wake.service" => {
            "wake notifications aren't firing (the backstop timer still covers it)"
        }
        _ => "the unit is failed",
    }
}

/// Render human-readable status lines. Empty for a healthy fleet — a
/// healthy status view must print nothing extra, so a line here always
/// means something an operator has to decide about.
pub fn render_text(health: &FleetHealth) -> Vec<String> {
    let mut lines = Vec::new();

    if health.systemd_unavailable {
        lines.push(
            "unit health: systemctl unavailable — fleet unit state unchecked (advisory only)"
                .to_string(),
        );
    }

    for u in &health.failed {
        let exit = u
            .exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "?".to_string());
        let at = u.failed_at.as_deref().unwrap_or("an unknown time");
        lines.push(format!(
            "unit health: {} FAILED (exit {exit}) at {at} — {}",
            u.name,
            failure_meaning(&u.name),
        ));
    }

    lines.extend(health.active.iter().filter_map(deploy_age_warning));
    lines
}

/// The one line the 2026-08-19 amendment asks for, for a single unit:
/// in-flight work that predates the installed binary. `None` for anything
/// else — including a healthy active sweep.
fn deploy_age_warning(u: &UnitHealth) -> Option<String> {
    if !u.predates_binary {
        return None;
    }
    let since = u
        .started_at
        .as_deref()
        .map(short_time)
        .unwrap_or_else(|| "an unknown time".to_string());
    let dur = u
        .running_secs
        .map(|s| format!(" ({})", format_duration(s)))
        .unwrap_or_default();
    Some(format!(
        "unit health: {} has been running since {since}{dur} and predates this binary; \
         its remaining tasks will use the OLD code (not killed — that is your call)",
        u.name,
    ))
}

/// Render the deploy-age warnings alone — what an installer prints at the
/// end of a deploy, where a failed-unit report would be about something
/// the install did not touch. Empty when every active sweep is current.
pub fn render_deploy_warnings(health: &FleetHealth) -> Vec<String> {
    health
        .active
        .iter()
        .filter_map(deploy_age_warning)
        .collect()
}

/// `HH:MM` from an RFC 3339 stamp, for the human line. Falls back to the
/// whole stamp rather than dropping the information.
fn short_time(stamp: &str) -> String {
    match chrono::DateTime::parse_from_rfc3339(stamp) {
        Ok(dt) => dt.format("%H:%M").to_string(),
        Err(_) => stamp.to_string(),
    }
}

fn format_duration(secs: i64) -> String {
    let secs = secs.max(0);
    let hours = secs / 3600;
    let minutes = (secs % 3600) / 60;
    if hours > 0 {
        format!("{hours}h{minutes}m")
    } else {
        format!("{minutes}m")
    }
}

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Parse a systemd `--timestamp=unix` value: `@<epoch>`, or empty when the
/// property was never set. Anything else (an older systemd that doesn't
/// know `--timestamp=unix` and prints the localized form) is `None`: the
/// deploy-age report goes quiet rather than guessing.
fn parse_unix_ts(raw: &str) -> Option<i64> {
    raw.trim().strip_prefix('@').and_then(|s| s.parse().ok())
}

/// Local-time RFC 3339. Local, not UTC: the operator reads this beside
/// `systemctl status`, which prints local time.
fn format_ts(ts: i64) -> String {
    match Local.timestamp_opt(ts, 0).single() {
        Some(dt) => dt.to_rfc3339(),
        None => ts.to_string(),
    }
}

/// Parse `systemctl show` output into a key-value map.
fn parse_systemd_show(output: &[u8]) -> HashMap<String, String> {
    let mut props = HashMap::new();
    let stdout = String::from_utf8_lossy(output);
    for line in stdout.lines() {
        if let Some((key, value)) = line.split_once('=') {
            props.insert(key.to_string(), value.to_string());
        }
    }
    props
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;

    /// Write a fake `systemctl` that answers `--user --timestamp=unix show
    /// <unit> ...` with a canned property block per unit name. The default
    /// arm mirrors real systemctl's reply for an unknown unit: exit 0 with
    /// `ActiveState=inactive`, NOT a failure.
    fn fake_systemctl(dir: &std::path::Path, responses: &[(&str, &str)]) -> std::path::PathBuf {
        let path = dir.join("systemctl");
        let mut body = String::from(
            "#!/bin/sh\nunit=\"\"\nfor a in \"$@\"; do case \"$a\" in *.service) unit=\"$a\" ;; esac; done\ncase \"$unit\" in\n",
        );
        for (unit, block) in responses {
            body.push_str(&format!(
                "  {unit})\ncat <<'FIXTURE_EOF'\n{block}\nFIXTURE_EOF\n    ;;\n"
            ));
        }
        body.push_str(
            "  *)\ncat <<'FIXTURE_EOF'\nActiveState=inactive\nSubState=dead\nExecMainStatus=0\nExecMainStartTimestamp=\nExecMainExitTimestamp=\nFIXTURE_EOF\n    ;;\nesac\nexit 0\n",
        );
        crate::fixture::write_executable(&path, body);
        path
    }

    /// Run `check_fleet_health_with` against a fixture systemctl, passing
    /// the fixture explicitly instead of mutating process environment.
    fn with_fake(
        dir: &std::path::Path,
        responses: &[(&str, &str)],
        bin_mtime: Option<i64>,
    ) -> FleetHealth {
        // The fixture is passed explicitly; no PATH mutation is involved.
        let fake = fake_systemctl(dir, responses);
        // ETXTBSY window: in a multi-threaded test process a sibling thread's
        // fork can briefly hold the just-written script's write fd, so the
        // first exec of the fresh fake can fail (Rust std write-then-exec
        // race). The probe folds that into `systemd_unavailable`; retry
        // briefly before believing it. Bounded, so a genuinely broken fake
        // still fails.
        let mut attempt = 0;
        loop {
            let health = check_fleet_health_with(&fake, bin_mtime);
            if !health.systemd_unavailable || attempt >= 50 {
                return health;
            }
            attempt += 1;
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    const ACTIVE_OLD: &str = "ActiveState=active\nSubState=running\nExecMainStatus=0\n\
                              ExecMainStartTimestamp=@1755600000\nExecMainExitTimestamp=";
    const FAILED_REFINE: &str = "ActiveState=failed\nSubState=failed\nExecMainStatus=1\n\
                                 ExecMainStartTimestamp=@1755599000\nExecMainExitTimestamp=@1755600000";

    #[test]
    fn parse_systemd_show_reads_key_value_lines() {
        let output = b"LoadState=loaded\nActiveState=active\nSubState=running\n";
        let props = parse_systemd_show(output);
        assert_eq!(props.get("ActiveState"), Some(&"active".to_string()));
        assert_eq!(props.get("SubState"), Some(&"running".to_string()));
    }

    #[test]
    fn parse_unix_ts_handles_set_empty_and_localized() {
        assert_eq!(parse_unix_ts("@1755600000"), Some(1755600000));
        assert_eq!(parse_unix_ts(""), None);
        // An older systemd that doesn't know --timestamp=unix: no guess.
        assert_eq!(parse_unix_ts("Mon 2026-08-18 16:43:12 AEST"), None);
    }

    /// A start time is in the past, so `running_secs` must be a positive
    /// duration measured from now — the bug that made an earlier cut of
    /// this module report `None` for every realistically-aged unit was a
    /// stamp parsed ~34h into the future.
    #[test]
    fn running_secs_is_a_positive_duration_from_a_past_start() {
        let dir = tempfile::TempDir::new().unwrap();
        let start = now_epoch() - 7_200;
        let block = format!(
            "ActiveState=active\nSubState=running\nExecMainStatus=0\n\
             ExecMainStartTimestamp=@{start}\nExecMainExitTimestamp="
        );
        let health = with_fake(dir.path(), &[("foreman-dispatch.service", &block)], None);

        let secs = health.active[0].running_secs.expect("a runtime");
        assert!(
            (7_200..7_260).contains(&secs),
            "expected ~7200s of runtime, got {secs}"
        );
        assert_eq!(format_duration(secs), "2h0m");
    }

    #[test]
    fn failed_unit_surfaces_with_exit_code_and_time() {
        let dir = tempfile::TempDir::new().unwrap();
        let health = with_fake(
            dir.path(),
            &[("foreman-refine.service", FAILED_REFINE)],
            None,
        );

        assert!(!health.systemd_unavailable);
        assert_eq!(health.failed.len(), 1);
        let f = &health.failed[0];
        assert_eq!(f.name, "foreman-refine.service");
        assert_eq!(f.exit_code, Some(1));
        assert!(
            f.failed_at.is_some(),
            "a failed unit reports when it failed"
        );
        assert!(health.has_issues());
        assert!(health.has_report());

        let lines = render_text(&health);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("foreman-refine.service"));
        assert!(lines[0].contains("exit 1"));
        // The plain-English meaning is the point: a bare state is as
        // silent as no report at all.
        assert!(lines[0].contains("nothing is landing"));
    }

    #[test]
    fn healthy_fleet_prints_nothing_extra() {
        let dir = tempfile::TempDir::new().unwrap();
        let health = with_fake(dir.path(), &[], Some(0));

        assert!(health.failed.is_empty());
        assert!(health.active.is_empty());
        assert!(!health.systemd_unavailable);
        assert!(!health.has_issues());
        assert!(!health.has_report());
        assert!(render_text(&health).is_empty());
        assert!(render_deploy_warnings(&health).is_empty());
    }

    #[test]
    fn unavailable_systemctl_degrades_to_a_note() {
        let health = check_fleet_health_with(Path::new("/nonexistent/systemctl-fixture"), None);

        assert!(health.systemd_unavailable);
        assert!(health.failed.is_empty());
        assert!(health.has_issues());
        let lines = render_text(&health);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("systemctl unavailable"));
        assert!(lines[0].contains("advisory"));
    }

    #[test]
    fn active_unit_predating_binary_is_flagged() {
        let dir = tempfile::TempDir::new().unwrap();
        // Unit started at 1755600000; binary installed one hour later.
        let health = with_fake(
            dir.path(),
            &[("foreman-dispatch.service", ACTIVE_OLD)],
            Some(1755603600),
        );

        assert_eq!(health.active.len(), 1);
        let a = &health.active[0];
        assert!(a.predates_binary);
        assert!(a.started_at.is_some());
        assert!(health.has_issues());

        let lines = render_text(&health);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("foreman-dispatch.service"));
        assert!(lines[0].contains("predates this binary"));
        assert!(lines[0].contains("OLD code"));
        // Nothing is killed — the operator decides.
        assert!(lines[0].contains("your call"));
        // The installer prints exactly this line and nothing else.
        assert_eq!(render_deploy_warnings(&health), lines);
    }

    #[test]
    fn active_unit_started_after_binary_is_not_flagged() {
        let dir = tempfile::TempDir::new().unwrap();
        // Binary installed at 1755599000, before the unit started.
        let health = with_fake(
            dir.path(),
            &[("foreman-dispatch.service", ACTIVE_OLD)],
            Some(1755599000),
        );

        assert_eq!(health.active.len(), 1);
        assert!(!health.active[0].predates_binary);
        assert!(!health.has_issues());
        assert!(render_text(&health).is_empty());
        // …but a healthy active sweep is still MACHINE-reportable: the
        // amendment asks for any active unit and how long it has run.
        assert!(health.has_report());
        assert!(health.active[0].running_secs.is_some());
    }

    /// Same second is not evidence of stale code.
    #[test]
    fn active_unit_started_exactly_at_install_is_not_flagged() {
        let dir = tempfile::TempDir::new().unwrap();
        let health = with_fake(
            dir.path(),
            &[("foreman-dispatch.service", ACTIVE_OLD)],
            Some(1755600000),
        );
        assert!(!health.active[0].predates_binary);
    }

    /// With no binary mtime to compare against there is nothing to claim.
    #[test]
    fn unknown_binary_mtime_never_flags() {
        let dir = tempfile::TempDir::new().unwrap();
        let health = with_fake(
            dir.path(),
            &[("foreman-dispatch.service", ACTIVE_OLD)],
            None,
        );
        assert!(!health.active[0].predates_binary);
        assert!(render_deploy_warnings(&health).is_empty());
    }

    /// The regression that would have made this whole feature silent: all
    /// three sweep units are `Type=oneshot`, so systemd reports a RUNNING
    /// sweep as `activating`/`start`, never `active`.
    #[test]
    fn a_oneshot_sweep_mid_run_reports_as_activating_and_is_still_reported() {
        let dir = tempfile::TempDir::new().unwrap();
        let health = with_fake(
            dir.path(),
            &[(
                "foreman-dispatch.service",
                "ActiveState=activating\nSubState=start\nExecMainStatus=0\n\
                 ExecMainStartTimestamp=@1755600000\nExecMainExitTimestamp=",
            )],
            Some(1755603600),
        );

        assert_eq!(health.active.len(), 1, "a oneshot mid-run is in flight");
        assert_eq!(health.active[0].active_state, "activating");
        assert!(health.active[0].predates_binary);
        assert!(render_text(&health)[0].contains("predates this binary"));
    }

    /// A unit shutting down is not work anyone has to decide about.
    #[test]
    fn deactivating_is_not_reported_as_in_flight() {
        let dir = tempfile::TempDir::new().unwrap();
        let health = with_fake(
            dir.path(),
            &[(
                "foreman-dispatch.service",
                "ActiveState=deactivating\nSubState=stop\nExecMainStatus=0\n\
                 ExecMainStartTimestamp=@1755600000\nExecMainExitTimestamp=",
            )],
            Some(1755603600),
        );
        assert!(health.active.is_empty());
        assert!(!health.has_issues());
    }

    #[test]
    fn wake_unit_is_checked_for_failure_but_not_for_deploy_age() {
        let dir = tempfile::TempDir::new().unwrap();
        // Active wake: excluded from the deploy-age report.
        let health = with_fake(
            dir.path(),
            &[("foreman-wake.service", ACTIVE_OLD)],
            Some(1755603600),
        );
        assert!(health.active.is_empty());
        assert!(!health.has_issues());

        // Failed wake: reported, with its own meaning.
        let failed = with_fake(
            dir.path(),
            &[(
                "foreman-wake.service",
                "ActiveState=failed\nSubState=failed\nExecMainStatus=2\n\
                 ExecMainStartTimestamp=@1755599000\nExecMainExitTimestamp=@1755600000",
            )],
            None,
        );
        assert_eq!(failed.failed.len(), 1);
        assert!(render_text(&failed)[0].contains("backstop timer"));
    }

    #[test]
    fn every_fleet_unit_is_reported_when_all_are_failed() {
        let dir = tempfile::TempDir::new().unwrap();
        let responses: Vec<(&str, &str)> =
            FLEET_UNITS.iter().map(|u| (*u, FAILED_REFINE)).collect();
        let health = with_fake(dir.path(), &responses, None);
        assert_eq!(health.failed.len(), FLEET_UNITS.len());
        assert_eq!(render_text(&health).len(), FLEET_UNITS.len());
    }

    #[test]
    fn binary_mtime_reads_a_real_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("foreman");
        std::fs::write(&path, b"#!/bin/sh\n").unwrap();
        let mtime = binary_mtime(&path).expect("a freshly written file has an mtime");
        assert!((mtime - now_epoch()).abs() < 60, "mtime {mtime} is not now");
        assert_eq!(binary_mtime(&dir.path().join("absent")), None);
    }

    #[test]
    fn format_duration_reads_as_hours_and_minutes() {
        assert_eq!(format_duration(0), "0m");
        assert_eq!(format_duration(-5), "0m");
        assert_eq!(format_duration(90), "1m");
        assert_eq!(format_duration(3_600), "1h0m");
        assert_eq!(format_duration(7_500), "2h5m");
    }

    #[test]
    fn short_time_extracts_hh_mm_and_falls_back_whole() {
        let stamp = format_ts(1755600000);
        let short = short_time(&stamp);
        assert_eq!(short.len(), 5, "{short} should be HH:MM");
        assert_eq!(short_time("not a timestamp"), "not a timestamp");
    }

    /// A `RemainAfterExit=yes` oneshot parks in `active`/`exited` AFTER
    /// its work is done; reporting that as in-flight would be a permanent
    /// false alarm.
    #[test]
    fn a_finished_remain_after_exit_oneshot_is_not_in_flight() {
        let dir = tempfile::TempDir::new().unwrap();
        let health = with_fake(
            dir.path(),
            &[(
                "foreman-tier2.service",
                "ActiveState=active\nSubState=exited\nExecMainStatus=0\n\
                 ExecMainStartTimestamp=@1755600000\nExecMainExitTimestamp=@1755601000",
            )],
            Some(1755603600),
        );
        assert!(health.active.is_empty());
        assert!(!health.has_issues());
        assert!(!health.has_report());
    }

    /// Regression test: two tests running concurrently must not race on a
    /// shared environment variable. Each thread uses a different fixture
    /// and must see only its own responses.
    #[test]
    fn concurrent_tests_use_different_fixtures_without_racing() {
        let dir1 = tempfile::TempDir::new().unwrap();
        let dir2 = tempfile::TempDir::new().unwrap();

        let fake1 = fake_systemctl(
            dir1.path(),
            &[(
                "foreman-dispatch.service",
                "ActiveState=active\nSubState=running\nExecMainStatus=0\n\
                 ExecMainStartTimestamp=@1755600000\nExecMainExitTimestamp=",
            )],
        );
        let fake2 = fake_systemctl(
            dir2.path(),
            &[(
                "foreman-refine.service",
                "ActiveState=failed\nSubState=failed\nExecMainStatus=1\n\
                 ExecMainStartTimestamp=@1755599000\nExecMainExitTimestamp=@1755600000",
            )],
        );

        let barrier = Arc::new(Barrier::new(3));

        // Thread 1: active dispatch, binary predates install.
        let barrier1 = Arc::clone(&barrier);
        let handle1 = thread::spawn(move || {
            barrier1.wait();
            let health = check_fleet_health_with(&fake1, Some(1755603600));
            assert_eq!(health.active.len(), 1, "thread 1: one active unit");
            assert_eq!(
                health.active[0].name, "foreman-dispatch.service",
                "thread 1: got dispatch, not refine"
            );
            assert!(
                health.active[0].predates_binary,
                "thread 1: dispatch predates binary"
            );
            health
        });

        // Thread 2: failed refine.
        let barrier2 = Arc::clone(&barrier);
        let handle2 = thread::spawn(move || {
            barrier2.wait();
            let health = check_fleet_health_with(&fake2, None);
            assert_eq!(health.failed.len(), 1, "thread 2: one failed unit");
            assert_eq!(
                health.failed[0].name, "foreman-refine.service",
                "thread 2: got refine, not dispatch"
            );
            assert_eq!(health.failed[0].exit_code, Some(1), "thread 2: exit code 1");
            health
        });

        barrier.wait();
        let health1 = handle1.join().unwrap();
        let health2 = handle2.join().unwrap();

        // Final verification: each thread saw only its own fixture
        assert_eq!(health1.active[0].name, "foreman-dispatch.service");
        assert_eq!(health2.failed[0].name, "foreman-refine.service");
    }
}
