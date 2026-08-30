//! `fsops` — the generic live-filesystem layer (filesd's SECOND capability, the
//! Dolphin-style file-manager backend), distinct from the markdown-corpus indexer.
//!
//! Design: `~/.cosmix/_plan/2026-06-30-dcs-dual-pane-file-manager.md`. Unlike the
//! corpus side (a SQLite projection of `*.md` frontmatter), this is **stateless live
//! `read_dir`/`stat` + atomic mutations** over a configured **places allowlist** — a
//! set of bounded roots the daemon may read (and, per-place, write). It is the only
//! place filesystem authority is enforced, so ALL scoping + `writable` checks live
//! here (the daemon adds the `$cosmix_delegation` admin gate on top).
//!
//! Security model (every op):
//! - A path is `"<place_id>/<rel>"`; the place id must be in the allowlist.
//! - `rel` is scoped against the place root by [`resolve_within`] — rejecting
//!   absolute/`.`/`..` AND symlink escape (canonicalise the deepest existing
//!   ancestor, require it under the canonical root). An EMPTY `rel` means the place
//!   root itself (the one case the corpus `is_safe_rel` rejects — hence the shared
//!   `allow_empty` resolver, not a verbatim copy of `safe_full`).
//! - `writable` is checked **per-verb, per-operand**: create/write → dest writable;
//!   copy → source readable + dest writable; move → both writable; trash/delete →
//!   source writable; restore → recorded origin writable. (A read-only place is a
//!   valid copy *source*.)
//! - Symlinks are *shown* (`is_symlink`) but never *followed out of* a place.

use std::fs::{self, Metadata};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::error::{FilesError, Result};

/// Resolve a root-relative `rel` to the absolute IO path, but ONLY if it is
/// guaranteed to stay within `root` — defeating textual `..`/absolute escapes AND
/// symlink traversal. The deepest existing ancestor is canonicalised and must remain
/// under the canonical root (the non-existent suffix can't contain a symlink, so the
/// prefix check suffices). `None` if unsafe.
///
/// `allow_empty`: when `true`, an empty `rel` resolves to the place root itself
/// (needed to list/stat a place root); when `false` it is rejected (the corpus
/// `filesd.*` contract — a corpus path is always a file under the root). With
/// `allow_empty = false` this is byte-for-byte the old `safe_full` (empty → reject,
/// non-Normal → reject, else join + ancestor-canon containment check).
///
/// **TOCTOU note (same posture as the shipped corpus `safe_full`).** This proves
/// containment at resolve time, but a subsequent path-based syscall could in principle
/// follow an ancestor a concurrent writer swaps to a symlink between the check and the
/// op. Two backstops bound this: (1) the daemon runs under a systemd sandbox whose
/// `ReadWritePaths`/`ReadOnlyPaths` cover ONLY the configured place roots + trash —
/// the kernel hard floor, so even a successful swap can't reach outside them; (2) the
/// places are operator-owned trees, so there is normally no adversarial concurrent
/// writer. Closing the residual window with `openat2(RESOLVE_BENEATH)` per component is
/// the SAME deferred hardening tracked for the corpus writer — see the daemon plan §9.
pub fn resolve_within(root: &Path, rel: &str, allow_empty: bool) -> Option<PathBuf> {
    if rel.is_empty() {
        if !allow_empty {
            return None;
        }
    } else if !Path::new(rel)
        .components()
        .all(|c| matches!(c, Component::Normal(_)))
    {
        return None;
    }
    let full = if rel.is_empty() {
        root.to_path_buf()
    } else {
        root.join(rel)
    };
    let root_canon = root.canonicalize().ok()?;
    let mut anc: &Path = &full;
    let existing = loop {
        match anc.canonicalize() {
            Ok(c) => break c,
            Err(_) => anc = anc.parent()?,
        }
    };
    existing.starts_with(&root_canon).then_some(full)
}

/// One configured place — a bounded root the file manager may browse (and, if
/// `writable`, mutate). `group`/`icon`/`order` are presentation hints for the
/// Places sidebar.
///
/// `allow`/`deny` are an OPTIONAL per-place path policy (both empty = today's
/// all-or-nothing subtree grant, byte-identical behaviour). When set, only paths
/// permitted by the policy are readable/writable/listable — the mechanism a
/// place rooted at a sensitive tree (e.g. `~/.ssh`) uses to expose a curated
/// subset (`hosts/*`, `config`, `keys/*.pub`) while hiding the rest
/// (private keys, `known_hosts`, `.git`). Enforced centrally in
/// [`FsLayer::resolve`] plus per-entry filters in list/search/tree. Patterns are
/// `/`-separated; each segment may contain `*` (matches any run of non-`/`
/// characters, INCLUDING a leading dot, so `hosts/*` covers `hosts/.ephemeral`).
#[derive(Debug, Clone)]
pub struct Place {
    pub id: String,
    pub label: String,
    pub group: String,
    pub icon: String,
    pub root: PathBuf,
    pub writable: bool,
    pub order: usize,
    /// Allow patterns; empty ⇒ unrestricted (subject to `deny`).
    pub allow: Vec<String>,
    /// Deny patterns; a match (exact or ancestor-prefix of the target) refuses
    /// the whole subtree. Evaluated before `allow`.
    pub deny: Vec<String>,
}

impl Place {
    /// True when this place carries a path policy (either list non-empty).
    fn policied(&self) -> bool {
        !self.allow.is_empty() || !self.deny.is_empty()
    }
}

/// Match one path SEGMENT against a glob pattern. `*` matches any run of
/// characters except `/` (including empty, and including a leading dot); no
/// `?`, `**`, or classes — deliberately minimal and dependency-free.
fn seg_match(pat: &str, name: &str) -> bool {
    let parts: Vec<&str> = pat.split('*').collect();
    if parts.len() == 1 {
        return pat == name; // no wildcard → exact
    }
    let mut pos = 0usize;
    let last = parts.len() - 1;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 {
            if !name[pos..].starts_with(part) {
                return false;
            }
            pos += part.len();
        } else if i == last {
            // Suffix must sit at or after the cursor.
            if name.len() < pos + part.len() || !name.ends_with(part) {
                return false;
            }
        } else {
            match name.get(pos..).and_then(|rest| rest.find(part)) {
                Some(idx) => pos += idx + part.len(),
                None => return false,
            }
        }
    }
    true
}

/// How a place-relative path relates to a place's allow/deny policy.
enum Access {
    /// Refused outright.
    Denied,
    /// An operable node (readable/writable file or dir).
    Node,
    /// A traversable ancestor of an allowed path — listable, but not itself a
    /// readable blob or a write target.
    Prefix,
}

/// Resolve a NON-EMPTY place-relative path against a policy. `deny` wins; then a
/// full-length allow match is a `Node`, a strict-ancestor allow match is a
/// `Prefix`, anything else is `Denied`. Empty `allow` ⇒ `Node` (subject to
/// `deny`). Callers handle the empty-rel (place-root) case separately.
fn policy_access(allow: &[String], deny: &[String], rel: &str) -> Access {
    let segs: Vec<&str> = rel.split('/').collect();
    for d in deny {
        let dsegs: Vec<&str> = d.split('/').collect();
        if dsegs.len() <= segs.len() && dsegs.iter().zip(&segs).all(|(dp, s)| seg_match(dp, s)) {
            return Access::Denied;
        }
    }
    if allow.is_empty() {
        return Access::Node;
    }
    for a in allow {
        let asegs: Vec<&str> = a.split('/').collect();
        if asegs.len() == segs.len() && asegs.iter().zip(&segs).all(|(ap, s)| seg_match(ap, s)) {
            return Access::Node;
        }
        if segs.len() < asegs.len() && segs.iter().zip(&asegs).all(|(s, ap)| seg_match(ap, s)) {
            return Access::Prefix;
        }
    }
    Access::Denied
}

/// Should a path at place-root-relative `rel_from_root` (with on-disk location
/// `entry_path`) be shown / traversed under this place's policy? For a policied
/// place: deny policy-excluded paths, and never advertise symlinks or
/// non-regular files. Non-policied places always return true.
fn path_visible(place: &Place, rel_from_root: &str, entry_path: &Path) -> bool {
    if !place.policied() {
        return true;
    }
    if rel_from_root.is_empty() {
        return true; // the place root itself
    }
    if matches!(
        policy_access(&place.allow, &place.deny, rel_from_root),
        Access::Denied
    ) {
        return false;
    }
    match fs::symlink_metadata(entry_path) {
        Ok(m) => {
            let ft = m.file_type();
            !ft.is_symlink() && (m.is_file() || m.is_dir())
        }
        Err(_) => false,
    }
}

