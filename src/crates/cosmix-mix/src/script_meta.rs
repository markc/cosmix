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
//! path. A script that wants its own `--version` semantics there declares
//! `-- version-flag: script` in its leading comment region, and is then run
//! with `--version` in `args()` like any other argument.

use std::io::Read;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use cosmix_mix::{ScriptProvenance, parse_script_header};
use sha2::{Digest, Sha256};

/// Leading interpreter flags that may precede a script path without changing
/// which file runs or that it runs as a script. Audited against the arms of
/// `real_main`'s flag loop that `continue` rather than return: `-i`,
/// `--no-prelude`, `--no-traceback`, `--strict-arity`. (`--result-fd` also
/// continues, but it refuses any mode except `-c`, so it never precedes a
/// script run.) A new `continue` arm there must be added here too.
pub(crate) const NEUTRAL_FLAGS: &[&str] = &["-i", "--no-prelude", "--no-traceback", "--strict-arity"];

fn is_version_flag(arg: Option<&String>) -> bool {
    matches!(arg.map(String::as_str), Some("--version" | "-V"))
}

/// Where the script for a version query comes from.
#[derive(Debug, PartialEq, Eq)]
enum Target<'a> {
    File(&'a str),
    Serve(&'a str),
    Stdin,
}

/// A recognised query: its target, and whether `--json` follows the flag.
#[derive(Debug, PartialEq, Eq)]
struct Query<'a> {
    target: Target<'a>,
    json: bool,
}

/// Recognise the three query shapes, after any [`NEUTRAL_FLAGS`]:
/// `mix SCRIPT --version`, `mix --serve SCRIPT --version`, `mix - --version`
/// (`-V` everywhere `--version` is accepted), each optionally followed by
/// `--json`. `reserved` names the words `real_main` dispatches as
/// subcommands rather than script paths.
fn classify<'a>(args: &'a [String], reserved: &dyn Fn(&str) -> bool) -> Option<Query<'a>> {
    let mut i = 1;
    while args.get(i).is_some_and(|a| NEUTRAL_FLAGS.contains(&a.as_str())) {
        i += 1;
    }
    let first = args.get(i)?.as_str();
    let (target, flag_at) = match first {
        "--serve" => (Target::Serve(args.get(i + 1)?.as_str()), i + 2),
        "-" => (Target::Stdin, i + 1),
        s if s.starts_with('-') || reserved(s) => return None,
        s => (Target::File(s), i + 1),
    };
    if !is_version_flag(args.get(flag_at)) {
        return None;
    }
    let json = args.get(flag_at + 1).map(String::as_str) == Some("--json");
    Some(Query { target, json })
}

/// Read a script file with ONE open: the bytes and the mtime come from the
/// same inode (fstat of the open fd), so a rename-over between two path
/// lookups cannot pair one file's hash with another's mtime. Through a
/// symlink that is the target's mtime.
pub(crate) fn read_script(path: &str) -> std::io::Result<(Vec<u8>, Option<SystemTime>)> {
    let mut file = std::fs::File::open(path)?;
    let mtime = file.metadata().and_then(|m| m.modified()).ok();
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok((bytes, mtime))
}

/// [`read_script`] as UTF-8 text plus its provenance — the script-run path.
/// Errors carry `read_to_string`'s wording for invalid UTF-8.
pub(crate) fn read_script_text(path: &str) -> std::io::Result<(String, ScriptProvenance)> {
    let (bytes, mtime) = read_script(path)?;
    let provenance = provenance(Some(path), &bytes, mtime);
    let text = String::from_utf8(bytes).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "stream did not contain valid UTF-8",
        )
    })?;
    Ok((text, provenance))
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
    let header = parse_script_header(&String::from_utf8_lossy(bytes));
    ScriptProvenance {
        name,
        version: header.version.version().map(str::to_string),
        sha256,
        modified,
        mix_version: crate::VERSION.to_string(),
        mix_sha: bi.git_sha.to_string(),
        mix_dirty: bi.git_dirty,
    }
}

/// Wrap a record for [`cosmix_mix::evaluator::Evaluator::set_script_provenance`].
pub(crate) fn shared(p: ScriptProvenance) -> Option<Arc<ScriptProvenance>> {
    Some(Arc::new(p))
}

/// `mix SCRIPT --version --json`: the `script_version()` map as one object.
fn provenance_json(p: &ScriptProvenance) -> serde_json::Value {
    serde_json::json!({
        "name": p.name,
        "version": p.version,
        "sha": p.sha12(),
        "sha256": p.sha256,
        "modified": p.modified,
        "mix": {"version": p.mix_version, "sha": p.mix_sha, "dirty": p.mix_dirty},
    })
}

