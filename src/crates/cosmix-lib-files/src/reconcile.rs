//! The reconcile pass (§6): walk the corpus tree, diff it against the index, and
//! apply the changes. This is the correctness backstop the daemon runs on a timer
//! and after every settled watcher burst; it is synchronous and fully testable over
//! a tempdir corpus + an in-memory [`Store`].
//!
//! It is **read-only on the files** — it never mints an id (a passive scan must not,
//! §6; an id-less file becomes an `unmanaged` row). The single explicit authoring
//! path (S4 `filesd.save`) is what mints + writes.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::error::Result;
use crate::store::{DocRecord, Store};
use crate::{diff, ingest};

/// What a reconcile pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// New managed docs indexed.
    pub added: usize,
    /// Existing docs reprojected (content or id changed).
    pub changed: usize,
    /// Docs whose file vanished (tombstoned).
    pub removed: usize,
    /// Docs moved (id stable).
    pub renamed: usize,
    /// Copies (id already owned by another live file).
    pub duplicates: usize,
    /// Id-less files seen (awaiting adoption by the authoring path).
    pub unmanaged: usize,
    /// Syncthing conflict files currently present.
    pub conflicts: usize,
    /// Files that failed to ingest, as `(rel_path, error)`.
    pub errors: Vec<(String, String)>,
}

/// Walk `root`, diff against `store`, and apply. `now_ms` stamps every mutation.
pub fn reconcile(root: &Path, store: &mut Store, now_ms: i64) -> Result<ReconcileReport> {
    let mut docs = Vec::new();
    let mut conflict_files = Vec::new();
    walk(root, root, &mut docs, &mut conflict_files)?;

    let mut report = ReconcileReport::default();
    let mut disk = Vec::with_capacity(docs.len());
    let mut records: HashMap<String, DocRecord> = HashMap::new();
    for rel in &docs {
        match ingest::scan(root, rel) {
            Ok(rec) => {
                disk.push(diff::Entry {
                    rel_path: rel.clone(),
                    id: rec.id.clone(),
                    content_hash: rec.content_hash.clone(),
                    dup_of: None, // disk entries carry no keeper flag; the diff computes it
                });
                records.insert(rel.clone(), rec);
            }
            Err(e) => report.errors.push((rel.clone(), e.to_string())),
        }
    }

    let index = store.live_entries()?;
    // Apply each change independently: a single doc's store error is recorded and
    // skipped (like an ingest error), never aborting the whole pass — so one poisoned
    // doc can't freeze indexing of the rest of the corpus (the next pass retries it).
    macro_rules! applied {
        ($path:expr, $counter:ident, $op:expr) => {
            match $op {
                Ok(_) => report.$counter += 1,
                Err(e) => report.errors.push(($path.clone(), e.to_string())),
            }
        };
    }
    for change in diff::diff(&disk, &index) {
        match change {
            diff::Change::Added { rel_path } => {
                applied!(
                    rel_path,
                    added,
                    apply_record(store, &records, &rel_path, None, now_ms)
                )
            }
            diff::Change::Changed { rel_path } => {
                applied!(
                    rel_path,
                    changed,
                    apply_record(store, &records, &rel_path, None, now_ms)
                )
            }
            diff::Change::Unmanaged { rel_path } => {
                applied!(
                    rel_path,
                    unmanaged,
                    apply_record(store, &records, &rel_path, None, now_ms)
                )
            }
            diff::Change::DuplicateId { rel_path, keeper } => applied!(
                rel_path,
                duplicates,
                apply_record(store, &records, &rel_path, Some(keeper), now_ms)
            ),
            diff::Change::Removed { rel_path } => {
                applied!(rel_path, removed, store.tombstone(&rel_path, now_ms))
            }
            diff::Change::Renamed { from, to, .. } => {
                if let Some(rec) = records.get(&to) {
                    applied!(to, renamed, store.rename(&from, &to, rec, now_ms));
                }
            }
        }
    }

    // Conflict files (Q6): record current sightings, reconcile-down the resolved.
    let on_disk: HashSet<String> = conflict_files.into_iter().collect();
    for p in &on_disk {
        store.record_conflict(p, now_ms)?;
    }
    for existing in store.conflict_paths()? {
        if !on_disk.contains(&existing) {
            store.remove_conflict(&existing)?;
        }
    }
    report.conflicts = on_disk.len();

    Ok(report)
}

fn apply_record(
    store: &mut Store,
    records: &HashMap<String, DocRecord>,
    rel_path: &str,
    dup_of: Option<String>,
    now_ms: i64,
) -> Result<()> {
    let Some(rec) = records.get(rel_path) else {
        return Ok(());
    };
    if dup_of.is_some() {
        let mut rec = rec.clone();
        rec.dup_of = dup_of;
        store.upsert(&rec, now_ms)?;
    } else {
        store.upsert(rec, now_ms)?;
    }
    Ok(())
}

