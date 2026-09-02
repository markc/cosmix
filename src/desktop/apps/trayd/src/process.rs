//! Independent transient-unit launch and bounded daemon control.

use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(test)]
use std::sync::{Arc, Mutex};
use std::thread;

#[cfg(test)]
use crate::bus::TestOpenGate;
use crate::systemd::Manager;
use crate::xwayland;

static LAUNCH_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Default)]
pub(crate) struct ProcessLauncher {
    #[cfg(test)]
    test_launch_gate: Arc<Mutex<Option<Arc<TestOpenGate>>>>,
}

impl ProcessLauncher {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    pub(crate) fn block_next_launch(&self) -> Arc<TestOpenGate> {
        let gate = Arc::new(TestOpenGate::new());
        *self
            .test_launch_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::clone(&gate));
        gate
    }

    /// Launch an application asynchronously in its own transient user unit.
    pub(crate) fn launch(&self, slug: &str, argv: &[String], failure_summary: &'static str) {
        #[cfg(test)]
        {
            let gate = self
                .test_launch_gate
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            if let Some(gate) = gate {
                gate.block();
                return;
            }
        }
        let worker = self.clone();
        let fallback = self.clone();
        let slug = slug.to_owned();
        let argv = argv.to_vec();
        if let Err(error) = thread::Builder::new()
            .name(format!("cosmix-trayd-launch-{}", sanitise_component(&slug)))
            .spawn(move || {
                if let Err(error) = run_transient(&slug, &argv) {
                    worker.notify(failure_summary, &error);
                }
            })
        {
            fallback.notify(
                failure_summary,
                &format!("cannot start launch worker: {error}"),
            );
        }
    }

    pub(crate) fn control_daemon(&self, manager: Manager, verb: &str, unit: &str) {
        let worker = self.clone();
        let fallback = self.clone();
        let verb = verb.to_owned();
        let unit = unit.to_owned();
        if let Err(error) = thread::Builder::new()
            .name(format!("cosmix-trayd-{verb}"))
            .spawn(move || {
                if let Err(error) = run_control(manager, &verb, &unit) {
                    worker.notify(
                        &format!("Could not {verb} {} {unit}", manager.label()),
                        &error,
                    );
                }
            })
        {
            fallback.notify(
                "Could not start daemon control",
                &format!("cannot start worker: {error}"),
            );
        }
    }

    pub(crate) fn open_logs(&self, manager: Manager, unit: &str) {
        let argv = logs_argv(manager, unit);
        self.launch("logs", &argv, "Could not open daemon logs");
    }

    pub(crate) fn notify(&self, summary: &str, body: &str) {
        let _ = Command::new("timeout")
            .args([
                "--signal=KILL",
                "3s",
                "notify-send",
                "--app-name=CosMix Tray Daemon",
                summary,
                &concise(body, 500),
            ])
            .output();
    }
}

