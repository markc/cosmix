//! Ingest: read + parse one corpus file into a [`DocRecord`] (the read half of
//! reconcile). Pure with respect to the index — it never writes the file and never
//! mints an id (a passive scan must not, §6); an id-less file yields
//! `id: None, unmanaged: true` for the daemon's explicit authoring path to adopt.

use std::path::Path;
use std::time::UNIX_EPOCH;

use crate::error::Result;
use crate::store::DocRecord;
use crate::{frontmatter, hash, id};

/// Build the index projection record for `root/rel_path`.
pub fn scan(root: &Path, rel_path: &str) -> Result<DocRecord> {
    let full = root.join(rel_path);
    let bytes = std::fs::read(&full)?;
    let content_hash = hash::content_hash(&bytes);
    let size_bytes = Some(bytes.len() as i64);
    // Markdown is UTF-8; tolerate a stray bad byte rather than failing the whole walk.
    let text = String::from_utf8_lossy(&bytes);
    let parsed = frontmatter::read(&text)?;

    let id = parsed
        .headers
        .get("id")
        .map(|s| strip_quotes(s.trim()).to_string())
        .filter(|s| id::is_valid_id(s));
    let unmanaged = id.is_none();

    let header = |k: &str| parsed.headers.get(k).cloned();
    let frontmatter_json = frontmatter::headers_to_json(&parsed.headers).to_string();
    let parse_warnings = if parsed.warnings.is_empty() {
        None
    } else {
        serde_json::to_string(&parsed.warnings).ok()
    };
    let mtime = std::fs::metadata(&full)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64);

    Ok(DocRecord {
        rel_path: rel_path.to_string(),
        id,
        content_hash,
        title: header("title"),
        slug: header("slug"),
        content_type: header("content_type"),
        created: header("created"),
        raw_header: frontmatter::raw_header(&text).unwrap_or_default(),
        frontmatter_json,
        tags_json: header("tags"),
        category: header("category"),
        word_count: Some(parsed.body.split_whitespace().count() as i64),
        size_bytes,
        mtime,
        parse_warnings,
        dup_of: None,
        unmanaged,
    })
}

fn strip_quotes(s: &str) -> &str {
    s.strip_prefix('"')
        .and_then(|x| x.strip_suffix('"'))
        .unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("cosmix_files_ingest_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn scans_managed_file() {
        let dir = scratch();
        std::fs::write(
            dir.join("note.md"),
            "---\nid: 0190aaaa-bbbb-7ccc-8ddd-eeeeffff0000\ntitle: Hi\ntags: [\"a\",\"b\"]\n---\nbody words here\n",
        )
        .unwrap();
        let r = scan(&dir, "note.md").unwrap();
        assert_eq!(
            r.id.as_deref(),
            Some("0190aaaa-bbbb-7ccc-8ddd-eeeeffff0000")
        );
        assert!(!r.unmanaged);
        assert_eq!(r.title.as_deref(), Some("Hi"));
        assert_eq!(r.tags_json.as_deref(), Some("[\"a\",\"b\"]"));
        assert_eq!(r.word_count, Some(3));
        assert_eq!(r.content_hash.len(), 64);
        assert_eq!(
            r.raw_header,
            "id: 0190aaaa-bbbb-7ccc-8ddd-eeeeffff0000\ntitle: Hi\ntags: [\"a\",\"b\"]\n"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn idless_file_is_unmanaged() {
        let dir = scratch();
        std::fs::write(dir.join("draft.md"), "# just a heading\n\nno frontmatter\n").unwrap();
        let r = scan(&dir, "draft.md").unwrap();
        assert_eq!(r.id, None);
        assert!(r.unmanaged);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn quoted_id_accepted() {
        let dir = scratch();
        std::fs::write(
            dir.join("q.md"),
            "---\nid: \"0190aaaa-bbbb-7ccc-8ddd-eeeeffff0000\"\n---\nx\n",
        )
        .unwrap();
        let r = scan(&dir, "q.md").unwrap();
        assert_eq!(
            r.id.as_deref(),
            Some("0190aaaa-bbbb-7ccc-8ddd-eeeeffff0000")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
