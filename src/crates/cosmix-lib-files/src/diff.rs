//! The pure reconcile diff (design §6): compare a disk scan against the current
//! index state and classify each file. This encodes the two review BLOCKER fixes —
//! a copy (`cp a.md b.md`) is a `DuplicateId`, not a clobber; an id-less file is
//! `Unmanaged` (a passive scan never mints — only the explicit authoring path does).

use std::collections::{HashMap, HashSet};

use serde::Serialize;

/// A snapshot entry — used for both the disk scan and the index snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Path within the corpus root.
    pub rel_path: String,
    /// Frontmatter id, if the file carries one.
    pub id: Option<String>,
    /// BLAKE3 hex of the whole file.
    pub content_hash: String,
    /// Index-only: the keeper this row is flagged as a copy of (`None` = it is the
    /// keeper, or it has no id). Ignored on disk entries; used so the diff can detect
    /// a stale keeper-role (a copy that should be promoted, or a keeper demoted) even
    /// when the file's content is unchanged.
    pub dup_of: Option<String>,
}

impl Entry {
    /// Convenience constructor (`dup_of` defaults to `None`).
    pub fn new(
        rel_path: impl Into<String>,
        id: Option<&str>,
        content_hash: impl Into<String>,
    ) -> Self {
        Entry {
            rel_path: rel_path.into(),
            id: id.map(str::to_string),
            content_hash: content_hash.into(),
            dup_of: None,
        }
    }

    /// Builder: set the `dup_of` keeper pointer (for index snapshots).
    pub fn with_dup_of(mut self, keeper: Option<&str>) -> Self {
        self.dup_of = keeper.map(str::to_string);
        self
    }
}

/// One reconcile action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Change {
    /// A new managed file (id present, not seen before).
    Added { rel_path: String },
    /// An existing path whose content hash changed.
    Changed { rel_path: String },
    /// An indexed path that is gone from disk (and not consumed by a rename).
    Removed { rel_path: String },
    /// A file whose id matches an indexed file whose old path is now gone. The
    /// daemon must re-read+re-hash the file on rename — content may have changed in
    /// the same operation, and `Renamed` alone does not imply an unchanged body.
    Renamed {
        from: String,
        to: String,
        id: String,
    },
    /// A copy: another live file (`keeper`, the lexicographically lowest path holding
    /// the id) is the canonical doc; this one is a duplicate. Informational only — the
    /// store enforces no id-uniqueness (two live files legitimately share an id after
    /// a `cp`/Syncthing copy), so a copy never errors. Like `Renamed`, a known path
    /// can become `DuplicateId` while its content also changed, so the daemon
    /// re-reads+re-hashes when handling it.
    DuplicateId { rel_path: String, keeper: String },
    /// An id-less file; a passive scan must not mint (the explicit authoring path
    /// does). Flagged for an operator/import action.
    Unmanaged { rel_path: String },
}