/// Recursively collect corpus files. `docs` gets `*.md` (excluding conflict copies,
/// editor temp files, and any dotfile/dotdir); `conflicts` gets Syncthing
/// `*.sync-conflict-*` sidecars. Symlinks are not followed (avoids loops).
fn walk(
    root: &Path,
    dir: &Path,
    docs: &mut Vec<String>,
    conflicts: &mut Vec<String>,
) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue; // dotfiles / dotdirs (incl. .git, .#emacs-lock, the index dir)
        }
        let path = entry.path();
        let ft = entry.file_type()?;
        if ft.is_dir() {
            walk(root, &path, docs, conflicts)?;
        } else if ft.is_file() {
            let rel = rel_path(root, &path);
            if name.contains(".sync-conflict-") {
                conflicts.push(rel);
            } else if name.ends_with(".md") && !is_editor_temp(&name) {
                docs.push(rel);
            }
        }
    }
    Ok(())
}

fn rel_path(root: &Path, path: &Path) -> String {
    // Join the path's Normal components with '/', so the rel_path is consistent
    // cross-platform and a literal backslash in a unix filename is never mangled
    // into a fake separator.
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => Some(s.to_string_lossy()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn is_editor_temp(name: &str) -> bool {
    name.ends_with('~') || name.ends_with(".swp") || name.ends_with(".tmp")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Corpus {
        dir: std::path::PathBuf,
    }
    impl Corpus {
        fn new() -> Self {
            let dir =
                std::env::temp_dir().join(format!("cosmix_files_recon_{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            Corpus { dir }
        }
        fn write(&self, rel: &str, body: &str) {
            let p = self.dir.join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(p, body).unwrap();
        }
        fn rm(&self, rel: &str) {
            std::fs::remove_file(self.dir.join(rel)).unwrap();
        }
        fn mv(&self, from: &str, to: &str) {
            std::fs::rename(self.dir.join(from), self.dir.join(to)).unwrap();
        }
    }
    impl Drop for Corpus {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn doc(id: &str, body: &str) -> String {
        format!("---\nid: {id}\n---\n{body}\n")
    }

    fn run(c: &Corpus, s: &mut Store, now: i64) -> ReconcileReport {
        reconcile(&c.dir, s, now).unwrap()
    }

    #[test]
    fn cold_start_indexes_managed_files() {
        let c = Corpus::new();
        c.write(
            "a.md",
            &doc("0190aaaa-bbbb-7ccc-8ddd-eeeeffff0001", "alpha"),
        );
        c.write(
            "sub/b.md",
            &doc("0190aaaa-bbbb-7ccc-8ddd-eeeeffff0002", "beta"),
        );
        let mut s = Store::open_in_memory("notes").unwrap();
        let r = run(&c, &mut s, 1);
        assert_eq!(r.added, 2);
        assert_eq!(s.live_entries().unwrap().len(), 2);
    }

    #[test]
    fn edit_reprojects_and_is_otherwise_idempotent() {
        let c = Corpus::new();
        c.write("a.md", &doc("0190aaaa-bbbb-7ccc-8ddd-eeeeffff0001", "v1"));
        let mut s = Store::open_in_memory("notes").unwrap();
        run(&c, &mut s, 1);
        // No change → nothing happens (restart-repair idempotency).
        let r2 = run(&c, &mut s, 2);
        assert_eq!((r2.added, r2.changed, r2.removed), (0, 0, 0));
        // Edit the body → exactly one Changed.
        c.write(
            "a.md",
            &doc("0190aaaa-bbbb-7ccc-8ddd-eeeeffff0001", "v2 longer body"),
        );
        let r3 = run(&c, &mut s, 3);
        assert_eq!(r3.changed, 1);
    }

    #[test]
    fn remove_tombstones() {
        let c = Corpus::new();
        c.write("a.md", &doc("0190aaaa-bbbb-7ccc-8ddd-eeeeffff0001", "a"));
        c.write("b.md", &doc("0190aaaa-bbbb-7ccc-8ddd-eeeeffff0002", "b"));
        let mut s = Store::open_in_memory("notes").unwrap();
        run(&c, &mut s, 1);
        c.rm("b.md");
        let r = run(&c, &mut s, 2);
        assert_eq!(r.removed, 1);
        assert_eq!(s.live_entries().unwrap().len(), 1);
    }

    #[test]
    fn copy_yields_duplicate_not_clobber() {
        let c = Corpus::new();
        let dup = doc("0190aaaa-bbbb-7ccc-8ddd-eeeeffff00cc", "shared");
        c.write("a.md", &dup);
        c.write("b.md", &dup); // a `cp` — same id on two live paths
        let mut s = Store::open_in_memory("notes").unwrap();
        let r = run(&c, &mut s, 1);
        assert_eq!(r.duplicates, 1);
        assert_eq!(s.live_entries().unwrap().len(), 2); // both survive
    }

    #[test]
    fn move_is_rename_not_remove_plus_add() {
        let c = Corpus::new();
        c.write("a.md", &doc("0190aaaa-bbbb-7ccc-8ddd-eeeeffff00aa", "x"));
        let mut s = Store::open_in_memory("notes").unwrap();
        run(&c, &mut s, 1);
        c.mv("a.md", "c.md"); // id stable, path moved
        let r = run(&c, &mut s, 2);
        assert_eq!((r.renamed, r.removed, r.added), (1, 0, 0));
        let paths: Vec<_> = s
            .live_entries()
            .unwrap()
            .into_iter()
            .map(|e| e.rel_path)
            .collect();
        assert_eq!(paths, vec!["c.md".to_string()]);
    }

    #[test]
    fn idless_file_is_unmanaged() {
        let c = Corpus::new();
        c.write("draft.md", "# heading\n\nno frontmatter\n");
        let mut s = Store::open_in_memory("notes").unwrap();
        let r = run(&c, &mut s, 1);
        assert_eq!(r.unmanaged, 1);
        let entries = s.live_entries().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, None);
    }

    #[test]
    fn sync_conflict_excluded_from_docs_recorded_then_reconciled_down() {
        let c = Corpus::new();
        c.write(
            "note.md",
            &doc("0190aaaa-bbbb-7ccc-8ddd-eeeeffff0001", "real"),
        );
        c.write(
            "note.sync-conflict-20260629-120000-ABCDEF1.md",
            &doc("0190aaaa-bbbb-7ccc-8ddd-eeeeffff0001", "conflict copy"),
        );
        let mut s = Store::open_in_memory("notes").unwrap();
        let r = run(&c, &mut s, 1);
        // The conflict copy is NOT a doc (so no id-collision), and IS recorded.
        assert_eq!(s.live_entries().unwrap().len(), 1);
        assert_eq!(r.conflicts, 1);
        assert_eq!(s.conflict_paths().unwrap().len(), 1);
        // Operator resolves it → reconcile-down removes the conflict row.
        c.rm("note.sync-conflict-20260629-120000-ABCDEF1.md");
        let r2 = run(&c, &mut s, 2);
        assert_eq!(r2.conflicts, 0);
        assert!(s.conflict_paths().unwrap().is_empty());
    }

    #[test]
    fn idempotent_with_unmanaged_and_duplicate_present() {
        // A corpus containing an id-less draft and a copy must reconcile to a
        // STEADY state: the second pass does nothing (no perpetual re-upsert churn).
        let c = Corpus::new();
        c.write("a.md", &doc("0190aaaa-bbbb-7ccc-8ddd-eeeeffff0001", "a"));
        c.write("draft.md", "no frontmatter\n");
        let dup = doc("0190aaaa-bbbb-7ccc-8ddd-eeeeffff00cc", "shared");
        c.write("orig.md", &dup);
        c.write("copy.md", &dup);
        let mut s = Store::open_in_memory("notes").unwrap();
        run(&c, &mut s, 1); // cold start
        let tip = s.current_modseq().unwrap();
        let r2 = run(&c, &mut s, 2);
        assert_eq!(
            (
                r2.added,
                r2.changed,
                r2.removed,
                r2.renamed,
                r2.duplicates,
                r2.unmanaged
            ),
            (0, 0, 0, 0, 0, 0)
        );
        // The no-op second pass must not advance the modseq tip (no churn).
        assert_eq!(s.current_modseq().unwrap(), tip);
    }

    #[test]
    fn keeper_transition_converges_and_is_stable() {
        // Add a lexicographically-lower copy of an existing keeper: the copy becomes
        // the keeper and the old one is demoted, then the corpus is stable (the
        // incremental projection matches a fresh rebuild — no perpetual churn).
        let c = Corpus::new();
        let body = doc("0190aaaa-bbbb-7ccc-8ddd-eeeeffff00cc", "shared");
        c.write("orig.md", &body);
        let mut s = Store::open_in_memory("notes").unwrap();
        run(&c, &mut s, 1); // orig.md is the sole keeper
        c.write("aaa.md", &body); // a lower-named identical copy appears
        let r2 = run(&c, &mut s, 2);
        assert_eq!(r2.added, 1); // aaa.md (new keeper)
        assert_eq!(r2.duplicates, 1); // orig.md demoted despite unchanged content
        let r3 = run(&c, &mut s, 3); // converged → stable
        assert_eq!(
            (r3.added, r3.changed, r3.removed, r3.duplicates),
            (0, 0, 0, 0)
        );
    }

    #[test]
    fn dotfiles_and_editor_temp_excluded() {
        let c = Corpus::new();
        c.write("real.md", &doc("0190aaaa-bbbb-7ccc-8ddd-eeeeffff0001", "r"));
        c.write(
            ".hidden.md",
            &doc("0190aaaa-bbbb-7ccc-8ddd-eeeeffff0002", "h"),
        );
        c.write("real.md~", "backup");
        c.write("scratch.md.tmp", "tmp");
        let mut s = Store::open_in_memory("notes").unwrap();
        run(&c, &mut s, 1);
        let paths: Vec<_> = s
            .live_entries()
            .unwrap()
            .into_iter()
            .map(|e| e.rel_path)
            .collect();
        assert_eq!(paths, vec!["real.md".to_string()]);
    }
}
