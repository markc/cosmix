//! Walk `src/_tasks/*.spec.mix` and assert two CI guards:
//!
//! 1. **Parse**: every file parses in strict-data mode.
//! 2. **Path validity**: every string entry in the load-bearing
//!    scope-frame fields (`sources`, `allow_list`, `deny_list`) is
//!    repo-local, with the depth of validation depending on whether
//!    the entry is a glob:
//!    - **Always validated** (concrete and glob): no absolute paths,
//!      and no lexical `..` escape above `src/`. `/tmp/**` and
//!      `../**` are rejected even though they never resolve a file.
//!    - **Non-glob only**: target exists, and the canonicalized path
//!      stays under `src/` (catches symlink-based exits). Globs are
//!      allowed to match nothing — that's how new-file allowances
//!      are expressed today.
//!
//! These fields steer where future implementers — human or agent —
//! look and what they edit, so a stale or out-of-scope path will
//! mislead silently. The check is deliberately narrow: only the
//! three top-level keys, only string entries. Schema-discipline
//! beyond this is parked until a real consumer exists; see
//! `src/_tasks/README.md` for the authoritative convention.
//!
//! The directory may legitimately be empty (or missing) on a fresh
//! checkout — in that case the test is a silent no-op rather than a
//! failure.

use cosmix_config::load_mix_data;
use cosmix_mix::value::Value;
use std::path::{Path, PathBuf};

/// Resolve `src/_tasks/` from the crate manifest dir
/// (`src/crates/cosmix-lib-config/`).
fn tasks_dir() -> PathBuf {
    manifest_dir()
        .join("../../_tasks")
        .canonicalize()
        .unwrap_or_else(|_| manifest_dir().join("../../_tasks"))
}

/// Workspace `src/` root, the resolution base for path entries in
/// task specs.
fn src_root() -> PathBuf {
    manifest_dir()
        .join("../..")
        .canonicalize()
        .expect("workspace src/ root resolves")
}

fn is_glob(s: &str) -> bool {
    s.contains('*') || s.contains('?') || s.contains('[') || s.contains(']')
}

/// Lexical-only check: walk path components and return true if any
/// `..` would take resolution above the base. `foo/../bar` is fine
/// (depth 1 → 0 → 1); `../foo` and `foo/../../bar` escape.
fn escapes_lexically(rel: &Path) -> bool {
    let mut depth: i64 = 0;
    for component in rel.components() {
        match component {
            std::path::Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return true;
                }
            }
            std::path::Component::Normal(_) => depth += 1,
            std::path::Component::CurDir => {}
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                return true;
            }
        }
    }
    false
}

/// Validate every concrete string entry in `field` is repo-local and
/// (for non-glob entries) actually exists. Returns
/// `field[idx]: <path>: <reason>` strings for failing entries;
/// non-string entries are skipped.
///
/// Failure modes (a single entry hits at most one):
/// 1. **absolute path** — rejected for *any* entry, glob or not.
///    `Path::join` silently swallows the root for absolute paths,
///    and `_tasks/` paths are repo-local by contract.
/// 2. **escapes src/ scope frame (lexical)** — rejected for *any*
///    entry. Catches `../foo` and `src/../foo` regardless of whether
///    the target exists. Glob entries hit this too: `../**` is not
///    "no matches yet," it's a contract violation.
/// 3. **path does not exist** — canonicalize fails. Only applied to
///    non-glob entries; globs may legitimately match nothing on disk
///    (e.g. an `allow_list` that anticipates new files).
/// 4. **escapes via symlink** — canonicalized target sits outside
///    `src_root()` even though the lexical form looked clean.
///    Belt-and-suspenders against a directory in the path being a
///    symlink that exits the workspace.
fn check_path_field(map: &Value, field: &str, root: &Path) -> Vec<String> {
    let Value::Map(m) = map else {
        return Vec::new();
    };
    let Some(Value::List(items)) = m.get(field) else {
        return Vec::new();
    };
    let canonical_root = root
        .canonicalize()
        .expect("workspace src/ root canonicalizes");
    let mut failures = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let Value::String(s) = item else { continue };
        // Strip optional `#anchor` suffix used by markdown sources.
        let path_part = s.split('#').next().unwrap_or(s);
        if Path::new(path_part).is_absolute() {
            failures.push(format!(
                "{field}[{i}]: {s}: absolute paths not allowed in scope frames"
            ));
            continue;
        }
        // Workspace-relative entries start with `src/`; resolve against
        // `<repo>/src` by stripping the leading `src/`. Anything else
        // is treated as already-relative-to-src so authors don't have
        // to remember the prefix.
        let rel = path_part.strip_prefix("src/").unwrap_or(path_part);
        if escapes_lexically(Path::new(rel)) {
            failures.push(format!("{field}[{i}]: {s}: escapes src/ scope frame"));
            continue;
        }
        if is_glob(s) {
            // Structural checks passed; skip existence — globs may
            // legitimately match nothing yet.
            continue;
        }
        let resolved = root.join(rel);
        let canonical = match resolved.canonicalize() {
            Ok(c) => c,
            Err(_) => {
                failures.push(format!("{field}[{i}]: {s}: path does not exist"));
                continue;
            }
        };
        if !canonical.starts_with(&canonical_root) {
            failures.push(format!(
                "{field}[{i}]: {s}: escapes src/ scope frame via symlink"
            ));
        }
    }
    failures
}

#[test]
fn every_task_spec_mix_parses_in_strict_data_mode() {
    let dir = tasks_dir();
    if !dir.exists() {
        return;
    }
    let root = src_root();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(err) => panic!("failed to read {}: {}", dir.display(), err),
    };

    let mut checked = 0usize;
    let mut all_failures: Vec<String> = Vec::new();
    for entry in entries {
        let entry = entry.expect("read_dir entry");
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = match path.file_name().and_then(|s| s.to_str()) {
            Some(s) => s,
            None => continue,
        };
        if !name.ends_with(".spec.mix") {
            continue;
        }
        let value = load_mix_data(&path)
            .unwrap_or_else(|e| panic!("strict-data parse failed for {}: {e}", path.display()));

        for field in ["sources", "allow_list", "deny_list"] {
            for failure in check_path_field(&value, field, &root) {
                all_failures.push(format!("{name}: {failure}"));
            }
        }
        checked += 1;
    }

    if !all_failures.is_empty() {
        panic!(
            "task spec path-existence check failed:\n  {}",
            all_failures.join("\n  ")
        );
    }
    eprintln!("verified {checked} task spec(s) under {}", dir.display());
}

/// Run-time manifest directory rather than the `env!`-baked one: cargo exports
/// `CARGO_MANIFEST_DIR` into the test process, and that names the tree cargo is
/// actually running in, whereas `env!` records whichever tree last *compiled*
/// the binary. The two diverge when one `CARGO_TARGET_DIR` is shared across
/// several git worktrees of this repo — cargo writes workspace-relative paths
/// into its dep-info, so an artefact built in a sibling worktree is judged
/// fresh and rerun here, still pointing at that tree's fixtures. Falls back to
/// the compile-time value when the binary is run outside cargo.
fn manifest_dir() -> std::path::PathBuf {
    std::env::var_os("CARGO_MANIFEST_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")))
}
