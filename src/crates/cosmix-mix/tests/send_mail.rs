//! `send_mail(msg[, opts])` end to end, against a fake `sendmail`.
//!
//! The fake records its argv and the message it was handed on stdin, and
//! exits 75 (EX_TEMPFAIL) with a stderr line when `SENDMAIL_FAIL` is set.
//! It sits FIRST on PATH, so the lookup can never reach a real MTA on the
//! build worker. A second fake, `ssh`, runs the mix under test as `mix -` so
//! the `host` hop runs its real `ssh_exec` path. Out-of-process: PATH must
//! not be mutated in the test process (see `which_executable.rs`).

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

const FAKE_SENDMAIL: &str = r#"#!/bin/sh
printf '%s\n' "$@" > "$SENDMAIL_CAPTURE.args"
cat > "$SENDMAIL_CAPTURE.eml"
if [ -n "$SENDMAIL_FAIL" ]; then
  echo "sendmail: queue unavailable" >&2
  exit 75
fi
exit 0
"#;

/// Records its own argv, then runs the shipped driver under the mix being
/// tested. With REMOTE_SENDMAIL_STANDIN set it first rewrites the driver's
/// `/usr/sbin/sendmail` to the fake, and refuses outright (exit 99) if the
/// real path survives the rewrite — a test must never reach a real MTA.
const FAKE_SSH: &str = r#"#!/bin/sh
printf '%s\n' "$0" "$@" > "$SENDMAIL_CAPTURE.ssh-argv"
host=
while [ $# -gt 0 ]; do
  if [ "$1" = "--" ]; then shift; host=$1; break; fi
  shift
done
if [ -n "$REMOTE_SENDMAIL_STANDIN" ]; then
  cat > "$SENDMAIL_CAPTURE.driver"
  sed "s#/usr/sbin/sendmail#$REMOTE_SENDMAIL_STANDIN#g" "$SENDMAIL_CAPTURE.driver" > "$SENDMAIL_CAPTURE.rewritten"
  if grep -q /usr/sbin/sendmail "$SENDMAIL_CAPTURE.rewritten"; then
    echo "fake ssh: refusing to run a real sendmail" >&2
    exit 99
  fi
  FAKE_HOST=$host exec "$MIX_UNDER_TEST" - < "$SENDMAIL_CAPTURE.rewritten"
fi
FAKE_HOST=$host exec "$MIX_UNDER_TEST" -
"#;

struct Bed {
    dir: tempfile::TempDir,
}

impl Bed {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        for (name, body) in [("sendmail", FAKE_SENDMAIL), ("ssh", FAKE_SSH)] {
            let p = dir.path().join(name);
            fs::write(&p, body).unwrap();
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        }
        Bed { dir }
    }

    fn capture(&self) -> PathBuf {
        self.dir.path().join("capture")
    }

    fn sendmail(&self) -> String {
        self.dir.path().join("sendmail").display().to_string()
    }

    fn run(&self, program: &str, fail: bool) -> (String, String, bool) {
        self.run_env(program, fail, &[])
    }

    fn run_env(&self, program: &str, fail: bool, extra: &[(&str, &str)]) -> (String, String, bool) {
        let path = format!("{}:/usr/bin:/bin", self.dir.path().display());
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_mix"));
        cmd.arg("-c")
            .arg(program)
            .env("PATH", path)
            .env("MIX_UNDER_TEST", env!("CARGO_BIN_EXE_mix"))
            .env("SENDMAIL_CAPTURE", self.capture())
            .env_remove("REMOTE_SENDMAIL_STANDIN");
        for (k, v) in extra {
            cmd.env(k, v);
        }
        if fail {
            cmd.env("SENDMAIL_FAIL", "1");
        } else {
            cmd.env_remove("SENDMAIL_FAIL");
        }
        let out = cmd.output().expect("spawn mix");
        (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
            out.status.success(),
        )
    }

    fn args(&self) -> Vec<String> {
        fs::read_to_string(self.capture().with_extension("args"))
            .expect("fake sendmail ran")
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn eml(&self) -> String {
        fs::read_to_string(self.capture().with_extension("eml")).expect("fake sendmail ran")
    }
}

const MSG: &str = r#"{to: ["ops@example.com", "Audit <audit@example.com>"], from: "Reports <reports@example.com>", subject: "Weekly spam report", body: "total: 12\nham: 3\n.\nend"}"#;

#[test]
fn local_sendmail_gets_the_envelope_and_the_rendered_message() {
    let bed = Bed::new();
    let (out, err, ok) = bed.run(
        &format!(
            "$r = send_mail({MSG})\nprint($r.ok .. \" \" .. $r.exit_code)\nprint($r.message_id)\n"
        ),
        false,
    );
    assert!(ok, "stdout={out} stderr={err}");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "true 0", "{out}");
    let message_id = lines[1];

    // `-t` recipients from headers, `-i` so a lone "." line is body text,
    // `-f` the bare envelope address.
    assert_eq!(bed.args(), ["-t", "-i", "-f", "reports@example.com"]);

    let eml = bed.eml();
    let (head, body) = eml.split_once("\n\n").expect("header/body separator");
    assert!(head.contains("From: Reports <reports@example.com>\n"), "{eml}");
    assert!(head.contains("To: ops@example.com, Audit <audit@example.com>\n"), "{eml}");
    assert!(head.contains("Subject: Weekly spam report\n"), "{eml}");
    assert!(head.contains(&format!("Message-ID: {message_id}")), "{eml}");
    assert!(head.contains("\nDate: "), "{eml}");
    assert_eq!(body, "total: 12\nham: 3\n.\nend\n", "{eml}");
}

