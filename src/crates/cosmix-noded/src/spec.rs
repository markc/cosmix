//! SPEC 07 §5.1–§5.2 — spec distribution.
//!
//! Reads `_spec/NN_*.md` chapters and serves them via `spec.get` and
//! `world.specs.NN` retained topics. Frontmatter scalars are surfaced
//! as Bus headers; list values and prose body are passed through as
//! the Bus body.
//!
//! Discovery: `--spec-dir` flag → `$COSMIX_SPEC_DIR` env →
//! `$COSMIX_SRC/_spec` → walk parents of `cwd` looking for `_spec/`.

use std::path::{Path, PathBuf};

use cosmix_bus::bus::BusMessage;
use serde_json::Value as Json;

/// Locate the `_spec/` directory.
///
/// Search order:
/// 1. Explicit `override` (CLI flag).
/// 2. `$COSMIX_SPEC_DIR`
/// 3. `$COSMIX_SRC/_spec`
/// 4. Walk up from cwd looking for a `_spec/` sibling to `Cargo.toml`.
pub fn locate_spec_dir(override_path: Option<&str>) -> Option<PathBuf> {
    if let Some(p) = override_path {
        let pb = PathBuf::from(p);
        if pb.is_dir() {
            return Some(pb);
        }
    }
    if let Ok(p) = std::env::var("COSMIX_SPEC_DIR") {
        let pb = PathBuf::from(p);
        if pb.is_dir() {
            return Some(pb);
        }
    }
    if let Ok(p) = std::env::var("COSMIX_SRC") {
        let pb = PathBuf::from(p).join("_spec");
        if pb.is_dir() {
            return Some(pb);
        }
    }
    let mut cur = std::env::current_dir().ok()?;
    for _ in 0..6 {
        let candidate = cur.join("_spec");
        if candidate.is_dir() {
            return Some(candidate);
        }
        if !cur.pop() {
            break;
        }
    }
    None
}

/// Available chapter numbers, sorted ascending.
pub fn available_chapters(spec_dir: &Path) -> Vec<u32> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(spec_dir) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let s = name.to_string_lossy();
        if let Some(n) = parse_chapter_prefix(&s) {
            out.push(n);
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Parse `NN_anything.md` → Some(NN). Files like `00_index.md` are
/// included; `00b_*.md` is skipped (duplicate chapter number with the
/// canonical `00_*.md`).
fn parse_chapter_prefix(filename: &str) -> Option<u32> {
    if !filename.ends_with(".md") {
        return None;
    }
    let stem = &filename[..filename.len() - 3];
    let underscore = stem.find('_')?;
    let prefix = &stem[..underscore];
    if !prefix.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    prefix.parse().ok()
}

/// Find the canonical file for a chapter number — the `NN_*.md` whose
/// prefix has no trailing letters (skip `01b_*.md`).
pub fn find_chapter(spec_dir: &Path, chapter: u32) -> Option<PathBuf> {
    let prefix = format!("{:02}_", chapter);
    let entries = std::fs::read_dir(spec_dir).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let s = name.to_string_lossy();
        if s.starts_with(&prefix) && s.ends_with(".md") {
            return Some(entry.path());
        }
    }
    None
}

/// Find a spec file by exact filename (with or without `.md` suffix).
///
/// `name` is treated as a single filename: any path separator, parent
/// reference (`..`), absolute path, or empty string is rejected. This
/// prevents an Bus caller from using `spec.get name="../../etc/passwd"`
/// to read files outside the spec directory.
pub fn find_by_name(spec_dir: &Path, name: &str) -> Option<PathBuf> {
    if name.is_empty() {
        return None;
    }
    if name.contains('/') || name.contains('\\') {
        return None;
    }
    if name == ".." || name == "." {
        return None;
    }
    // Reject any name that decomposes into more than one component, or
    // whose single component is a parent/root reference.
    let candidate = Path::new(name);
    let mut comps = candidate.components();
    let only = comps.next()?;
    if comps.next().is_some() {
        return None;
    }
    use std::path::Component;
    if !matches!(only, Component::Normal(_)) {
        return None;
    }

    let with_ext = if name.ends_with(".md") {
        name.to_string()
    } else {
        format!("{}.md", name)
    };
    let pb = spec_dir.join(&with_ext);
    if pb.is_file() { Some(pb) } else { None }
}