/// Compute the reconcile changes from a disk scan and the index snapshot.
///
/// Identity is resolved per-id by a deterministic **keeper** = the lexicographically
/// lowest path holding that id, computed the SAME way for the disk scan and the index
/// so classification is stable across passes. The store enforces no id-uniqueness
/// (two live files legitimately share an id after a copy), so the keeper only drives
/// classification — a copy becomes `DuplicateId`, never a constraint error. A keeper
/// path that is new (its id's index-keeper vanished) is a `Renamed`; otherwise `Added`.
pub fn diff(disk: &[Entry], index: &[Entry]) -> Vec<Change> {
    let disk_paths: HashSet<&str> = disk.iter().map(|e| e.rel_path.as_str()).collect();
    let index_paths: HashMap<&str, &Entry> =
        index.iter().map(|e| (e.rel_path.as_str(), e)).collect();
    let disk_keeper = min_path_by_id(disk);
    let index_keeper = min_path_by_id(index);

    let mut changes = Vec::new();
    let mut renamed_from: HashSet<String> = HashSet::new();

    for d in disk {
        // The keeper-role this path SHOULD have, given the current disk keeper set:
        // None if it's the keeper (or id-less), else the keeper's path.
        let expected_dup_of: Option<&str> = match d.id.as_deref() {
            None => None,
            Some(id) => match disk_keeper.get(id).map(String::as_str) {
                Some(k) if k == d.rel_path.as_str() => None,
                other => other,
            },
        };

        // A known path that is unchanged (same id AND hash) AND already has the
        // correct keeper-role is a no-op. The keeper-role check is essential: when an
        // id's keeper changes (a lower-named copy appears, or the keeper is deleted),
        // a content-unchanged sibling must still be reprojected to fix its `dup_of`,
        // or the incremental projection would drift from a rebuild (two `dup_of IS
        // NULL` rows, or zero) and the change stream would miss the promotion. The
        // unchanged+correct-role skip is what keeps an id-less/duplicate file from
        // re-emitting every pass.
        if let Some(existing) = index_paths.get(d.rel_path.as_str())
            && existing.content_hash == d.content_hash
            && existing.id == d.id
            && existing.dup_of.as_deref() == expected_dup_of
        {
            continue;
        }

        let Some(id) = d.id.as_deref() else {
            // No identity; a passive scan never mints (only the authoring path does).
            changes.push(Change::Unmanaged {
                rel_path: d.rel_path.clone(),
            });
            continue;
        };

        // Not the keeper for this id → a copy (informational; never errors).
        if disk_keeper.get(id).map(String::as_str) != Some(d.rel_path.as_str()) {
            changes.push(Change::DuplicateId {
                rel_path: d.rel_path.clone(),
                keeper: disk_keeper.get(id).cloned().unwrap_or_default(),
            });
            continue;
        }

        // d is the keeper for its id.
        if index_paths.contains_key(d.rel_path.as_str()) {
            // Known keeper path that changed (content or id) → reproject.
            changes.push(Change::Changed {
                rel_path: d.rel_path.clone(),
            });
        } else if let Some(old) = index_keeper
            .get(id)
            .filter(|p| !disk_paths.contains(p.as_str()))
        {
            // New keeper path + the id's index-keeper is gone from disk → rename.
            changes.push(Change::Renamed {
                from: old.clone(),
                to: d.rel_path.clone(),
                id: id.to_string(),
            });
            renamed_from.insert(old.clone());
        } else {
            changes.push(Change::Added {
                rel_path: d.rel_path.clone(),
            });
        }
    }

    // Removals: indexed paths gone from disk and not consumed by a rename.
    for ix in index {
        if !disk_paths.contains(ix.rel_path.as_str()) && !renamed_from.contains(&ix.rel_path) {
            changes.push(Change::Removed {
                rel_path: ix.rel_path.clone(),
            });
        }
    }

    changes
}

