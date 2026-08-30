//! Collision-safe local filesystem operations for FileMgr.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileOpKind {
    Copy,
    Move,
    Delete,
    NewFolder,
    Rename,
    BatchCopy,
    BatchMove,
}

#[derive(Clone, Debug)]
pub struct FileOperation {
    pub kind: FileOpKind,
    pub source: PathBuf,
    pub destination_dir: Option<PathBuf>,
    batch_sources: Vec<PathBuf>,
}

impl FileOperation {
    pub fn copy(source: PathBuf, destination_dir: PathBuf) -> Self {
        Self {
            kind: FileOpKind::Copy,
            source,
            destination_dir: Some(destination_dir),
            batch_sources: Vec::new(),
        }
    }

    pub fn move_to(source: PathBuf, destination_dir: PathBuf) -> Self {
        Self {
            kind: FileOpKind::Move,
            source,
            destination_dir: Some(destination_dir),
            batch_sources: Vec::new(),
        }
    }

    pub fn delete(source: PathBuf) -> Self {
        Self {
            kind: FileOpKind::Delete,
            source,
            destination_dir: None,
            batch_sources: Vec::new(),
        }
    }

    pub fn new_folder(path: PathBuf) -> Self {
        Self {
            kind: FileOpKind::NewFolder,
            source: path,
            destination_dir: None,
            batch_sources: Vec::new(),
        }
    }

    pub fn rename(source: PathBuf, target: PathBuf) -> Self {
        Self {
            kind: FileOpKind::Rename,
            source,
            destination_dir: Some(target),
            batch_sources: Vec::new(),
        }
    }

    pub fn copy_batch(sources: Vec<PathBuf>, destination_dir: PathBuf) -> Result<Self, String> {
        Self::batch(FileOpKind::BatchCopy, sources, destination_dir)
    }

    pub fn move_batch(sources: Vec<PathBuf>, destination_dir: PathBuf) -> Result<Self, String> {
        Self::batch(FileOpKind::BatchMove, sources, destination_dir)
    }

    fn batch(
        kind: FileOpKind,
        sources: Vec<PathBuf>,
        destination_dir: PathBuf,
    ) -> Result<Self, String> {
        let source = sources
            .first()
            .cloned()
            .ok_or_else(|| "a batch transfer needs at least one source".to_string())?;
        Ok(Self {
            kind,
            source,
            destination_dir: Some(destination_dir),
            batch_sources: sources,
        })
    }

    pub fn execute(&self) -> Result<String, String> {
        match self.kind {
            FileOpKind::Copy => {
                let destination = self.destination()?;
                copy_entry(&self.source, destination)?;
                Ok(format!(
                    "Copied {} to {}",
                    self.source.display(),
                    destination.display()
                ))
            }
            FileOpKind::Move => {
                let destination = self.destination()?;
                move_entry(&self.source, destination)?;
                Ok(format!(
                    "Moved {} to {}",
                    self.source.display(),
                    destination.display()
                ))
            }
            FileOpKind::Delete => {
                remove_entry(&self.source)?;
                Ok(format!("Deleted {}", self.source.display()))
            }
            FileOpKind::NewFolder => {
                fs::create_dir(&self.source)
                    .map_err(|error| format!("creating {}: {error}", self.source.display()))?;
                Ok(format!("Created {}", self.source.display()))
            }
            FileOpKind::Rename => {
                let target = self
                    .destination_dir
                    .as_deref()
                    .ok_or_else(|| "rename operation has no target".to_string())?;
                if target.exists() || fs::symlink_metadata(target).is_ok() {
                    return Err(format!("{} already exists", target.display()));
                }
                fs::rename(&self.source, target).map_err(|error| {
                    format!(
                        "renaming {} to {}: {error}",
                        self.source.display(),
                        target.display()
                    )
                })?;
                Ok(format!(
                    "Renamed {} to {}",
                    self.source.display(),
                    target.display()
                ))
            }
            FileOpKind::BatchCopy | FileOpKind::BatchMove => {
                let destination = self.destination()?;
                execute_batch(self.kind, &self.batch_sources, destination)
            }
        }
    }

