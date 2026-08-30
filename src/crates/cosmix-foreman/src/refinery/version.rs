use super::*;

#[derive(Debug)]
pub(super) struct PackageManifest {
    pub(super) name: String,
    pub(super) version: String,
    pub(super) manifest: PathBuf,
    pub(super) workspace: Option<PathBuf>,
    pub(super) version_source: VersionSource,
}

#[derive(Debug)]
pub(super) struct WorkspaceManifest {
    pub(super) manifest: PathBuf,
    pub(super) members: Option<Vec<String>>,
    pub(super) exclude: Option<Vec<String>>,
    pub(super) version: Option<String>,
}

#[derive(Debug, Default)]
pub(super) struct VersionCommitOutcome {
    pub(super) bumped: Vec<String>,
    pub(super) discarded: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum VersionSource {
    Package(PathBuf),
    Workspace(PathBuf),
}

pub(super) fn create_landing_version_commit(
    worktree: &Path,
    base: &str,
    task: &Task,
) -> Result<VersionCommitOutcome> {
    // The healing exception below probes whether a removed base manifest
    // would still fail today's bump — it must probe with the SAME bump
    // kind this landing would actually apply, or a task with an effective
    // MINOR bump can wrongly fail an eligibility check sized for PATCH.
    let bump_minor = task.effective_version_bump()? == crate::ledger::VersionBump::Minor;
    let changed = git(worktree, &["diff", "--name-only", "-z", base, "HEAD"])?;
    let mut changed_paths = Vec::new();
    let mut packages = BTreeMap::new();
    for relative in changed.split('\0').filter(|path| !path.is_empty()) {
        let relative = safe_git_relative(relative)?;
        changed_paths.push(relative.clone());
        let mut parent = relative.parent();
        while let Some(dir) = parent {
            let candidate = dir.join("Cargo.toml");
            if candidate == relative
                && unusable_orphan_manifest_removed_at_head(worktree, base, &candidate, bump_minor)?
            {
                // The changed path is a manifest removed by this task. Skip
                // its base bytes so a task can remove a previously-landed,
                // poisoned orphan manifest and heal the integration tree.
                parent = dir.parent();
                continue;
            }
            if let Some(package) = package_manifest_at_base(worktree, base, &candidate)? {
                packages.insert(candidate, package);
                break;
            }
            if dir.as_os_str().is_empty() {
                break;
            }
            parent = dir.parent();
        }
    }

    let mut workspaces = BTreeMap::new();
    for relative in &changed_paths {
        if !matches!(
            relative.file_name().and_then(|name| name.to_str()),
            Some("Cargo.toml" | "Cargo.lock")
        ) {
            continue;
        }
        let root = relative.parent().unwrap_or_else(|| Path::new(""));
        let manifest = root.join("Cargo.toml");
        if &manifest == relative
            && unusable_orphan_manifest_removed_at_head(worktree, base, &manifest, bump_minor)?
        {
            // A task may heal a poisoned orphan manifest already present in
            // the integration base. Do not parse the bytes it is removing.
            continue;
        }
        if let Some(workspace) = workspace_manifest_at_base(worktree, base, &manifest)? {
            workspaces.insert(manifest, workspace);
        }
    }
    if !task.crates.is_empty() {
        let tree = git(worktree, &["ls-tree", "-r", "--name-only", base])?;
        let wanted: std::collections::BTreeSet<_> = task.crates.iter().cloned().collect();
        let mut found = std::collections::BTreeSet::new();
        for relative in tree.lines().filter(|path| path.ends_with("Cargo.toml")) {
            let relative = safe_git_relative(relative)?;
            if changed_paths.contains(&relative)
                && unusable_orphan_manifest_removed_at_head(worktree, base, &relative, bump_minor)?
            {
                // The operator-designated crate walk normally parses every
                // base manifest. Preserve the same healing exception here:
                // unrelated designations must not force a removal task to
                // parse the poisoned manifest it deletes.
                continue;
            }
            if let Some(package) = package_manifest_at_base(worktree, base, &relative)?
                && wanted.contains(&package.name)
            {
                found.insert(package.name.clone());
                packages.insert(relative, package);
            }
        }
        let missing: Vec<_> = wanted.difference(&found).cloned().collect();
        if !missing.is_empty() {
            return Err(task_fault(anyhow::anyhow!(
                "task --crate designations have no integration-base package manifest: {}",
                missing.join(", ")
            )));
        }
    }
    let mut discarded = Vec::new();
    let mut add_paths = Vec::new();
    for workspace in workspaces.values() {
        match validate_live_workspace_manifest(worktree, workspace) {
            Ok(Some(workspace_discarded)) => {
                discarded.push(workspace_discarded);
                add_paths.push(worktree.join(&workspace.manifest));
            }
            Ok(None) => {}
            Err(error) if error.downcast_ref::<LandingInfrastructure>().is_some() => {
                return Err(error);
            }
            Err(error) => return Err(task_fault(error)),
        }
        validate_workspace_lockfile(worktree, base, workspace)?;
    }

    for package in packages.values() {
        match validate_live_package(worktree, package) {
            Ok(package_discarded) => discarded.extend(package_discarded),
            Err(error) if error.downcast_ref::<LandingInfrastructure>().is_some() => {
                return Err(error);
            }
            Err(error) => return Err(task_fault(error)),
        }
    }

    let mut rewrites = BTreeMap::new();
    let mut bumped = Vec::new();
    for package in packages.values() {
        let next = bump_semver(&package.version, bump_minor).with_context(|| {
            format!(
                "bumping package {} from integration-base manifest {}",
                package.name,
                package.manifest.display()
            )
        })?;
        match rewrites.entry(package.version_source.clone()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert((package.version.clone(), next.clone()));
            }
            std::collections::btree_map::Entry::Occupied(entry) => {
                anyhow::ensure!(
                    entry.get() == &(package.version.clone(), next.clone()),
                    "packages sharing one version source disagree on the authoritative version"
                );
            }
        }
        bumped.push(format!("{} {} -> {}", package.name, package.version, next));
    }
    for (source, (old, next)) in &rewrites {
        let (path, workspace) = match source {
            VersionSource::Package(path) => (path, false),
            VersionSource::Workspace(path) => (path, true),
        };
        rewrite_package_version(worktree, path, old, next, workspace)?;
        add_paths.push(worktree.join(path));
    }
    for package in packages.values() {
        let next = bump_semver(&package.version, bump_minor).with_context(|| {
            format!(
                "bumping package {} from integration-base manifest {}",
                package.name,
                package.manifest.display()
            )
        })?;
        let path = worktree.join(&package.manifest);
        if let Some(lock) =
            nearest_lockfile_at_base(worktree, base, path.parent().unwrap_or(worktree))?
        {
            let operation = format!("cargo update --offline for {} -> {}", package.name, next);
            let mut command = Command::new("cargo");
            command
                .args(["update", "--offline", "--manifest-path"])
                .arg(&path)
                .args(["-p", &package.name, "--precise", &next])
                .current_dir(worktree);
            let output = run_bounded_cargo_child(command, &operation, CARGO_CHILD_DEADLINE)?;
            if !output.status.success() {
                return Err(cargo_child_failure(
                    &operation,
                    &String::from_utf8_lossy(&output.stderr),
                    worktree,
                ));
            }
            add_paths.push(lock);
        }
    }
    if packages.is_empty() && add_paths.is_empty() {
        return Ok(VersionCommitOutcome { bumped, discarded });
    }
    let mut command = Command::new("git");
    command.arg("add").arg("--");
    for path in &add_paths {
        command.arg(path);
    }
    let output = command.current_dir(worktree).output().map_err(|error| {
        infrastructure_message(format!(
            "spawning git add for refinery version bump: {error}"
        ))
    })?;
    if !output.status.success() {
        return Err(infrastructure_message(format!(
            "staging refinery version bump failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let message = format!("refinery: version packages for task {}", task.id);
    let output = Command::new("git")
        .args(["commit", "--allow-empty", "-m", &message])
        .current_dir(worktree)
        .output()
        .map_err(|error| {
            infrastructure_message(format!("spawning git commit for refinery landing: {error}"))
        })?;
    if !output.status.success() {
        return Err(infrastructure_message(format!(
            "creating refinery landing commit failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(VersionCommitOutcome { bumped, discarded })
}
