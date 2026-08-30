use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use cosmix_foreman::remote_git::{
    RemoteGitRunner, RemoteGitTermination, RemoteOutcome, classify_remote_delivery,
};

struct GitFixture {
    _temp: tempfile::TempDir,
    source: PathBuf,
    remote: PathBuf,
}

impl GitFixture {
    fn new() -> Self {
        let temp = tempfile::TempDir::new().unwrap();
        let source = temp.path().join("source");
        let remote = temp.path().join("remote.git");
        fs::create_dir(&source).unwrap();
        git(temp.path(), ["init", "--bare", remote.to_str().unwrap()]);
        git(&source, ["init", "-b", "main"]);
        git(&source, ["config", "user.name", "Foreman Test"]);
        git(&source, ["config", "user.email", "foreman@example.test"]);
        fs::write(source.join("payload"), "first\n").unwrap();
        git(&source, ["add", "payload"]);
        git(&source, ["commit", "-m", "fixture"]);
        Self {
            _temp: temp,
            source,
            remote,
        }
    }

    fn hook(&self, body: &str) {
        let path = self.remote.join("hooks/pre-receive");
        fs::write(&path, format!("#!/bin/sh\nset -eu\n{body}\n")).unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }

    fn remote_arg(&self) -> &OsStr {
        self.remote.as_os_str()
    }
}

#[test]
fn bare_remote_clean_success_is_succeeded() {
    let fixture = GitFixture::new();
    fixture.hook(
        "test \"${HOME+x}\" != x\n\
         test \"${SSH_AUTH_SOCK+x}\" != x\n\
         test \"$GIT_TERMINAL_PROMPT\" = 0\n\
         test \"$GIT_ASKPASS\" = /bin/false\n\
         test \"$SSH_ASKPASS\" = /bin/false\n\
         test \"$GIT_PAGER\" = cat\n\
         test \"$PAGER\" = cat",
    );
    let runner = RemoteGitRunner::new(Duration::from_secs(5), 16 * 1024).unwrap();
    let run = runner.run(
        &fixture.source,
        [
            OsStr::new("push"),
            OsStr::new("--porcelain"),
            OsStr::new("--"),
            fixture.remote_arg(),
            OsStr::new("HEAD:refs/heads/main"),
        ],
    );

    assert_eq!(run.outcome, RemoteOutcome::Succeeded, "{run:?}");
    assert_eq!(run.termination, RemoteGitTermination::Exited(0));
    assert_eq!(
        git_output(&fixture.remote, ["rev-parse", "refs/heads/main"])
            .stdout
            .split(|byte| *byte == b'\n')
            .next()
            .unwrap()
            .len(),
        40
    );
}

#[test]
fn only_explicit_manifest_credentials_enter_the_cleared_child() {
    let fixture = GitFixture::new();
    fixture.hook(
        "test \"${HOME+x}\" != x\n\
         test \"${UNLISTED_SECRET+x}\" != x\n\
         test \"$PUBLISH_TOKEN\" = selected-token",
    );
    let runner = RemoteGitRunner::new(Duration::from_secs(5), 16 * 1024).unwrap();
    let credentials = vec![(
        "PUBLISH_TOKEN".to_string(),
        std::ffi::OsString::from("selected-token"),
    )];
    let run = runner.run_with_credentials(
        &fixture.source,
        [
            OsStr::new("push"),
            OsStr::new("--porcelain"),
            OsStr::new("--"),
            fixture.remote_arg(),
            OsStr::new("HEAD:refs/heads/main"),
        ],
        &credentials,
    );

    assert_eq!(run.outcome, RemoteOutcome::Succeeded, "{run:?}");
}

#[test]
fn bare_remote_hook_rejection_is_provably_failed_and_output_is_capped() {
    let fixture = GitFixture::new();
    fixture.hook("i=0\nwhile [ \"$i\" -lt 4096 ]; do printf x >&2; i=$((i + 1)); done\nexit 1");
    let runner = RemoteGitRunner::new(Duration::from_secs(5), 512).unwrap();
    let run = runner.run(
        &fixture.source,
        [
            OsStr::new("push"),
            OsStr::new("--porcelain"),
            OsStr::new("--"),
            fixture.remote_arg(),
            OsStr::new("HEAD:refs/heads/main"),
        ],
    );

    assert_eq!(run.outcome, RemoteOutcome::Failed, "{run:?}");
    assert_eq!(run.termination, RemoteGitTermination::Exited(1));
    assert!(run.stderr_truncated, "hook noise should hit the fixed cap");
    assert_eq!(run.stderr.len(), 512);
    assert!(
        !git_output(&fixture.remote, ["show-ref", "--verify", "refs/heads/main"])
            .status
            .success()
    );
}

#[test]
fn timeout_is_unknown_and_kills_the_remote_process_tree() {
    let fixture = GitFixture::new();
    let pid_file = fixture._temp.path().join("hook-pids");
    fixture.hook(&format!(
        "sleep 30 &\nchild=$!\nprintf '%s\\n%s\\n' \"$$\" \"$child\" > '{}'\nwait \"$child\"",
        pid_file.display()
    ));
    let runner = RemoteGitRunner::new(Duration::from_millis(250), 16 * 1024).unwrap();
    let started = Instant::now();
    let run = runner.run(
        &fixture.source,
        [
            OsStr::new("push"),
            OsStr::new("--porcelain"),
            OsStr::new("--"),
            fixture.remote_arg(),
            OsStr::new("HEAD:refs/heads/main"),
        ],
    );

    assert_eq!(run.outcome, RemoteOutcome::Unknown, "{run:?}");
    assert_eq!(run.termination, RemoteGitTermination::TimedOut);
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "the explicit deadline must bound the lane"
    );
    let pids: Vec<i32> = fs::read_to_string(&pid_file)
        .expect("the hook started before the timeout")
        .lines()
        .map(|line| line.parse().unwrap())
        .collect();
    assert_eq!(pids.len(), 2);
    let deadline = Instant::now() + Duration::from_secs(2);
    for pid in pids {
        while process_exists(pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!process_exists(pid), "timed-out descendant {pid} survived");
    }
}

#[test]
fn simulated_ambiguous_nonzero_and_kill_are_unknown_not_failed() {
    for termination in [
        RemoteGitTermination::Exited(1),
        RemoteGitTermination::Exited(128),
        RemoteGitTermination::Signalled(Some(libc::SIGKILL)),
        RemoteGitTermination::WaitFailed,
    ] {
        assert_eq!(
            classify_remote_delivery(&termination, b"transport ended without a status", true),
            RemoteOutcome::Unknown,
            "{termination:?}"
        );
    }
}

#[test]
fn simulated_spawn_failure_is_provably_failed() {
    assert_eq!(
        classify_remote_delivery(&RemoteGitTermination::SpawnFailed, b"", false),
        RemoteOutcome::Failed
    );
}

fn git<const N: usize>(cwd: &Path, args: [&str; N]) {
    let output = git_output(cwd, args);
    assert!(
        output.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_output<const N: usize>(cwd: &Path, args: [&str; N]) -> Output {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "/bin/false")
        .env("GIT_PAGER", "cat")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn process_exists(pid: i32) -> bool {
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}
