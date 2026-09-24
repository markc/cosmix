//! Script provenance — the Mix-script half of the fleet `--version` contract
//! (Mark 2026-09-25: "ALL binaries and mix script should emit a --version with
//! build details").
//!
//! A script declares its own version with ONE header form, in its LEADING
//! COMMENT REGION (see [`parse_script_header`]):
//!
//! ```text
//! -- version: 1.2.3
//! ```
//!
//! This module owns the pieces every consumer must agree on: the header
//! parser (read by `mix SCRIPT --version`, by the CLI when it builds the
//! record `script_version()` returns, and by the MIX-D3016 lint note) and the
//! provenance record itself. The record is installed PER EVALUATOR
//! ([`crate::Evaluator::set_script_provenance`]), so each `--serve`
//! generation answers for its own file. Hashing and file metadata are the
//! CLI's job — this crate's default build carries no hash dependency — so the
//! record arrives fully computed.

use crate::value::Value;
use indexmap::IndexMap;

/// How many leading lines are searched for header lines. The scan also stops
/// earlier, at the first line outside the leading comment region.
pub const HEADER_SCAN_LINES: usize = 32;

/// What the scan found for `-- version:`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionHeader {
    /// A well-formed `-- version: X.Y.Z` on 1-based line `line`.
    Declared { version: String, line: usize },
    /// A `-- version:` line whose value is not `X.Y.Z` (with an optional
    /// `-pre` / `+build` suffix). Treated as undeclared at runtime; lint
    /// names the line so the typo is found rather than silently ignored.
    Malformed { raw: String, line: usize },
    /// No header in the leading comment region.
    Absent,
}

impl VersionHeader {
    /// The declared version, if any.
    pub fn version(&self) -> Option<&str> {
        match self {
            VersionHeader::Declared { version, .. } => Some(version),
            _ => None,
        }
    }
}

/// Everything the leading comment region declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptHeader {
    /// The `-- version:` declaration.
    pub version: VersionHeader,
    /// `-- version-flag: script` was declared: the script answers `--version`
    /// itself, so `mix SCRIPT --version` runs it with `--version` in `args()`
    /// instead of answering on its behalf. For wrappers that forward argv.
    pub version_flag_script: bool,
}

/// Parse one comment line of the form `-- <key>: <value>` (whitespace-
/// tolerant, lowercase key, colon required). Returns the trimmed value.
fn header_value<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let rest = line.trim_start().strip_prefix("--")?;
    let rest = rest.trim_start().strip_prefix(key)?;
    let value = rest.trim_start().strip_prefix(':')?;
    Some(value.trim())
}

/// Scan the LEADING COMMENT REGION of `source`: an optional `#!` shebang on
/// line 1, then blank lines and `--` comment lines, at most
/// [`HEADER_SCAN_LINES`] lines. The scan stops at the first line that is none
/// of those, so a `-- version:` inside a heredoc or string further down — a
/// script that generates scripts — is never mistaken for this file's header.
/// Text, not syntax: the source is never lexed, so a script with a parse
/// error still answers.
///
/// Recognised lines, first occurrence of each wins:
/// - `-- version: X.Y.Z` — `-- version 1.2.3` (no colon) is prose, not a
///   header; `-- versioning: …` and `-- Version: …` are not headers either.
/// - `-- version-flag: script` — any other value is ignored.
pub fn parse_script_header(source: &str) -> ScriptHeader {
    let mut version = VersionHeader::Absent;
    let mut version_flag_script = false;
    let mut flag_seen = false;
    for (line_no, line) in leading_comment_lines(source) {
        if !flag_seen && let Some(value) = header_value(line, "version-flag") {
            flag_seen = true;
            version_flag_script = value == "script";
            continue;
        }
        if matches!(version, VersionHeader::Absent)
            && let Some(value) = header_value(line, "version")
        {
            let line = line_no;
            version = if is_semver(value) {
                VersionHeader::Declared {
                    version: value.to_string(),
                    line,
                }
            } else {
                VersionHeader::Malformed {
                    raw: value.to_string(),
                    line,
                }
            };
        }
    }
    ScriptHeader {
        version,
        version_flag_script,
    }
}

/// The `--` comment lines of the leading comment region, as (1-based line
/// number, line): skips a line-1 `#!` shebang and blank lines, stops at the
/// first line that is neither those nor a `--` comment, and never looks past
/// [`HEADER_SCAN_LINES`]. The one definition of "the header region", shared
/// by the header parser and lint.
pub fn leading_comment_lines(source: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    for (idx, line) in source.lines().take(HEADER_SCAN_LINES).enumerate() {
        let trimmed = line.trim_start();
        if (idx == 0 && line.starts_with("#!")) || trimmed.is_empty() {
            continue;
        }
        if !trimmed.starts_with("--") {
            break;
        }
        out.push((idx + 1, line));
    }
    out
}

