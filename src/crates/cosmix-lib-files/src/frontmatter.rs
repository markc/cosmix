//! Frontmatter: canonical READ + surgical byte-preserving WRITE.
//!
//! The corpus grammar is the Crisp/Bus `---`-fenced flat `Key: value` header
//! (values starting `[` or `{` are JSON, else plain strings). READ delegates to
//! `cosmix_bus::bus::parse_lenient` — the single canonical parser, so the corpus
//! never forks a second divergent one.
//!
//! WRITE is the load-bearing piece. There is **no lossless round-trip** through the
//! Bus parser (its `headers` is a key-sorted `BTreeMap`, and it drops list / comment
//! / blank lines and trims the body) and **no header serializer exists anywhere**.
//! So edits are done as a minimal in-place line replacement over the raw text:
//! every untouched byte — field order, quoting, JSON values, list lines, comments,
//! blanks, and the entire body — is preserved exactly. Only the one edited value
//! line changes (its separator normalized to the canonical `": "`).

use std::collections::BTreeMap;

use crate::error::{FilesError, Result};
use crate::id;

const FENCE: &str = "---";

/// A parsed view for the index projection. `headers` are key-sorted (the Bus
/// grammar) — fine for the index, which never reconstructs the file; the file on
/// disk stays the only authoritative copy of authored bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    /// Parsed header fields (raw string values; JSON values kept verbatim).
    pub headers: BTreeMap<String, String>,
    /// Document body (the Bus parser trims trailing whitespace).
    pub body: String,
    /// Non-fatal ingest warnings (non-canonical / skipped / invalid-JSON lines).
    pub warnings: Vec<String>,
}

/// True if `raw` opens with a `---` fence line *followed by a newline* (a
/// frontmatter block is present). The newline requirement keeps this in agreement
/// with [`header_block`]: a lone `"---"` with no newline cannot close a block.
pub fn has_frontmatter(raw: &str) -> bool {
    raw.split_once('\n')
        .is_some_and(|(first, _)| first.trim_end_matches('\r') == FENCE)
}

/// The dominant line ending of `s` (`"\r\n"` if any CRLF is present, else `"\n"`).
/// Used so an inserted/created header line matches the file's existing convention.
fn dominant_eol(s: &str) -> &'static str {
    if s.contains("\r\n") { "\r\n" } else { "\n" }
}

/// Read scalar fields via the canonical grammar. A document with no `---` fence is
/// treated as headerless (empty headers, whole text as body).
pub fn read(raw: &str) -> Result<Parsed> {
    if !has_frontmatter(raw) {
        return Ok(Parsed {
            headers: BTreeMap::new(),
            body: raw.trim_end().to_string(),
            warnings: Vec::new(),
        });
    }
    let (msg, report) =
        cosmix_bus::bus::parse_lenient(raw).map_err(|e| FilesError::Parse(e.to_string()))?;
    let warnings = if report.is_empty() {
        Vec::new()
    } else {
        vec!["non-canonical frontmatter (skipped or invalid-JSON header lines)".to_string()]
    };
    Ok(Parsed {
        headers: msg.headers,
        body: msg.body,
        warnings,
    })
}

/// Build the `frontmatter_json` index projection from parsed headers: a value
/// whose first non-whitespace char is `[` or `{` is parsed as JSON (per §5.5.1),
/// everything else is a JSON string. Invalid JSON falls back to a string.
pub fn headers_to_json(headers: &BTreeMap<String, String>) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for (k, v) in headers {
        let t = v.trim_start();
        let parsed = if t.starts_with('[') || t.starts_with('{') {
            serde_json::from_str::<serde_json::Value>(v).ok()
        } else {
            None
        };
        map.insert(
            k.clone(),
            parsed.unwrap_or_else(|| serde_json::Value::String(v.clone())),
        );
    }
    serde_json::Value::Object(map)
}

/// Get a single header field's raw value via a surgical scan (no full parse).
/// Returns `None` if there is no header block or the key is absent.
pub fn get_field(raw: &str, key: &str) -> Option<String> {
    let (hs, he) = header_block(raw).ok()?;
    for line in raw[hs..he].split('\n') {
        if let Some((k, v)) = split_header_line(line)
            && k == key
        {
            return Some(v.to_string());
        }
    }
    None
}