/// [`path_visible`] for a directory ENTRY named `name` under parent place-rel
/// `parent_rel` (the list() case, where the child rel is composed from the two).
fn entry_visible(place: &Place, parent_rel: &str, name: &str, entry_path: &Path) -> bool {
    if !place.policied() {
        return true;
    }
    let child_rel = if parent_rel.is_empty() {
        name.to_string()
    } else {
        format!("{parent_rel}/{name}")
    };
    path_visible(place, &child_rel, entry_path)
}

/// The place-root-relative path of `p` (no place-id prefix), `""` for the root.
fn root_rel_of(place: &Place, p: &Path) -> String {
    match p.strip_prefix(&place.root) {
        Ok(rel) => rel.to_string_lossy().into_owned(),
        Err(_) => String::new(),
    }
}

/// Require that `p` is a plain regular file with exactly one hard link (not a
/// directory, symlink, hardlink, or special file). Used to vet copy/move
/// endpoints touching a policy-scoped place, where a non-plain source could
/// smuggle content across the policy boundary.
fn require_plain_file(p: &Path, label: &str) -> Result<()> {
    let m = fs::symlink_metadata(p)
        .map_err(|_| FilesError::NotFound(format!("source does not exist: {label}")))?;
    if m.file_type().is_symlink() {
        return Err(FilesError::BadRequest(format!(
            "source is a symlink: {label}"
        )));
    }
    if !m.is_file() {
        return Err(FilesError::BadRequest(format!(
            "policy-scoped copy/move requires a regular file: {label}"
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if m.nlink() > 1 {
            return Err(FilesError::BadRequest(format!(
                "source is a hardlinked file: {label}"
            )));
        }
    }
    Ok(())
}

/// The live-filesystem layer: the places allowlist + the trash root. Immutable
/// after construction (stateless — the filesystem is the only mutable state), so an
/// `Arc<FsLayer>` is freely shared across the daemon's async handlers.
#[derive(Debug, Clone)]
pub struct FsLayer {
    places: Vec<Place>,
    trash_root: PathBuf,
}

impl FsLayer {
    pub fn new(places: Vec<Place>, trash_root: PathBuf) -> Self {
        FsLayer { places, trash_root }
    }

    fn place(&self, id: &str) -> Option<&Place> {
        self.places.iter().find(|p| p.id == id)
    }

    /// Split `"<place_id>/<rel>"` into the place + the resolved absolute path,
    /// enforcing the allowlist, the scoping, and (when `need_write`) `writable`.
    ///
    /// A `need_write` operand with an EMPTY rel (the place root itself) is refused —
    /// otherwise `fs.delete("home")` / `fs.trash("home")` / a move onto `"home"`
    /// would destroy or relocate the whole configured root. Reads still allow the
    /// empty rel (list/stat a place root).
    fn resolve(&self, place_rel: &str, need_write: bool) -> Result<(&Place, PathBuf)> {
        let (id, rel) = match place_rel.split_once('/') {
            Some((id, rel)) => (id, rel),
            None => (place_rel, ""),
        };
        let place = self
            .place(id)
            .ok_or_else(|| FilesError::Denied(format!("unknown place: {id:?}")))?;
        if need_write {
            if !place.writable {
                return Err(FilesError::Denied(format!(
                    "place {:?} is read-only",
                    place.id
                )));
            }
            if rel.is_empty() {
                return Err(FilesError::Denied(format!(
                    "refusing to mutate the place root {:?}",
                    place.id
                )));
            }
        }
        let full = resolve_within(&place.root, rel, true)
            .ok_or_else(|| FilesError::Denied(format!("path escapes place {:?}", place.id)))?;
        // Per-place path policy (policied places only). resolve_within has
        // already pinned the path inside the root and canonicalized ancestors;
        // this additionally (a) confines the path to the allow/deny policy and
        // (b) lstat-guards the FINAL component so an in-root symlink, hardlink,
        // or non-regular file can't smuggle sensitive bytes (e.g. a symlink or
        // hardlink named `hosts/x` that points at `id_ed25519`, or a FIFO that
        // would block a blob read forever).
        if place.policied() {
            if !rel.is_empty() {
                match policy_access(&place.allow, &place.deny, rel) {
                    Access::Denied => {
                        return Err(FilesError::Denied(format!(
                            "path outside place policy: {place_rel}"
                        )));
                    }
                    Access::Prefix => {
                        if need_write {
                            return Err(FilesError::Denied(format!(
                                "path is a policy prefix, not a write target: {place_rel}"
                            )));
                        }
                    }
                    Access::Node => {}
                }
            }
            // Walk EVERY existing component from the root down: an in-root
            // ancestor symlink (e.g. `hosts` → a sibling dir) would keep the
            // path textually inside the allow policy while the bytes come from a
            // denied location, and resolve_within's ancestor-canonicalization
            // alone would still pass it. Reject a symlink at any level; reject a
            // hardlinked or non-regular FINAL file.
            let mut cur = place.root.clone();
            for seg in rel.split('/').filter(|s| !s.is_empty()) {
                cur.push(seg);
                let m = match fs::symlink_metadata(&cur) {
                    Ok(m) => m,
                    // NotFound ⇒ this component (and the rest) don't exist yet (a
                    // create path); any OTHER error fails closed.
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
                    Err(_) => {
                        return Err(FilesError::Denied(format!(
                            "cannot stat path component in a policy-scoped place: {place_rel}"
                        )));
                    }
                };
                if m.file_type().is_symlink() {
                    return Err(FilesError::Denied(format!(
                        "symlink component in a policy-scoped place: {place_rel}"
                    )));
                }
                let is_final = cur == full;
                if is_final {
                    if !m.is_file() && !m.is_dir() {
                        return Err(FilesError::Denied(format!(
                            "non-regular file in a policy-scoped place: {place_rel}"
                        )));
                    }
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::MetadataExt;
                        if m.is_file() && m.nlink() > 1 {
                            return Err(FilesError::Denied(format!(
                                "hardlinked file in a policy-scoped place: {place_rel}"
                            )));
                        }
                    }
                }
            }
        }
        Ok((place, full))
    }

    // ── reads ────────────────────────────────────────────────────────────────

    /// The Places sidebar source.
    pub fn places(&self) -> Value {
        let arr: Vec<Value> = self
            .places
            .iter()
            .map(|p| {
                json!({
                    "id": p.id, "label": p.label, "group": p.group, "icon": p.icon,
                    "root": p.root.display().to_string(), "writable": p.writable, "order": p.order,
                })
            })
            .collect();
        json!({ "places": arr })
    }

    /// Live directory listing (one `read_dir` + a `stat` per entry).
    pub fn list(&self, place_rel: &str, show_hidden: bool, sort: &str, dir: &str) -> Result<Value> {
        let (place, full) = self.resolve(place_rel, false)?;
        let md = fs::metadata(&full)?;
        if !md.is_dir() {
            return Err(FilesError::BadRequest(format!(
                "not a directory: {place_rel}"
            )));
        }
        let mut entries: Vec<Value> = Vec::new();
        let (mut folder_count, mut file_count, mut total_bytes) = (0u64, 0u64, 0u64);
        let parent_rel = place_rel.split_once('/').map(|(_, r)| r).unwrap_or("");
        for ent in fs::read_dir(&full)? {
            let ent = ent?;
            let name = ent.file_name().to_string_lossy().into_owned();
            let is_hidden = name.starts_with('.');
            if is_hidden && !show_hidden {
                continue;
            }
            let p = ent.path();
            // Policy-scoped places: never advertise a denied path, symlink, or
            // non-regular entry (matches what resolve() would refuse on access).
            if !entry_visible(place, parent_rel, &name, &p) {
                continue;
            }
            // symlink_metadata never follows; then a follow for type/size where it's
            // a symlink (so a symlink-to-dir lists as a navigable dir, but escape is
            // still blocked at navigation time by resolve_within).
            let lmeta = fs::symlink_metadata(&p).ok();
            let is_symlink = lmeta
                .as_ref()
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false);
            let meta = fs::metadata(&p).ok().or(lmeta);
            let is_dir = meta.as_ref().map(Metadata::is_dir).unwrap_or(false);
            let size = if is_dir {
                0
            } else {
                meta.as_ref().map(Metadata::len).unwrap_or(0)
            };
            let item_count = if is_dir { shallow_count(&p) } else { None };
            if is_dir {
                folder_count += 1;
            } else {
                file_count += 1;
                total_bytes += size;
            }
            entries.push(json!({
                "name": name,
                "is_dir": is_dir,
                "is_symlink": is_symlink,
                "is_hidden": is_hidden,
                "size": size.to_string(),
                "item_count": item_count,
                "mtime": meta.as_ref().and_then(time_secs(Metadata::modified)),
                "mode": meta.as_ref().map(mode_bits).unwrap_or(0),
                "mime": if is_dir { Value::Null } else { json!(mime_for(&name)) },
            }));
        }
        sort_entries(&mut entries, sort, dir);
        let parent = parent_place_rel(place, place_rel);
        Ok(json!({
            "path": place_rel,
            "parent": parent,
            "place": place.id,
            "writable": place.writable,
            "entries": entries,
            "folder_count": folder_count,
            "file_count": file_count,
            "total_bytes": total_bytes.to_string(),
        }))
    }

    /// Rich stat for the Information panel.
    pub fn stat(&self, place_rel: &str) -> Result<Value> {
        let (_place, full) = self.resolve(place_rel, false)?;
        let lmeta = fs::symlink_metadata(&full)?;
        let is_symlink = lmeta.file_type().is_symlink();
        let meta = fs::metadata(&full).unwrap_or(lmeta);
        let is_dir = meta.is_dir();
        let (item_count, hidden_item_count) = if is_dir {
            dir_counts(&full)
        } else {
            (None, None)
        };
        let target = if is_symlink {
            fs::read_link(&full).ok().map(|t| t.display().to_string())
        } else {
            None
        };
        Ok(json!({
            "path": place_rel,
            "type": if is_dir { "dir" } else { "file" },
            "size": meta.len().to_string(),
            "item_count": item_count,
            "hidden_item_count": hidden_item_count,
            "modified": meta.modified().ok().and_then(systime_secs),
            "accessed": meta.accessed().ok().and_then(systime_secs),
            "created": meta.created().ok().and_then(systime_secs),
            "mode": mode_bits(&meta),
            "is_symlink": is_symlink,
            "target": target,
            "mime": if is_dir { Value::Null } else { json!(mime_for(file_name(place_rel))) },
        }))
    }

    /// Text preview for the editor/preview pane. Reads up to `max` bytes (default
    /// 1 MiB, cap 4 MiB). UTF-8 → `text`; otherwise `binary:true` (a real
    /// blob/thumbnail fetch is a later phase — base64 is intentionally not pulled in
    /// yet).
    pub fn read_blob(&self, place_rel: &str, max: u64) -> Result<Value> {
        let (_place, full) = self.resolve(place_rel, false)?;
        let md = fs::metadata(&full)?;
        if md.is_dir() {
            return Err(FilesError::BadRequest(
                "read_blob: path is a directory".into(),
            ));
        }
        let cap = max.clamp(1, 4 * 1024 * 1024);
        let size = md.len();
        let want = size.min(cap) as usize;
        let bytes = read_capped(&full, want)?;
        let truncated = size > bytes.len() as u64;
        let value = match std::str::from_utf8(&bytes) {
            Ok(s) => json!({
                "text": s, "binary": false, "truncated": truncated,
                "size": size.to_string(), "mime": mime_for(file_name(place_rel)),
            }),
            Err(_) => json!({
                "binary": true, "truncated": truncated,
                "size": size.to_string(), "mime": mime_for(file_name(place_rel)),
            }),
        };
        Ok(value)
    }

    /// Recursive (case-insensitive substring) filename search under a directory.
    pub fn search(
        &self,
        place_rel: &str,
        query: &str,
        recursive: bool,
        limit: usize,
    ) -> Result<Value> {
        if query.is_empty() {
            return Err(FilesError::BadRequest("search: empty query".into()));
        }
        let (place, base) = self.resolve(place_rel, false)?;
        let needle = query.to_lowercase();
        let limit = limit.clamp(1, 5000);
        let mut hits: Vec<Value> = Vec::new();
        let mut truncated = false;
        let mut stack = vec![base.clone()];
        while let Some(dir) = stack.pop() {
            let rd = match fs::read_dir(&dir) {
                Ok(rd) => rd,
                Err(_) => continue, // unreadable subdir — skip, don't abort
            };
            for ent in rd.flatten() {
                let name = ent.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') {
                    continue;
                }
                let p = ent.path();
                // Policy-scoped places: skip denied paths, symlinks, non-regular
                // files — both as hits and as recursion targets.
                if !path_visible(place, &root_rel_of(place, &p), &p) {
                    continue;
                }
                let is_dir = p.is_dir();
                if name.to_lowercase().contains(&needle) {
                    if hits.len() >= limit {
                        truncated = true;
                        break;
                    }
                    let rel = abs_to_place_rel(place, &p);
                    hits.push(json!({ "name": name, "is_dir": is_dir, "path": rel }));
                }
                if recursive && is_dir && !p.is_symlink() {
                    stack.push(p);
                }
            }
            if truncated {
                break;
            }
        }
        Ok(json!({ "entries": hits, "truncated": truncated }))
    }

    /// Recursive **folders-only** tree under a place path (depth + node capped) —
    /// the source for the file-manager's LHS folder tree. Returns folder place-rels
    /// (sorted); the web layer builds the parent→child adjacency. Skips dotdirs and
    /// symlinked dirs (loop-safe).
    pub fn tree(&self, place_rel: &str, max_depth: usize, max_nodes: usize) -> Result<Value> {
        let (place_ref, base) = self.resolve(place_rel, false)?;
        let root_rel = place_rel.trim_end_matches('/').to_string();
        // Total directory ENTRIES examined across the whole walk — bounds the cost
        // for a place holding a directory full of (non-folder) files, which the
        // folder-count cap alone would let scan to exhaustion.
        let max_scan = max_nodes.saturating_mul(200).clamp(10_000, 500_000);
        let mut folders: Vec<String> = Vec::new();
        let mut truncated = false;
        let mut scanned = 0usize;
        let mut stack: Vec<(PathBuf, String, usize)> = vec![(base, root_rel, 0)];
        while let Some((dir, rel, depth)) = stack.pop() {
            if depth >= max_depth {
                continue;
            }
            let rd = match fs::read_dir(&dir) {
                Ok(rd) => rd,
                Err(_) => continue,
            };
            for ent in rd.flatten() {
                scanned += 1;
                if scanned > max_scan || folders.len() >= max_nodes {
                    truncated = true;
                    break;
                }
                let name = ent.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') {
                    continue;
                }
                let p = ent.path();
                let is_symlink = fs::symlink_metadata(&p)
                    .map(|m| m.file_type().is_symlink())
                    .unwrap_or(true);
                if is_symlink || !p.is_dir() {
                    continue;
                }
                // Policy-scoped places: don't expose denied folders in the tree.
                if !path_visible(place_ref, &root_rel_of(place_ref, &p), &p) {
                    continue;
                }
                let child_rel = format!("{rel}/{name}");
                folders.push(child_rel.clone());
                stack.push((p, child_rel, depth + 1));
            }
            if truncated {
                break;
            }
        }
        folders.sort();
        Ok(json!({ "folders": folders, "truncated": truncated }))
    }

    // ── mutations ────────────────────────────────────────────────────────────

    pub fn mkdir(&self, place_rel: &str, parents: bool) -> Result<Value> {
        let (_place, full) = self.resolve(place_rel, true)?;
        if full.exists() {
            return Err(FilesError::Exists(format!("already exists: {place_rel}")));
        }
        if parents {
            fs::create_dir_all(&full)?;
        } else {
            fs::create_dir(&full)?;
        }
        Ok(json!({ "ok": true, "path": place_rel }))
    }

    pub fn touch(&self, place_rel: &str) -> Result<Value> {
        let (_place, full) = self.resolve(place_rel, true)?;
        if full.exists() {
            return Err(FilesError::Exists(format!("already exists: {place_rel}")));
        }
        ensure_parent(&full)?;
        // create_new is the atomic no-clobber primitive (TOCTOU-safe).
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&full)?;
        Ok(json!({ "ok": true, "path": place_rel }))
    }

    pub fn write(&self, place_rel: &str, bytes: &[u8], overwrite: bool) -> Result<Value> {
        let (_place, full) = self.resolve(place_rel, true)?;
        if full.is_dir() {
            return Err(FilesError::BadRequest(
                "write: target is a directory".into(),
            ));
        }
        if !overwrite && full.exists() {
            return Err(FilesError::Exists(format!(
                "already exists (overwrite=false): {place_rel}"
            )));
        }
        ensure_parent(&full)?;
        crate::atomic::write_atomic(&full, bytes)?;
        Ok(json!({ "ok": true, "path": place_rel }))
    }

    pub fn copy(&self, from_pr: &str, to_pr: &str, overwrite: bool) -> Result<Value> {
        // copy needs only READ on the source (a read-only place is a valid source)
        // + WRITE on the destination.
        let (fp, from) = self.resolve(from_pr, false)?;
        let (tp, to) = self.resolve(to_pr, true)?;
        // When EITHER endpoint is policy-scoped, the source must be a plain
        // regular, single-link file: a directory (copy_recursive preserves
        // symlinks), a symlink, or a hardlink could smuggle out-of-policy content
        // into — or a private key out of — a scoped tree. resolve() already
        // vetted the source's own path; this vets its type from an unpolicied
        // source too.
        if fp.policied() || tp.policied() {
            require_plain_file(&from, from_pr)?;
        }
        if !from.exists() {
            return Err(FilesError::NotFound(format!(
                "source does not exist: {from_pr}"
            )));
        }
        if !overwrite && to.exists() {
            return Err(FilesError::Exists(format!(
                "destination exists (overwrite=false): {to_pr}"
            )));
        }
        reject_self_or_descendant(&from, &to)?; // no copy-into-self / onto-self
        ensure_parent(&to)?;
        copy_recursive(&from, &to)?;
        Ok(json!({ "ok": true, "from": from_pr, "to": to_pr }))
    }

    pub fn move_(&self, from_pr: &str, to_pr: &str, overwrite: bool) -> Result<Value> {
        // move REMOVES the source → both operands must be writable.
        let (fp, from) = self.resolve(from_pr, true)?;
        let (tp, to) = self.resolve(to_pr, true)?;
        // Same source vetting as copy: a policy-scoped endpoint accepts only a
        // plain regular single-link file (no dir/symlink/hardlink smuggling).
        if fp.policied() || tp.policied() {
            require_plain_file(&from, from_pr)?;
        }
        if !from.exists() {
            return Err(FilesError::NotFound(format!(
                "source does not exist: {from_pr}"
            )));
        }
        if !overwrite && to.exists() {
            return Err(FilesError::Exists(format!(
                "destination exists (overwrite=false): {to_pr}"
            )));
        }
        reject_self_or_descendant(&from, &to)?; // no move-into-self / onto-self
        ensure_parent(&to)?;
        move_path(&from, &to)?;
        Ok(json!({ "ok": true, "from": from_pr, "to": to_pr }))
    }

    pub fn delete(&self, place_rel: &str, recursive: bool) -> Result<Value> {
        let (place, full) = self.resolve(place_rel, true)?;
        let md = match fs::symlink_metadata(&full) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(json!({ "ok": true, "path": place_rel })); // idempotent
            }
            Err(e) => return Err(FilesError::Io(e)),
        };
        // In a policy-scoped place, recursive directory delete would remove
        // children the policy never permitted operating on individually. Single
        // files only — the sshm surface never deletes trees.
        if place.policied() && md.is_dir() {
            return Err(FilesError::BadRequest(
                "directory delete is not supported in a policy-scoped place".into(),
            ));
        }
        if md.is_dir() {
            if !recursive {
                return Err(FilesError::BadRequest(
                    "delete: directory needs recursive=true".into(),
                ));
            }
            fs::remove_dir_all(&full)?;
        } else {
            fs::remove_file(&full)?;
        }
        Ok(json!({ "ok": true, "path": place_rel }))
    }

    // ── trash (reversible) ─────────────────────────────────────────────────────

    fn trash_files(&self) -> PathBuf {
        self.trash_root.join("files")
    }
    fn trash_info(&self) -> PathBuf {
        self.trash_root.join("info")
    }

    /// Move a path into the trash (reversible). Records the origin `<place>/<rel>` in
    /// a `.trashinfo` sidecar so restore can re-scope it.
    pub fn trash(&self, place_rel: &str) -> Result<Value> {
        let (place, full) = self.resolve(place_rel, true)?;
        if !full.exists() {
            return Err(FilesError::NotFound(format!("does not exist: {place_rel}")));
        }
        // A directory in a policy-scoped place could carry denied children into
        // (and back out of) the trash; refuse it (single files only).
        if place.policied() && full.is_dir() {
            return Err(FilesError::BadRequest(
                "directory trash is not supported in a policy-scoped place".into(),
            ));
        }
        fs::create_dir_all(self.trash_files())?;
        fs::create_dir_all(self.trash_info())?;
        let base = file_name(place_rel);
        let token = unique_token(&self.trash_files(), base);
        let dest = self.trash_files().join(&token);
        move_path(&full, &dest)?;
        let info = format!(
            "[Trash Info]\nOriginPlaceRel={}\nName={}\nDeletionDate={}\n",
            place_rel,
            base,
            now_rfc3339()
        );
        let info_path = self.trash_info().join(format!("{token}.trashinfo"));
        if let Err(e) = crate::atomic::write_atomic(&info_path, info.as_bytes()) {
            // Roll the file back out of trash so we never orphan it without metadata.
            let _ = move_path(&dest, &full);
            return Err(e);
        }
        Ok(json!({ "ok": true, "token": token, "origin": place_rel }))
    }

    pub fn trash_list(&self) -> Result<Value> {
        let dir = self.trash_info();
        let mut items: Vec<Value> = Vec::new();
        if let Ok(rd) = fs::read_dir(&dir) {
            for ent in rd.flatten() {
                let fname = ent.file_name().to_string_lossy().into_owned();
                let Some(token) = fname.strip_suffix(".trashinfo") else {
                    continue;
                };
                let body = fs::read_to_string(ent.path()).unwrap_or_default();
                items.push(json!({
                    "token": token,
                    "name": info_field(&body, "Name").unwrap_or_else(|| token.to_string()),
                    "origin": info_field(&body, "OriginPlaceRel"),
                    "deleted_at": info_field(&body, "DeletionDate"),
                }));
            }
        }
        Ok(json!({ "items": items }))
    }

    /// Restore a trashed entry to its recorded origin — re-validating that the origin
    /// resolves to a WRITABLE place (the security check) and never clobbering.
    pub fn trash_restore(&self, token: &str) -> Result<Value> {
        // The token must be ONE normal path component — rejects "", ".", "..",
        // separators, and any traversal, while still accepting a real filename that
        // merely *contains* dots (e.g. "a..b"). This blocks token "." addressing the
        // trash `files/` dir itself, and "../x" escaping the trash root.
        if !is_single_component(token) {
            return Err(FilesError::BadRequest("restore: bad token".into()));
        }
        let info_path = self.trash_info().join(format!("{token}.trashinfo"));
        let body = fs::read_to_string(&info_path)
            .map_err(|_| FilesError::NotFound(format!("no such trash item: {token}")))?;
        let origin = info_field(&body, "OriginPlaceRel")
            .ok_or_else(|| FilesError::Malformed("trashinfo missing OriginPlaceRel".into()))?;
        // Re-scope the origin against a WRITABLE place — the load-bearing check.
        let (place, dest) = self.resolve(&origin, true)?;
        if dest.exists() {
            return Err(FilesError::Exists(format!(
                "restore target exists: {origin}"
            )));
        }
        let src = self.trash_files().join(token);
        if !src.exists() {
            return Err(FilesError::NotFound(format!("trashed file gone: {token}")));
        }
        // Restoring into a policy-scoped place: the trashed item must be a plain
        // regular single-link file (`src.is_dir()` would follow a symlink; a
        // trashed symlink/hardlink/dir/special could reinstate never-validated
        // content). A directory can't normally be trashed from a policied place,
        // but a roster change between trash and restore could route one here.
        if place.policied() {
            require_plain_file(&src, token)?;
        }
        ensure_parent(&dest)?;
        move_path(&src, &dest)?;
        let _ = fs::remove_file(&info_path);
        Ok(json!({ "ok": true, "path": origin }))
    }

    /// Permanently empty the trash (irreversible — the daemon gates this behind an
    /// extra confirm token).
    pub fn trash_empty(&self) -> Result<Value> {
        for d in [self.trash_files(), self.trash_info()] {
            if let Ok(rd) = fs::read_dir(&d) {
                for ent in rd.flatten() {
                    let p = ent.path();
                    let r = if p.is_dir() {
                        fs::remove_dir_all(&p)
                    } else {
                        fs::remove_file(&p)
                    };
                    r?;
                }
            }
        }
        Ok(json!({ "ok": true }))
    }
}