/// The `-- version:` half of [`parse_script_header`].
pub fn parse_version_header(source: &str) -> VersionHeader {
    parse_script_header(source).version
}

/// `MAJOR.MINOR.PATCH` of ASCII digits, optionally followed by `-pre` and/or
/// `+build` made of `[0-9A-Za-z.-]`. Deliberately a shape check, not a full
/// SemVer 2.0 validator (leading zeros are tolerated).
fn is_semver(v: &str) -> bool {
    let (core, suffix) = match v.find(['-', '+']) {
        Some(at) => (&v[..at], &v[at..]),
        None => (v, ""),
    };
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() != 3
        || parts
            .iter()
            .any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()))
    {
        return false;
    }
    if suffix.is_empty() {
        return true;
    }
    // Each of `-pre` / `+build` needs a non-empty body.
    let body_ok = |s: &str| {
        !s.is_empty()
            && s
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
    };
    let (pre, build) = match suffix.strip_prefix('-') {
        Some(rest) => match rest.split_once('+') {
            Some((p, b)) => (Some(p), Some(b)),
            None => (Some(rest), None),
        },
        None => (None, suffix.strip_prefix('+')),
    };
    pre.is_none_or(body_ok) && build.is_none_or(|b| body_ok(b) && !b.contains('+'))
}

/// Provenance of the running entry script, computed by the CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptProvenance {
    /// Basename of the script path as given (a symlink's own name); `-` for
    /// a script read from stdin.
    pub name: String,
    /// The declared `-- version:` value, `None` when absent or malformed.
    pub version: Option<String>,
    /// Full lowercase hex SHA-256 of the script's bytes.
    pub sha256: String,
    /// mtime of the file the bytes were read from (a symlink's target), as
    /// RFC 3339 UTC; `None` for stdin.
    pub modified: Option<String>,
    /// The interpreter's version (`0.94.0`).
    pub mix_version: String,
    /// The interpreter's build sha (short).
    pub mix_sha: String,
    /// Whether the interpreter was built from a modified tree.
    pub mix_dirty: bool,
}

impl ScriptProvenance {
    /// First 12 hex digits of the content hash.
    pub fn sha12(&self) -> &str {
        &self.sha256[..self.sha256.len().min(12)]
    }

    /// The one-line `mix SCRIPT --version` answer:
    /// `name version (sha12, modified TIME; mix X.Y.Z (sha))`, with
    /// `unversioned` for a missing header and no `modified` for stdin.
    pub fn version_line(&self) -> String {
        let version = self.version.as_deref().unwrap_or("unversioned");
        let modified = match &self.modified {
            Some(m) => format!(", modified {m}"),
            None => String::new(),
        };
        let dirty = if self.mix_dirty { "-dirty" } else { "" };
        format!(
            "{} {} ({}{}; mix {} ({}{}))",
            self.name,
            version,
            self.sha12(),
            modified,
            self.mix_version,
            self.mix_sha,
            dirty
        )
    }

