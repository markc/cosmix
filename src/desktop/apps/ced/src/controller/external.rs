//! External diagnostic sets (Scene Editor plan §4.4.3): what `ced.diagnostics`
//! stores per `(path, source)` and applies to every tab showing that path.
//!
//! The store is an LRU of [`verbs::DIAG_STORE_PATHS`] paths. A set is applied
//! with `Diagnostics::accept_items` at the tab's current gen (so it follows
//! covered-range invalidation from there), and only when the tab's text
//! hashes to the set's `digest`; otherwise the tab shows nothing for that
//! source and the set stays stored for the next application (a Resync, the
//! tab opening, or its `dirty` going false).

use std::collections::BTreeMap;
use std::fmt::Write;

use cosmix_edit_client::diag::{DiagItem, Severity};
use cosmix_edit_client::highlight::ResultTag;
use cosmix_edit_client::mirror::Phase;
use sha2::{Digest, Sha256};

use super::Tab;
use crate::verbs::{self, DiagSeverity};

/// Sets kept per path. A caller looping over fresh source names evicts its
/// own oldest set, never grows the store without bound (review m8).
const SOURCES_PER_PATH: usize = 8;

/// One stored set: the file digest it describes (if the sender gave one),
/// and when it was stored (for per-path eviction).
pub(super) struct StoredSet {
    digest: Option<String>,
    items: Vec<DiagItem>,
    seq: u64,
}

/// Sets per path, least recently stored first.
#[derive(Default)]
pub(super) struct Store {
    paths: Vec<(String, BTreeMap<String, StoredSet>)>,
    seq: u64,
}

impl Store {
    /// Store (or, for an empty list, drop) `source`'s set for `path`.
    pub(super) fn put(&mut self, req: &verbs::DiagnosticsReq) {
        let at = self.paths.iter().position(|(p, _)| *p == req.path);
        let mut sets = at.map(|i| self.paths.remove(i).1).unwrap_or_default();
        if req.diagnostics.is_empty() {
            sets.remove(&req.source);
        } else {
            let items = req.diagnostics.iter().map(item).collect();
            self.seq += 1;
            sets.insert(req.source.clone(), StoredSet { digest: req.digest.clone(), items, seq: self.seq });
            while sets.len() > SOURCES_PER_PATH {
                let oldest = sets.iter().min_by_key(|(_, s)| s.seq).map(|(k, _)| k.clone());
                match oldest {
                    Some(k) => sets.remove(&k),
                    None => break,
                };
            }
        }
        if !sets.is_empty() {
            self.paths.push((req.path.clone(), sets));
        }
        while self.paths.len() > verbs::DIAG_STORE_PATHS {
            self.paths.remove(0);
        }
    }

    fn sets(&self, path: &str) -> Option<&BTreeMap<String, StoredSet>> {
        self.paths.iter().find(|(p, _)| p == path).map(|(_, s)| s)
    }

    #[cfg(test)]
    pub(super) fn paths(&self) -> Vec<&str> {
        self.paths.iter().map(|(p, _)| p.as_str()).collect()
    }
}

