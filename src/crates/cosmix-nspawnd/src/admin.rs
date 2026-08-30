//! Root-only C0 state import. These commands read C0 evidence and write only
//! nspawnd's local canonical store; they never mutate the C0 placement record.

use std::fs;
use std::path::{Path, PathBuf};

use cosmix_nspawnd::core::{
    C0PlacementRecord, C0Tombstone, DesiredState, GRANT_SCHEMA, Grant, GrantSource,
    INSTANCE_SCHEMA, InstanceName, InstanceRecord, Tombstone,
};
use serde::Serialize;

use crate::lock::{LockError, LockHolder, LockManager};
use crate::store::{StateStore, StoreError, load_required};

#[derive(Debug, thiserror::Error)]
pub enum AdminError {
    #[error("admin import requires effective UID 0")]
    NotRoot,
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Lock(#[from] LockError),
    #[error("invalid C0 input at {path}: {message}")]
    Invalid { path: PathBuf, message: String },
}

#[derive(Serialize)]
struct LegacyImportMarker {
    schema: &'static str,
    imported_at: String,
    source_directory: String,
    imported_count: usize,
}

pub fn require_root() -> Result<(), AdminError> {
    if unsafe { libc::geteuid() } == 0 {
        Ok(())
    } else {
        Err(AdminError::NotRoot)
    }
}

pub fn import_c0_tombstones(store: &StateStore, locks: &LockManager) -> Result<usize, AdminError> {
    require_root()?;
    store.ensure_layout()?;
    let source = store.legacy_root();
    let entries = fs::read_dir(source).map_err(|error| AdminError::Invalid {
        path: source.to_path_buf(),
        message: error.to_string(),
    })?;
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| AdminError::Invalid {
            path: source.to_path_buf(),
            message: error.to_string(),
        })?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) == Some("json") {
            paths.push(path);
        }
    }
    paths.sort();
    let mut converted = Vec::new();
    for path in paths {
        let c0: C0Tombstone = load_required(&path)?;
        let stem = path
            .file_stem()
            .and_then(|value| value.to_str())
            .ok_or_else(|| AdminError::Invalid {
                path: path.clone(),
                message: "non-UTF-8 filename".into(),
            })?;
        let filename_name = InstanceName::parse(stem).map_err(|error| AdminError::Invalid {
            path: path.clone(),
            message: error.to_string(),
        })?;
        if filename_name != c0.name {
            return Err(AdminError::Invalid {
                path,
                message: "filename/name mismatch".into(),
            });
        }
        let canonical = Tombstone::try_from(c0).map_err(|message| AdminError::Invalid {
            path: path.clone(),
            message,
        })?;
        converted.push(canonical);
    }
    let imported = converted.len();
    for canonical in converted {
        let _lock = locks.acquire(
            &canonical.name,
            admin_holder("nspawnd.admin.import-c0-tombstone"),
        )?;
        store.merge_tombstone(canonical)?;
    }
    store.save_legacy_import_marker(&LegacyImportMarker {
        schema: "cosmix.nspawnd.legacy-import.v1",
        imported_at: now(),
        source_directory: source.display().to_string(),
        imported_count: imported,
    })?;
    Ok(imported)
}

pub fn import_c0_grant(
    store: &StateStore,
    locks: &LockManager,
    path: &Path,
    local_node: &str,
    legacy_absent_ok: bool,
) -> Result<Grant, AdminError> {
    require_root()?;
    store.ensure_layout()?;
    let record: C0PlacementRecord = load_required(path)?;
    let name = record.name.clone();
    let _lock = locks.acquire(&name, admin_holder("nspawnd.admin.import-c0-grant"))?;
    audit_legacy_state(store, legacy_absent_ok)?;
    let (grant, desired) = convert_c0_grant(record, path, local_node, now())?;
    let grant = store.merge_grant(grant)?;
    store.save_instance(&InstanceRecord {
        schema: INSTANCE_SCHEMA.into(),
        name,
        desired,
        updated_at: now(),
        last_operation: None,
    })?;
    Ok(grant)
}

fn audit_legacy_state(store: &StateStore, legacy_absent_ok: bool) -> Result<(), AdminError> {
    match fs::metadata(store.legacy_root()) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && legacy_absent_ok => {
            eprintln!(
                "legacy C0 tombstone directory {} absent — treating as no legacy state",
                store.legacy_root().display()
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(AdminError::Invalid {
                path: store.legacy_root().to_path_buf(),
                message: "legacy C0 tombstone directory is absent; pass --legacy-absent-ok to explicitly treat this as no legacy state".into(),
            });
        }
        Err(error) => {
            return Err(AdminError::Invalid {
                path: store.legacy_root().to_path_buf(),
                message: error.to_string(),
            });
        }
    }
    store.assert_legacy_import_complete()?;
    Ok(())
}