fn run_transient(slug: &str, argv: &[String]) -> Result<(), String> {
    let Some(program) = argv.first() else {
        return Err("no program was supplied".into());
    };
    let executable = resolve_executable(program)?;
    let unit = launch_unit(slug);
    let output = transient_command(
        &unit,
        &executable,
        &argv[1..],
        xwayland::launch_display().as_deref(),
    )
    .output()
    .map_err(|error| format!("cannot run systemd-run: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(command_error("systemd-run", &output))
    }
}

/// Build the transient-unit invocation for one application.
///
/// `display` is the XWayland `DISPLAY` published by *this* compositor, or
/// `None` when there is none — see `crate::xwayland`. The two arms are
/// deliberate and neither of them is "inherit":
///
/// * `Some` — the child is pinned to our X server with `--setenv`, so an X11
///   application launches without anyone having to know XWayland exists.
/// * `None` — `DISPLAY` is stripped from the unit's environment rather than
///   left alone. A transient unit inherits the systemd *user manager's*
///   environment, and that manager may well carry a `DISPLAY` belonging to
///   somebody else's X server (the host session's, whenever comp runs
///   nested — verified live: a bare `systemd-run --user` child inherits the
///   manager's `DISPLAY` verbatim). Leaving it in place is the worse of the
///   two failures, because it half-works: the application starts, on the
///   wrong server, in the wrong session. Failing to connect is a legible
///   fault an operator can act on; silently landing in another session's
///   display is not. systemd-run has no `--unsetenv`, so this is
///   `UnsetEnvironment=`, which the manual documents as the final step in
///   compiling the environment — it beats inheritance from every source.
fn transient_command(
    unit: &str,
    executable: &Path,
    arguments: &[String],
    display: Option<&str>,
) -> Command {
    let mut command = Command::new("systemd-run");
    command.args([
        "--user",
        "--collect",
        "--slice=app.slice",
        "--service-type=exec",
        "--quiet",
        "--unit",
        unit,
    ]);
    match display {
        Some(display) => command.arg(format!("--setenv=DISPLAY={display}")),
        None => command.arg("--property=UnsetEnvironment=DISPLAY"),
    };
    command.arg("--").arg(executable).args(arguments);
    command
}

fn resolve_executable(program: &str) -> Result<PathBuf, String> {
    if program.is_empty() {
        return Err("application Exec= has an empty executable".into());
    }
    let candidate = Path::new(program);
    if program.contains('/') {
        return executable_file(candidate)
            .then(|| candidate.to_path_buf())
            .ok_or_else(|| format!("{program} does not exist or is not executable"));
    }

    let Some(path) = env::var_os("PATH") else {
        return Err(format!("cannot find {program}: PATH is not set"));
    };
    env::split_paths(&path)
        .map(|directory| directory.join(program))
        .find(|candidate| executable_file(candidate))
        .ok_or_else(|| format!("cannot find executable {program} in PATH"))
}

fn executable_file(path: &Path) -> bool {
    match fs::metadata(path) {
        Ok(metadata) => metadata.is_file() && metadata.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

fn launch_unit(slug: &str) -> String {
    let sequence = LAUNCH_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!(
        "cosmix-launch-{}-{:x}{:x}",
        sanitise_component(slug),
        std::process::id(),
        sequence
    )
}

fn sanitise_component(input: &str) -> String {
    let component = input
        .chars()
        .take(32)
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    let component = component.trim_matches('-');
    if component.is_empty() {
        "app".into()
    } else {
        component.into()
    }
}

fn logs_argv(manager: Manager, unit: &str) -> Vec<String> {
    let mut argv = vec![
        "konsole".into(),
        "-e".into(),
        "journalctl".into(),
        "-f".into(),
    ];
    match manager {
        Manager::System => argv.push(format!("--unit={unit}")),
        Manager::User => argv.push(format!("--user-unit={unit}")),
    }
    argv
}

fn control_command(manager: Manager, verb: &str, unit: &str) -> Command {
    let mut command = Command::new("timeout");
    command.args(["--signal=KILL", "15s"]);
    command.arg("systemctl");
    if manager == Manager::User {
        command.arg("--user");
    }
    command.args([verb, unit]);
    command
}

fn run_control(manager: Manager, verb: &str, unit: &str) -> Result<(), String> {
    let output = control_command(manager, verb, unit)
        .output()
        .map_err(|error| format!("cannot run systemctl: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(command_error("systemctl", &output))
    }
}

fn command_error(label: &str, output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    if detail.is_empty() {
        format!("{label} exited with {}", output.status)
    } else {
        format!(
            "{label} exited with {}: {}",
            output.status,
            concise(detail, 500)
        )
    }
}

fn concise(message: &str, limit: usize) -> String {
    let single_line = message.split_whitespace().collect::<Vec<_>>().join(" ");
    if single_line.chars().count() <= limit {
        return single_line;
    }
    let mut shortened = single_line.chars().take(limit).collect::<String>();
    shortened.push('…');
    shortened
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::os::unix::fs::OpenOptionsExt;

    #[test]
    fn resolves_only_executable_files() {
        assert!(resolve_executable("/bin/sh").is_ok());

        let path = env::temp_dir().join(format!(
            "cosmix-trayd-not-executable-{}-{}",
            std::process::id(),
            LAUNCH_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&path)
            .expect("create fixture");
        drop(file);
        assert!(resolve_executable(path.to_str().expect("UTF-8 fixture path")).is_err());
        fs::remove_file(path).expect("remove fixture");
    }

    #[test]
    fn transient_unit_names_are_safe_and_unique() {
        let first = launch_unit("FileMgr unsafe/value");
        let second = launch_unit("FileMgr unsafe/value");
        assert!(first.starts_with("cosmix-launch-filemgr-unsafe-value-"));
        assert_ne!(first, second);
        assert!(first
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-'));
    }

    #[test]
    fn a_published_display_is_pinned_onto_the_launched_unit() {
        let rendered = format!(
            "{:?}",
            transient_command(
                "cosmix-launch-xterm-1",
                Path::new("/usr/bin/xterm"),
                &["-e".to_owned(), "top".to_owned()],
                Some(":4"),
            )
        );
        assert!(rendered.contains("\"--setenv=DISPLAY=:4\""), "{rendered}");
        assert!(!rendered.contains("UnsetEnvironment"), "{rendered}");
        // The environment argument stays on systemd-run's side of `--`, and
        // the application's own arguments survive untouched.
        let separator = rendered.find("\"--\"").expect("argument separator");
        let setenv = rendered.find("\"--setenv=").expect("setenv argument");
        assert!(setenv < separator, "{rendered}");
        assert!(
            rendered.contains("\"/usr/bin/xterm\" \"-e\" \"top\""),
            "{rendered}"
        );
    }

    #[test]
    fn no_descriptor_means_no_display_rather_than_an_inherited_one() {
        let rendered = format!(
            "{:?}",
            transient_command(
                "cosmix-launch-xterm-1",
                Path::new("/usr/bin/xterm"),
                &[],
                None,
            )
        );
        assert!(
            rendered.contains("\"--property=UnsetEnvironment=DISPLAY\""),
            "{rendered}"
        );
        assert!(!rendered.contains("--setenv"), "{rendered}");
        // Absence changes only the environment: the unit still launches.
        assert!(rendered.contains("\"/usr/bin/xterm\""), "{rendered}");
        assert!(rendered.contains("\"--slice=app.slice\""), "{rendered}");
    }

    #[test]
    fn daemon_control_routes_to_the_units_manager() {
        let system = format!(
            "{:?}",
            control_command(Manager::System, "restart", "cosmix-shared.service")
        );
        let user = format!(
            "{:?}",
            control_command(Manager::User, "restart", "cosmix-shared.service")
        );
        assert!(!system.contains("\"--user\""));
        assert!(user.contains("\"--user\""));
        assert!(system.contains("\"cosmix-shared.service\""));
        assert!(user.contains("\"cosmix-shared.service\""));
    }

    #[test]
    fn log_viewing_routes_to_the_units_manager() {
        assert_eq!(
            logs_argv(Manager::System, "cosmix-shared.service"),
            [
                "konsole",
                "-e",
                "journalctl",
                "-f",
                "--unit=cosmix-shared.service"
            ]
        );
        assert_eq!(
            logs_argv(Manager::User, "cosmix-shared.service"),
            [
                "konsole",
                "-e",
                "journalctl",
                "-f",
                "--user-unit=cosmix-shared.service"
            ]
        );
    }
}
