use super::*;

pub(super) fn manifest_removed_at_head(worktree: &Path, manifest: &Path) -> Result<bool> {
    match git_regular_blob(worktree, "HEAD", manifest) {
        Ok(content) => Ok(content.is_none()),
        Err(error) if error.downcast_ref::<LandingInfrastructure>().is_some() => Err(error),
        Err(error) => Err(task_fault(error)),
    }
}

pub(super) fn unusable_orphan_manifest_removed_at_head(
    worktree: &Path,
    base: &str,
    manifest: &Path,
    bump_minor: bool,
) -> Result<bool> {
    if !manifest_removed_at_head(worktree, manifest)? {
        return Ok(false);
    }
    if git_regular_blob(worktree, base, manifest)?.is_none() {
        return Ok(false);
    }
    // Only an already-unusable manifest with no Cargo-manifest ancestor is
    // eligible for healing. A root manifest, package child or workspace
    // member may not bypass normal base-authority checks merely because a
    // branch deletes it, and manifests the task keeps never enter this path.
    let mut ancestor = manifest.parent().and_then(Path::parent);
    while let Some(dir) = ancestor {
        if git_regular_blob(worktree, base, &dir.join("Cargo.toml"))?.is_some() {
            return Ok(false);
        }
        ancestor = dir.parent();
    }
    if manifest
        .parent()
        .is_none_or(|parent| parent.as_os_str().is_empty())
    {
        return Ok(false);
    }

    let package = match package_manifest_at_base(worktree, base, manifest) {
        Ok(package) => package,
        Err(error) if error.downcast_ref::<LandingInfrastructure>().is_some() => {
            return Err(error);
        }
        Err(_) => return Ok(true),
    };
    let workspace = match workspace_manifest_at_base(worktree, base, manifest) {
        Ok(workspace) => workspace,
        Err(error) if error.downcast_ref::<LandingInfrastructure>().is_some() => {
            return Err(error);
        }
        Err(_) => return Ok(true),
    };
    if let Some(package) = package
        && bump_semver(&package.version, bump_minor).is_err()
    {
        return Ok(true);
    }
    if let Some(version) = workspace.and_then(|workspace| workspace.version)
        && bump_semver(&version, bump_minor).is_err()
    {
        return Ok(true);
    }
    Ok(false)
}

pub(super) fn safe_git_relative(path: &str) -> Result<PathBuf> {
    let check = || -> Result<PathBuf> {
        anyhow::ensure!(
            !path.contains(':'),
            "changed path contains unsupported ':' byte"
        );
        let path = PathBuf::from(path);
        anyhow::ensure!(
            !path.as_os_str().is_empty()
                && path
                    .components()
                    .all(|part| matches!(part, Component::Normal(_))),
            "git reported a non-relative changed path: {}",
            path.display()
        );
        Ok(path)
    };
    check().map_err(task_fault)
}

pub(super) fn workspace_manifest_at_base(
    worktree: &Path,
    base: &str,
    manifest: &Path,
) -> Result<Option<WorkspaceManifest>> {
    let Some(content) = git_regular_blob(worktree, base, manifest)? else {
        return Ok(None);
    };
    let doc = content
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("parsing integration-base manifest {}", manifest.display()))?;
    if doc.get("workspace").is_none() {
        return Ok(None);
    }
    Ok(Some(WorkspaceManifest {
        manifest: manifest.to_path_buf(),
        members: workspace_members(&doc).with_context(|| {
            format!(
                "reading integration-base `[workspace].members` in {}",
                manifest.display()
            )
        })?,
        exclude: workspace_exclude(&doc).with_context(|| {
            format!(
                "reading integration-base `[workspace].exclude` in {}",
                manifest.display()
            )
        })?,
        version: workspace_version(&doc).with_context(|| {
            format!(
                "reading integration-base `[workspace.package].version` in {}",
                manifest.display()
            )
        })?,
    }))
}

pub(super) fn workspace_members(doc: &toml_edit::DocumentMut) -> Result<Option<Vec<String>>> {
    workspace_string_array(doc, "members")
}

pub(super) fn workspace_exclude(doc: &toml_edit::DocumentMut) -> Result<Option<Vec<String>>> {
    workspace_string_array(doc, "exclude")
}