/// Map each id to the lexicographically lowest path holding it (the keeper).
fn min_path_by_id(entries: &[Entry]) -> HashMap<String, String> {
    let mut m: HashMap<String, String> = HashMap::new();
    for e in entries {
        if let Some(id) = &e.id {
            m.entry(id.clone())
                .and_modify(|cur| {
                    if e.rel_path < *cur {
                        *cur = e.rel_path.clone();
                    }
                })
                .or_insert_with(|| e.rel_path.clone());
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn added_changed_unchanged_removed() {
        let index = vec![
            Entry::new("keep.md", Some("id-keep"), "h1"),
            Entry::new("edit.md", Some("id-edit"), "h2"),
            Entry::new("gone.md", Some("id-gone"), "h3"),
        ];
        let disk = vec![
            Entry::new("keep.md", Some("id-keep"), "h1"), // unchanged
            Entry::new("edit.md", Some("id-edit"), "h2-NEW"), // changed
            Entry::new("new.md", Some("id-new"), "h4"),   // added
                                                          // gone.md absent → removed
        ];
        let mut got = diff(&disk, &index);
        got.sort_by_key(|c| format!("{c:?}"));
        assert_eq!(got, {
            let mut v = vec![
                Change::Changed {
                    rel_path: "edit.md".into(),
                },
                Change::Added {
                    rel_path: "new.md".into(),
                },
                Change::Removed {
                    rel_path: "gone.md".into(),
                },
            ];
            v.sort_by_key(|c| format!("{c:?}"));
            v
        });
    }

    #[test]
    fn rename_by_id_not_remove_plus_add() {
        let index = vec![Entry::new("old.md", Some("id-x"), "h1")];
        let disk = vec![Entry::new("new.md", Some("id-x"), "h1")];
        let got = diff(&disk, &index);
        assert_eq!(
            got,
            vec![Change::Renamed {
                from: "old.md".into(),
                to: "new.md".into(),
                id: "id-x".into()
            }]
        );
        // Crucially: no Removed for old.md.
        assert!(!got.iter().any(|c| matches!(c, Change::Removed { .. })));
    }

    #[test]
    fn copy_is_duplicate_not_clobber() {
        // Original still present AND a copy with the same id appears.
        let index = vec![Entry::new("a.md", Some("id-x"), "h1")];
        let disk = vec![
            Entry::new("a.md", Some("id-x"), "h1"), // original, unchanged
            Entry::new("b.md", Some("id-x"), "h1"), // copy
        ];
        let got = diff(&disk, &index);
        assert_eq!(
            got,
            vec![Change::DuplicateId {
                rel_path: "b.md".into(),
                keeper: "a.md".into()
            }]
        );
        // Original survives (no Removed, no clobber).
        assert!(!got.iter().any(|c| matches!(c, Change::Removed { .. })));
    }

    #[test]
    fn idless_file_is_unmanaged() {
        let index: Vec<Entry> = vec![];
        let disk = vec![Entry::new("draft.md", None, "h1")];
        assert_eq!(
            diff(&disk, &index),
            vec![Change::Unmanaged {
                rel_path: "draft.md".into()
            }]
        );
    }

    #[test]
    fn two_copies_of_an_unindexed_id_dedupe() {
        // First-ever scan of `cp a.md b.md`: the id is not in the index yet, but the
        // disk-wide keeper resolution must still emit exactly one Added + one
        // DuplicateId (never two Addeds, which would violate the live-id uniqueness).
        let index: Vec<Entry> = vec![];
        let disk = vec![
            Entry::new("a.md", Some("id-x"), "h1"),
            Entry::new("b.md", Some("id-x"), "h1"),
        ];
        assert_eq!(
            diff(&disk, &index),
            vec![
                Change::Added {
                    rel_path: "a.md".into()
                },
                Change::DuplicateId {
                    rel_path: "b.md".into(),
                    keeper: "a.md".into()
                },
            ]
        );
    }

    #[test]
    fn id_changed_in_place_is_changed_not_silent() {
        // A known path whose frontmatter id was edited (content hash unchanged) must
        // still be reprojected, not silently skipped.
        let index = vec![Entry::new("a.md", Some("id-1"), "h1")];
        let disk = vec![Entry::new("a.md", Some("id-2"), "h1")];
        assert_eq!(
            diff(&disk, &index),
            vec![Change::Changed {
                rel_path: "a.md".into()
            }]
        );
    }

    #[test]
    fn unchanged_unmanaged_and_duplicate_are_idempotent() {
        // A consistent steady state (id-less file + a correctly-flagged copy pair)
        // must produce NO changes on a re-run (no churn). The keeper is the lowest
        // path (copy.md < orig.md), so orig.md must carry dup_of = copy.md.
        let index = vec![
            Entry::new("draft.md", None, "h0"), // unmanaged, already indexed
            Entry::new("copy.md", Some("id-x"), "h1"), // keeper (lowest path)
            Entry::new("orig.md", Some("id-x"), "h1").with_dup_of(Some("copy.md")),
        ];
        let disk = index.clone();
        assert!(diff(&disk, &index).is_empty());
    }

    #[test]
    fn lower_named_copy_demotes_old_keeper() {
        // orig.md was the sole keeper; a lexicographically-lower copy appears. The
        // unchanged orig.md must still be reprojected (DuplicateId) to fix its role,
        // so the id ends with exactly one keeper (aaa.md) — incremental == rebuild.
        let index = vec![Entry::new("orig.md", Some("id-x"), "h1")]; // dup_of None (keeper)
        let disk = vec![
            Entry::new("aaa.md", Some("id-x"), "h1"),
            Entry::new("orig.md", Some("id-x"), "h1"),
        ];
        let mut got = diff(&disk, &index);
        got.sort_by_key(|c| format!("{c:?}"));
        let mut want = vec![
            Change::Added {
                rel_path: "aaa.md".into(),
            },
            Change::DuplicateId {
                rel_path: "orig.md".into(),
                keeper: "aaa.md".into(),
            },
        ];
        want.sort_by_key(|c| format!("{c:?}"));
        assert_eq!(got, want);
    }

    #[test]
    fn keeper_deletion_promotes_survivor() {
        // copy.md was keeper, orig.md a copy; copy.md is deleted. The surviving
        // orig.md (content unchanged) must be reprojected (Changed → clears dup_of)
        // so the id is not left with ZERO keepers, and the promotion hits the stream.
        let index = vec![
            Entry::new("copy.md", Some("id-x"), "h1"),
            Entry::new("orig.md", Some("id-x"), "h1").with_dup_of(Some("copy.md")),
        ];
        let disk = vec![Entry::new("orig.md", Some("id-x"), "h1")];
        let mut got = diff(&disk, &index);
        got.sort_by_key(|c| format!("{c:?}"));
        let mut want = vec![
            Change::Changed {
                rel_path: "orig.md".into(),
            },
            Change::Removed {
                rel_path: "copy.md".into(),
            },
        ];
        want.sort_by_key(|c| format!("{c:?}"));
        assert_eq!(got, want);
    }
}
