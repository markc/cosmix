use super::*;

pub(super) fn rewrite_package_version(
    worktree: &Path,
    relative: &Path,
    old: &str,
    new: &str,
    workspace: bool,
) -> Result<()> {
    let content = safe_read(worktree, relative, "version manifest")?;
    let mut doc = content.parse::<toml_edit::DocumentMut>()?;
    let item = if workspace {
        &mut doc["workspace"]["package"]["version"]
    } else {
        &mut doc["package"]["version"]
    };
    anyhow::ensure!(
        item.as_str() == Some(old),
        "authoritative version {old:?} not found in {}",
        relative.display()
    );
    *item = toml_edit::value(new);
    safe_write(worktree, relative, &doc.to_string(), "version manifest")?;
    Ok(())
}

pub(super) fn nearest_lockfile_at_base(
    root: &Path,
    base: &str,
    start: &Path,
) -> Result<Option<PathBuf>> {
    let mut dir = Some(start);
    while let Some(current) = dir {
        let lock = current.join("Cargo.lock");
        let relative = lock
            .strip_prefix(root)
            .with_context(|| format!("lockfile escaped landing worktree: {}", lock.display()))?;
        if git_regular_blob(root, base, relative)?.is_some() {
            match fs::symlink_metadata(&lock) {
                Ok(metadata) if metadata.file_type().is_file() => {
                    safe_read(root, relative, "lockfile").map_err(task_fault)?;
                    return Ok(Some(lock));
                }
                Ok(_) => {
                    return Err(task_fault(anyhow::anyhow!(
                        "task branch replaced integration-base lockfile {} with a non-regular file",
                        relative.display()
                    )));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Err(task_fault(anyhow::anyhow!(
                        "task branch deleted integration-base lockfile {}",
                        relative.display()
                    )));
                }
                Err(error) => {
                    return Err(filesystem_error(
                        error,
                        format!("inspecting lockfile {}", lock.display()),
                    ));
                }
            }
        }
        if current == root {
            break;
        }
        dir = current.parent();
    }
    Ok(None)
}

pub(super) fn safe_read(root: &Path, relative: &Path, label: &str) -> Result<String> {
    let path = safe_regular_path(root, relative, label)?;
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .map_err(|error| {
            filesystem_error(
                error,
                format!(
                    "opening {label} {} without following symlinks",
                    path.display()
                ),
            )
        })?;
    #[cfg(test)]
    FAIL_NEXT_LOCKFILE_READ.with(|fail| -> Result<()> {
        if label.contains("lockfile") && fail.replace(false) {
            return Err(infrastructure_message(format!(
                "reading {label} {}: injected EIO",
                path.display()
            )));
        }
        Ok(())
    })?;
    let mut content = String::new();
    file.read_to_string(&mut content)
        .map_err(|error| filesystem_error(error, format!("reading {label} {}", path.display())))?;
    Ok(content)
}

pub(super) fn safe_write(root: &Path, relative: &Path, content: &str, label: &str) -> Result<()> {
    let path = safe_regular_path(root, relative, label)?;
    #[cfg(test)]
    FAIL_NEXT_WORKSPACE_WRITE.with(|fail| -> Result<()> {
        if label == "workspace manifest" && fail.replace(false) {
            return Err(infrastructure_message(format!(
                "writing workspace manifest {}: injected EIO",
                path.display()
            )));
        }
        Ok(())
    })?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .map_err(|error| {
            filesystem_error(
                error,
                format!(
                    "opening {label} {} without following symlinks",
                    path.display()
                ),
            )
        })?;
    file.write_all(content.as_bytes())
        .map_err(|error| filesystem_error(error, format!("writing {label} {}", path.display())))?;
    file.flush()
        .map_err(|error| filesystem_error(error, format!("flushing {label} {}", path.display())))?;
    Ok(())
}

pub(super) fn filesystem_error(error: std::io::Error, operation: String) -> anyhow::Error {
    match error.kind() {
        // These can be produced directly by task-controlled path/content
        // shape. Leave them unannotated so the landing-boundary default is a
        // task bounce.
        std::io::ErrorKind::NotFound
        | std::io::ErrorKind::NotADirectory
        | std::io::ErrorKind::InvalidData
        | std::io::ErrorKind::InvalidInput => anyhow::anyhow!("{operation}: {error}"),
        // Host-side I/O (including ENOSPC/EIO/EROFS/permissions) is explicit
        // infrastructure and receives infrastructure backoff.
        _ => infrastructure_message(format!("{operation}: {error}")),
    }
}

pub(super) fn verifier_directory_error_is_infrastructure(error: &anyhow::Error) -> bool {
    error.downcast_ref::<std::io::Error>().is_some_and(|error| {
        !matches!(
            error.kind(),
            // A branch can delete the selected directory, replace an
            // ancestor with a file, or supply an invalid path. Those are
            // verifier-red content faults. Other canonicalisation errors,
            // including EIO, permissions and filesystem exhaustion, are
            // host infrastructure.
            std::io::ErrorKind::NotFound
                | std::io::ErrorKind::NotADirectory
                | std::io::ErrorKind::InvalidData
                | std::io::ErrorKind::InvalidInput
        )
    })
}

pub(super) fn safe_regular_path(root: &Path, relative: &Path, label: &str) -> Result<PathBuf> {
    anyhow::ensure!(
        relative
            .components()
            .all(|part| matches!(part, Component::Normal(_))),
        "{label} is not a contained relative path: {}",
        relative.display()
    );
    let root = root.canonicalize().map_err(|error| {
        infrastructure_message(format!(
            "canonicalizing landing worktree {}: {error}",
            root.display()
        ))
    })?;
    let path = root.join(relative);
    let metadata = fs::symlink_metadata(&path).map_err(|error| {
        filesystem_error(error, format!("inspecting {label} {}", path.display()))
    })?;
    anyhow::ensure!(
        metadata.file_type().is_file(),
        "{label} is not a regular file: {}",
        path.display()
    );
    let parent = path.parent().context("contained file has no parent")?;
    anyhow::ensure!(
        parent.canonicalize().map_err(|error| filesystem_error(
            error,
            format!("canonicalizing {label} parent {}", parent.display())
        ))? == parent,
        "{label} reaches a symlinked directory: {}",
        path.display()
    );
    Ok(path)
}