// ── free helpers ──────────────────────────────────────────────────────────────

fn ensure_parent(full: &Path) -> Result<()> {
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent)?;
    }
    Ok(())
}

/// True iff `s` is exactly one `Normal` path component (no `.`/`..`/separators/empty).
fn is_single_component(s: &str) -> bool {
    let mut comps = Path::new(s).components();
    matches!(
        (comps.next(), comps.next()),
        (Some(Component::Normal(_)), None)
    )
}

/// Reject a copy/move whose destination IS the source or sits INSIDE it (which would
/// recurse into itself / clobber the source). Compares canonical paths: `from` exists
/// (the caller checked), and `to` is canonicalised via its deepest existing ancestor.
fn reject_self_or_descendant(from: &Path, to: &Path) -> Result<()> {
    let from_c = from.canonicalize()?;
    let to_c = canonicalize_target(to);
    if to_c == from_c || to_c.starts_with(&from_c) {
        return Err(FilesError::BadRequest(
            "destination is the source or inside it".into(),
        ));
    }
    Ok(())
}

/// Canonicalise a (possibly not-yet-existing) target: if it exists, canonicalise it;
/// else canonicalise its parent and re-join the file name (the suffix can't be a
/// symlink because it doesn't exist).
fn canonicalize_target(to: &Path) -> PathBuf {
    if let Ok(c) = to.canonicalize() {
        return c;
    }
    match (to.parent(), to.file_name()) {
        (Some(p), Some(n)) => p
            .canonicalize()
            .map(|pc| pc.join(n))
            .unwrap_or_else(|_| to.to_path_buf()),
        _ => to.to_path_buf(),
    }
}