/// Set (or insert) a single header field, preserving every other byte.
///
/// - If the key exists, replace only that line's value (keeping the original key
///   text; the separator is normalized to `": "`).
/// - If the key is absent, append `key: value` as the last line of the header block.
/// - If there is no header block at all, create a minimal one at the top and treat
///   the whole prior content as the body.
///
/// Errors if `value` contains a newline (frontmatter values are single-line).
pub fn set_field(raw: &str, key: &str, value: &str) -> Result<String> {
    if value.contains('\n') {
        return Err(FilesError::MultilineValue(value.to_string()));
    }
    let eol = dominant_eol(raw);

    let (hs, he) = match header_block(raw) {
        Ok(span) => span,
        Err(FilesError::NoFrontmatter) => {
            // Create a fresh header block; the whole prior text becomes the body.
            let mut out = String::with_capacity(raw.len() + key.len() + value.len() + 12);
            out.push_str(FENCE);
            out.push_str(eol);
            out.push_str(key);
            out.push_str(": ");
            out.push_str(value);
            out.push_str(eol);
            out.push_str(FENCE);
            out.push_str(eol);
            out.push_str(raw);
            return Ok(out);
        }
        Err(e) => return Err(e),
    };

    let header = &raw[hs..he];
    let mut out = String::with_capacity(raw.len() + key.len() + value.len() + 4);
    out.push_str(&raw[..hs]);

    let mut replaced = false;
    // `header` is a run of "line<eol>" segments (it always ends with '\n' unless
    // empty). Each edited/inserted line reuses the file's existing EOL so a CRLF
    // document is not silently given a mixed line ending (invariant #2).
    for seg in header.split_inclusive('\n') {
        let line = seg.strip_suffix('\n').unwrap_or(seg);
        if !replaced
            && let Some((k, _)) = split_header_line(line)
            && k == key
        {
            let colon = line.find(':').expect("split_header_line implies a colon");
            let seg_eol = if seg.ends_with("\r\n") { "\r\n" } else { "\n" };
            out.push_str(&line[..colon]); // preserve the original key text exactly
            out.push_str(": ");
            out.push_str(value);
            out.push_str(seg_eol);
            replaced = true;
            continue;
        }
        out.push_str(seg);
    }

    if !replaced {
        // Append as the last header line, before the closing fence at `he`.
        out.push_str(key);
        out.push_str(": ");
        out.push_str(value);
        out.push_str(eol);
    }
    out.push_str(&raw[he..]); // closing fence + body, untouched
    Ok(out)
}

/// Ensure the document carries a valid opaque `id` (invariant #4). If absent or
/// invalid, mint a UUIDv7 and insert it. Returns `(content, id, minted)`; idempotent
/// when a valid id is already present (`minted == false`, content unchanged).
pub fn ensure_id(raw: &str) -> Result<(String, String, bool)> {
    if let Some(existing) = get_field(raw, "id") {
        let t = existing.trim();
        // Tolerate an author-quoted id (`id: "uuid"`): strip one matching pair so a
        // valid id is not needlessly re-minted (which would change the identity).
        let unquoted = t
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .unwrap_or(t);
        if id::is_valid_id(unquoted) {
            return Ok((raw.to_string(), unquoted.to_string(), false));
        }
    }
    let new = id::new_id();
    let out = set_field(raw, "id", &new)?;
    Ok((out, new, true))
}

/// The verbatim authored header block (the bytes between the fences, not including
/// the `---` lines), or `None` if the document has no frontmatter. Stored in the
/// index for audit / change-detection — never used to reconstruct the file.
pub fn raw_header(raw: &str) -> Option<String> {
    header_block(raw)
        .ok()
        .map(|(hs, he)| raw[hs..he].to_string())
}

/// Byte offsets of the header block *content* — `[start, end)` where `end` is the
/// start of the closing fence line. Content is a run of `line\n` segments (possibly
/// empty). Errors with `NoFrontmatter` when there is no opening fence, or
/// `Malformed` when the block is unterminated.
fn header_block(raw: &str) -> Result<(usize, usize)> {
    if !has_frontmatter(raw) {
        return Err(FilesError::NoFrontmatter);
    }
    let nl0 = raw.find('\n').ok_or(FilesError::NoFrontmatter)?;
    let header_start = nl0 + 1;

    let mut idx = header_start;
    loop {
        let rest = &raw[idx..];
        let line_end = rest.find('\n').map(|p| idx + p).unwrap_or(raw.len());
        let line = &raw[idx..line_end];
        if line.trim_end_matches('\r') == FENCE {
            return Ok((header_start, idx));
        }
        if line_end >= raw.len() {
            break;
        }
        idx = line_end + 1;
    }
    Err(FilesError::Malformed(
        "unterminated frontmatter (no closing ---)".to_string(),
    ))
}

/// Split a single header line into `(key, value)` per the flat grammar. Returns
/// `None` for comment / blank / list lines (anything without a bare-identifier key
/// before a `:`). The value keeps everything after the first `": "` verbatim.
fn split_header_line(line: &str) -> Option<(&str, &str)> {
    let line = line.trim_end_matches('\r');
    let colon = line.find(':')?;
    let key = line[..colon].trim();
    if key.is_empty() || key.contains(char::is_whitespace) {
        return None;
    }
    let mut val = &line[colon + 1..];
    if let Some(stripped) = val.strip_prefix(' ') {
        val = stripped;
    }
    Some((key, val))
}

#[cfg(test)]
mod tests {
    use super::*;

    // A representative file: id, plain value, JSON array, JSON object, a comment,
    // a blank line, a later scalar, then a body with both link syntaxes.
    const SAMPLE: &str = "---\nid: 0190aaaa-bbbb-7ccc-8ddd-eeeeffff0000\ntitle: Hello World\ntags: [\"rust\", \"mix\"]\nmeta: {\"draft\": true}\n# a comment\n\ncreated: 2026-06-29\n---\n# Body\n\nSee [[other-note]] and [link](other.md).\n";

