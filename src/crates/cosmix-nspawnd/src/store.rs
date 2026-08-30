//! Durable host-local state.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use chrono::{Duration as ChronoDuration, SecondsFormat, Utc};
use cosmix_nspawnd::core::{
    C0Tombstone, Grant, InstanceName, InstanceRecord, OperationRecord, Tombstone,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

const SUBDIRS: [&str; 5] = [
    "tombstones",
    "grants",
    "instances",
    "operations",
    "operations/requests",
];
const REPLAY_SCHEMA: &str = "cosmix.nspawnd.replay.v1";
pub const OPERATION_GC_HORIZON: usize = 500;
pub const OPERATION_GC_HARD_LIMIT: usize = 10_000;
const OPERATION_REPLAY_MIN_AGE_HOURS: i64 = 24;

#[derive(Debug)]
pub enum RequestClaim {
    Claimed,
    Existing(Box<OperationRecord>),
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ReplayRecord {
    schema: String,
    actor: String,
    request_id: String,
    request_hash: String,
    op_id: String,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("state I/O at {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("corrupt structured state at {path}: {message}")]
    Corrupt { path: PathBuf, message: String },
    #[error("legacy C0 tombstone has not been imported: {0}")]
    LegacyUnimported(PathBuf),
    #[error(
        "replay history for ({actor:?}, {request_id:?}) points to garbage-collected operation {op_id}; the original outcome was garbage-collected mid-eviction, so issue a new request_id"
    )]
    ReplayEvicted {
        actor: String,
        request_id: String,
        op_id: String,
    },
    #[error("state conflict: {0}")]
    Conflict(String),
}

impl StoreError {
    fn io(path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    fn corrupt(path: impl Into<PathBuf>, message: impl Into<String>) -> Self {
        Self::Corrupt {
            path: path.into(),
            message: message.into(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct StateStore {
    root: PathBuf,
    legacy_root: PathBuf,
    owner: Option<(u32, u32)>,
}

impl StateStore {
    pub fn new(root: impl Into<PathBuf>, legacy_root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            legacy_root: legacy_root.into(),
            owner: None,
        }
    }

    /// Root-run admin commands use this so their output remains writable by
    /// the long-running SPEC-10 daemon identity.
    pub fn with_owner(mut self, uid: u32, gid: u32) -> Self {
        self.owner = Some((uid, gid));
        self
    }

    pub fn legacy_root(&self) -> &Path {
        &self.legacy_root
    }

    pub fn ensure_layout(&self) -> Result<(), StoreError> {
        self.ensure_dir(&self.root)?;
        for subdir in SUBDIRS {
            self.ensure_dir(&self.root.join(subdir))?;
        }
        Ok(())
    }

    fn ensure_dir(&self, path: &Path) -> Result<(), StoreError> {
        fs::create_dir_all(path).map_err(|error| StoreError::io(path, error))?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| StoreError::io(path, error))?;
        if let Some((uid, gid)) = self.owner {
            chown_path(path, uid, gid).map_err(|error| StoreError::io(path, error))?;
        }
        Ok(())
    }

    fn named_path(&self, subdir: &str, name: &InstanceName) -> PathBuf {
        self.root.join(subdir).join(format!("{name}.json"))
    }

    #[cfg(test)]
    pub fn legacy_tombstone_path(&self, name: &InstanceName) -> PathBuf {
        self.legacy_root.join(format!("{name}.json"))
    }

    /// Audit every C0 tombstone before making a generation decision. One
    /// corrupt or unimported legacy fence blocks all starts, so an operator
    /// cannot accidentally work around bad evidence by asking for another
    /// instance first.
    pub fn assert_legacy_import_complete(&self) -> Result<(), StoreError> {
        for legacy in json_paths(&self.legacy_root)? {
            let c0: C0Tombstone = load_required(&legacy)?;
            let stem = legacy
                .file_stem()
                .and_then(|value| value.to_str())
                .ok_or_else(|| StoreError::corrupt(&legacy, "non-UTF-8 filename"))?;
            let filename_name = InstanceName::parse(stem)
                .map_err(|error| StoreError::corrupt(&legacy, error.to_string()))?;
            if filename_name != c0.name {
                return Err(StoreError::corrupt(&legacy, "filename/name mismatch"));
            }
            let expected_name = c0.name.clone();
            let expected_floor = c0.generation_next;
            Tombstone::try_from(c0).map_err(|error| StoreError::corrupt(&legacy, error))?;
            let canonical_path = self.named_path("tombstones", &expected_name);
            let Some(canonical) = load_optional::<Tombstone>(&canonical_path)? else {
                return Err(StoreError::LegacyUnimported(legacy));
            };
            canonical
                .validate()
                .map_err(|error| StoreError::corrupt(&canonical_path, error))?;
            if canonical.name != expected_name {
                return Err(StoreError::corrupt(
                    &canonical_path,
                    "filename/name mismatch",
                ));
            }
            if canonical.minimum_generation < expected_floor {
                return Err(StoreError::LegacyUnimported(legacy));
            }
        }
        Ok(())
    }

    pub fn load_tombstone(&self, name: &InstanceName) -> Result<Option<Tombstone>, StoreError> {
        self.assert_legacy_import_complete()?;
        let path = self.named_path("tombstones", name);
        let value = load_optional::<Tombstone>(&path)?;
        if let Some(value) = &value {
            value
                .validate()
                .map_err(|error| StoreError::corrupt(&path, error))?;
            if &value.name != name {
                return Err(StoreError::corrupt(&path, "filename/name mismatch"));
            }
        }
        Ok(value)
    }

    pub fn merge_tombstone(&self, incoming: Tombstone) -> Result<Tombstone, StoreError> {
        incoming.validate().map_err(|error| {
            StoreError::corrupt(self.named_path("tombstones", &incoming.name), error)
        })?;
        let _guard = self.lock_store()?;
        let current = load_optional::<Tombstone>(&self.named_path("tombstones", &incoming.name))?;
        let selected = match current {
            Some(current) => {
                current.validate().map_err(|error| {
                    StoreError::corrupt(self.named_path("tombstones", &incoming.name), error)
                })?;
                if current.minimum_generation > incoming.minimum_generation {
                    current
                } else if current.minimum_generation == incoming.minimum_generation {
                    // Equal floors are idempotent. Keep the first durable evidence.
                    current
                } else {
                    incoming
                }
            }
            None => incoming,
        };
        self.write_named("tombstones", &selected.name, &selected)?;
        Ok(selected)
    }

    pub fn load_grant(&self, name: &InstanceName) -> Result<Option<Grant>, StoreError> {
        let path = self.named_path("grants", name);
        let value = load_optional::<Grant>(&path)?;
        if let Some(value) = &value {
            value
                .validate()
                .map_err(|error| StoreError::corrupt(&path, error))?;
            if &value.name != name {
                return Err(StoreError::corrupt(&path, "filename/name mismatch"));
            }
        }
        Ok(value)
    }

    pub fn merge_grant(&self, incoming: Grant) -> Result<Grant, StoreError> {
        incoming.validate().map_err(|error| {
            StoreError::corrupt(self.named_path("grants", &incoming.name), error)
        })?;
        let _guard = self.lock_store()?;
        let current = self.load_grant(&incoming.name)?;
        let selected = match current {
            Some(current) if current.generation > incoming.generation => {
                return Err(StoreError::Conflict(format!(
                    "refusing to lower {} grant generation {} -> {}",
                    incoming.name, current.generation, incoming.generation
                )));
            }
            Some(current) if current.generation == incoming.generation => {
                if current.owner != incoming.owner {
                    return Err(StoreError::Conflict(format!(
                        "generation {} has conflicting owners {:?} and {:?}",
                        current.generation, current.owner, incoming.owner
                    )));
                }
                if current.source != incoming.source {
                    return Err(StoreError::Conflict(format!(
                        "generation {} has conflicting grant evidence",
                        current.generation
                    )));
                }
                current
            }
            _ => incoming,
        };
        self.write_named("grants", &selected.name, &selected)?;
        Ok(selected)
    }

    pub fn load_instance(&self, name: &InstanceName) -> Result<Option<InstanceRecord>, StoreError> {
        let path = self.named_path("instances", name);
        let value = load_optional::<InstanceRecord>(&path)?;
        if let Some(value) = &value {
            value
                .validate()
                .map_err(|error| StoreError::corrupt(&path, error))?;
            if &value.name != name {
                return Err(StoreError::corrupt(&path, "filename/name mismatch"));
            }
        }
        Ok(value)
    }

    pub fn save_instance(&self, value: &InstanceRecord) -> Result<(), StoreError> {
        value.validate().map_err(|error| {
            StoreError::corrupt(self.named_path("instances", &value.name), error)
        })?;
        self.write_named("instances", &value.name, value)
    }

    pub fn list_managed_names(&self) -> Result<Vec<InstanceName>, StoreError> {
        self.list_named("instances")
    }

    pub fn save_operation(&self, value: &OperationRecord) -> Result<(), StoreError> {
        value.validate().map_err(|error| {
            StoreError::corrupt(self.root.join("operations").join(&value.op_id), error)
        })?;
        let path = self
            .root
            .join("operations")
            .join(format!("{}.json", value.op_id));
        self.atomic_write(&path, value)
    }

    /// Durably claims the global `(actor, request_id)` replay key before any
    /// actuation. Replay protection lasts while the pair is among the newest
    /// 500 completed operations or its completion is less than 24 hours old,
    /// subject to the hard 10,000-completed-pair disk bound. The store lock
    /// closes the cross-instance/cross-process race; the operation is written
    /// before its index, so a crash can leave only a harmless, unclaimed
    /// operation record rather than a dangling mapping.
    pub fn claim_operation(&self, value: &OperationRecord) -> Result<RequestClaim, StoreError> {
        value.validate().map_err(|error| {
            StoreError::corrupt(self.root.join("operations").join(&value.op_id), error)
        })?;
        let _guard = self.lock_store()?;
        if let Some(existing) = self.find_request_unlocked(&value.actor, &value.request_id)? {
            return Ok(RequestClaim::Existing(Box::new(existing)));
        }
        self.save_operation(value)?;
        let replay = ReplayRecord {
            schema: REPLAY_SCHEMA.into(),
            actor: value.actor.clone(),
            request_id: value.request_id.clone(),
            request_hash: value.request_hash.clone(),
            op_id: value.op_id.clone(),
        };
        self.atomic_write(&self.replay_path(&value.actor, &value.request_id), &replay)?;
        Ok(RequestClaim::Claimed)
    }

    pub fn find_request(
        &self,
        actor: &str,
        request_id: &str,
    ) -> Result<Option<OperationRecord>, StoreError> {
        let _guard = self.lock_store()?;
        self.find_request_unlocked(actor, request_id)
    }

    pub fn load_operation(&self, op_id: &str) -> Result<Option<OperationRecord>, StoreError> {
        let path = self.root.join("operations").join(format!("{op_id}.json"));
        let Some(operation) = load_optional::<OperationRecord>(&path)? else {
            return Ok(None);
        };
        operation
            .validate()
            .map_err(|error| StoreError::corrupt(&path, error))?;
        if operation.op_id != op_id {
            return Err(StoreError::corrupt(&path, "filename/op_id mismatch"));
        }
        Ok(Some(operation))
    }

    pub fn running_operations(&self) -> Result<Vec<OperationRecord>, StoreError> {
        let _guard = self.lock_store()?;
        let mut running = Vec::new();
        for path in json_paths(&self.root.join("operations"))? {
            let Some(operation) = load_during_scan::<OperationRecord>(&path)? else {
                continue;
            };
            operation
                .validate()
                .map_err(|error| StoreError::corrupt(&path, error))?;
            if !operation_filename_matches(&path, &operation) {
                tracing::warn!(path = %path.display(), op_id = %operation.op_id, "skipping operation whose filename does not match its record");
                continue;
            }
            if operation.state == cosmix_nspawnd::core::OperationState::Running {
                running.push(operation);
            }
        }
        running.sort_by(|left, right| left.op_id.cmp(&right.op_id));
        Ok(running)
    }

    /// Completed operation/replay pairs are retained while either among the
    /// newest 500 or less than 24 hours old. A hard backstop retains at most
    /// 10,000 completed pairs regardless of age. Running operations are never
    /// collected. Replay protection and operation history therefore share
    /// these same age, horizon, and hard-limit bounds.
    /// The 10,000 cap intentionally wins inside 24 hours because bounding disk
    /// use must not let a runaway authorised client deny new safety-stop claims.
    pub fn gc_operations(&self) -> Result<usize, StoreError> {
        let _guard = self.lock_store()?;
        let operations_dir = self.root.join("operations");
        let requests_dir = operations_dir.join("requests");
        let mut completed = Vec::new();
        for path in json_paths(&operations_dir)? {
            let Some(operation) = load_during_scan::<OperationRecord>(&path)? else {
                continue;
            };
            operation
                .validate()
                .map_err(|error| StoreError::corrupt(&path, error))?;
            if !operation_filename_matches(&path, &operation) {
                tracing::warn!(path = %path.display(), op_id = %operation.op_id, "skipping operation whose filename does not match its record during GC");
                continue;
            }
            if operation.state != cosmix_nspawnd::core::OperationState::Running {
                completed.push((operation, path));
            }
        }
        completed.sort_by(|left, right| right.0.op_id.cmp(&left.0.op_id));
        let cutoff = (Utc::now() - ChronoDuration::hours(OPERATION_REPLAY_MIN_AGE_HOURS))
            .to_rfc3339_opts(SecondsFormat::Secs, true);
        let mut removed = 0;
        let mut replay_paths = Vec::new();
        for (index, (operation, path)) in completed.into_iter().enumerate() {
            if !should_collect_completed(index, operation.completed_at.as_deref(), &cutoff) {
                continue;
            }
            remove_if_present(&path)?;
            replay_paths.push(self.replay_path(&operation.actor, &operation.request_id));
            removed += 1;
        }
        if removed > 0 {
            sync_dir(&operations_dir)?;
        }
        for replay_path in replay_paths {
            remove_if_present(&replay_path)?;
        }
        let dangling_removed = self.remove_dangling_replays(&operations_dir, &requests_dir)?;
        if removed > 0 || dangling_removed > 0 {
            sync_dir(&requests_dir)?;
        }
        Ok(removed)
    }

    fn remove_dangling_replays(
        &self,
        operations_dir: &Path,
        requests_dir: &Path,
    ) -> Result<usize, StoreError> {
        let mut removed = 0;
        for path in json_paths(requests_dir)? {
            let Some(replay) = load_during_scan::<ReplayRecord>(&path)? else {
                continue;
            };
            validate_replay_record(self, &path, &replay)?;
            let operation_path = operations_dir.join(format!("{}.json", replay.op_id));
            match fs::metadata(&operation_path) {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    tracing::warn!(
                        path = %path.display(),
                        op_id = %replay.op_id,
                        "removing dangling replay key left by interrupted operation eviction"
                    );
                    remove_if_present(&path)?;
                    removed += 1;
                }
                Err(error) => return Err(StoreError::io(&operation_path, error)),
            }
        }
        Ok(removed)
    }

    pub fn latest_operation_for(
        &self,
        name: &InstanceName,
    ) -> Result<Option<OperationRecord>, StoreError> {
        self.latest_operation_matching(name, |_| true)
    }

    pub fn latest_controller_operation_for(
        &self,
        name: &InstanceName,
    ) -> Result<Option<OperationRecord>, StoreError> {
        self.latest_operation_matching(name, |operation| operation.actor.starts_with("bridge-"))
    }

    fn latest_operation_matching(
        &self,
        name: &InstanceName,
        predicate: impl Fn(&OperationRecord) -> bool,
    ) -> Result<Option<OperationRecord>, StoreError> {
        let _guard = self.lock_store()?;
        let dir = self.root.join("operations");
        let mut paths = json_paths(&dir)?;
        // ULIDs sort chronologically. Their random tail gives no strict order
        // within the same millisecond; that is accepted for this diagnostic.
        paths.sort_by(|left, right| right.cmp(left));
        for path in paths {
            let Some(operation) = load_during_scan::<OperationRecord>(&path)? else {
                continue;
            };
            operation
                .validate()
                .map_err(|error| StoreError::corrupt(&path, error))?;
            if !operation_filename_matches(&path, &operation) {
                tracing::warn!(path = %path.display(), op_id = %operation.op_id, "skipping operation whose filename does not match its record");
                continue;
            }
            if &operation.name == name && predicate(&operation) {
                return Ok(Some(operation));
            }
        }
        Ok(None)
    }

    pub fn save_legacy_import_marker<T: Serialize>(&self, marker: &T) -> Result<(), StoreError> {
        self.atomic_write(&self.root.join("legacy-import.json"), marker)
    }

    fn list_named(&self, subdir: &str) -> Result<Vec<InstanceName>, StoreError> {
        let dir = self.root.join(subdir);
        let mut names = Vec::new();
        for path in json_paths(&dir)? {
            let stem = path
                .file_stem()
                .and_then(|value| value.to_str())
                .ok_or_else(|| StoreError::corrupt(&path, "non-UTF-8 filename"))?;
            names.push(
                InstanceName::parse(stem)
                    .map_err(|error| StoreError::corrupt(&path, error.to_string()))?,
            );
        }
        names.sort();
        names.dedup();
        Ok(names)
    }

    fn write_named<T: Serialize>(
        &self,
        subdir: &str,
        name: &InstanceName,
        value: &T,
    ) -> Result<(), StoreError> {
        self.atomic_write(&self.named_path(subdir, name), value)
    }

    fn find_request_unlocked(
        &self,
        actor: &str,
        request_id: &str,
    ) -> Result<Option<OperationRecord>, StoreError> {
        let replay_path = self.replay_path(actor, request_id);
        let Some(replay) = load_optional::<ReplayRecord>(&replay_path)? else {
            return Ok(None);
        };
        if replay.schema != REPLAY_SCHEMA
            || replay.actor != actor
            || replay.request_id != request_id
            || replay.request_hash.is_empty()
            || replay.op_id.is_empty()
        {
            return Err(StoreError::corrupt(
                &replay_path,
                "invalid replay record or replay-key collision",
            ));
        }
        let operation_path = self
            .root
            .join("operations")
            .join(format!("{}.json", replay.op_id));
        let Some(operation) = load_optional::<OperationRecord>(&operation_path)? else {
            return Err(StoreError::ReplayEvicted {
                actor: replay.actor,
                request_id: replay.request_id,
                op_id: replay.op_id,
            });
        };
        operation
            .validate()
            .map_err(|error| StoreError::corrupt(&operation_path, error))?;
        if operation.op_id != replay.op_id
            || operation.actor != replay.actor
            || operation.request_id != replay.request_id
            || operation.request_hash != replay.request_hash
        {
            return Err(StoreError::corrupt(
                &operation_path,
                "operation/replay mapping mismatch",
            ));
        }
        Ok(Some(operation))
    }

    fn replay_path(&self, actor: &str, request_id: &str) -> PathBuf {
        let mut input = Vec::with_capacity(actor.len() + request_id.len() + 1);
        input.extend_from_slice(actor.as_bytes());
        input.push(0);
        input.extend_from_slice(request_id.as_bytes());
        let key = blake3::hash(&input).to_hex();
        self.root
            .join("operations/requests")
            .join(format!("{key}.json"))
    }

    fn lock_store(&self) -> Result<StoreLock, StoreError> {
        self.ensure_dir(&self.root)?;
        let path = self.root.join(".store.lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&path)
            .map_err(|error| StoreError::io(&path, error))?;
        if let Some((uid, gid)) = self.owner {
            let rc = unsafe { libc::fchown(file.as_raw_fd(), uid, gid) };
            if rc != 0 {
                return Err(StoreError::io(&path, io::Error::last_os_error()));
            }
        }
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if rc != 0 {
            return Err(StoreError::io(&path, io::Error::last_os_error()));
        }
        Ok(StoreLock(file))
    }

    fn atomic_write<T: Serialize>(&self, path: &Path, value: &T) -> Result<(), StoreError> {
        let parent = path
            .parent()
            .ok_or_else(|| StoreError::corrupt(path, "path has no parent"))?;
        self.ensure_dir(parent)?;
        let bytes = serde_json::to_vec(value)
            .map_err(|error| StoreError::corrupt(path, error.to_string()))?;
        let tmp = parent.join(format!(
            ".{}.tmp.{}",
            path.file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("state"),
            ulid::Ulid::new()
        ));
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|error| StoreError::io(&tmp, error))?;
        if let Some((uid, gid)) = self.owner {
            let rc = unsafe { libc::fchown(file.as_raw_fd(), uid, gid) };
            if rc != 0 {
                return Err(StoreError::io(&tmp, io::Error::last_os_error()));
            }
        }
        file.write_all(&bytes)
            .and_then(|()| file.write_all(b"\n"))
            .and_then(|()| file.sync_all())
            .map_err(|error| StoreError::io(&tmp, error))?;
        fs::rename(&tmp, path).map_err(|error| StoreError::io(path, error))?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| StoreError::io(parent, error))?;
        Ok(())
    }
}

struct StoreLock(File);

impl Drop for StoreLock {
    fn drop(&mut self) {
        let _ = unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn load_optional<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, StoreError> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| StoreError::corrupt(path, error.to_string())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(StoreError::io(path, error)),
    }
}

fn operation_filename_matches(path: &Path, operation: &OperationRecord) -> bool {
    path.file_stem().and_then(|value| value.to_str()) == Some(operation.op_id.as_str())
}

fn validate_replay_record(
    store: &StateStore,
    path: &Path,
    replay: &ReplayRecord,
) -> Result<(), StoreError> {
    if replay.schema != REPLAY_SCHEMA
        || replay.actor.is_empty()
        || replay.request_id.is_empty()
        || replay.request_hash.is_empty()
        || replay.op_id.is_empty()
        || store.replay_path(&replay.actor, &replay.request_id) != path
    {
        return Err(StoreError::corrupt(
            path,
            "invalid replay record or replay-key collision",
        ));
    }
    Ok(())
}

fn load_during_scan<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, StoreError> {
    match load_required(path) {
        Ok(record) => Ok(Some(record)),
        Err(StoreError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            tracing::debug!(path = %path.display(), "record disappeared during directory scan");
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn should_collect_completed(index: usize, completed_at: Option<&str>, cutoff: &str) -> bool {
    index >= OPERATION_GC_HARD_LIMIT
        || (index >= OPERATION_GC_HORIZON
            && completed_at.is_some_and(|completed_at| completed_at < cutoff))
}

fn remove_if_present(path: &Path) -> Result<(), StoreError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(StoreError::io(path, error)),
    }
}

fn sync_dir(path: &Path) -> Result<(), StoreError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| StoreError::io(path, error))
}

pub fn load_required<T: DeserializeOwned>(path: &Path) -> Result<T, StoreError> {
    let bytes = fs::read(path).map_err(|error| StoreError::io(path, error))?;
    serde_json::from_slice(&bytes).map_err(|error| StoreError::corrupt(path, error.to_string()))
}

fn json_paths(dir: &Path) -> Result<Vec<PathBuf>, StoreError> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(StoreError::io(dir, error)),
    };
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| StoreError::io(dir, error))?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) == Some("json") {
            paths.push(path);
        }
    }
    Ok(paths)
}