/// Move `from`→`to`, falling back to copy+remove across filesystems (EXDEV).
fn move_path(from: &Path, to: &Path) -> Result<()> {
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(_) => {
            copy_recursive(from, to)?;
            if from.is_dir() {
                fs::remove_dir_all(from)?;
            } else {
                fs::remove_file(from)?;
            }
            Ok(())
        }
    }
}

/// Recursively copy a file or directory tree (symlinks copied as symlinks).
fn copy_recursive(from: &Path, to: &Path) -> Result<()> {
    let md = fs::symlink_metadata(from)?;
    if md.file_type().is_symlink() {
        let target = fs::read_link(from)?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, to)?;
        #[cfg(not(unix))]
        let _ = target;
        return Ok(());
    }
    if md.is_dir() {
        fs::create_dir_all(to)?;
        for ent in fs::read_dir(from)? {
            let ent = ent?;
            copy_recursive(&ent.path(), &to.join(ent.file_name()))?;
        }
    } else {
        fs::copy(from, to)?;
    }
    Ok(())
}

fn read_capped(path: &Path, want: usize) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut f = fs::File::open(path)?;
    let mut buf = vec![0u8; want];
    let mut filled = 0;
    while filled < want {
        let n = f.read(&mut buf[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    buf.truncate(filled);
    Ok(buf)
}

/// Count entries of a directory (shallow), `None` if unreadable.
fn shallow_count(p: &Path) -> Option<Value> {
    fs::read_dir(p)
        .ok()
        .map(|rd| json!(rd.flatten().count() as u64))
}

/// (item_count, hidden_item_count) for a directory; both `None` if unreadable.
fn dir_counts(p: &Path) -> (Option<Value>, Option<Value>) {
    match fs::read_dir(p) {
        Ok(rd) => {
            let mut total = 0u64;
            let mut hidden = 0u64;
            for ent in rd.flatten() {
                total += 1;
                if ent.file_name().to_string_lossy().starts_with('.') {
                    hidden += 1;
                }
            }
            (Some(json!(total)), Some(json!(hidden)))
        }
        Err(_) => (None, None),
    }
}

/// `Metadata` → epoch-seconds extractor for a chosen time accessor (curried for the
/// `.and_then` in `list`).
fn time_secs(
    f: fn(&Metadata) -> std::io::Result<SystemTime>,
) -> impl Fn(&Metadata) -> Option<Value> {
    move |m| f(m).ok().and_then(systime_secs)
}

fn systime_secs(t: SystemTime) -> Option<Value> {
    t.duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| json!(d.as_secs() as i64))
}

fn mode_bits(m: &Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        m.permissions().mode() & 0o7777
    }
    #[cfg(not(unix))]
    {
        let _ = m;
        0
    }
}