#[test]
fn an_mta_failure_is_data_not_a_raise() {
    let bed = Bed::new();
    let (out, err, ok) = bed.run(
        &format!(
            "$r = send_mail({MSG})\nprint($r.ok .. \" \" .. $r.exit_code .. \" \" .. (pos(\"queue unavailable\", $r.stderr) > 0))\n"
        ),
        true,
    );
    assert!(ok, "the call must not raise: stdout={out} stderr={err}");
    assert_eq!(out.trim(), "false 75 true", "{out}");
}

#[test]
fn a_missing_sendmail_is_data_not_a_raise() {
    let bed = Bed::new();
    let (out, err, ok) = bed.run(
        &format!(
            "$r = send_mail({MSG}, {{sendmail: \"/nonexistent/sendmail\"}})\nprint($r.ok .. \" \" .. ($r.error_code != nil))\n"
        ),
        false,
    );
    assert!(ok, "the call must not raise: stdout={out} stderr={err}");
    assert_eq!(out.trim(), "false true", "{out}");
}

#[test]
fn missing_to_raises_before_anything_runs() {
    let bed = Bed::new();
    let (out, err, _) = bed.run(
        "try\n  $r = send_mail({from: \"a@example.com\", subject: \"s\", body: \"b\"})\n  print(\"sent\")\ncatch $e\n  print(\"raised \" .. (pos(\"msg.to is required\", $e) > 0))\nend\n",
        false,
    );
    assert_eq!(out.trim(), "raised true", "stdout={out} stderr={err}");
    assert!(
        !bed.capture().with_extension("args").exists(),
        "sendmail must not have run"
    );
}

#[test]
fn host_sends_through_ssh_exec_on_that_host() {
    let bed = Bed::new();
    let (out, err, ok) = bed.run(
        &format!(
            "$r = send_mail({MSG}, {{host: \"alpha\", sendmail: \"{}\", timeout: 20}})\nprint($r.ok .. \" \" .. $r.exit_code .. \" \" .. $r.host)\nprint($r.message_id)\n",
            bed.sendmail()
        ),
        false,
    );
    assert!(ok, "stdout={out} stderr={err}");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "true 0 alpha", "{out}");
    assert_eq!(bed.args(), ["-t", "-i", "-f", "reports@example.com"]);
    assert!(
        bed.eml().contains(&format!("Message-ID: {}\n", lines[1])),
        "{}",
        bed.eml()
    );
    bed.assert_ssh_argv("alpha");
}

