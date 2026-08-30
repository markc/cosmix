use super::*;

pub(super) fn validate_live_package(
    worktree: &Path,
    package: &PackageManifest,
) -> Result<Vec<String>> {
    let manifest = safe_read(worktree, &package.manifest, "package manifest")?;
    let mut doc = manifest
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("parsing rebased manifest {}", package.manifest.display()))?;
    anyhow::ensure!(
        toml_value(&doc, &["package", "name"]).and_then(toml_edit::Item::as_str)
            == Some(package.name.as_str()),
        "rebased manifest {} changed package name from integration-base {:?}",
        package.manifest.display(),
        package.name
    );
    let live_workspace = toml_value(&doc, &["package", "workspace"])
        .and_then(toml_edit::Item::as_str)
        .map(|workspace| workspace_manifest_path(&package.manifest, workspace))
        .transpose()?;
    anyhow::ensure!(
        live_workspace == package.workspace,
        "rebased manifest {} changed `[package].workspace` from {} to {}",
        package.manifest.display(),
        package
            .workspace
            .as_deref()
            .map_or_else(|| "<absent>".into(), |path| path.display().to_string()),
        live_workspace
            .as_deref()
            .map_or_else(|| "<absent>".into(), |path| path.display().to_string())
    );
    if let Some(workspace) = &package.workspace {
        safe_read(worktree, workspace, "workspace manifest")?;
        validate_live_workspace_root(worktree, package, workspace)?;
    }
    let mut discarded = Vec::new();
    match &package.version_source {
        VersionSource::Package(path) => {
            anyhow::ensure!(path == &package.manifest);
            let live_version = toml_value(&doc, &["package", "version"])
                .and_then(toml_edit::Item::as_str)
                .with_context(|| {
                    format!(
                        "rebased manifest {} removed its concrete package version",
                        package.manifest.display()
                    )
                })?;
            if live_version != package.version {
                discarded.push(format!(
                    "Package {} changed `[package].version` from integration-base {} to {}. \
                     The refinery reset it to the integration-base value before applying its own bump.",
                    package.name, package.version, live_version
                ));
                doc["package"]["version"] = toml_edit::value(&package.version);
                safe_write(
                    worktree,
                    &package.manifest,
                    &doc.to_string(),
                    "package manifest",
                )?;
            }
        }
        VersionSource::Workspace(path) => {
            anyhow::ensure!(
                toml_value(&doc, &["package", "version", "workspace"])
                    .and_then(toml_edit::Item::as_bool)
                    == Some(true),
                "rebased manifest {} replaced its integration-base workspace version inheritance",
                package.manifest.display()
            );
            let workspace = safe_read(worktree, path, "workspace manifest")?;
            let mut workspace_doc = workspace.parse::<toml_edit::DocumentMut>()?;
            validate_live_workspace_root(worktree, package, path)?;
            let live_version = toml_value(&workspace_doc, &["workspace", "package", "version"])
                .and_then(toml_edit::Item::as_str)
                .with_context(|| {
                    format!(
                        "rebased workspace manifest {} removed the inherited package version",
                        path.display()
                    )
                })?;
            if live_version != package.version {
                discarded.push(format!(
                    "Package {} changed inherited `[workspace.package].version` from \
                     integration-base {} to {}. The refinery reset it before applying its own bump.",
                    package.name, package.version, live_version
                ));
                workspace_doc["workspace"]["package"]["version"] =
                    toml_edit::value(&package.version);
                safe_write(
                    worktree,
                    path,
                    &workspace_doc.to_string(),
                    "workspace manifest",
                )?;
            }
        }
    }
    Ok(discarded)
}

pub(super) fn validate_live_workspace_root(
    worktree: &Path,
    package: &PackageManifest,
    expected_manifest: &Path,
) -> Result<()> {
    let manifest = worktree.join(&package.manifest);
    let operation = format!(
        "cargo metadata --offline while resolving workspace for {}",
        package.manifest.display()
    );
    let mut command = Command::new("cargo");
    command
        .args([
            "metadata",
            "--offline",
            "--no-deps",
            "--format-version",
            "1",
            "--manifest-path",
        ])
        .arg(&manifest)
        .current_dir(worktree);
    let output = run_bounded_cargo_child(command, &operation, CARGO_CHILD_DEADLINE)?;
    if !output.status.success() {
        return Err(cargo_child_failure(
            &operation,
            &String::from_utf8_lossy(&output.stderr),
            worktree,
        ));
    }
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout).map_err(|error| {
        LandingInfrastructure(format!(
            "cargo metadata returned malformed workspace JSON for {}: {error}",
            package.manifest.display()
        ))
    })?;
    let root = metadata["workspace_root"].as_str().ok_or_else(|| {
        LandingInfrastructure(format!(
            "cargo metadata omitted workspace_root for {}",
            package.manifest.display()
        ))
    })?;
    let expected_root = worktree.join(
        expected_manifest
            .parent()
            .context("workspace manifest has no parent")?,
    );
    anyhow::ensure!(
        Path::new(root) == expected_root,
        "rebased package {} redirected its workspace from {} to {}",
        package.manifest.display(),
        expected_manifest.display(),
        Path::new(root).join("Cargo.toml").display()
    );
    let expected_package = worktree.join(&package.manifest);
    anyhow::ensure!(
        metadata["packages"].as_array().is_some_and(|packages| {
            packages.iter().any(|candidate| {
                candidate["manifest_path"].as_str() == expected_package.to_str()
                    && candidate["name"].as_str() == Some(package.name.as_str())
            })
        }),
        "rebased workspace membership no longer contains package {} at {}",
        package.name,
        package.manifest.display()
    );
    Ok(())
}

pub(super) fn bump_semver(version: &str, minor: bool) -> Result<String> {
    // A bump creates a new release identity. Pre-release identifiers and build
    // metadata describe the old identity, so both are deliberately dropped.
    let core = version.split(['-', '+']).next().unwrap_or(version);
    let mut parts = core.split('.');
    let major: u64 = parts.next().context("missing semver major")?.parse()?;
    let old_minor: u64 = parts.next().context("missing semver minor")?.parse()?;
    let patch: u64 = parts.next().context("missing semver patch")?.parse()?;
    anyhow::ensure!(
        parts.next().is_none(),
        "version {version:?} is not three-part semver"
    );
    Ok(if minor {
        let next_minor = old_minor.checked_add(1).ok_or_else(|| {
            task_fault(anyhow::anyhow!(
                "cannot bump version {version:?}: minor component overflow"
            ))
        })?;
        format!("{major}.{next_minor}.0")
    } else {
        let next_patch = patch.checked_add(1).ok_or_else(|| {
            task_fault(anyhow::anyhow!(
                "cannot bump version {version:?}: patch component overflow"
            ))
        })?;
        format!("{major}.{old_minor}.{next_patch}")
    })
}