    /// The `script_version()` map.
    pub fn to_value(&self) -> Value {
        let opt = |s: &Option<String>| match s {
            Some(v) => Value::String(v.clone()),
            None => Value::Nil,
        };
        let mut mix = IndexMap::new();
        mix.insert("version".to_string(), Value::String(self.mix_version.clone()));
        mix.insert("sha".to_string(), Value::String(self.mix_sha.clone()));
        mix.insert("dirty".to_string(), Value::Bool(self.mix_dirty));
        let mut map = IndexMap::new();
        map.insert("name".to_string(), Value::String(self.name.clone()));
        map.insert("version".to_string(), opt(&self.version));
        map.insert("sha".to_string(), Value::String(self.sha12().to_string()));
        map.insert("sha256".to_string(), Value::String(self.sha256.clone()));
        map.insert("modified".to_string(), opt(&self.modified));
        map.insert("mix".to_string(), Value::map(mix));
        Value::map(map)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declared(v: &str, line: usize) -> VersionHeader {
        VersionHeader::Declared {
            version: v.to_string(),
            line,
        }
    }

    #[test]
    fn header_forms() {
        assert_eq!(parse_version_header("-- version: 1.2.3\n"), declared("1.2.3", 1));
        assert_eq!(
            parse_version_header("#!/opt/cosmix/bin/mix\n  --version:0.3.6  \n"),
            declared("0.3.6", 2)
        );
        assert_eq!(parse_version_header("--  version  :  2.0.0-rc.1+b7\n"), declared("2.0.0-rc.1+b7", 1));
        assert_eq!(parse_version_header("-- version: 1.0.0+build.5\n"), declared("1.0.0+build.5", 1));
        assert_eq!(
            parse_version_header("-- a tool\n\n--\n-- version: 4.0.0\n"),
            declared("4.0.0", 4)
        );
    }

    #[test]
    fn not_a_header() {
        // Colon-less, other words, other comment styles, code.
        for src in [
            "-- version 1.2.3\n",
            "-- versioning: 1.2.3\n",
            "-- Version: 1.2.3\n",
            "# version: 1.2.3\n",
            "$version = \"1.2.3\"\n",
            "",
        ] {
            assert_eq!(parse_version_header(src), VersionHeader::Absent, "{src:?}");
        }
    }

    #[test]
    fn scan_stops_at_the_first_code_line() {
        // A generator script: the header-shaped line is inside a heredoc.
        let src = "-- makes scripts\n$out = <<EOF\n-- version: 9.9.9\nEOF\nprint($out)\n";
        assert_eq!(parse_version_header(src), VersionHeader::Absent);
        // Same inside a multi-line string.
        let src = "print(\"x\n-- version: 9.9.9\n\")\n";
        assert_eq!(parse_version_header(src), VersionHeader::Absent);
        // A shebang only counts on line 1.
        let src = "-- a\n#!/bin/mix\n-- version: 1.0.0\n";
        assert_eq!(parse_version_header(src), VersionHeader::Absent);
    }

    #[test]
    fn malformed_values_are_named() {
        for (src, raw) in [
            ("-- version: 1.2\n", "1.2"),
            ("-- version: v1.2.3\n", "v1.2.3"),
            ("-- version:\n", ""),
            ("-- version: 1.2.3 beta\n", "1.2.3 beta"),
            ("-- version: 1.2.3-\n", "1.2.3-"),
            ("-- version: 1.2.3+a+b\n", "1.2.3+a+b"),
        ] {
            assert_eq!(
                parse_version_header(src),
                VersionHeader::Malformed {
                    raw: raw.to_string(),
                    line: 1
                },
                "{src:?}"
            );
        }
    }

    #[test]
    fn first_header_wins_and_scan_is_bounded() {
        assert_eq!(
            parse_version_header("-- version: 1.0.0\n-- version: 2.0.0\n"),
            declared("1.0.0", 1)
        );
        let mut late = "--\n".repeat(HEADER_SCAN_LINES - 1);
        late.push_str("-- version: 3.0.0\n");
        assert_eq!(parse_version_header(&late), declared("3.0.0", HEADER_SCAN_LINES));
        let mut too_late = "--\n".repeat(HEADER_SCAN_LINES);
        too_late.push_str("-- version: 3.0.0\n");
        assert_eq!(parse_version_header(&too_late), VersionHeader::Absent);
    }

    #[test]
    fn version_flag_opt_out() {
        let h = parse_script_header("#!/usr/bin/env mix\n-- version: 1.0.0\n-- version-flag: script\n");
        assert_eq!(h.version, declared("1.0.0", 2));
        assert!(h.version_flag_script);
        // Whitespace-tolerant, exact value, and it does not shadow version.
        assert!(parse_script_header("--version-flag :  script \n").version_flag_script);
        assert!(!parse_script_header("-- version-flag: mix\n").version_flag_script);
        assert!(!parse_script_header("-- version-flag script\n").version_flag_script);
        assert!(!parse_script_header("print(1)\n-- version-flag: script\n").version_flag_script);
        assert_eq!(
            parse_script_header("-- version-flag: script\n").version,
            VersionHeader::Absent
        );
    }

    #[test]
    fn version_line_shapes() {
        let mut p = ScriptProvenance {
            name: "deploy.mix".into(),
            version: Some("1.2.3".into()),
            sha256: "0123456789abcdef".repeat(4),
            modified: Some("2026-09-25T01:02:03Z".into()),
            mix_version: "0.94.0".into(),
            mix_sha: "abc1234".into(),
            mix_dirty: false,
        };
        assert_eq!(
            p.version_line(),
            "deploy.mix 1.2.3 (0123456789ab, modified 2026-09-25T01:02:03Z; mix 0.94.0 (abc1234))"
        );
        p.version = None;
        p.modified = None;
        p.name = "-".into();
        p.mix_dirty = true;
        assert_eq!(
            p.version_line(),
            "- unversioned (0123456789ab; mix 0.94.0 (abc1234-dirty))"
        );
    }
}