/// Stdin the cold path had to read before it could see a
/// `-- version-flag: script` opt-out; the `mix -` arm runs these bytes
/// instead of reading an already-drained stdin.
static PREREAD_STDIN: Mutex<Option<Vec<u8>>> = Mutex::new(None);

/// Take the stdin bytes the cold path read, if it read any.
pub(crate) fn take_preread_stdin() -> Option<Vec<u8>> {
    PREREAD_STDIN.lock().ok().and_then(|mut g| g.take())
}

/// What `main()` should do with a possible script version query.
pub(crate) enum Answer {
    /// Not a query (or the script opted out): run normally.
    NotQuery,
    /// Print this line to stdout and exit 0.
    Print(String),
    /// Print this to stderr and exit 1 (unreadable script).
    Fail(String),
}

/// Answer a script version query. A script declaring
/// `-- version-flag: script` is not answered for (except under `--serve`,
/// where a daemon has no argv to hand the flag to).
pub(crate) fn script_version_request(args: &[String], reserved: &dyn Fn(&str) -> bool) -> Answer {
    let Some(query) = classify(args, reserved) else {
        return Answer::NotQuery;
    };
    let (path, bytes, mtime) = match query.target {
        Target::Stdin => {
            let mut bytes = Vec::new();
            if let Err(e) = std::io::stdin().read_to_end(&mut bytes) {
                return Answer::Fail(format!("mix: error reading script from stdin: {e}"));
            }
            (None, bytes, None)
        }
        Target::File(path) | Target::Serve(path) => match read_script(path) {
            Ok((bytes, mtime)) => (Some(path), bytes, mtime),
            Err(e) => return Answer::Fail(format!("Error reading '{path}': {e}")),
        },
    };
    let opted_out = parse_script_header(&String::from_utf8_lossy(&bytes)).version_flag_script;
    if opted_out && !matches!(query.target, Target::Serve(_)) {
        if matches!(query.target, Target::Stdin)
            && let Ok(mut slot) = PREREAD_STDIN.lock()
        {
            *slot = Some(bytes);
        }
        return Answer::NotQuery;
    }
    let p = provenance(path, &bytes, mtime);
    Answer::Print(if query.json {
        provenance_json(&p).to_string()
    } else {
        p.version_line()
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
        classify(&a, &reserved).map(|q| format!("{:?}{}", q.target, if q.json { "+json" } else { "" }))
    }

    #[test]
    fn query_shapes() {
        assert_eq!(cls(&["mix", "a.mix", "--version"]).as_deref(), Some("File(\"a.mix\")"));
        assert_eq!(cls(&["mix", "a.mix", "-V"]).as_deref(), Some("File(\"a.mix\")"));
        assert_eq!(
            cls(&["mix", "--no-prelude", "--strict-arity", "a.mix", "--version"]).as_deref(),
            Some("File(\"a.mix\")")
        );
        assert_eq!(cls(&["mix", "-i", "a.mix", "--version"]).as_deref(), Some("File(\"a.mix\")"));
        assert_eq!(cls(&["mix", "--serve", "c.mix", "--version"]).as_deref(), Some("Serve(\"c.mix\")"));
        assert_eq!(cls(&["mix", "-", "--version"]).as_deref(), Some("Stdin"));
        assert_eq!(cls(&["mix", "a.mix", "--version", "--json"]).as_deref(), Some("File(\"a.mix\")+json"));
        assert_eq!(cls(&["mix", "-", "-V", "--json"]).as_deref(), Some("Stdin+json"));
    }

    #[test]
    fn not_queries() {
        for v in [
            &["mix", "a.mix"][..],
            &["mix", "a.mix", "x", "--version"],
            &["mix", "--version"],
            &["mix", "-c", "print(1)", "--version"],
            &["mix", "-i", "-c", "print(1)", "--version"],
            &["mix", "lint", "--version"],
            &["mix", "help", "--version"],
            &["mix", "--serve", "c.mix", "--name", "x", "--version"],
            &["mix", "--serve", "c.mix", "--no-prelude", "--version"],
            &["mix", "-", "x", "--version"],
            &["mix", "--check", "a.mix", "--version"],
            &["mix", "--result-fd", "3", "a.mix", "--version"],
        ] {
            assert_eq!(cls(v), None, "{v:?}");
        }
    }
}