/// Folders first, then by the chosen key; `dir` flips the order. Garbage `sort`
/// falls back to name-ascending.
fn sort_entries(entries: &mut [Value], sort: &str, dir: &str) {
    let desc = dir == "desc";
    entries.sort_by(|a, b| {
        let ad = a["is_dir"].as_bool().unwrap_or(false);
        let bd = b["is_dir"].as_bool().unwrap_or(false);
        if ad != bd {
            return bd.cmp(&ad); // dirs first, regardless of dir
        }
        let ord = match sort {
            "size" => num_str(&a["size"]).cmp(&num_str(&b["size"])),
            "modified" => a["mtime"]
                .as_i64()
                .unwrap_or(0)
                .cmp(&b["mtime"].as_i64().unwrap_or(0)),
            _ => name_lc(a).cmp(&name_lc(b)),
        };
        if desc { ord.reverse() } else { ord }
    });
}

fn name_lc(v: &Value) -> String {
    v["name"].as_str().unwrap_or("").to_lowercase()
}
fn num_str(v: &Value) -> u64 {
    v.as_str().and_then(|s| s.parse().ok()).unwrap_or(0)
}

fn file_name(place_rel: &str) -> &str {
    place_rel.rsplit('/').next().unwrap_or(place_rel)
}

/// The parent `<place>/<rel>` of a `place_rel`, or `None` at a place root.
fn parent_place_rel(place: &Place, place_rel: &str) -> Value {
    match place_rel.rsplit_once('/') {
        Some((head, _)) if head != place.id => json!(head),
        // one segment past the place id (e.g. "home/Docs") → the place root.
        Some((head, _)) if head == place.id => json!(place.id.clone()),
        _ => Value::Null, // already at the place root ("home")
    }
}

/// Absolute path under a place → its `<place>/<rel>` address.
fn abs_to_place_rel(place: &Place, p: &Path) -> String {
    match p.strip_prefix(&place.root) {
        Ok(rel) if rel.as_os_str().is_empty() => place.id.clone(),
        Ok(rel) => format!("{}/{}", place.id, rel.to_string_lossy()),
        Err(_) => place.id.clone(),
    }
}

/// A unique trash token: `name`, then `name.1`, `name.2`, … if taken.
fn unique_token(files_dir: &Path, base: &str) -> String {
    let base = if base.is_empty() { "untitled" } else { base };
    if !files_dir.join(base).exists() {
        return base.to_string();
    }
    for n in 1..100_000 {
        let cand = format!("{base}.{n}");
        if !files_dir.join(&cand).exists() {
            return cand;
        }
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{base}.{nanos}")
}

fn info_field(body: &str, key: &str) -> Option<String> {
    body.lines()
        .find_map(|l| l.strip_prefix(&format!("{key}=")))
        .map(str::to_string)
}

/// Extension → MIME (a small, dependency-free table; the FM uses it for icons).
fn mime_for(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "md" | "markdown" => "text/markdown",
        "txt" | "log" | "conf" => "text/plain",
        "rs" => "text/x-rust",
        "mix" => "text/x-mix",
        "toml" => "text/x-toml",
        "json" => "application/json",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" => "text/javascript",
        "sh" => "text/x-shellscript",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "vcf" => "text/vcard",
        "ics" => "text/calendar",
        "mp3" | "flac" | "ogg" | "wav" => "audio/*",
        "mp4" | "mkv" | "webm" | "mov" => "video/*",
        "zip" | "tar" | "gz" | "xz" | "zst" => "application/octet-stream",
        _ => "application/octet-stream",
    }
}

