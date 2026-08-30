use super::*;

/// The shared repo must be on the integration branch with a clean tree —
/// anything else is an operator problem to resolve, not a task bounce.
pub(super) fn preflight(
    opts: &RefineOptions,
    fleet_policy: &crate::config::FleetPolicy,
) -> Result<()> {
    let current = git(&opts.repo, &["branch", "--show-current"])?;
    anyhow::ensure!(
        current.trim() == opts.integration,
        "repo {} is on {:?}, not the integration branch {:?} — refusing to refine",
        opts.repo.display(),
        current.trim(),
        opts.integration
    );
    let dirty = git(&opts.repo, &["status", "--porcelain"])?;
    anyhow::ensure!(
        dirty.trim().is_empty(),
        "repo {} has uncommitted changes — refusing to refine over them:\n{}",
        opts.repo.display(),
        dirty.trim()
    );
    // Ambient fleet sibling clones belong to the legacy cmctl/cos workspace.
    // A project manifest owns only its named repository and state root; it
    // must never fast-forward unrelated clones inherited from the operator's
    // environment.
    if opts.project_root.is_none() {
        refresh_sibling_repos(fleet_policy)?;
    }
    Ok(())
}

/// Refresh the fleet-home clones that relative workspace path dependencies
/// resolve through. These are infrastructure shared by every queued task, so
/// a failure stops the queue before any task enters `landing`.
pub(super) fn refresh_sibling_repos(fleet_policy: &crate::config::FleetPolicy) -> Result<()> {
    let Some(spec) = &fleet_policy.sibling_repos.value else {
        return Ok(());
    };
    for repo in std::env::split_paths(spec) {
        anyhow::ensure!(
            repo.is_absolute(),
            "INFRA: {SIBLING_REPOS_ENV} entry {:?} is not an absolute path — \
             refusing to process the refinery queue",
            repo
        );
        anyhow::ensure!(
            repo.is_dir(),
            "INFRA: sibling repo {} is not a directory — refusing to process \
             the refinery queue",
            repo.display()
        );

        let (code, stdout, stderr) = bounded_git_status(&repo, &["fetch", "origin"])?;
        if code != Some(0) {
            anyhow::bail!(
                "INFRA: sibling repo {} could not fetch origin{} — refusing to \
                 process the refinery queue: {}",
                repo.display(),
                sibling_timeout_note(code),
                git_failure_detail(&stdout, &stderr)
            );
        }

        let (code, stdout, stderr) = bounded_git_status(&repo, &["merge", "--ff-only", "@{u}"])?;
        if code == Some(0) {
            continue;
        }

        // `merge --ff-only` can also fail for a dirty checkout or damaged
        // repository. Ask Git whether HEAD is an ancestor so the operator
        // gets the accurate diverged-clone diagnosis when that is the cause.
        let diverged = matches!(
            bounded_git_status(&repo, &["merge-base", "--is-ancestor", "HEAD", "@{u}"]),
            Ok((Some(1), _, _))
        );
        let reason = if diverged {
            "has diverged from its upstream"
        } else {
            "could not fast-forward to its upstream"
        };
        anyhow::bail!(
            "INFRA: sibling repo {} {reason}{} — refusing to process the \
             refinery queue: git merge --ff-only @{{u}}: {}",
            repo.display(),
            sibling_timeout_note(code),
            git_failure_detail(&stdout, &stderr)
        );
    }
    Ok(())
}

pub(super) fn sibling_timeout_note(code: Option<i32>) -> &'static str {
    if matches!(code, Some(124 | 137)) {
        " (bounded git call timed out)"
    } else {
        ""
    }
}

pub(super) fn git_failure_detail(stdout: &str, stderr: &str) -> String {
    let stderr = stderr.trim();
    if stderr.is_empty() {
        stdout.trim().to_string()
    } else {
        stderr.to_string()
    }
}

/// Git runner for remote-capable refinery I/O. Both the cooperative TERM
/// deadline and the hard KILL deadline are explicit so a wedged remote cannot
/// hold the single refinery lane forever.
pub(super) fn bounded_git_status(
    repo: &Path,
    args: &[&str],
) -> Result<(Option<i32>, String, String)> {
    let output = Command::new("timeout")
        .args([
            "-k",
            &SIBLING_GIT_KILL_AFTER_SECS.to_string(),
            &SIBLING_GIT_TIMEOUT_SECS.to_string(),
            "git",
        ])
        .args(args)
        .current_dir(repo)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|error| {
            infrastructure_message(format!(
                "spawning bounded git {args:?} in {}: {error}",
                repo.display()
            ))
        })?;
    Ok((
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    ))
}