#[test]
fn host_without_the_sendmail_option_runs_usr_sbin_sendmail_remotely() {
    // The remote default must be /usr/sbin/sendmail. The fake ssh swaps that
    // path for the fake in the shipped driver (and refuses if the swap
    // missed), so the default is proven without a real MTA in reach.
    let bed = Bed::new();
    let standin = bed.sendmail();
    let (out, err, ok) = bed.run_env(
        &format!(
            "$r = send_mail({MSG}, {{host: \"beta\", timeout: 20}})\nprint($r.ok .. \" \" .. $r.exit_code .. \" \" .. $r.host)\n"
        ),
        false,
        &[("REMOTE_SENDMAIL_STANDIN", standin.as_str())],
    );
    assert!(ok, "stdout={out} stderr={err}");
    assert_eq!(out.trim(), "true 0 beta", "stdout={out} stderr={err}");
    let driver = fs::read_to_string(bed.capture().with_extension("driver")).expect("driver captured");
    assert!(
        driver.contains("/usr/sbin/sendmail"),
        "the remote argv[0] must default to /usr/sbin/sendmail: {driver}"
    );
    assert_eq!(bed.args(), ["-t", "-i", "-f", "reports@example.com"]);
    bed.assert_ssh_argv("beta");
}

#[test]
fn a_long_line_body_arrives_quoted_printable_and_decodes_exactly() {
    let bed = Bed::new();
    let (out, err, ok) = bed.run(
        "$line = \"\"\nfor each $i in range(1, 300)\n  $line = $line .. \"a=b; \"\nend\n$r = send_mail({to: \"ops@example.com\", from: \"r@example.com\", subject: \"csv\", body: \"head\\n\" .. $line .. \"\\ntail\"})\nprint($r.ok .. \" \" .. length($line))\n",
        false,
    );
    assert!(ok, "stdout={out} stderr={err}");
    assert_eq!(out.trim(), "true 1500", "{out}");
    let eml = bed.eml();
    let (head, encoded) = eml.split_once("\n\n").unwrap();
    assert!(head.contains("Content-Transfer-Encoding: quoted-printable"), "{head}");
    assert!(encoded.lines().all(|l| l.len() <= 76), "{encoded}");
    let joined = encoded.replace("=\n", "");
    let mut decoded = Vec::new();
    let b = joined.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'=' {
            decoded.push(u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap(), 16).unwrap());
            i += 3;
        } else {
            decoded.push(b[i]);
            i += 1;
        }
    }
    let expected = format!("head\n{}\ntail\n", "a=b; ".repeat(300));
    assert_eq!(String::from_utf8(decoded).unwrap(), expected);
}

impl Bed {
    /// The fake ssh's recorded argv: the real transport shape, not just
    /// "some ssh ran".
    fn assert_ssh_argv(&self, host: &str) {
        let argv: Vec<String> = fs::read_to_string(self.capture().with_extension("ssh-argv"))
            .expect("fake ssh ran")
            .lines()
            .map(str::to_string)
            .collect();
        assert!(argv[0].ends_with("/ssh"), "{argv:?}");
        assert!(argv.iter().any(|a| a == "BatchMode=yes"), "{argv:?}");
        assert!(argv.iter().any(|a| a.starts_with("ConnectTimeout=")), "{argv:?}");
        let n = argv.len();
        assert_eq!(&argv[n - 3..], ["--", host, "/opt/cosmix/bin/mix -"], "{argv:?}");
    }
}
