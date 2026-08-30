//! Unit health through the real binary — the surface an operator actually
//! reads.
//!
//! The unit tests in `unit_health` prove the parsing and the predicates;
//! these prove what the 2026-08-19 incident was actually about: whether
//! `foreman status` SAYS anything. A merge queue deadlocked for two hours
//! was invisible because no fleet surface ever looked at the fleet's own
//! units, so a module that computes the answer correctly and never prints
//! it would be the same bug with more code.
//!
//! Every test drives a fixture `systemctl` through `FOREMAN_SYSTEMCTL_BIN`;
//! nothing here touches the real user session.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

mod support;

/// Write a fake `systemctl` answering `show <unit>` with a canned property
/// block per unit. The default arm mirrors real systemctl's reply for an
/// unknown unit: exit 0, `ActiveState=inactive`.
fn fake_systemctl(dir: &Path, responses: &[(&str, String)]) -> PathBuf {
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
    support::write_executable(&path, body);
    path
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn active_since(start: i64) -> String {
    format!(
        "ActiveState=active\nSubState=running\nExecMainStatus=0\n\
         ExecMainStartTimestamp=@{start}\nExecMainExitTimestamp="
    )
}

fn failed_run(exit: i32, start: i64, end: i64) -> String {
    format!(
        "ActiveState=failed\nSubState=failed\nExecMainStatus={exit}\n\
         ExecMainStartTimestamp=@{start}\nExecMainExitTimestamp=@{end}"
    )
}

struct Fleet {
    dir: tempfile::TempDir,
    db: PathBuf,
    systemctl: PathBuf,
}

impl Fleet {
    /// A ledger plus a fixture systemctl with the given canned replies.
    fn new(responses: &[(&str, String)]) -> Self {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("fleet-store.sqlite");
        let systemctl = fake_systemctl(dir.path(), responses);
        let fleet = Fleet { dir, db, systemctl };
        assert!(fleet.run(&["init"]).status.success());
        fleet
    }

    /// A fleet whose systemctl cannot be executed at all.
    fn without_systemctl() -> Self {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("fleet-store.sqlite");
        let systemctl = dir.path().join("no-such-systemctl");
        let fleet = Fleet { dir, db, systemctl };
        assert!(fleet.run(&["init"]).status.success());
        fleet
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_foreman"))
            .args(args)
            .env("FOREMAN_SYSTEMCTL_BIN", &self.systemctl)
            .env("FOREMAN_DB", &self.db)
            .env("FOREMAN_VERIFY_LANE", self.dir.path().join("verify.lock"))
            .env("FOREMAN_VERIFY_LANE_WAIT_SECS", "30")
            .output()
            .expect("spawning foreman")
    }

    fn stdout(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "foreman {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn status_json(&self) -> serde_json::Value {
        serde_json::from_str(&self.stdout(&["status", "--json"])).expect("status --json")
    }

    /// A file standing in for a freshly-installed foreman binary.
    fn fresh_binary(&self, name: &str) -> PathBuf {
        let path = self.dir.path().join(name);
        std::fs::write(&path, b"#!/bin/sh\n").unwrap();
        path
    }
}

// ---------------------------------------------------------------------
// (1) a failed unit surfaces in the status output
// ---------------------------------------------------------------------

#[test]
fn failed_refine_unit_surfaces_in_status_text() {
    let fleet = Fleet::new(&[(
        "foreman-refine.service",
        failed_run(1, now() - 7_200, now() - 6_300),
    )]);

    let text = fleet.stdout(&["status"]);
    assert!(
        text.contains("foreman-refine.service"),
        "the failed unit must be named: {text}"
    );
    assert!(text.contains("FAILED"), "{text}");
    assert!(text.contains("exit 1"), "{text}");
    // The whole point of the 2026-08-19 incident: say what it MEANS.
    assert!(
        text.contains("nothing is landing"),
        "a failed refine must say what it costs: {text}"
    );
    // Still a full status view, not a report that replaced it.
    assert!(text.contains("tasks:"), "{text}");
}

#[test]
fn failed_unit_surfaces_in_status_json() {
    let fleet = Fleet::new(&[(
        "foreman-refine.service",
        failed_run(1, now() - 7_200, now() - 6_300),
    )]);

    let json = fleet.status_json();
    let failed = &json["unit_health"]["failed"];
    assert_eq!(failed[0]["name"], "foreman-refine.service");
    assert_eq!(failed[0]["exit_code"], 1);
    assert!(
        failed[0]["failed_at"].is_string(),
        "a failed unit reports when it failed: {json:#}"
    );
    // The rest of the status object is untouched.
    assert!(json["tasks"].is_object(), "{json:#}");
    assert_eq!(json["runs"]["delivery_void_fraction"], 0.0);
    assert_eq!(json["runs"]["quality_void_fraction"], 0.0);
    assert_eq!(
        json["total_spend_usd_delivery_void"]["contributing_runs"],
        0
    );
    assert_eq!(json["governor"]["delivery_void"]["fraction"], 0.0);
}

// ---------------------------------------------------------------------
// (2) a healthy fleet prints nothing extra
// ---------------------------------------------------------------------

#[test]
fn healthy_fleet_prints_nothing_extra() {
    let fleet = Fleet::new(&[]);

    let text = fleet.stdout(&["status"]);
    assert!(!text.contains("unit health"), "{text}");
    assert!(text.contains("tasks:"), "{text}");

    // Nothing to report at all, so the key is absent from the JSON too.
    let json = fleet.status_json();
    assert!(json.get("unit_health").is_none(), "{json:#}");

    assert_eq!(fleet.stdout(&["fleet-check"]), "");
}

// ---------------------------------------------------------------------
// (3) an unavailable systemctl degrades to a note, not an error
// ---------------------------------------------------------------------

#[test]
fn unavailable_systemctl_degrades_to_a_note() {
    let fleet = Fleet::without_systemctl();

    let out = fleet.run(&["status"]);
    assert!(
        out.status.success(),
        "an absent systemctl must not fail the status view"
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("systemctl unavailable"), "{text}");
    assert!(text.contains("advisory"), "{text}");
    // The rest of the view still works — that is the whole contract.
    assert!(text.contains("tasks:"), "{text}");

    let json = fleet.status_json();
    assert_eq!(json["unit_health"]["systemd_unavailable"], true);

    let check = fleet.run(&["fleet-check"]);
    assert!(check.status.success(), "fleet-check is advisory too");
}

// ---------------------------------------------------------------------
// Deploy age: in-flight work that predates the installed binary
// ---------------------------------------------------------------------

/// The 2026-08-19 amendment, end to end: a sweep started before the binary
/// was installed keeps running the old code, and the surface says so.
#[test]
fn active_sweep_predating_the_binary_is_named_in_status_and_fleet_check() {
    let started = now() - 7_500;
    let fleet = Fleet::new(&[("foreman-dispatch.service", active_since(started))]);

    // A binary written now is newer than a sweep started 2h05m ago.
    let newer = fleet.fresh_binary("foreman-new");

    let text = fleet.stdout(&["fleet-check", "--binary", newer.to_str().unwrap()]);
    assert!(text.contains("foreman-dispatch.service"), "{text}");
    assert!(text.contains("predates this binary"), "{text}");
    assert!(text.contains("OLD code"), "{text}");
    assert!(text.contains("2h5m"), "the runtime must be stated: {text}");
    // Explicitly NOT killed: the operator decides whether to interrupt.
    assert!(text.contains("your call"), "{text}");

    // The machine surface carries the same facts, structured.
    let json = fleet.status_json();
    let active = &json["unit_health"]["active"][0];
    assert_eq!(active["name"], "foreman-dispatch.service");
    assert!(active["running_secs"].as_i64().unwrap() >= 7_500);
    assert!(active["started_at"].is_string(), "{json:#}");
}

/// An active sweep started AFTER the deploy is healthy: no warning line,
/// but still present in the machine surface, since "which sweeps are
/// running, and for how long" is itself the thing that was invisible.
#[test]
fn active_sweep_newer_than_the_binary_is_json_only() {
    // Started NOW, deliberately: bare `status` compares against the
    // running binary's own mtime, and the test binary's mtime is whenever
    // cargo last built it. A sweep dated "60s ago" therefore passes or
    // fails depending on how recently the tree was compiled — a fixture
    // must not be a stopwatch race against the build.
    let fleet = Fleet::new(&[("foreman-dispatch.service", active_since(now()))]);
    // …and an explicitly-named binary, back-dated an hour, for the
    // installer path.
    let older = fleet.fresh_binary("foreman-old");
    let ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3_600);
    std::fs::OpenOptions::new()
        .write(true)
        .open(&older)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(ago))
        .unwrap();

    let text = fleet.stdout(&["status"]);
    assert!(!text.contains("unit health"), "{text}");
    assert_eq!(
        fleet.stdout(&["fleet-check", "--binary", older.to_str().unwrap()]),
        "",
        "a sweep started after the install is not stale"
    );

    let json = fleet.status_json();
    let active = &json["unit_health"]["active"][0];
    assert_eq!(active["name"], "foreman-dispatch.service");
    assert_eq!(active["predates_binary"], false);
    assert!(active["running_secs"].as_i64().unwrap() >= 0);

    // --all reports it in the human surface on request.
    let all = fleet.stdout(&["fleet-check", "--all"]);
    assert!(all.contains("foreman-dispatch.service"), "{all}");
    assert!(all.contains("active for"), "{all}");
}

