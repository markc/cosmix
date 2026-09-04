//! `mix doctor` — substrate self-check. Pins that it runs, emits the expected
//! check lines, and its exit status reflects health (0 when no ✗). The Bus line
//! is environment-dependent (noded may or may not be up), so it is not asserted.

use std::process::Command;

fn doctor() -> std::process::Output {
    // MIX_DOCTOR_SKIP_BUS keeps the run hermetic — no connection to a real
    // broker if one happens to be configured on the machine running the suite.
    Command::new(env!("CARGO_BIN_EXE_mix"))
        .arg("doctor")
        .env("MIX_DOCTOR_SKIP_BUS", "1")
        .output()
        .expect("spawn mix doctor")
}

#[test]
fn doctor_reports_every_local_check() {
    let out = doctor();
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("mix doctor"), "{s}");
    // The local checks that do not depend on a live mesh.
    for label in ["version", "features", "prelude", "manual", "stats"] {
        assert!(s.contains(label), "doctor missing '{label}':\n{s}");
    }
    // A standard build reports its features; regex/json are always on here.
    assert!(s.contains("regex"), "features line missing regex:\n{s}");
}

#[test]
fn doctor_exit_status_reflects_health() {
    // In the test environment stats is writable and the prelude loads, so the
    // only non-✓ line is the (absent) mesh — a ⚠, not a ✗ — hence exit 0.
    let out = doctor();
    let s = String::from_utf8_lossy(&out.stdout);
    if s.contains("healthy — no ✗ checks") {
        assert_eq!(out.status.code(), Some(0), "healthy run must exit 0:\n{s}");
    } else {
        // If something genuinely failed (✗), the exit must be non-zero — the
        // whole point of a gateable doctor.
        assert_ne!(out.status.code(), Some(0), "a failed check must exit non-zero:\n{s}");
    }
}