pub(super) fn workspace_string_array(
    doc: &toml_edit::DocumentMut,
    field: &str,
) -> Result<Option<Vec<String>>> {
    let Some(item) = toml_value(doc, &["workspace", field]) else {
        return Ok(None);
    };
    let array = item
        .as_array()
        .with_context(|| format!("`[workspace].{field}` is not an array"))?;
    array
        .iter()
        .map(|member| {
            member
                .as_str()
                .map(str::to_string)
                .with_context(|| format!("`[workspace].{field}` contains a non-string entry"))
        })
        .collect::<Result<Vec<_>>>()
        .map(Some)
}

pub(super) fn workspace_version(doc: &toml_edit::DocumentMut) -> Result<Option<String>> {
    let Some(item) = toml_value(doc, &["workspace", "package", "version"]) else {
        return Ok(None);
    };
    item.as_str()
        .map(str::to_string)
        .context("`[workspace.package].version` is not a string")
        .map(Some)
}

pub(super) fn validate_live_workspace_manifest(
    worktree: &Path,
    expected: &WorkspaceManifest,
) -> Result<Option<String>> {
    let content = safe_read(worktree, &expected.manifest, "workspace manifest")?;
    let mut doc = content.parse::<toml_edit::DocumentMut>().with_context(|| {
        format!(
            "parsing rebased workspace manifest {}",
            expected.manifest.display()
        )
    })?;
    anyhow::ensure!(
        workspace_members(&doc)? == expected.members,
        "rebased workspace manifest {} changed `[workspace].members` from integration base",
        expected.manifest.display()
    );
    anyhow::ensure!(
        workspace_exclude(&doc)? == expected.exclude,
        "rebased workspace manifest {} changed `[workspace].exclude` from integration base",
        expected.manifest.display()
    );
    let live_version =
        toml_value(&doc, &["workspace", "package", "version"]).map(ToString::to_string);
    let version_matches = match (
        &expected.version,
        toml_value(&doc, &["workspace", "package", "version"]),
    ) {
        (Some(expected), Some(live)) => live.as_str() == Some(expected),
        (None, None) => true,
        _ => false,
    };
    if version_matches {
        return Ok(None);
    }
    if let Some(base_version) = &expected.version {
        doc["workspace"]["package"]["version"] = toml_edit::value(base_version);
    } else {
        doc["workspace"]["package"]
            .as_table_like_mut()
            .context("rebased `[workspace.package]` is not a table")?
            .remove("version");
    }
    safe_write(
        worktree,
        &expected.manifest,
        &doc.to_string(),
        "workspace manifest",
    )?;
    Ok(Some(format!(
        "Workspace {} changed `[workspace.package].version` from integration-base {} to {}. The refinery reset it to the integration-base value.",
        expected.manifest.display(),
        expected.version.as_deref().unwrap_or("<absent>"),
        live_version.as_deref().unwrap_or("<absent>")
    )))
}