    fn destination(&self) -> Result<&Path, String> {
        let destination = self
            .destination_dir
            .as_deref()
            .ok_or_else(|| "operation has no destination directory".to_string())?;
        if !destination.is_dir() {
            return Err(format!("{} is not a directory", destination.display()));
        }
        Ok(destination)
    }
}

#[derive(Debug)]
struct PreparedTransfer<'a> {
    source: &'a Path,
    target: PathBuf,
}

/// Preflight the complete batch before the first filesystem mutation.
///
/// Races and I/O failures can still occur after preflight. Execution is
/// ordered and stops on the first such failure, reporting the completed item,
/// failed item, and unattempted count instead of hiding a partial result.
fn execute_batch(
    kind: FileOpKind,
    sources: &[PathBuf],
    destination_dir: &Path,
) -> Result<String, String> {
    let prepared = preflight_batch(kind, sources, destination_dir)?;
    let total = prepared.len();
    for (index, item) in prepared.iter().enumerate() {
        let result = match kind {
            FileOpKind::BatchCopy => copy_entry(item.source, destination_dir).map(|_| ()),
            FileOpKind::BatchMove => move_entry(item.source, destination_dir).map(|_| ()),
            _ => unreachable!("only batch operations reach execute_batch"),
        };
        if let Err(error) = result {
            let completed = index;
            let remaining = total.saturating_sub(index + 1);
            return Err(format!(
                "{} batch partially failed after {completed}/{total} completed: {} -> {}: \
                 {error}; {remaining} item(s) not attempted",
                batch_verb(kind),
                item.source.display(),
                item.target.display(),
            ));
        }
    }
    Ok(format!(
        "{} {total} item(s) to {}",
        match kind {
            FileOpKind::BatchCopy => "Copied",
            FileOpKind::BatchMove => "Moved",
            _ => unreachable!("only batch operations reach execute_batch"),
        },
        destination_dir.display()
    ))
}

fn preflight_batch<'a>(
    kind: FileOpKind,
    sources: &'a [PathBuf],
    destination_dir: &Path,
) -> Result<Vec<PreparedTransfer<'a>>, String> {
    if sources.is_empty() {
        return Err("a batch transfer needs at least one source".into());
    }
    if !destination_dir.is_dir() {
        return Err(format!("{} is not a directory", destination_dir.display()));
    }
    let destination_root = destination_dir
        .canonicalize()
        .map_err(|error| format!("resolving {}: {error}", destination_dir.display()))?;
    let mut targets = HashSet::new();
    let mut source_roots = Vec::with_capacity(sources.len());
    let mut prepared = Vec::with_capacity(sources.len());

    for source in sources {
        let metadata = fs::symlink_metadata(source)
            .map_err(|error| format!("preflight reading {}: {error}", source.display()))?;
        let target = target_path(source, destination_dir)?;
        if target.exists() || fs::symlink_metadata(&target).is_ok() {
            return Err(format!(
                "batch preflight found an existing target: {}",
                target.display()
            ));
        }
        if !targets.insert(target.clone()) {
            return Err(format!(
                "batch preflight found duplicate target name: {}",
                target.display()
            ));
        }
        let parent = source
            .parent()
            .and_then(|parent| parent.canonicalize().ok())
            .ok_or_else(|| format!("preflight cannot resolve parent of {}", source.display()))?;
        // Resolve the entry lexically against its canonical parent rather than
        // canonicalising the entry itself. Preflight must never be stricter
        // than execution, and `canonicalize` follows symlinks: it fails
        // outright on a dangling one, rejecting a whole batch that
        // `copy_entry`/`move_entry` would have transferred happily. The two
        // containment checks below are about the entry's own location, and
        // `symlink_metadata` already reports `is_dir() == false` for a
        // symlink, so a link's target is never the thing being contained.
        let source_root = match source.file_name() {
            Some(name) => parent.join(name),
            // A path ending in `..` or `/` has no final component to transfer.
            None => {
                return Err(format!(
                    "preflight cannot resolve the name of {}",
                    source.display()
                ));
            }
        };
        if parent == destination_root {
            return Err(format!(
                "batch preflight: {} is already in {}",
                source.display(),
                destination_dir.display()
            ));
        }
        if metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && destination_root.starts_with(&source_root)
        {
            return Err(format!(
                "batch preflight: cannot transfer folder {} into itself",
                source.display()
            ));
        }
        source_roots.push((source.as_path(), source_root));
        prepared.push(PreparedTransfer {
            source: source.as_path(),
            target,
        });
    }

    if kind == FileOpKind::BatchMove {
        for (index, (source, root)) in source_roots.iter().enumerate() {
            for (other_index, (other, other_root)) in source_roots.iter().enumerate() {
                if index != other_index && other_root.starts_with(root) {
                    return Err(format!(
                        "batch preflight: move sources overlap ({} contains {})",
                        source.display(),
                        other.display()
                    ));
                }
            }
        }
    }
    Ok(prepared)
}