/// The request's refusal (message, reason), if any. `body_len` is the raw
/// request body's size.
pub(super) fn check(req: &verbs::DiagnosticsReq, body_len: usize) -> Result<(), (String, &'static str)> {
    if body_len > verbs::MAX_DIAGNOSTICS_BODY {
        return Err((format!("the request is {body_len} bytes (at most {})", verbs::MAX_DIAGNOSTICS_BODY), "too_large"));
    }
    if req.source == verbs::LINT_SOURCE {
        return Err(("source lint is reserved for ced's own lint".into(), "bad_source"));
    }
    if !valid_source(&req.source) {
        return Err((format!("source {:?} must match ^[a-z][a-z0-9-]{{0,31}}$", req.source), "bad_source"));
    }
    if !std::path::Path::new(&req.path).is_absolute() {
        return Err((format!("path {:?} is not absolute", req.path), "bad_path"));
    }
    if req.diagnostics.len() > verbs::MAX_EXTERNAL_DIAGNOSTICS {
        return Err((format!("{} diagnostics (at most {})", req.diagnostics.len(), verbs::MAX_EXTERNAL_DIAGNOSTICS), "too_many"));
    }
    if let Some(d) = &req.digest
        && !(d.len() == 64 && d.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
    {
        return Err((format!("digest {d:?} is not 64 lower-hex characters (sha256)"), "bad_digest"));
    }
    Ok(())
}

fn valid_source(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 32
        && b[0].is_ascii_lowercase()
        && b.iter().all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}

fn item(d: &verbs::ExternalDiagnostic) -> DiagItem {
    DiagItem {
        line: d.line,
        column: d.col,
        severity: match d.severity {
            DiagSeverity::Error => Severity::Error,
            DiagSeverity::Warning => Severity::Warning,
            DiagSeverity::Note => Severity::Note,
        },
        code: d.code.clone(),
        message: d.message.clone(),
        hint: None,
    }
}

/// The path a tab shows: the service's, else the one it was opened with.
pub(super) fn tab_path(t: &Tab) -> Option<&str> {
    t.mirror.as_ref().and_then(|m| m.meta().path.as_deref()).or(t.path.as_deref())
}

/// Lower-hex sha256 of the bytes the tab's text saves as (BOM restored,
/// text verbatim — `cosmix_edit_core::buffer::Buffer::to_bytes`).
fn digest_of(t: &Tab) -> Option<String> {
    let m = t.mirror.as_ref()?;
    let mut h = Sha256::new();
    if m.meta().bom {
        h.update("\u{feff}".as_bytes());
    }
    h.update(super::text_string(m.text()).as_bytes());
    let mut hex = String::with_capacity(64);
    for b in h.finalize() {
        let _ = write!(hex, "{b:02x}");
    }
    Some(hex)
}

/// What one application did to one tab.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct Applied {
    /// Diagnostics now shown for the applied source(s).
    pub(super) shown: usize,
    /// A set's digest did not match the tab's text.
    pub(super) stale: bool,
}

