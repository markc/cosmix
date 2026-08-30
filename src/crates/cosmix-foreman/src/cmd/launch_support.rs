use super::*;

/// Sweep `policy-settings-*-<pid>.json` (hook settings, mayor configs) left
/// behind by crashed/killed processes — signals skip Drop guards. Stale =
/// the trailing pid no longer exists. (A recycled pid keeps a file alive
/// until the next sweep after that process exits — the O_EXCL create then
/// fails loudly rather than silently reusing it.)
pub(super) fn sweep_stale_configs(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(stem) = name
            .strip_prefix("policy-settings-")
            .and_then(|s| s.strip_suffix(".json"))
            && let Some(pid) = stem.rsplit('-').next().and_then(|s| s.parse::<i32>().ok())
            && pid > 0
            && unsafe { libc::kill(pid, 0) } != 0
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}
/// O_EXCL creation: refuses a pre-existing file OR symlink — a planted
/// symlink at a predictable config path must not become an arbitrary-path
/// write.
pub(super) fn write_new(path: &std::path::Path, contents: &str) -> Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("creating {} (pre-existing file?)", path.display()))?;
    f.write_all(contents.as_bytes())
        .with_context(|| format!("writing {}", path.display()))
}

/// Removed on every exit path, loudly if removal fails.
pub(super) struct RemoveOnDrop(pub(super) PathBuf);
impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.0)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!("foreman: could not remove {}: {e}", self.0.display());
        }
    }
}