/// A minimal RFC3339 UTC timestamp (no chrono dep; trash deletion date only).
fn now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // days-since-epoch → y/m/d (civil calendar, Howard Hinnant's algorithm).
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn scratch() -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let d = std::env::temp_dir().join(format!(
            "cosmix_fsops_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn layer(root: &Path, writable: bool) -> (FsLayer, PathBuf) {
        let trash = root.join(".Trash");
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();
        let place = Place {
            id: "home".into(),
            label: "Home".into(),
            group: "places".into(),
            icon: "home".into(),
            root: home.clone(),
            writable,
            order: 0,
            allow: Vec::new(),
            deny: Vec::new(),
        };
        (FsLayer::new(vec![place], trash), home)
    }

    #[test]
    fn resolve_within_scoping() {
        let dir = scratch();
        let root = dir.join("r");
        fs::create_dir_all(root.join("sub")).unwrap();
        // empty rel + allow_empty → the root; without allow_empty → None.
        assert_eq!(
            resolve_within(&root, "", true).unwrap(),
            root.canonicalize().unwrap()
        );
        assert!(resolve_within(&root, "", false).is_none());
        // normal subpath ok
        assert!(resolve_within(&root, "sub", true).is_some());
        // escapes rejected
        assert!(resolve_within(&root, "../x", true).is_none());
        assert!(resolve_within(&root, "/abs", true).is_none());
        assert!(resolve_within(&root, "a/../../b", true).is_none());
        fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn resolve_within_blocks_symlink_escape() {
        let dir = scratch();
        let root = dir.join("r");
        let outside = dir.join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        assert!(
            resolve_within(&root, "link/evil", true).is_none(),
            "symlink escape blocked"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn list_counts_and_sorts_dirs_first() {
        let dir = scratch();
        let (fs_, home) = layer(&dir, true);
        fs::create_dir_all(home.join("bdir")).unwrap();
        fs::write(home.join("afile.txt"), b"hello").unwrap();
        fs::write(home.join(".hidden"), b"x").unwrap();
        let v = fs_.list("home", false, "name", "asc").unwrap();
        assert_eq!(v["folder_count"], 1);
        assert_eq!(v["file_count"], 1); // .hidden excluded
        assert_eq!(v["total_bytes"], "5");
        let e = v["entries"].as_array().unwrap();
        assert_eq!(e[0]["name"], "bdir", "dirs sort first");
        assert_eq!(e[0]["is_dir"], true);
        assert_eq!(e[1]["name"], "afile.txt");
        // show_hidden surfaces the dotfile
        let vh = fs_.list("home", true, "name", "asc").unwrap();
        assert_eq!(vh["file_count"], 2);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unknown_place_and_escape_denied() {
        let dir = scratch();
        let (fs_, _home) = layer(&dir, true);
        assert!(matches!(
            fs_.list("nope", false, "name", "asc"),
            Err(FilesError::Denied(_))
        ));
        assert!(matches!(
            fs_.list("home/../x", false, "name", "asc"),
            Err(FilesError::Denied(_))
        ));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn readonly_place_refuses_mutation_but_allows_read() {
        let dir = scratch();
        let (fs_, home) = layer(&dir, false); // read-only
        fs::write(home.join("f.txt"), b"x").unwrap();
        assert!(fs_.list("home", false, "name", "asc").is_ok()); // read ok
        assert!(matches!(
            fs_.mkdir("home/new", false),
            Err(FilesError::Denied(_))
        ));
        assert!(matches!(
            fs_.delete("home/f.txt", false),
            Err(FilesError::Denied(_))
        ));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mkdir_touch_write_no_clobber() {
        let dir = scratch();
        let (fs_, _home) = layer(&dir, true);
        assert!(fs_.mkdir("home/d", false).is_ok());
        assert!(matches!(
            fs_.mkdir("home/d", false),
            Err(FilesError::Exists(_))
        ));
        assert!(fs_.touch("home/d/f.txt").is_ok());
        assert!(matches!(
            fs_.touch("home/d/f.txt"),
            Err(FilesError::Exists(_))
        ));
        // write overwrite=false refuses an existing file; =true replaces.
        assert!(matches!(
            fs_.write("home/d/f.txt", b"x", false),
            Err(FilesError::Exists(_))
        ));
        assert!(fs_.write("home/d/f.txt", b"new", true).is_ok());
        assert_eq!(fs::read(dir.join("home/d/f.txt")).unwrap(), b"new");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn copy_from_readonly_to_writable() {
        let dir = scratch();
        let trash = dir.join(".Trash");
        let ro = dir.join("ro");
        let rw = dir.join("rw");
        fs::create_dir_all(&ro).unwrap();
        fs::create_dir_all(&rw).unwrap();
        fs::write(ro.join("src.txt"), b"data").unwrap();
        let fs_ = FsLayer::new(
            vec![
                Place {
                    id: "ro".into(),
                    label: "RO".into(),
                    group: "g".into(),
                    icon: "i".into(),
                    root: ro,
                    writable: false,
                    order: 0,
                    allow: Vec::new(),
                    deny: Vec::new(),
                },
                Place {
                    id: "rw".into(),
                    label: "RW".into(),
                    group: "g".into(),
                    icon: "i".into(),
                    root: rw.clone(),
                    writable: true,
                    order: 1,
                    allow: Vec::new(),
                    deny: Vec::new(),
                },
            ],
            trash,
        );
        // copy: read-only source → writable dest is ALLOWED.
        assert!(fs_.copy("ro/src.txt", "rw/dst.txt", false).is_ok());
        assert_eq!(fs::read(rw.join("dst.txt")).unwrap(), b"data");
        // move: source must be writable → a read-only source is refused.
        assert!(matches!(
            fs_.move_("ro/src.txt", "rw/m.txt", false),
            Err(FilesError::Denied(_))
        ));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn move_within_and_no_clobber() {
        let dir = scratch();
        let (fs_, home) = layer(&dir, true);
        fs::write(home.join("a.txt"), b"a").unwrap();
        fs::write(home.join("b.txt"), b"b").unwrap();
        assert!(matches!(
            fs_.move_("home/a.txt", "home/b.txt", false),
            Err(FilesError::Exists(_))
        ));
        assert!(fs_.move_("home/a.txt", "home/sub/a.txt", false).is_ok());
        assert!(!home.join("a.txt").exists());
        assert_eq!(fs::read(home.join("sub/a.txt")).unwrap(), b"a");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn trash_then_restore_roundtrip() {
        let dir = scratch();
        let (fs_, home) = layer(&dir, true);
        fs::write(home.join("doc.md"), b"keep me").unwrap();
        let t = fs_.trash("home/doc.md").unwrap();
        let token = t["token"].as_str().unwrap().to_string();
        assert!(!home.join("doc.md").exists(), "file left its place");
        // it appears in the listing
        let list = fs_.trash_list().unwrap();
        assert_eq!(list["items"].as_array().unwrap().len(), 1);
        assert_eq!(list["items"][0]["origin"], "home/doc.md");
        // restore puts it back, removing the info sidecar
        fs_.trash_restore(&token).unwrap();
        assert_eq!(fs::read(home.join("doc.md")).unwrap(), b"keep me");
        assert_eq!(
            fs_.trash_list().unwrap()["items"].as_array().unwrap().len(),
            0
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn trash_restore_refuses_when_origin_readonly() {
        let dir = scratch();
        let trash = dir.join(".Trash");
        let ro = dir.join("ro");
        fs::create_dir_all(&ro).unwrap();
        fs::write(ro.join("x.txt"), b"x").unwrap();
        // Two layers over the SAME root: one writable (to trash it), one read-only
        // (to prove restore re-checks writability at restore time).
        let writable = FsLayer::new(
            vec![Place {
                id: "p".into(),
                label: "P".into(),
                group: "g".into(),
                icon: "i".into(),
                root: ro.clone(),
                writable: true,
                order: 0,
                allow: Vec::new(),
                deny: Vec::new(),
            }],
            trash.clone(),
        );
        let t = writable.trash("p/x.txt").unwrap();
        let token = t["token"].as_str().unwrap().to_string();
        let readonly = FsLayer::new(
            vec![Place {
                id: "p".into(),
                label: "P".into(),
                group: "g".into(),
                icon: "i".into(),
                root: ro,
                writable: false,
                order: 0,
                allow: Vec::new(),
                deny: Vec::new(),
            }],
            trash,
        );
        assert!(
            matches!(readonly.trash_restore(&token), Err(FilesError::Denied(_))),
            "restore re-validates writability"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mutating_a_place_root_is_refused() {
        let dir = scratch();
        let (fs_, home) = layer(&dir, true);
        fs::write(home.join("x.txt"), b"x").unwrap();
        // Destructive/mutating ops targeting the place ROOT (empty rel) are refused —
        // they would relocate/destroy the whole configured tree.
        assert!(matches!(
            fs_.delete("home", true),
            Err(FilesError::Denied(_))
        ));
        assert!(matches!(fs_.trash("home"), Err(FilesError::Denied(_))));
        assert!(matches!(
            fs_.move_("home", "home/sub", false),
            Err(FilesError::Denied(_))
        ));
        // But the root is still LISTABLE/STATable (reads allow the empty rel).
        assert!(fs_.list("home", false, "name", "asc").is_ok());
        assert!(fs_.stat("home").is_ok());
        // The file is untouched.
        assert!(home.join("x.txt").exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn copy_move_into_self_refused() {
        let dir = scratch();
        let (fs_, home) = layer(&dir, true);
        fs::create_dir_all(home.join("d")).unwrap();
        fs::write(home.join("d/f.txt"), b"x").unwrap();
        // copy/move a directory INTO itself (a descendant dest) is rejected.
        assert!(matches!(
            fs_.copy("home/d", "home/d/inner", false),
            Err(FilesError::BadRequest(_))
        ));
        assert!(matches!(
            fs_.move_("home/d", "home/d/inner", false),
            Err(FilesError::BadRequest(_))
        ));
        // copy onto the exact same file (overwrite) is rejected (would destroy it).
        assert!(matches!(
            fs_.copy("home/d/f.txt", "home/d/f.txt", true),
            Err(FilesError::BadRequest(_))
        ));
        // a sibling copy still works.
        assert!(fs_.copy("home/d/f.txt", "home/d/g.txt", false).is_ok());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn trash_restore_token_validation() {
        let dir = scratch();
        let (fs_, home) = layer(&dir, true);
        // A filename that merely CONTAINS ".." must trash AND restore cleanly.
        fs::write(home.join("a..b.txt"), b"dots").unwrap();
        let t = fs_.trash("home/a..b.txt").unwrap();
        let token = t["token"].as_str().unwrap().to_string();
        assert!(
            fs_.trash_restore(&token).is_ok(),
            "a dotted filename must restore"
        );
        assert_eq!(fs::read(home.join("a..b.txt")).unwrap(), b"dots");
        // Crafted tokens addressing the trash dir / escaping it are rejected.
        for bad in [".", "..", "a/b", "/x", ""] {
            assert!(
                matches!(fs_.trash_restore(bad), Err(FilesError::BadRequest(_))),
                "token {bad:?} must be rejected"
            );
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn delete_dir_needs_recursive() {
        let dir = scratch();
        let (fs_, home) = layer(&dir, true);
        fs::create_dir_all(home.join("d/e")).unwrap();
        assert!(matches!(
            fs_.delete("home/d", false),
            Err(FilesError::BadRequest(_))
        ));
        assert!(fs_.delete("home/d", true).is_ok());
        assert!(!home.join("d").exists());
        // delete of a missing path is idempotent
        assert!(fs_.delete("home/gone.txt", false).is_ok());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn stat_and_read_blob() {
        let dir = scratch();
        let (fs_, home) = layer(&dir, true);
        fs::write(home.join("n.md"), b"# hi\n").unwrap();
        let st = fs_.stat("home/n.md").unwrap();
        assert_eq!(st["type"], "file");
        assert_eq!(st["size"], "5");
        assert_eq!(st["mime"], "text/markdown");
        let blob = fs_.read_blob("home/n.md", 1024).unwrap();
        assert_eq!(blob["binary"], false);
        assert_eq!(blob["text"], "# hi\n");
        // dir stat reports a type + item_count
        let sd = fs_.stat("home").unwrap();
        assert_eq!(sd["type"], "dir");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn search_recursive() {
        let dir = scratch();
        let (fs_, home) = layer(&dir, true);
        fs::create_dir_all(home.join("a/b")).unwrap();
        fs::write(home.join("a/b/Target.md"), b"x").unwrap();
        fs::write(home.join("other.txt"), b"x").unwrap();
        let r = fs_.search("home", "target", true, 50).unwrap();
        let hits = r["entries"].as_array().unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["path"], "home/a/b/Target.md");
        // non-recursive finds nothing here
        let r2 = fs_.search("home", "target", false, 50).unwrap();
        assert_eq!(r2["entries"].as_array().unwrap().len(), 0);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tree_folders_only_depth_capped() {
        let dir = scratch();
        let (fs_, home) = layer(&dir, true);
        fs::create_dir_all(home.join("a/b/c")).unwrap();
        fs::create_dir_all(home.join("d")).unwrap();
        fs::write(home.join("a/file.txt"), b"x").unwrap(); // files excluded
        let f: Vec<String> = fs_.tree("home", 4, 100).unwrap()["folders"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_str().unwrap().to_string())
            .collect();
        assert!(f.contains(&"home/a".to_string()));
        assert!(f.contains(&"home/a/b".to_string()));
        assert!(f.contains(&"home/a/b/c".to_string()));
        assert!(f.contains(&"home/d".to_string()));
        assert!(!f.iter().any(|x| x.contains("file.txt")), "files excluded");
        // depth cap: depth-2 dirs present, depth-3 excluded
        let f2: Vec<String> = fs_.tree("home", 2, 100).unwrap()["folders"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_str().unwrap().to_string())
            .collect();
        assert!(f2.contains(&"home/a/b".to_string()));
        assert!(!f2.contains(&"home/a/b/c".to_string()));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rfc3339_shape() {
        let s = now_rfc3339();
        assert_eq!(s.len(), 20);
        assert!(s.ends_with('Z') && s.contains('T'));
    }

    // ── policy-scoped place (the sshm shape) ──────────────────────────────────

    /// The unit matcher / policy resolver, independent of the filesystem.
    #[test]
    fn seg_match_and_policy_access() {
        assert!(seg_match("config", "config"));
        assert!(!seg_match("config", "configx"));
        assert!(seg_match("*", "anything"));
        assert!(seg_match("*", ".ephemeral")); // * matches a leading dot
        assert!(seg_match("*.pub", "alpha.pub"));
        assert!(!seg_match("*.pub", "alpha"));
        assert!(!seg_match("*.pub", "pub")); // needs the dot
        assert!(seg_match("a*b", "axxb"));
        assert!(!seg_match("a*b", "axx"));

        let allow = [
            "config".to_string(),
            "hosts/*".to_string(),
            "keys/*.pub".to_string(),
        ];
        let deny: Vec<String> = vec![];
        let acc = |rel: &str| policy_access(&allow, &deny, rel);
        assert!(matches!(acc("config"), Access::Node));
        assert!(matches!(acc("hosts/gw"), Access::Node));
        assert!(matches!(acc("hosts/.ephemeral"), Access::Node));
        assert!(matches!(acc("hosts"), Access::Prefix)); // traversable ancestor
        assert!(matches!(acc("keys"), Access::Prefix));
        assert!(matches!(acc("keys/alpha.pub"), Access::Node));
        assert!(matches!(acc("keys/alpha"), Access::Denied)); // private key
        assert!(matches!(acc("id_ed25519"), Access::Denied));
        assert!(matches!(acc("known_hosts"), Access::Denied));
        assert!(matches!(acc("hosts/a/b"), Access::Denied)); // exact depth only

        // deny wins and covers the whole subtree.
        let deny2 = [".git".to_string()];
        assert!(matches!(
            policy_access(&allow, &deny2, ".git"),
            Access::Denied
        ));
        assert!(matches!(
            policy_access(&[], &deny2, ".git/config"),
            Access::Denied
        ));
        // empty allow ⇒ unrestricted (subject to deny)
        assert!(matches!(
            policy_access(&[], &[], "anything/at/all"),
            Access::Node
        ));
    }

    /// Build a `~/.ssh`-shaped policied place and its on-disk fixture.
    fn sshm_layer(root: &Path) -> (FsLayer, PathBuf) {
        let trash = root.join(".Trash");
        let ssh = root.join("ssh");
        fs::create_dir_all(ssh.join("hosts")).unwrap();
        fs::create_dir_all(ssh.join("keys")).unwrap();
        fs::write(ssh.join("config"), b"Include hosts/*\n").unwrap();
        fs::write(ssh.join("hosts/gw"), b"Host gw\n").unwrap();
        fs::write(ssh.join("hosts/.ephemeral"), b"gw\n").unwrap();
        fs::write(ssh.join("keys/alpha"), b"PRIVATE KEY").unwrap();
        fs::write(ssh.join("keys/alpha.pub"), b"ssh-ed25519 AAAA").unwrap();
        fs::write(ssh.join("id_ed25519"), b"TOP SECRET").unwrap();
        fs::write(ssh.join("known_hosts"), b"gw ssh-ed25519 AAAA").unwrap();
        let place = Place {
            id: "sshm".into(),
            label: "SSH".into(),
            group: "admin".into(),
            icon: "key".into(),
            root: ssh.clone(),
            writable: true,
            order: 0,
            allow: vec!["config".into(), "hosts/*".into(), "keys/*.pub".into()],
            deny: Vec::new(),
        };
        (FsLayer::new(vec![place], trash), ssh)
    }

    #[test]
    fn policied_place_reads_are_scoped() {
        let dir = scratch();
        let (fs_, _ssh) = sshm_layer(&dir);
        // Allowed reads.
        assert!(fs_.read_blob("sshm/config", 4096).is_ok());
        assert!(fs_.read_blob("sshm/hosts/gw", 4096).is_ok());
        assert!(fs_.read_blob("sshm/hosts/.ephemeral", 4096).is_ok());
        assert!(fs_.read_blob("sshm/keys/alpha.pub", 4096).is_ok());
        // Denied reads — private key material, host-key store, exact-depth.
        assert!(matches!(
            fs_.read_blob("sshm/keys/alpha", 4096),
            Err(FilesError::Denied(_))
        ));
        assert!(matches!(
            fs_.read_blob("sshm/id_ed25519", 4096),
            Err(FilesError::Denied(_))
        ));
        assert!(matches!(
            fs_.read_blob("sshm/known_hosts", 4096),
            Err(FilesError::Denied(_))
        ));
        assert!(matches!(
            fs_.stat("sshm/keys/alpha"),
            Err(FilesError::Denied(_))
        ));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn policied_place_listings_hide_denied_names() {
        let dir = scratch();
        let (fs_, _ssh) = sshm_layer(&dir);
        // Root listing shows only the allowed top-level names.
        let root = fs_.list("sshm", true, "name", "asc").unwrap();
        let names: Vec<String> = root["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap().to_string())
            .collect();
        assert!(names.contains(&"config".to_string()));
        assert!(names.contains(&"hosts".to_string()));
        assert!(names.contains(&"keys".to_string()));
        assert!(
            !names.contains(&"id_ed25519".to_string()),
            "private key hidden"
        );
        assert!(
            !names.contains(&"known_hosts".to_string()),
            "known_hosts hidden"
        );
        // keys/ listing shows the .pub but not the private half.
        let keys = fs_.list("sshm/keys", true, "name", "asc").unwrap();
        let knames: Vec<String> = keys["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap().to_string())
            .collect();
        assert!(knames.contains(&"alpha.pub".to_string()));
        assert!(
            !knames.contains(&"alpha".to_string()),
            "private key hidden in keys/"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn policied_place_writes_are_scoped() {
        let dir = scratch();
        let (fs_, _ssh) = sshm_layer(&dir);
        // Writing a new host fragment is allowed.
        assert!(
            fs_.write("sshm/hosts/newhost", b"Host newhost\n", false)
                .is_ok()
        );
        // Writing outside policy (a private key, authorized_keys) is denied.
        assert!(matches!(
            fs_.write("sshm/keys/evil", b"x", false),
            Err(FilesError::Denied(_))
        ));
        assert!(matches!(
            fs_.write("sshm/authorized_keys", b"x", false),
            Err(FilesError::Denied(_))
        ));
        // Writing onto a bare prefix dir is refused (Prefix is not a write target).
        assert!(matches!(
            fs_.write("sshm/hosts", b"x", true),
            Err(FilesError::Denied(_))
        ));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn policied_place_rejects_symlink_and_nonregular() {
        let dir = scratch();
        let (fs_, ssh) = sshm_layer(&dir);
        // An in-root symlink whose NAME is policy-allowed but which points at a
        // denied private key must not read through it.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(ssh.join("id_ed25519"), ssh.join("hosts/leak")).unwrap();
            assert!(
                matches!(
                    fs_.read_blob("sshm/hosts/leak", 4096),
                    Err(FilesError::Denied(_))
                ),
                "symlink in a policy-scoped place must be refused"
            );
            // And it must not appear in the listing.
            let hosts = fs_.list("sshm/hosts", true, "name", "asc").unwrap();
            let hnames: Vec<String> = hosts["entries"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| e["name"].as_str().unwrap().to_string())
                .collect();
            assert!(
                !hnames.contains(&"leak".to_string()),
                "symlink hidden from listing"
            );

            // A hardlink to the private key with an allowed name must be refused
            // (canonicalization can't see the shared inode; the nlink guard does).
            std::fs::hard_link(ssh.join("id_ed25519"), ssh.join("hosts/hl")).unwrap();
            assert!(
                matches!(
                    fs_.read_blob("sshm/hosts/hl", 4096),
                    Err(FilesError::Denied(_))
                ),
                "hardlinked file must be refused"
            );

            // An in-root ANCESTOR symlink: `linkdir` → the real `hosts` dir. The
            // path `linkdir/gw` is not policy-allowed anyway, but even a symlink
            // whose NAME is an allowed prefix must be refused as a component.
            std::os::unix::fs::symlink(ssh.join("hosts"), ssh.join("keys/hostslink")).unwrap();
            assert!(
                matches!(
                    fs_.read_blob("sshm/keys/hostslink", 4096),
                    Err(FilesError::Denied(_))
                ),
                "ancestor/component symlink must be refused"
            );
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn policied_place_ancestor_symlink_refused() {
        // The precise #7 bypass: replace an allowed prefix dir with a symlink to
        // another in-root dir. resolve_within's containment check passes (target
        // is in-root) and the final file is regular — only the component-chain
        // lstat catches it.
        #[cfg(unix)]
        {
            let dir = scratch();
            let trash = dir.join(".Trash");
            let ssh = dir.join("ssh");
            fs::create_dir_all(ssh.join("real")).unwrap();
            fs::write(ssh.join("real/id_ed25519"), b"SECRET").unwrap();
            // `hosts` is a symlink to `real` — both inside the place root.
            std::os::unix::fs::symlink(ssh.join("real"), ssh.join("hosts")).unwrap();
            let place = Place {
                id: "sshm".into(),
                label: "SSH".into(),
                group: "admin".into(),
                icon: "key".into(),
                root: ssh.clone(),
                writable: true,
                order: 0,
                allow: vec!["hosts/*".into()],
                deny: Vec::new(),
            };
            let fs_ = FsLayer::new(vec![place], trash);
            // `hosts/id_ed25519` is textually allowed by `hosts/*`, but `hosts`
            // is a symlink component → refused.
            assert!(
                matches!(
                    fs_.read_blob("sshm/hosts/id_ed25519", 4096),
                    Err(FilesError::Denied(_))
                ),
                "reading through an ancestor symlink must be refused"
            );
            fs::remove_dir_all(&dir).ok();
        }
    }

    #[test]
    fn nonpolicied_place_unchanged() {
        // A place with empty allow/deny behaves exactly as before: everything
        // under the root is reachable, including dotfiles.
        let dir = scratch();
        let (fs_, home) = layer(&dir, true);
        fs::write(home.join("secret"), b"s").unwrap();
        fs::write(home.join(".hidden"), b"h").unwrap();
        assert!(fs_.read_blob("home/secret", 4096).is_ok());
        assert!(fs_.read_blob("home/.hidden", 4096).is_ok());
        fs::remove_dir_all(&dir).ok();
    }
}
