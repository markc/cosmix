//! Frontend lint (ced E1 plan §4.10, D16): `mix lint --json -` on CAPTURED
//! bytes (stdin, cwd = the file's directory so relative `require()`
//! resolves), off the UI thread, for `mix` / `scene` / `mix-data` buffers on
//! open and after a save (and 1 s after the last edit under 1 MiB). Results
//! are tagged (`ResultTag`) and handed to `cosmix_edit_client::diag`.
//!
//! [`run`] blocks (it waits for the child); the app calls it through
//! [`spawn`], which runs it on its own short-lived thread and hands the
//! result back as a future, so the single-thread iced executor never waits on
//! a lint.
//!
//! `scene` buffers are linted **in-process** with `cosmix-scene` (parse →
//! lint → resolve, the registry Quoin validates with; Scene Editor plan D8):
//! `mix lint` reports a false `MIX-E1003` at the fence of every `scene.mix`.
//! The result takes the same `mix lint --json` report shape, so it reaches
//! `Diagnostics` as the lint source like every other language.

use std::io::Write;
use std::process::{Command, Stdio};

use cosmix_edit_client::highlight::ResultTag;

/// The lint binary (never a fallback: a missing binary is an error naming it).
pub const MIX: &str = "/opt/cosmix/bin/mix";

/// Buffers above this are linted only on save, never on the edit debounce.
pub const DEBOUNCE_MAX_BYTES: usize = 1024 * 1024;

/// The idle time after the last edit before a relint, ms.
pub const DEBOUNCE_MS: u64 = 1000;

/// Languages linted.
pub fn lints(language: &str) -> bool {
    matches!(language, "mix" | "scene" | "mix-data")
}

/// Run `mix lint --json -` over `text` with `cwd`; the raw JSON on success.
/// A `scene` buffer is linted in-process instead ([`scene_report`]).
pub fn run(tag: &ResultTag, text: &str, cwd: Option<&std::path::Path>) -> Result<String, String> {
    if tag.language == "scene" {
        return Ok(scene_report(text));
    }
    run_with(std::path::Path::new(MIX), tag, text, cwd)
}

/// One scene-lint finding: 1-based line (scene diagnostics carry no column).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SceneFinding {
    pub line: usize,
    pub error: bool,
    pub code: String,
    pub message: String,
}

/// `cosmix_scene::parse` → `lint` → `resolve` over `text`: every finding,
/// in the order the registry reports them. `resolve` runs only when lint
/// found no error (it would repeat lint's findings), and adds what only it
/// checks.
pub fn scene_findings(text: &str) -> Vec<SceneFinding> {
    let found = match cosmix_scene::parse(text) {
        Err(ds) => ds,
        Ok(doc) => {
            let mut ds = cosmix_scene::lint(&doc);
            if !ds.iter().any(|d| d.severity == cosmix_scene::Severity::Error)
                && let Err(more) = cosmix_scene::resolve(&doc)
            {
                for d in more {
                    if !ds.contains(&d) {
                        ds.push(d);
                    }
                }
            }
            ds
        }
    };
    found
        .into_iter()
        .map(|d| SceneFinding {
            line: d.line.max(1),
            error: d.severity == cosmix_scene::Severity::Error,
            code: d.code,
            message: d.message,
        })
        .collect()
}

/// [`scene_findings`] as a `mix lint --json` (schema 2) report.
pub fn scene_report(text: &str) -> String {
    let diagnostics: Vec<serde_json::Value> = scene_findings(text)
        .into_iter()
        .map(|f| {
            serde_json::json!({
                "file": "-", "line": f.line, "column": null, "code": f.code,
                "severity": if f.error { "error" } else { "warning" }, "message": f.message, "hint": null,
            })
        })
        .collect();
    serde_json::json!({"schema_version": 2, "tool": "cosmix-scene", "diagnostics": diagnostics}).to_string()
}

/// [`run`] with an explicit binary (tests).
pub fn run_with(
    mix: &std::path::Path,
    tag: &ResultTag,
    text: &str,
    cwd: Option<&std::path::Path>,
) -> Result<String, String> {
    if !mix.exists() {
        return Err(format!("lint: {} is not installed", mix.display()));
    }
    let mut command = Command::new(mix);
    command.args(["lint", "--json", "-"]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(dir) = cwd.filter(|d| d.is_dir()) {
        command.current_dir(dir);
    }
    let mut child = command.spawn().map_err(|e| format!("lint: spawning {}: {e}", mix.display()))?;
    // Feed stdin from its own thread: a large buffer would otherwise
    // deadlock against a child that fills its stdout pipe first.
    let mut stdin = child.stdin.take().expect("piped stdin");
    let bytes = text.as_bytes().to_vec();
    let feeder = std::thread::spawn(move || {
        let _ = stdin.write_all(&bytes);
    });
    let output = child.wait_with_output().map_err(|e| format!("lint: waiting for mix: {e}"))?;
    let _ = feeder.join();
    // 0 = clean, 1 = diagnostics (both carry the JSON report); 2 = usage or
    // internal failure.
    match output.status.code() {
        Some(0 | 1) => String::from_utf8(output.stdout).map_err(|_| "lint: mix printed non-UTF-8".to_owned()),
        code => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(format!(
                "lint of {} ({}) failed ({}): {}",
                tag.buffer,
                tag.language,
                code.map_or_else(|| "signal".to_owned(), |c| format!("exit {c}")),
                stderr.trim()
            ))
        }
    }
}