/// Read a spec file and return an BusMessage with frontmatter scalars as
/// headers and prose body as `body`. Multi-line / list frontmatter values
/// are flattened into a `frontmatter_extras` JSON header.
pub fn load_spec_file(path: &Path) -> std::io::Result<BusMessage> {
    let raw = std::fs::read_to_string(path)?;
    let (headers, body) = split_frontmatter(&raw);
    let mut msg = BusMessage::new();
    // Default command so retained-topic subscribers can dispatch via
    // `on spec do ... end`. Frontmatter `command:` (rare) wins over this.
    msg.set("command", "spec");
    for (k, v) in headers {
        msg.set(&k, &v);
    }
    msg.body = body;
    Ok(msg)
}

/// Split `---\n<frontmatter>\n---\n<body>` into a header vec and the
/// remaining body string. Only top-level scalar `key: value` lines from
/// the frontmatter become headers; list items and indented continuation
/// lines are dropped (the full source is still available via the
/// underlying file or `world.specs.<n>` retained topic body).
pub fn split_frontmatter(raw: &str) -> (Vec<(String, String)>, String) {
    let trimmed = raw.trim_start_matches('\u{feff}');
    if !trimmed.starts_with("---") {
        return (Vec::new(), raw.to_string());
    }
    let after_first = match trimmed.find('\n') {
        Some(i) => &trimmed[i + 1..],
        None => return (Vec::new(), String::new()),
    };
    let end_marker = match after_first.find("\n---") {
        Some(i) => i,
        None => return (Vec::new(), raw.to_string()),
    };
    let frontmatter = &after_first[..end_marker];
    let rest = &after_first[end_marker + 4..]; // skip "\n---"
    let rest = rest.strip_prefix('\n').unwrap_or(rest);

    let mut headers = Vec::new();
    for line in frontmatter.lines() {
        if line.is_empty() {
            continue;
        }
        // Skip indented continuation lines (lists, multi-line values).
        if line.starts_with(' ') || line.starts_with('\t') || line.starts_with('-') {
            continue;
        }
        if let Some(idx) = line.find(':') {
            let key = line[..idx].trim().to_string();
            let val = line[idx + 1..].trim().to_string();
            // Drop empty-value scalars (these introduce a list block).
            if val.is_empty() {
                continue;
            }
            // Strip surrounding quotes if present.
            let val = val.trim_matches('"').to_string();
            if !key.is_empty() {
                headers.push((key, val));
            }
        }
    }
    (headers, rest.to_string())
}

/// Outcome of `spec.get`. Body is the Bus message wire form (so it can be
/// dropped straight into a response body or retained-topic snapshot).
pub struct SpecResponse {
    pub rc: i32,
    pub headers: Vec<(String, String)>,
    pub body: String,
    pub error: Option<String>,
}