fn batch_verb(kind: FileOpKind) -> &'static str {
    match kind {
        FileOpKind::BatchCopy => "copy",
        FileOpKind::BatchMove => "move",
        _ => unreachable!("only batch operations have a batch verb"),
    }
}

fn copy_entry(source: &Path, destination_dir: &Path) -> Result<PathBuf, String> {
    let target = target_path(source, destination_dir)?;
    if target.exists() || fs::symlink_metadata(&target).is_ok() {
        return Err(format!("{} already exists", target.display()));
    }
    let metadata = fs::symlink_metadata(source)
        .map_err(|error| format!("reading {}: {error}", source.display()))?;
    if metadata.is_dir() {
        let source_root = source
            .canonicalize()
            .map_err(|error| format!("resolving {}: {error}", source.display()))?;
        let destination_root = destination_dir
            .canonicalize()
            .map_err(|error| format!("resolving {}: {error}", destination_dir.display()))?;
        if destination_root.starts_with(&source_root) {
            return Err("cannot copy a folder into itself".into());
        }
    }
    if let Err(error) = copy_entry_to(source, &target) {
        let _ = remove_entry(&target);
        return Err(error);
    }
    Ok(target)
}

fn copy_entry_to(source: &Path, target: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(source)
        .map_err(|error| format!("reading {}: {error}", source.display()))?;
    if metadata.file_type().is_symlink() {
        let link = fs::read_link(source)
            .map_err(|error| format!("reading link {}: {error}", source.display()))?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(&link, target)
            .map_err(|error| format!("copying link to {}: {error}", target.display()))?;
        #[cfg(not(unix))]
        return Err(format!(
            "copying symbolic links is unsupported: {}",
            source.display()
        ));
        return Ok(());
    }
    if metadata.is_dir() {
        fs::create_dir(target)
            .map_err(|error| format!("creating {}: {error}", target.display()))?;
        for entry in fs::read_dir(source)
            .map_err(|error| format!("reading {}: {error}", source.display()))?
        {
            let entry = entry.map_err(|error| format!("reading {}: {error}", source.display()))?;
            copy_entry_to(&entry.path(), &target.join(entry.file_name()))?;
        }
        fs::set_permissions(target, metadata.permissions())
            .map_err(|error| format!("setting permissions on {}: {error}", target.display()))?;
    } else if metadata.is_file() {
        fs::copy(source, target).map_err(|error| {
            format!(
                "copying {} to {}: {error}",
                source.display(),
                target.display()
            )
        })?;
    } else {
        return Err(format!("unsupported file type: {}", source.display()));
    }
    Ok(())
}

fn move_entry(source: &Path, destination_dir: &Path) -> Result<PathBuf, String> {
    let target = target_path(source, destination_dir)?;
    if target.exists() || fs::symlink_metadata(&target).is_ok() {
        return Err(format!("{} already exists", target.display()));
    }
    match fs::rename(source, &target) {
        Ok(()) => Ok(target),
        Err(error) if error.kind() == std::io::ErrorKind::CrossesDevices => {
            let copied = copy_entry(source, destination_dir)?;
            if let Err(remove_error) = remove_entry(source) {
                return Err(format!(
                    "move copied data to {} but source cleanup was incomplete: {remove_error}",
                    copied.display()
                ));
            }
            Ok(copied)
        }
        Err(error) => Err(format!(
            "moving {} to {}: {error}",
            source.display(),
            target.display()
        )),
    }
}