    const NOID: &str = "---\ntitle: Untitled\n---\nbody\n";

    #[test]
    fn identity_write_is_byte_identical() {
        // Setting a field to its current value reproduces the file exactly.
        let out = set_field(SAMPLE, "title", "Hello World").unwrap();
        assert_eq!(out, SAMPLE);
    }

    #[test]
    fn edit_changes_only_the_target_line() {
        let out = set_field(SAMPLE, "title", "Changed").unwrap();
        let expected = SAMPLE.replace("title: Hello World", "title: Changed");
        assert_eq!(out, expected);
        // Everything else survived verbatim.
        assert!(out.contains("tags: [\"rust\", \"mix\"]"));
        assert!(out.contains("meta: {\"draft\": true}"));
        assert!(out.contains("# a comment"));
        assert!(out.contains("\n\ncreated: 2026-06-29"));
        assert!(out.contains("See [[other-note]] and [link](other.md)."));
    }

    #[test]
    fn get_field_reads_verbatim_including_json() {
        assert_eq!(
            get_field(SAMPLE, "tags").as_deref(),
            Some("[\"rust\", \"mix\"]")
        );
        assert_eq!(get_field(SAMPLE, "title").as_deref(), Some("Hello World"));
        assert_eq!(get_field(SAMPLE, "missing"), None);
    }

    #[test]
    fn absent_key_inserts_before_closing_fence() {
        let out = set_field(NOID, "slug", "x").unwrap();
        assert_eq!(out, "---\ntitle: Untitled\nslug: x\n---\nbody\n");
    }

    #[test]
    fn no_frontmatter_creates_a_block() {
        let out = set_field("plain body\n", "id", "X").unwrap();
        assert_eq!(out, "---\nid: X\n---\nplain body\n");
    }

    #[test]
    fn multiline_value_rejected() {
        assert!(matches!(
            set_field(SAMPLE, "title", "a\nb"),
            Err(FilesError::MultilineValue(_))
        ));
    }

    #[test]
    fn ensure_id_is_idempotent_when_present() {
        let (out, id_val, minted) = ensure_id(SAMPLE).unwrap();
        assert!(!minted);
        assert_eq!(out, SAMPLE);
        assert_eq!(id_val, "0190aaaa-bbbb-7ccc-8ddd-eeeeffff0000");
    }

    #[test]
    fn ensure_id_mints_and_inserts_when_absent() {
        let (out, id_val, minted) = ensure_id(NOID).unwrap();
        assert!(minted);
        assert!(id::is_valid_id(&id_val));
        assert_eq!(get_field(&out, "id").as_deref(), Some(id_val.as_str()));
        // The pre-existing field and body are intact.
        assert!(out.contains("title: Untitled"));
        assert!(out.ends_with("---\nbody\n"));
        // Second call is a no-op.
        let (out2, id2, minted2) = ensure_id(&out).unwrap();
        assert!(!minted2);
        assert_eq!(id2, id_val);
        assert_eq!(out2, out);
    }

    #[test]
    fn read_parses_headers_and_body() {
        let p = read(SAMPLE).unwrap();
        assert_eq!(
            p.headers.get("title").map(String::as_str),
            Some("Hello World")
        );
        assert!(p.body.contains("# Body"));
        // SAMPLE's header has a comment + blank line (non-canonical), so the
        // lenient reader flags them as warnings rather than silently dropping.
        assert!(!p.warnings.is_empty());
    }

    #[test]
    fn crlf_file_preserves_line_endings() {
        let raw = "---\r\nid: x\r\ntitle: Old\r\n---\r\nbody line\r\n";
        let out = set_field(raw, "title", "New").unwrap();
        assert_eq!(out, "---\r\nid: x\r\ntitle: New\r\n---\r\nbody line\r\n");
    }

    #[test]
    fn ensure_id_accepts_quoted_id() {
        let raw = "---\nid: \"0190aaaa-bbbb-7ccc-8ddd-eeeeffff0000\"\n---\nb\n";
        let (out, id_val, minted) = ensure_id(raw).unwrap();
        assert!(!minted);
        assert_eq!(out, raw);
        assert_eq!(id_val, "0190aaaa-bbbb-7ccc-8ddd-eeeeffff0000");
    }

    #[test]
    fn read_headerless_document() {
        let p = read("just text\nno fence\n").unwrap();
        assert!(p.headers.is_empty());
        assert_eq!(p.body, "just text\nno fence");
    }

    #[test]
    fn headers_to_json_typing() {
        let p = read(SAMPLE).unwrap();
        let j = headers_to_json(&p.headers);
        assert!(j["tags"].is_array());
        assert_eq!(j["tags"][0], "rust");
        assert!(j["title"].is_string());
    }

    #[test]
    fn raw_header_is_verbatim_block() {
        let h = raw_header(NOID).unwrap();
        assert_eq!(h, "title: Untitled\n");
        assert_eq!(raw_header("no fence here\n"), None);
    }
}