impl SpecResponse {
    fn ok(msg: BusMessage) -> Self {
        let headers: Vec<(String, String)> = msg
            .headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        Self {
            rc: 0,
            headers,
            body: msg.body,
            error: None,
        }
    }
    fn err(msg: impl Into<String>) -> Self {
        let m = msg.into();
        Self {
            rc: 10,
            headers: Vec::new(),
            body: format!(r#"{{"error":{}}}"#, json_escape(&m)),
            error: Some(m),
        }
    }
}

fn json_escape(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

/// Resolve a `spec.get` request. `args` is the JSON body or args header.
pub fn dispatch_spec_get(spec_dir: Option<&Path>, args: Option<&Json>) -> SpecResponse {
    let dir = match spec_dir {
        Some(d) => d,
        None => return SpecResponse::err("spec dir not configured"),
    };
    let path = match args {
        Some(Json::Object(_)) => {
            // Mix numerics arrive as JSON f64, so `as_u64` alone misses
            // integer-valued floats. Try u64 first, then i64, then f64.
            let ch_u = args.and_then(|v| v.get("chapter")).and_then(|v| {
                v.as_u64()
                    .or_else(|| v.as_i64().filter(|n| *n >= 0).map(|n| n as u64))
                    .or_else(|| {
                        v.as_f64()
                            .filter(|n| n.is_finite() && *n >= 0.0)
                            .map(|n| n as u64)
                    })
                    .or_else(|| v.as_str().and_then(|s| s.parse::<u64>().ok()))
            });
            if let Some(ch) = ch_u {
                find_chapter(dir, ch as u32)
            } else if let Some(name) = args.and_then(|v| v.get("name")).and_then(|v| v.as_str()) {
                find_by_name(dir, name)
            } else {
                return SpecResponse::err(format!(
                    "spec.get requires args.chapter or args.name; available: {:?}",
                    available_chapters(dir)
                ));
            }
        }
        _ => {
            return SpecResponse::err(format!(
                "spec.get requires args.chapter or args.name; available: {:?}",
                available_chapters(dir)
            ));
        }
    };
    match path {
        Some(p) => match load_spec_file(&p) {
            Ok(msg) => SpecResponse::ok(msg),
            Err(e) => SpecResponse::err(format!("read spec: {e}")),
        },
        None => SpecResponse::err(format!(
            "spec not found; available chapters: {:?}",
            available_chapters(dir)
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn parses_chapter_prefix() {
        assert_eq!(parse_chapter_prefix("07_self-aware.md"), Some(7));
        assert_eq!(parse_chapter_prefix("00_index.md"), Some(0));
        assert_eq!(parse_chapter_prefix("CHANGELOG.md"), None);
        assert_eq!(parse_chapter_prefix("01b_bus-ui.md"), None);
    }

    #[test]
    fn splits_frontmatter() {
        let raw = "---\ntitle: Foo\nchapter: 7\nversion: 0.1.0\ndraws_from:\n  - a\n  - b\n---\n# Body\n\nProse.";
        let (h, body) = split_frontmatter(raw);
        let map: std::collections::HashMap<_, _> = h.into_iter().collect();
        assert_eq!(map.get("title").map(String::as_str), Some("Foo"));
        assert_eq!(map.get("chapter").map(String::as_str), Some("7"));
        assert_eq!(map.get("version").map(String::as_str), Some("0.1.0"));
        // `draws_from:` has empty value → dropped, list items skipped.
        assert!(!map.contains_key("draws_from"));
        assert!(body.starts_with("# Body"));
    }

    #[test]
    fn find_by_name_rejects_path_traversal() {
        let tmp = std::env::temp_dir().join("cosmix-noded-spec-traversal-test");
        let _ = std::fs::create_dir_all(&tmp);
        // Sibling file outside the spec dir we must NOT reach.
        let outside = tmp.join("outside.md");
        std::fs::write(&outside, "secret").unwrap();
        let spec_dir = tmp.join("specs");
        let _ = std::fs::create_dir_all(&spec_dir);
        // Inside file — ordinary lookup must still work.
        std::fs::write(spec_dir.join("ok.md"), "ok").unwrap();

        assert!(find_by_name(&spec_dir, "ok").is_some());
        assert!(find_by_name(&spec_dir, "ok.md").is_some());

        // Traversal attempts.
        assert!(find_by_name(&spec_dir, "../outside").is_none());
        assert!(find_by_name(&spec_dir, "../outside.md").is_none());
        assert!(find_by_name(&spec_dir, "..").is_none());
        assert!(find_by_name(&spec_dir, "/etc/passwd").is_none());
        assert!(find_by_name(&spec_dir, "sub/file").is_none());
        assert!(find_by_name(&spec_dir, "").is_none());
        // Backslash on POSIX is a legal filename char but we reject it
        // defensively — keeps the contract uniform across platforms.
        assert!(find_by_name(&spec_dir, "..\\outside").is_none());

        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn loads_real_spec_file() {
        // Synthesize one in tmp.
        let tmp = std::env::temp_dir().join("cosmix-noded-spec-test.md");
        let mut f = std::fs::File::create(&tmp).unwrap();
        write!(f, "---\ntitle: Test\nchapter: 99\n---\n# Hi\n").unwrap();
        let msg = load_spec_file(&tmp).unwrap();
        assert_eq!(msg.get("title"), Some("Test"));
        assert_eq!(msg.get("chapter"), Some("99"));
        assert!(msg.body.starts_with("# Hi"));
        let _ = std::fs::remove_file(&tmp);
    }
}