/// Apply the stored sets for the tab's path — every source, or only `only`
/// (which is cleared when nothing is stored for it) — to a live tab. A
/// non-live tab is left alone; it is applied when it goes live.
///
/// `undigested`: whether sets sent without a digest are applied. Nothing can
/// tell such a set is still about this text, so the tab going clean (the one
/// trigger that is only about the text) must not bring back rows an edit
/// dropped (review m9); the verb, a tab opening and a Resync apply them.
pub(super) fn apply(store: &Store, t: &mut Tab, only: Option<&str>, undigested: bool) -> Applied {
    let mut out = Applied::default();
    let Some(m) = t.mirror.as_ref().filter(|m| matches!(m.phase(), Phase::Live)) else { return out };
    let Some(path) = tab_path(t).map(str::to_owned) else { return out };
    let sets = store.sets(&path);
    let sources: Vec<String> = match only {
        Some(s) => vec![s.to_owned()],
        None => sets.map(|s| s.keys().cloned().collect()).unwrap_or_default(),
    };
    if sources.is_empty() {
        return out;
    }
    let tag = ResultTag {
        epoch: m.epoch().to_string(),
        buffer: m.buffer().to_string(),
        view_gen: m.view_gen(),
        language: m.meta().language.clone(),
        cfg: 0,
    };
    let text = super::text_string(m.text());
    let mut digest = None;
    for source in sources {
        let set = sets.and_then(|s| s.get(&source));
        if set.is_some_and(|set| set.digest.is_none()) && !undigested {
            continue;
        }
        let items: &[DiagItem] = match set {
            Some(set) => {
                let fresh = match &set.digest {
                    None => true,
                    Some(want) => *digest.get_or_insert_with(|| digest_of(t).unwrap_or_default()) == *want,
                };
                out.stale |= !fresh;
                if fresh { set.items.as_slice() } else { &[] }
            }
            None => &[],
        };
        // Tagged at the tab's current gen: never stale.
        let _ = t.diagnostics.accept_items(&source, &tag, tag.clone(), &text, items, &[]);
        out.shown += t.diagnostics.items().iter().filter(|d| d.source == source).count();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(path: &str, source: &str, n: usize) -> verbs::DiagnosticsReq {
        let d = verbs::ExternalDiagnostic { line: 1, col: None, severity: DiagSeverity::Error, code: "c".into(), message: "m".into() };
        verbs::DiagnosticsReq { path: path.into(), source: source.into(), digest: None, diagnostics: vec![d; n] }
    }

    #[test]
    fn the_store_is_an_lru_of_paths_and_empty_lists_clear() {
        let mut s = Store::default();
        for i in 0..verbs::DIAG_STORE_PATHS + 2 {
            s.put(&req(&format!("/p/{i}"), "scenes", 1));
        }
        assert_eq!(s.paths().len(), verbs::DIAG_STORE_PATHS);
        assert_eq!(s.paths()[0], "/p/2", "the two least recently stored went first");
        // Storing again makes a path the most recent.
        s.put(&req("/p/2", "other", 1));
        assert_eq!(*s.paths().last().unwrap(), "/p/2");
        assert_eq!(s.sets("/p/2").unwrap().len(), 2);
        s.put(&req("/p/2", "other", 0));
        assert_eq!(s.sets("/p/2").unwrap().keys().collect::<Vec<_>>(), ["scenes"]);
        s.put(&req("/p/2", "scenes", 0));
        assert!(s.sets("/p/2").is_none(), "a path with no set left is dropped");
    }

    #[test]
    fn a_path_keeps_at_most_its_newest_sources() {
        let mut s = Store::default();
        s.put(&req("/p", "scenes", 1));
        for i in 0..SOURCES_PER_PATH + 3 {
            s.put(&req("/p", &format!("flood-{i}"), 1));
        }
        let sets = s.sets("/p").unwrap();
        assert_eq!(sets.len(), SOURCES_PER_PATH);
        assert!(!sets.contains_key("scenes") && !sets.contains_key("flood-0"), "the oldest sets went first");
        assert!(sets.contains_key(&format!("flood-{}", SOURCES_PER_PATH + 2)));
        // Re-storing a source makes it the newest again.
        s.put(&req("/p", "flood-3", 2));
        s.put(&req("/p", "late", 1));
        assert!(s.sets("/p").unwrap().contains_key("flood-3"));
    }

    #[test]
    fn requests_are_checked() {
        assert!(check(&req("/a/scene.mix", "scenes", 1), 100).is_ok());
        assert_eq!(check(&req("/a", "lint", 1), 100).unwrap_err().1, "bad_source");
        let long = "a".repeat(33);
        for bad in ["", "Scenes", "1x", "a_b", long.as_str()] {
            assert_eq!(check(&req("/a", bad, 1), 100).unwrap_err().1, "bad_source", "{bad:?}");
        }
        assert!(check(&req("/a", &"a".repeat(32), 1), 100).is_ok());
        assert_eq!(check(&req("a/scene.mix", "scenes", 1), 100).unwrap_err().1, "bad_path");
        assert_eq!(check(&req("/a", "scenes", verbs::MAX_EXTERNAL_DIAGNOSTICS + 1), 100).unwrap_err().1, "too_many");
        assert!(check(&req("/a", "scenes", verbs::MAX_EXTERNAL_DIAGNOSTICS), 100).is_ok());
        assert_eq!(check(&req("/a", "scenes", 1), verbs::MAX_DIAGNOSTICS_BODY + 1).unwrap_err().1, "too_large");
        let mut r = req("/a", "scenes", 1);
        r.digest = Some("ABC".into());
        assert_eq!(check(&r, 100).unwrap_err().1, "bad_digest");
        r.digest = Some("0".repeat(64));
        assert!(check(&r, 100).is_ok());
    }
}