/// An installer that names a binary foreman cannot read must be told the
/// check did not run, not left with a silent all-clear.
#[test]
fn unreadable_binary_argument_says_the_check_was_skipped() {
    let fleet = Fleet::new(&[("foreman-dispatch.service", active_since(now() - 7_200))]);
    let text = fleet.stdout(&["fleet-check", "--binary", "/nonexistent/foreman"]);
    assert!(text.contains("deploy-age check skipped"), "{text}");
}

/// `fleet-check` needs no ledger: a deploy can run it before `init`.
#[test]
fn fleet_check_needs_no_ledger() {
    let dir = tempfile::TempDir::new().unwrap();
    let systemctl = fake_systemctl(dir.path(), &[]);
    let out = Command::new(env!("CARGO_BIN_EXE_foreman"))
        .args(["fleet-check"])
        .env("FOREMAN_SYSTEMCTL_BIN", &systemctl)
        .env("FOREMAN_DB", dir.path().join("absent/store.sqlite"))
        .env("FOREMAN_VERIFY_LANE", dir.path().join("verify.lock"))
        .env("FOREMAN_VERIFY_LANE_WAIT_SECS", "30")
        .output()
        .expect("spawning foreman");
    assert!(
        out.status.success(),
        "fleet-check must not need a ledger: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