fn chown_path(path: &Path, uid: u32, gid: u32) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))?;
    let rc = unsafe { libc::chown(c_path.as_ptr(), uid, gid) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_nspawnd::core::{
        GRANT_SCHEMA, GrantSource, OPERATION_SCHEMA, ObservedInstance, OperationState,
        OperationVerb, TOMBSTONE_SCHEMA, TombstoneSource,
    };

    fn store(temp: &tempfile::TempDir) -> StateStore {
        let store = StateStore::new(temp.path().join("state"), temp.path().join("legacy"));
        store.ensure_layout().unwrap();
        store
    }

    fn name() -> InstanceName {
        InstanceName::parse("labspoke").unwrap()
    }

    fn tombstone(floor: u64) -> Tombstone {
        Tombstone {
            schema: TOMBSTONE_SCHEMA.into(),
            name: name(),
            minimum_generation: floor,
            moved_to: "beta".into(),
            op_id: format!("op-{floor}"),
            recorded_at: "now".into(),
            enforced: true,
            source: TombstoneSource {
                kind: "test".into(),
                advisory_source: false,
            },
        }
    }

    fn grant(generation: u64) -> Grant {
        Grant {
            schema: GRANT_SCHEMA.into(),
            name: name(),
            owner: "alpha".into(),
            generation,
            source: GrantSource {
                kind: "test".into(),
                record_version: 7,
                record_state: "complete".into(),
                record_updated: "now".into(),
            },
            installed_at: "now".into(),
        }
    }

    fn operation(sequence: usize, state: OperationState) -> OperationRecord {
        let op_id = format!("{sequence:026}");
        OperationRecord {
            schema: OPERATION_SCHEMA.into(),
            op_id,
            actor: "operator".into(),
            request_id: format!("request-{sequence}"),
            request_hash: format!("hash-{sequence}"),
            verb: OperationVerb::Start,
            name: name(),
            generation: 3,
            state,
            started_at: "now".into(),
            completed_at: (state != OperationState::Running).then(|| "2000-01-01T00:00:00Z".into()),
            observed_before: ObservedInstance {
                name: name(),
                running: false,
                image_present: true,
                machine_class: None,
                machine_service: None,
                machine_unit: None,
                unit_load: "loaded".into(),
                unit_active: "inactive".into(),
                unit_sub: "dead".into(),
                unit_file_state: "disabled".into(),
            },
            observed_after: None,
            response_rc: (state != OperationState::Running).then_some(0),
            response_body: (state != OperationState::Running)
                .then(|| serde_json::json!({"ok":true})),
        }
    }

    #[test]
    fn tombstone_max_merge_never_lowers_floor() {
        let temp = tempfile::tempdir().unwrap();
        let store = store(&temp);
        assert_eq!(
            store
                .merge_tombstone(tombstone(4))
                .unwrap()
                .minimum_generation,
            4
        );
        assert_eq!(
            store
                .merge_tombstone(tombstone(2))
                .unwrap()
                .minimum_generation,
            4
        );
        assert_eq!(
            store
                .load_tombstone(&name())
                .unwrap()
                .unwrap()
                .minimum_generation,
            4
        );
    }

    #[test]
    fn grant_merge_rejects_lower_generation() {
        let temp = tempfile::tempdir().unwrap();
        let store = store(&temp);
        store.merge_grant(grant(3)).unwrap();
        assert!(matches!(
            store.merge_grant(grant(2)),
            Err(StoreError::Conflict(_))
        ));
    }

    #[test]
    fn corrupt_state_and_unimported_legacy_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        let store = store(&temp);
        fs::write(store.named_path("grants", &name()), b"not json").unwrap();
        assert!(matches!(
            store.load_grant(&name()),
            Err(StoreError::Corrupt { .. })
        ));

        fs::create_dir_all(store.legacy_root()).unwrap();
        fs::write(
            store.legacy_tombstone_path(&name()),
            serde_json::to_vec(&serde_json::json!({
                "name":"labspoke", "op":"c0-op", "moved_to":"beta",
                "generation_next":2, "at":"now", "advisory":true
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(matches!(
            store.load_tombstone(&name()),
            Err(StoreError::LegacyUnimported(_))
        ));

        fs::write(store.legacy_tombstone_path(&name()), b"not json").unwrap();
        assert!(matches!(
            store.load_tombstone(&name()),
            Err(StoreError::Corrupt { .. })
        ));
    }

    #[test]
    fn missing_grant_is_explicit_none() {
        let temp = tempfile::tempdir().unwrap();
        let store = store(&temp);
        assert!(store.load_grant(&name()).unwrap().is_none());
    }

    #[test]
    fn operation_gc_keeps_500_completed_and_all_running_with_matching_replays() {
        let temp = tempfile::tempdir().unwrap();
        let store = store(&temp);
        for sequence in 0..505 {
            assert!(matches!(
                store
                    .claim_operation(&operation(sequence, OperationState::Succeeded))
                    .unwrap(),
                RequestClaim::Claimed
            ));
        }
        assert!(matches!(
            store
                .claim_operation(&operation(999, OperationState::Running))
                .unwrap(),
            RequestClaim::Claimed
        ));

        assert_eq!(store.gc_operations().unwrap(), 5);
        assert!(
            store
                .find_request("operator", "request-0")
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .find_request("operator", "request-504")
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .find_request("operator", "request-999")
                .unwrap()
                .is_some()
        );
        assert_eq!(
            json_paths(&store.root.join("operations")).unwrap().len(),
            501
        );
        assert_eq!(
            json_paths(&store.root.join("operations/requests"))
                .unwrap()
                .len(),
            501
        );
    }

    #[test]
    fn operation_gc_combines_horizon_age_floor_and_hard_limit() {
        let cutoff = "2026-08-08T00:00:00Z";
        assert!(!should_collect_completed(
            OPERATION_GC_HORIZON - 1,
            Some("2000-01-01T00:00:00Z"),
            cutoff
        ));
        assert!(should_collect_completed(
            OPERATION_GC_HORIZON,
            Some("2000-01-01T00:00:00Z"),
            cutoff
        ));
        assert!(!should_collect_completed(
            OPERATION_GC_HORIZON,
            Some("2026-08-08T00:00:00Z"),
            cutoff
        ));
        assert!(!should_collect_completed(
            OPERATION_GC_HORIZON,
            None,
            cutoff
        ));
        assert!(should_collect_completed(
            OPERATION_GC_HARD_LIMIT,
            Some("2099-01-01T00:00:00Z"),
            cutoff
        ));
    }

    #[test]
    fn operation_record_rejects_non_ulid_id() {
        let mut invalid = operation(1, OperationState::Running);
        invalid.op_id = "zzzzzzzzzzzzzzzzzzzzzzzzzz".into();
        assert!(invalid.validate().unwrap_err().contains("ULID"));
    }

    #[test]
    fn operation_record_requires_canonical_completed_timestamp_and_state_pairing() {
        let mut completed = operation(1, OperationState::Succeeded);
        completed.completed_at = None;
        assert!(completed.validate().unwrap_err().contains("must have"));

        completed.completed_at = Some("0".into());
        assert!(completed.validate().unwrap_err().contains("RFC3339"));

        completed.completed_at = Some("2000-01-01T00:00:00+00:00".into());
        assert!(completed.validate().unwrap_err().contains("canonical UTC"));

        let mut running = operation(2, OperationState::Running);
        running.completed_at = Some("2000-01-01T00:00:00Z".into());
        assert!(running.validate().unwrap_err().contains("must not have"));
    }

    #[test]
    fn operation_disappearing_during_scan_is_skipped() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("vanished.json");
        assert!(
            load_during_scan::<OperationRecord>(&missing)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn root_owned_store_outputs_are_reassigned_to_daemon_identity() {
        if unsafe { libc::geteuid() } != 0 {
            return;
        }
        use std::os::unix::fs::MetadataExt;
        let temp = tempfile::tempdir().unwrap();
        let store = StateStore::new(temp.path().join("state"), temp.path().join("legacy"))
            .with_owner(518, 518);
        store.ensure_layout().unwrap();
        store
            .save_instance(&InstanceRecord {
                schema: cosmix_nspawnd::core::INSTANCE_SCHEMA.into(),
                name: name(),
                desired: cosmix_nspawnd::core::DesiredState::Stopped,
                updated_at: "now".into(),
                last_operation: None,
            })
            .unwrap();
        let metadata = fs::metadata(store.named_path("instances", &name())).unwrap();
        assert_eq!((metadata.uid(), metadata.gid()), (518, 518));
    }
}