fn target_path(source: &Path, destination_dir: &Path) -> Result<PathBuf, String> {
    let name = source
        .file_name()
        .ok_or_else(|| format!("cannot operate on {}", source.display()))?;
    let target = destination_dir.join(name);
    if target == source {
        return Err("source is already in that folder".into());
    }
    Ok(target)
}

fn remove_entry(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("reading {}: {error}", path.display()))?;
    let result = if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    result.map_err(|error| format!("deleting {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new(name: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path =
                std::env::temp_dir().join(format!("filemgr-{name}-{}-{nonce}", std::process::id()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn recursively_copies_without_overwriting() {
        let root = TestRoot::new("copy");
        let source = root.0.join("source");
        let destination = root.0.join("destination");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&destination).unwrap();
        fs::write(source.join("hello.txt"), b"hello").unwrap();

        FileOperation::copy(source.clone(), destination.clone())
            .execute()
            .unwrap();
        assert_eq!(
            fs::read(destination.join("source/hello.txt")).unwrap(),
            b"hello"
        );
        assert!(FileOperation::copy(source, destination).execute().is_err());
    }

    #[test]
    fn moves_and_deletes_files_and_folders() {
        let root = TestRoot::new("move-delete");
        let source_dir = root.0.join("source");
        let destination = root.0.join("destination");
        fs::create_dir(&source_dir).unwrap();
        fs::create_dir(&destination).unwrap();
        let source = source_dir.join("file.txt");
        fs::write(&source, b"move me").unwrap();

        FileOperation::move_to(source.clone(), destination.clone())
            .execute()
            .unwrap();
        assert!(!source.exists());
        let moved = destination.join("file.txt");
        assert!(moved.exists());
        FileOperation::delete(moved).execute().unwrap();
        fs::create_dir(source_dir.join("nested")).unwrap();
        fs::write(source_dir.join("nested/child.txt"), b"delete me").unwrap();
        FileOperation::delete(source_dir.clone()).execute().unwrap();
        assert!(!source_dir.exists());
    }

    #[test]
    fn creates_and_renames_folders_without_overwriting() {
        let root = TestRoot::new("new-rename");
        let created = root.0.join("Created");
        FileOperation::new_folder(created.clone())
            .execute()
            .unwrap();
        assert!(created.is_dir());

        let renamed = root.0.join("Renamed");
        FileOperation::rename(created.clone(), renamed.clone())
            .execute()
            .unwrap();
        assert!(!created.exists());
        assert!(renamed.is_dir());

        fs::create_dir(root.0.join("Existing")).unwrap();
        assert!(FileOperation::rename(renamed, root.0.join("Existing"))
            .execute()
            .is_err());
    }

    #[test]
    fn refuses_to_copy_a_folder_into_itself() {
        let root = TestRoot::new("self-copy");
        let source = root.0.join("source");
        let child = source.join("child");
        fs::create_dir_all(&child).unwrap();
        let error = FileOperation::copy(source, child).execute().unwrap_err();
        assert!(error.contains("into itself"));
    }

    #[test]
    fn move_collision_keeps_both_existing_files_untouched() {
        let root = TestRoot::new("move-collision");
        let source_dir = root.0.join("source");
        let destination = root.0.join("destination");
        fs::create_dir(&source_dir).unwrap();
        fs::create_dir(&destination).unwrap();
        let source = source_dir.join("same.txt");
        let existing = destination.join("same.txt");
        fs::write(&source, b"source").unwrap();
        fs::write(&existing, b"existing").unwrap();

        assert!(FileOperation::move_to(source.clone(), destination)
            .execute()
            .is_err());
        assert_eq!(fs::read(source).unwrap(), b"source");
        assert_eq!(fs::read(existing).unwrap(), b"existing");
    }

    #[test]
    fn batch_preflight_checks_every_collision_before_any_mutation() {
        let root = TestRoot::new("batch-full-preflight");
        let sources = root.0.join("sources");
        let destination = root.0.join("destination");
        fs::create_dir(&sources).unwrap();
        fs::create_dir(&destination).unwrap();
        let paths = (1..=7)
            .map(|index| {
                let path = sources.join(format!("{index}.txt"));
                fs::write(&path, format!("source-{index}")).unwrap();
                path
            })
            .collect::<Vec<_>>();
        fs::write(destination.join("7.txt"), b"collision").unwrap();

        let error = FileOperation::move_batch(paths.clone(), destination.clone())
            .unwrap()
            .execute()
            .unwrap_err();

        assert!(error.contains("preflight"));
        for path in &paths {
            assert!(
                path.exists(),
                "{} was mutated before preflight ended",
                path.display()
            );
        }
        for index in 1..=6 {
            assert!(!destination.join(format!("{index}.txt")).exists());
        }
        assert_eq!(fs::read(destination.join("7.txt")).unwrap(), b"collision");
    }

    #[cfg(unix)]
    #[test]
    fn batch_runtime_failure_reports_completed_failed_and_unattempted_items() {
        use std::os::unix::net::UnixListener;

        let root = TestRoot::new("batch-partial-report");
        let sources = root.0.join("sources");
        let destination = root.0.join("destination");
        fs::create_dir(&sources).unwrap();
        fs::create_dir(&destination).unwrap();
        let first = sources.join("1.txt");
        let unsupported = sources.join("2.socket");
        let third = sources.join("3.txt");
        fs::write(&first, b"first").unwrap();
        let _listener = UnixListener::bind(&unsupported).unwrap();
        fs::write(&third, b"third").unwrap();

        let error = FileOperation::copy_batch(
            vec![first, unsupported.clone(), third.clone()],
            destination.clone(),
        )
        .unwrap()
        .execute()
        .unwrap_err();

        assert!(error.contains("1/3 completed"), "{error}");
        assert!(error.contains(&unsupported.to_string_lossy().to_string()));
        assert!(error.contains("1 item(s) not attempted"), "{error}");
        assert!(destination.join("1.txt").exists());
        assert!(!destination.join("3.txt").exists());
        assert!(third.exists());
    }

    #[test]
    fn batch_copy_rejects_duplicate_target_names_during_preflight() {
        let root = TestRoot::new("batch-duplicate-name");
        let left = root.0.join("left");
        let right = root.0.join("right");
        let destination = root.0.join("destination");
        fs::create_dir(&left).unwrap();
        fs::create_dir(&right).unwrap();
        fs::create_dir(&destination).unwrap();
        let first = left.join("same.txt");
        let second = right.join("same.txt");
        fs::write(&first, b"first").unwrap();
        fs::write(&second, b"second").unwrap();

        let error = FileOperation::copy_batch(vec![first, second], destination)
            .unwrap()
            .execute()
            .unwrap_err();
        assert!(error.contains("duplicate target name"));
    }

    #[test]
    fn batch_preflight_accepts_a_dangling_symlink() {
        // Preflight must never be stricter than execution. `canonicalize`
        // follows symlinks and fails outright on a dangling one, which
        // rejected the whole batch before any mutation even though
        // `copy_entry` transfers the link itself perfectly well.
        let root = TestRoot::new("batch-dangling-symlink");
        let sources = root.0.join("sources");
        let destination = root.0.join("destination");
        fs::create_dir(&sources).unwrap();
        fs::create_dir(&destination).unwrap();
        let regular = sources.join("regular.txt");
        fs::write(&regular, b"regular").unwrap();
        let dangling = sources.join("dangling");
        std::os::unix::fs::symlink(sources.join("does-not-exist"), &dangling).unwrap();

        FileOperation::copy_batch(vec![regular, dangling], destination.clone())
            .unwrap()
            .execute()
            .unwrap();

        assert!(destination.join("regular.txt").exists());
        assert!(
            fs::symlink_metadata(destination.join("dangling")).is_ok(),
            "the dangling symlink itself should have been transferred"
        );
    }
}