/// Run [`run`] on a dedicated thread; the future resolves with the tag and
/// the result.
pub fn spawn(
    tag: ResultTag,
    text: String,
    cwd: Option<std::path::PathBuf>,
) -> impl std::future::Future<Output = (ResultTag, Result<String, String>)> + Send + 'static {
    let (tx, rx) = iced::futures::channel::oneshot::channel();
    let own = tag.clone();
    let spawned = std::thread::Builder::new().name("ced-lint".into()).spawn(move || {
        let result = run(&tag, &text, cwd.as_deref());
        let _ = tx.send(result);
    });
    async move {
        let result = match spawned {
            Ok(_) => rx.await.unwrap_or_else(|_| Err("lint: the lint thread ended without a result".to_owned())),
            Err(error) => Err(format!("lint: cannot start a thread: {error}")),
        };
        (own, result)
    }
}

/// The lint settings hash that goes into `ResultTag.cfg` (the lint has no
/// settings yet beyond its binary; a change of binary invalidates results).
pub fn cfg_hash() -> u64 {
    let digest = blake3::hash(MIX.as_bytes());
    u64::from_le_bytes(digest.as_bytes()[..8].try_into().expect("8 bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag() -> ResultTag {
        ResultTag { epoch: "e".into(), buffer: "b1".into(), view_gen: 3, language: "mix".into(), cfg: cfg_hash() }
    }

    #[test]
    fn a_missing_binary_is_an_error_naming_it() {
        let err = run_with(std::path::Path::new("/nonexistent/mix"), &tag(), "x = 1\n", None).unwrap_err();
        assert!(err.contains("/nonexistent/mix"), "{err}");
    }

    /// Exit 1 (diagnostics) is a result, exit 2 an error, and the text
    /// arrives on stdin. The stand-in `mix` is `/bin/sh` running a script
    /// named `lint` in the cwd (`sh lint --json -`): executing a file this
    /// test just wrote races other tests' forks for ETXTBSY.
    #[test]
    fn exit_codes_and_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let fake = std::path::Path::new("/bin/sh");
        std::fs::write(
            dir.path().join("lint"),
            "[ \"$1 $2\" = \"--json -\" ] || exit 2\nbody=$(cat)\ncase \"$body\" in\n  bad*) echo nope >&2; exit 2;;\n  warn*) echo '{\"schema_version\":2,\"diagnostics\":[]}'; exit 1;;\n  *) printf '{\"schema_version\":2,\"diagnostics\":[],\"cwd\":\"%s\"}' \"$(pwd)\";;\nesac\n",
        )
        .unwrap();
        let ok = run_with(fake, &tag(), "x = 1\n", Some(dir.path())).unwrap();
        assert!(ok.contains(&*dir.path().to_string_lossy()), "cwd is the file's directory: {ok}");
        assert!(run_with(fake, &tag(), "warn\n", Some(dir.path())).unwrap().contains("schema_version"));
        let err = run_with(fake, &tag(), "bad\n", Some(dir.path())).unwrap_err();
        assert!(err.contains("exit 2") && err.contains("nope"), "{err}");
    }

    fn shipped_scenes() -> Vec<(std::path::PathBuf, String)> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../../share/scenes");
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&root).unwrap_or_else(|e| panic!("{}: {e}", root.display())) {
            let path = entry.unwrap().path().join("scene.mix");
            if path.is_file() {
                let text = std::fs::read_to_string(&path).unwrap();
                out.push((path, text));
            }
        }
        out.sort();
        out
    }

    fn scene_tag() -> ResultTag {
        ResultTag { language: "scene".into(), ..tag() }
    }

    /// The D8 regression: `mix lint` flagged the fence of every scene.mix
    /// (MIX-E1003). In-process, every shipped template is clean, whatever
    /// binary is (or is not) installed.
    #[test]
    fn every_shipped_scene_lints_clean_in_process() {
        let scenes = shipped_scenes();
        assert!(scenes.len() >= 6, "the five shipped templates and the editor: {scenes:?}");
        for (path, text) in scenes {
            let findings = scene_findings(&text);
            assert!(findings.is_empty(), "{}: {findings:?}", path.display());
            let report = run(&scene_tag(), &text, None).unwrap();
            let mut d = cosmix_edit_client::diag::Diagnostics::default();
            d.accept(&scene_tag(), scene_tag(), &text, &report, &[]).unwrap();
            assert!(d.items().is_empty(), "{}: {:?}", path.display(), d.items());
        }
    }

    #[test]
    fn a_binding_policy_error_lands_on_its_line() {
        let (_, text) = shipped_scenes().into_iter().find(|(p, _)| p.ends_with("calendar/scene.mix")).expect("calendar");
        let broken = text.replacen("\"text\":\"= $model.day\"", "\"text\":\"= $env.HOME\"", 1);
        assert_ne!(broken, text, "the calendar day binding is where the test expects it");
        let line = broken.lines().position(|l| l.starts_with("cal_day:")).unwrap() + 1;
        let findings = scene_findings(&broken);
        assert!(
            findings.iter().any(|f| f.code == "binding-policy" && f.error && f.line == line),
            "binding-policy on line {line}: {findings:?}"
        );
        let report = run(&scene_tag(), &broken, None).unwrap();
        let mut d = cosmix_edit_client::diag::Diagnostics::default();
        d.accept(&scene_tag(), scene_tag(), &broken, &report, &[]).unwrap();
        let hit = d.items().iter().find(|i| i.code == "binding-policy").expect("a binding-policy row");
        assert_eq!((hit.line, hit.source.as_str()), (line, cosmix_edit_client::diag::LINT_SOURCE));
        assert!(broken[hit.range.clone()].starts_with("cal_day:"), "the squiggle covers that line");
        // An envelope error (no fence at all) is reported, not a panic.
        assert!(scene_findings("not a scene").iter().any(|f| f.error));
    }

    #[test]
    fn only_mix_family_languages_lint() {
        assert!(lints("mix") && lints("scene") && lints("mix-data"));
        assert!(!lints("rust") && !lints("text"));
    }
}