fn admin_holder(verb: &str) -> LockHolder {
    LockHolder {
        op_id: format!("admin-{}", ulid::Ulid::new()),
        verb: verb.into(),
        actor: "local:root".into(),
        pid: std::process::id(),
        started_at: now(),
    }
}

fn convert_c0_grant(
    record: C0PlacementRecord,
    path: &Path,
    local_node: &str,
    installed_at: String,
) -> Result<(Grant, DesiredState), AdminError> {
    if record.owner != local_node {
        return Err(invalid(
            path,
            format!(
                "record owner {:?} does not match local node {:?}",
                record.owner, local_node
            ),
        ));
    }
    if record.generation == 0 || record.version == 0 {
        return Err(invalid(path, "generation and version must be >= 1"));
    }
    if record.op.is_some() {
        return Err(invalid(path, "record has an open operation"));
    }
    if record.op_dst.is_some() {
        return Err(invalid(path, "record has a residual operation destination"));
    }
    if !matches!(record.state.as_str(), "complete" | "placed") {
        return Err(invalid(
            path,
            format!("record state {:?} is not complete or placed", record.state),
        ));
    }
    if !matches!(record.desired.as_str(), "running" | "stopped") {
        return Err(invalid(
            path,
            format!("invalid desired state {:?}", record.desired),
        ));
    }
    if record.owner.is_empty() || record.updated.is_empty() {
        return Err(invalid(path, "owner and updated must be non-empty"));
    }
    let desired = match record.desired.as_str() {
        "running" => DesiredState::Running,
        "stopped" => DesiredState::Stopped,
        _ => unreachable!("validated above"),
    };
    let grant = Grant {
        schema: GRANT_SCHEMA.into(),
        name: record.name,
        owner: record.owner,
        generation: record.generation,
        source: GrantSource {
            kind: "c0-placement".into(),
            record_version: record.version,
            record_state: record.state,
            record_updated: record.updated,
        },
        installed_at,
    };
    grant.validate().map_err(|message| AdminError::Invalid {
        path: path.to_path_buf(),
        message,
    })?;
    Ok((grant, desired))
}

fn invalid(path: &Path, message: impl Into<String>) -> AdminError {
    AdminError::Invalid {
        path: path.to_path_buf(),
        message: message.into(),
    }
}

fn now() -> String {
    cosmix_buildinfo::now_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_legacy_root_requires_explicit_override() {
        let temp = tempfile::tempdir().unwrap();
        let store = StateStore::new(temp.path().join("state"), temp.path().join("absent"));
        store.ensure_layout().unwrap();
        assert!(matches!(
            audit_legacy_state(&store, false),
            Err(AdminError::Invalid { .. })
        ));
        audit_legacy_state(&store, true).unwrap();
    }

    #[test]
    fn c0_placement_parser_rejects_unknown_fields_and_open_ops() {
        let open: C0PlacementRecord = serde_json::from_value(serde_json::json!({
            "name":"labspoke", "owner":"alpha", "generation":3,
            "desired":"running", "state":"migrating", "op":"open",
            "version":7, "updated":"now", "op_dst":"beta"
        }))
        .unwrap();
        assert_eq!(open.op.as_deref(), Some("open"));
        assert!(open.op_dst.is_some());
        assert!(
            serde_json::from_value::<C0PlacementRecord>(serde_json::json!({
                "name":"labspoke", "owner":"alpha", "generation":3,
                "desired":"running", "state":"complete", "op":null,
                "version":7, "updated":"now", "surprise":true
            }))
            .is_err()
        );
    }

    #[test]
    fn c0_grant_conversion_accepts_only_completed_local_authority() {
        let path = Path::new("record.json");
        let valid: C0PlacementRecord = serde_json::from_value(serde_json::json!({
            "name":"labspoke", "owner":"alpha", "generation":3,
            "desired":"running", "state":"complete", "op":null,
            "version":7, "updated":"now", "op_dst":null
        }))
        .unwrap();
        let (grant, desired) = convert_c0_grant(valid, path, "alpha", "installed".into()).unwrap();
        assert_eq!(grant.generation, 3);
        assert_eq!(grant.owner, "alpha");
        assert_eq!(desired, DesiredState::Running);

        let wrong_owner: C0PlacementRecord = serde_json::from_value(serde_json::json!({
            "name":"labspoke", "owner":"beta", "generation":3,
            "desired":"running", "state":"complete", "op":null,
            "version":7, "updated":"now"
        }))
        .unwrap();
        assert!(matches!(
            convert_c0_grant(wrong_owner, path, "alpha", "installed".into()),
            Err(AdminError::Invalid { .. })
        ));
    }
}
