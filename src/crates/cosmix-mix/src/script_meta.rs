//! `mix SCRIPT --version` — the Mix-script half of the fleet `--version`
//! contract (Mark 2026-09-25), plus the provenance record `script_version()`
//! reads.
//!
//! Answered from the same COLD position as `mix --version`: `main()` calls
//! [`script_version_request`] before any session lane, Bus dispatch, thread,
//! prelude or rc. The script is READ, never parsed or executed, so a script
//! with a syntax error still reports its version.
//!
//! Mix owns exactly one argv position: the first argument after the script
//! path. A script that wants its own `--version` semantics cannot have them
//! there; `mix SCRIPT x --version` still passes `--version` to the script.

use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use cosmix_mix::{ScriptProvenance, parse_version_header};
use sha2::{Digest, Sha256};

/// Leading interpreter flags that may precede a script path without changing
/// which file runs. `real_main` accepts them in any order before the script.
const NEUTRAL_FLAGS: &[&str] = &["--no-prelude", "--no-traceback", "--strict-arity"];

fn is_version_flag(arg: Option<&String>) -> bool {
    matches!(arg.map(String::as_str), Some("--version" | "-V"))
}

/// Where the script for a version query comes from.
#[derive(Debug, PartialEq, Eq)]
enum Target<'a> {
    File(&'a str),
    Stdin,
}

/// Recognise the three query shapes, after any [`NEUTRAL_FLAGS`]:
/// `mix SCRIPT --version`, `mix --serve SCRIPT --version`, `mix - --version`
/// (`-V` everywhere `--version` is accepted). `reserved` names the words
/// `real_main` dispatches as subcommands rather than script paths.
fn classify<'a>(args: &'a [String], reserved: &dyn Fn(&str) -> bool) -> Option<Target<'a>> {
    let mut i = 1;
    while args.get(i).is_some_and(|a| NEUTRAL_FLAGS.contains(&a.as_str())) {
        i += 1;
    }
    let first = args.get(i)?.as_str();
    match first {
        "--serve" => {
            let script = args.get(i + 1)?;
            is_version_flag(args.get(i + 2)).then_some(Target::File(script.as_str()))
        }
        "-" => is_version_flag(args.get(i + 1)).then_some(Target::Stdin),
        s if s.starts_with('-') || reserved(s) => None,
        s => is_version_flag(args.get(i + 1)).then_some(Target::File(s)),
    }
}

/// Build the provenance record for a script's bytes. `path` is `None` for
/// stdin (named `-`, no mtime).
pub(crate) fn provenance(
    path: Option<&str>,
    bytes: &[u8],
    mtime: Option<SystemTime>,
) -> ScriptProvenance {
    let bi = cosmix_buildinfo::build_info!();
    let name = match path {
        None => "-".to_string(),
        Some(p) => Path::new(p)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| p.to_string()),
    };
    let sha256 = Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let modified = mtime.map(|t| {
        chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    });
    let header = parse_version_header(&String::from_utf8_lossy(bytes));
    ScriptProvenance {
        name,
        version: header.version().map(str::to_string),
        sha256,
        modified,
        mix_version: crate::VERSION.to_string(),
        mix_sha: bi.git_sha.to_string(),
        mix_dirty: bi.git_dirty,
    }
}

/// Provenance for a script file already read into `source`.
pub(crate) fn provenance_for_file(path: &str, source: &str) -> ScriptProvenance {
    let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
    provenance(Some(path), source.as_bytes(), mtime)
}

/// Install the running entry script's provenance for `script_version()`;
/// returns the previous record (a reverted serve RELOAD restores it).
pub(crate) fn install(p: ScriptProvenance) -> Option<Arc<ScriptProvenance>> {
    cosmix_mix::replace_script_provenance(Some(Arc::new(p)))
}

/// Answer a script version query, or `None` when argv is not one.
/// `Some(Err(msg))` is an unreadable script — the caller prints it and
/// exits 1, the same outcome as trying to run that path.
pub(crate) fn script_version_request(
    args: &[String],
    reserved: &dyn Fn(&str) -> bool,
) -> Option<Result<String, String>> {
    Some(match classify(args, reserved)? {
        Target::Stdin => {
            let mut bytes = Vec::new();
            match std::io::stdin().read_to_end(&mut bytes) {
                Ok(_) => Ok(provenance(None, &bytes, None).version_line()),
                Err(e) => Err(format!("mix: error reading script from stdin: {e}")),
            }
        }
        Target::File(path) => match std::fs::read(path) {
            Ok(bytes) => {
                let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
                Ok(provenance(Some(path), &bytes, mtime).version_line())
            }
            Err(e) => Err(format!("Error reading '{path}': {e}")),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn cls(v: &[&str]) -> Option<String> {
        let a = argv(v);
        let reserved = |s: &str| s == "lint" || s == "help";
        classify(&a, &reserved).map(|t| format!("{t:?}"))
    }

    #[test]
    fn query_shapes() {
        assert_eq!(cls(&["mix", "a.mix", "--version"]).as_deref(), Some("File(\"a.mix\")"));
        assert_eq!(cls(&["mix", "a.mix", "-V"]).as_deref(), Some("File(\"a.mix\")"));
        assert_eq!(
            cls(&["mix", "--no-prelude", "--strict-arity", "a.mix", "--version"]).as_deref(),
            Some("File(\"a.mix\")")
        );
        assert_eq!(cls(&["mix", "--serve", "c.mix", "--version"]).as_deref(), Some("File(\"c.mix\")"));
        assert_eq!(cls(&["mix", "-", "--version"]).as_deref(), Some("Stdin"));
    }

    #[test]
    fn not_queries() {
        for v in [
            &["mix", "a.mix"][..],
            &["mix", "a.mix", "x", "--version"],
            &["mix", "--version"],
            &["mix", "-c", "print(1)", "--version"],
            &["mix", "lint", "--version"],
            &["mix", "help", "--version"],
            &["mix", "--serve", "c.mix", "--name", "x", "--version"],
            &["mix", "-", "x", "--version"],
            &["mix", "--check", "a.mix", "--version"],
        ] {
            assert_eq!(cls(v), None, "{v:?}");
        }
    }
}
