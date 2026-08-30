//! `mix lint` must distinguish executable Mix from strict-data files.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let serial = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "mix-lint-strict-data-{}-{serial}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create temporary test directory");
        Self(path)
    }

    fn write(&self, name: &str, source: &str) -> PathBuf {
        let path = self.0.join(name);
        fs::write(&path, source).expect("write lint fixture");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn lint(path: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_mix"))
        .args(["lint", path.to_str().expect("UTF-8 test path")])
        .env("MIX_STATS", "off")
        .output()
        .expect("run mix lint")
}

#[test]
fn valid_data_under_arbitrary_name_is_identified_plainly() {
    let dir = TempDir::new();
    let path = dir.write(
        "settings.data",
        "public_hygiene: {\n  enabled: true,\n  names: [\"alpha\", \"beta\"]\n}\n",
    );
    let out = lint(&path);
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert_eq!(out.status.code(), Some(0), "stdout:\n{stdout}");
    assert!(
        stdout.contains("validated as strict data (not as a script)"),
        "stdout:\n{stdout}"
    );
    assert!(stdout.contains("0 error(s), 0 warning(s)"));
    assert!(out.stderr.is_empty());
}

#[test]
fn malformed_data_gets_its_own_parse_code() {
    let dir = TempDir::new();
    let path = dir.write(
        "broken.conf.mix",
        "public_hygiene: {\n  enabled: true\n  names: [\"alpha\",\n}\n",
    );
    let out = lint(&path);
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert_eq!(out.status.code(), Some(1), "stdout:\n{stdout}");
    assert!(stdout.contains("MIX-E1003 error: strict-data parse error"));
    assert!(!stdout.contains("MIX-E1002"));
}

#[test]
fn broken_script_keeps_the_script_parse_diagnostic() {
    let dir = TempDir::new();
    let path = dir.write("broken.mix", "print(\"unfinished\"\n");
    let out = lint(&path);
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert_eq!(out.status.code(), Some(1), "stdout:\n{stdout}");
    assert!(stdout.contains("MIX-E1002 error:"), "stdout:\n{stdout}");
    assert!(!stdout.contains("MIX-E1003"));
    assert!(!stdout.contains("validated as strict data"));
}

#[test]
fn assignment_chain_is_a_parse_error_not_a_lint_warning() {
    let dir = TempDir::new();
    let path = dir.write("assignment-chain.mix", "$ok = true || false\n");
    let out = lint(&path);
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert_eq!(out.status.code(), Some(1), "stdout:\n{stdout}");
    assert!(stdout.contains("MIX-E1002 error:"), "stdout:\n{stdout}");
    assert!(
        stdout.contains("assignment cannot be chained with `||`"),
        "stdout:\n{stdout}"
    );
    assert!(!stdout.contains("MIX-W2303"), "stdout:\n{stdout}");
}

/// The text assertion above also passes through the generic `ParseError`
/// catch-all, so it cannot tell whether the dedicated lint arm is still wired.
/// JSON can: the catch-all yields null line/column and re-embeds the whole
/// `Parse error at line N, column M:` prefix in the message.
#[test]
fn assignment_chain_json_carries_a_position_and_bare_prose() {
    let dir = TempDir::new();
    let path = dir.write("assignment-chain-json.mix", "$ok = true || false\n");
    let out = Command::new(env!("CARGO_BIN_EXE_mix"))
        .args(["lint", "--json", path.to_str().expect("UTF-8 test path")])
        .env("MIX_STATS", "off")
        .output()
        .expect("run mix lint --json");
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        stdout.contains("\"code\": \"MIX-E1002\""),
        "stdout:\n{stdout}"
    );
    assert!(stdout.contains("\"line\": 1"), "stdout:\n{stdout}");
    assert!(!stdout.contains("\"line\": null"), "stdout:\n{stdout}");
    assert!(!stdout.contains("\"column\": null"), "stdout:\n{stdout}");
    assert!(
        stdout.contains("assignment cannot be chained with `||`"),
        "stdout:\n{stdout}"
    );
    assert!(
        !stdout.contains("Parse error at line"),
        "message must be the bare diagnostic, not the Display prefix:\n{stdout}"
    );
}