pub(super) fn validate_workspace_lockfile(
    worktree: &Path,
    base: &str,
    workspace: &WorkspaceManifest,
) -> Result<()> {
    let root = workspace
        .manifest
        .parent()
        .context("workspace manifest has no parent")?;
    let lock = root.join("Cargo.lock");
    let existed_at_base = git_regular_blob(worktree, base, &lock)?.is_some();
    match fs::symlink_metadata(worktree.join(&lock)) {
        Ok(metadata) if metadata.file_type().is_file() => {
            // Validate both a base-owned lock and one newly added by the
            // branch before Cargo receives its path.
            safe_read(worktree, &lock, "workspace lockfile").map_err(task_fault)?;
        }
        Ok(_) => {
            return Err(task_fault(anyhow::anyhow!(
                "task branch supplied workspace lockfile {} as a non-regular file",
                lock.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !existed_at_base => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(task_fault(anyhow::anyhow!(
                "task branch deleted integration-base workspace lockfile {}",
                lock.display()
            )));
        }
        Err(error) => {
            return Err(filesystem_error(
                error,
                format!("inspecting workspace lockfile {}", lock.display()),
            ));
        }
    }
    Ok(())
}

pub(super) fn package_manifest_at_base(
    worktree: &Path,
    base: &str,
    manifest: &Path,
) -> Result<Option<PackageManifest>> {
    let Some(content) = git_regular_blob(worktree, base, manifest)? else {
        return Ok(None);
    };
    let doc = content
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("parsing integration-base manifest {}", manifest.display()))?;
    let Some(name) = toml_value(&doc, &["package", "name"]).and_then(toml_edit::Item::as_str)
    else {
        return Ok(None);
    };
    let workspace = toml_value(&doc, &["package", "workspace"])
        .and_then(toml_edit::Item::as_str)
        .map(|workspace| workspace_manifest_path(manifest, workspace))
        .transpose()?;
    let (version, version_source) = if let Some(version) =
        toml_value(&doc, &["package", "version"]).and_then(toml_edit::Item::as_str)
    {
        (
            version.to_string(),
            VersionSource::Package(manifest.to_path_buf()),
        )
    } else if toml_value(&doc, &["package", "version", "workspace"])
        .and_then(toml_edit::Item::as_bool)
        == Some(true)
    {
        let mut inherited = None;
        let mut candidates = Vec::new();
        if let Some(explicit) = &workspace {
            candidates.push(explicit.clone());
        } else {
            let mut dir = manifest.parent();
            while let Some(parent) = dir {
                candidates.push(parent.join("Cargo.toml"));
                if parent.as_os_str().is_empty() {
                    break;
                }
                dir = parent.parent();
            }
        }
        for candidate in candidates {
            if let Some(workspace_content) = git_regular_blob(worktree, base, &candidate)? {
                let workspace_doc = workspace_content
                    .parse::<toml_edit::DocumentMut>()
                    .with_context(|| {
                        format!("parsing integration-base workspace {}", candidate.display())
                    })?;
                if let Some(version) =
                    toml_value(&workspace_doc, &["workspace", "package", "version"])
                        .and_then(toml_edit::Item::as_str)
                {
                    inherited = Some((version.to_string(), VersionSource::Workspace(candidate)));
                    break;
                }
            }
        }
        inherited.with_context(|| {
            format!(
                "package {} inherits its version but no integration-base workspace version was found",
                manifest.display()
            )
        })?
    } else {
        anyhow::bail!(
            "integration-base package {} has no concrete or workspace-inherited version",
            manifest.display()
        );
    };
    Ok(Some(PackageManifest {
        name: name.to_string(),
        version,
        manifest: manifest.to_path_buf(),
        workspace,
        version_source,
    }))
}

pub(super) fn workspace_manifest_path(package_manifest: &Path, workspace: &str) -> Result<PathBuf> {
    let mut parts = Vec::new();
    for component in package_manifest
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .join(workspace)
        .components()
    {
        match component {
            Component::Normal(part) => parts.push(part.to_os_string()),
            Component::CurDir => {}
            Component::ParentDir => {
                anyhow::ensure!(
                    parts.pop().is_some(),
                    "[package].workspace escapes repository"
                );
            }
            Component::RootDir | Component::Prefix(_) => {
                anyhow::bail!("[package].workspace must be repository-relative")
            }
        }
    }
    let mut path = PathBuf::new();
    for part in parts {
        path.push(part);
    }
    path.push("Cargo.toml");
    Ok(path)
}

pub(super) fn toml_value<'a>(
    doc: &'a toml_edit::DocumentMut,
    keys: &[&str],
) -> Option<&'a toml_edit::Item> {
    let (first, rest) = keys.split_first()?;
    let mut item = doc.get(first)?;
    for key in rest {
        item = item.as_table_like()?.get(key)?;
    }
    Some(item)
}

pub(super) fn git_regular_blob(worktree: &Path, base: &str, path: &Path) -> Result<Option<String>> {
    let path = path
        .to_str()
        .with_context(|| format!("non-UTF-8 manifest path {}", path.display()))?;
    let (code, listing, stderr) = git_status(worktree, &["ls-tree", base, "--", path])?;
    if code != Some(0) {
        return Err(infrastructure_message(format!(
            "git ls-tree failed for {path}: {stderr}"
        )));
    }
    if listing.trim().is_empty() {
        return Ok(None);
    }
    let metadata = listing
        .split_once('\t')
        .map_or(listing.as_str(), |(meta, _)| meta);
    let mut fields = metadata.split_whitespace();
    let mode = fields.next().unwrap_or_default();
    let kind = fields.next().unwrap_or_default();
    anyhow::ensure!(
        matches!(mode, "100644" | "100755") && kind == "blob",
        "integration-base path {path} is not a regular file (mode {mode}, type {kind})"
    );
    git(worktree, &["show", &format!("{base}:{path}")]).map(Some)
}
