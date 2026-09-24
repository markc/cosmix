//! Script provenance — the Mix-script half of the fleet `--version` contract
//! (Mark 2026-09-25: "ALL binaries and mix script should emit a --version with
//! build details").
//!
//! A script declares its own version with ONE header form, somewhere in its
//! first [`HEADER_SCAN_LINES`] lines:
//!
//! ```text
//! -- version: 1.2.3
//! ```
//!
//! This module owns the pieces every consumer must agree on: the header
//! parser (read by `mix SCRIPT --version`, by `script_version()`, and by the
//! MIX-D3016 lint note), the provenance record the CLI installs for the
//! running entry script, and the `script_version()` builtin that reads it.
//! Hashing and file metadata are the CLI's job — this crate's default build
//! carries no hash dependency — so the record arrives fully computed.

use std::sync::{Arc, RwLock};

use crate::error::MixResult;
use crate::value::Value;
use indexmap::IndexMap;

/// How many leading lines are searched for the `-- version:` header. The
/// header belongs in the file's opening comment block; 32 lines leaves room
/// for a shebang and a paragraph of description without scanning the body.
pub const HEADER_SCAN_LINES: usize = 32;

/// What the header scan found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionHeader {
    /// A well-formed `-- version: X.Y.Z` on 1-based line `line`.
    Declared { version: String, line: usize },
    /// A `-- version:` line whose value is not `X.Y.Z` (with an optional
    /// `-pre` / `+build` suffix). Treated as undeclared at runtime; lint
    /// names the line so the typo is found rather than silently ignored.
    Malformed { raw: String, line: usize },
    /// No header in the scanned lines.
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

/// Scan the first [`HEADER_SCAN_LINES`] lines of `source` for the header.
/// The first `-- version:` line wins, well-formed or not. Text, not syntax:
/// the source is never lexed, so a script with a parse error still answers.
///
/// Accepted spelling, whitespace-tolerant: optional leading whitespace, `--`,
/// optional whitespace, the lowercase word `version`, optional whitespace,
/// `:`, then the value. `-- version 1.2.3` (no colon) is NOT a header — prose
/// comments say "version" too often for a colon-less form to be safe.
pub fn parse_version_header(source: &str) -> VersionHeader {
    for (idx, line) in source.lines().take(HEADER_SCAN_LINES).enumerate() {
        let Some(rest) = line.trim_start().strip_prefix("--") else {
            continue;
        };
        let Some(rest) = rest.trim_start().strip_prefix("version") else {
            continue;
        };
        let Some(value) = rest.trim_start().strip_prefix(':') else {
            continue;
        };
        let value = value.trim();
        let line = idx + 1;
        return if is_semver(value) {
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
    VersionHeader::Absent
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
    /// Basename of the script path; `-` for a script read from stdin.
    pub name: String,
    /// The declared `-- version:` value, `None` when absent or malformed.
    pub version: Option<String>,
    /// Full lowercase hex SHA-256 of the script's bytes.
    pub sha256: String,
    /// File mtime as RFC 3339 UTC; `None` for stdin.
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

/// The running entry script's provenance. Replaceable rather than
/// set-once: a `--serve` citizen's RELOAD swaps in a new script.
static PROVENANCE: RwLock<Option<Arc<ScriptProvenance>>> = RwLock::new(None);

/// Install (or clear) the running script's provenance; returns the previous
/// record so a reverted RELOAD can put it back.
pub fn replace_script_provenance(
    next: Option<Arc<ScriptProvenance>>,
) -> Option<Arc<ScriptProvenance>> {
    match PROVENANCE.write() {
        Ok(mut guard) => std::mem::replace(&mut *guard, next),
        Err(poisoned) => std::mem::replace(&mut *poisoned.into_inner(), next),
    }
}

/// The running script's provenance, if the CLI installed one.
pub fn script_provenance() -> Option<Arc<ScriptProvenance>> {
    match PROVENANCE.read() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

/// `script_version()` — the entry script's provenance map, or nil when no
/// script is running (REPL, `mix -c`, an embedder that never installed one).
pub(crate) fn builtin_script_version(_args: Vec<Value>) -> MixResult<Option<Value>> {
    Ok(Some(match script_provenance() {
        Some(p) => p.to_value(),
        None => Value::Nil,
    }))
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
        let mut late = "print(1)\n".repeat(HEADER_SCAN_LINES - 1);
        late.push_str("-- version: 3.0.0\n");
        assert_eq!(parse_version_header(&late), declared("3.0.0", HEADER_SCAN_LINES));
        let mut too_late = "print(1)\n".repeat(HEADER_SCAN_LINES);
        too_late.push_str("-- version: 3.0.0\n");
        assert_eq!(parse_version_header(&too_late), VersionHeader::Absent);
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
