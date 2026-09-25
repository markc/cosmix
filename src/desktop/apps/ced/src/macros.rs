//! Mix macros (ced E1 plan §4.9, D15 — cuttable to E1.1): `*.mix` files in
//! `<AppDirs ced>/config/macros/`, headers `-- ced-macro: <label>` and
//! optional `-- ced-key: <chord>`; run with `/opt/cosmix/bin/mix` in argv form
//! with `CED_BUFFER`, `CED_EPOCH`, `CED_REV`, `CED_PATH`, `CED_LANGUAGE`,
//! `CED_SEL_START`, `CED_SEL_END`, `CED_ORIGIN=agent:macro.<stem>`.
//!
//! A macro is an ordinary L2 client of the `edit` service (E0 D6): it reads
//! the environment, then drives `edit.*` itself under its own origin, so its
//! edits land in their own undo lane and show up in ced like any agent's.
//! ced waits for its pipeline to drain before starting one (so `CED_REV` and
//! the selection offsets are exact), streams the output to the Output panel,
//! and never falls back to another interpreter.

use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::keymap::{self, Chord};

/// The interpreter (no fallback).
pub const MIX: &str = "/opt/cosmix/bin/mix";

/// Header lines are read only from the leading comment block, at most this
/// many lines.
const HEADER_LINES: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MacroDef {
    /// File stem; the action id is `macro.<stem>`.
    pub stem: String,
    pub label: String,
    pub chord: Option<String>,
    pub path: std::path::PathBuf,
}

impl MacroDef {
    pub fn action_id(&self) -> String {
        format!("macro.{}", self.stem)
    }

    pub fn origin(&self) -> String {
        format!("agent:macro.{}", self.stem)
    }
}

/// A stem usable in an origin label (`agent:macro.<stem>`, ≤ 64 chars).
fn valid_stem(stem: &str) -> bool {
    !stem.is_empty() && stem.len() <= 58 && stem.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
}

/// Every macro in `dir`, sorted by label. Files without a `-- ced-macro:`
/// header, with a stem that cannot be an origin label, or unreadable, are
/// skipped. A `ced-key` chord that does not parse or collides with a built-in
/// chord, Ctrl+Alt+P (reserved), or an earlier macro is dropped — the macro
/// stays runnable from the menu. Use [`discover_with_notes`] to see why.
pub fn discover(dir: &std::path::Path) -> Vec<MacroDef> {
    discover_with_notes(dir).0
}

/// [`discover`] plus one note per skipped file or dropped chord.
pub fn discover_with_notes(dir: &Path) -> (Vec<MacroDef>, Vec<String>) {
    let mut notes = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (Vec::new(), notes);
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "mix") && p.is_file())
        .collect();
    paths.sort();
    let mut taken: Vec<Chord> = keymap::DEFAULT.iter().filter_map(|(c, _)| keymap::parse_chord(c)).collect();
    taken.extend(keymap::parse_chord("Ctrl+Alt+P"));
    let mut defs = Vec::new();
    for path in paths {
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_owned();
        if !valid_stem(&stem) {
            notes.push(format!("macro {}: the file name must be [A-Za-z0-9._-], ≤ 58 chars", path.display()));
            continue;
        }
        let Ok(file) = std::fs::File::open(&path) else {
            notes.push(format!("macro {}: unreadable", path.display()));
            continue;
        };
        let (mut label, mut chord) = (None, None);
        for line in std::io::BufReader::new(file).lines().take(HEADER_LINES).map_while(Result::ok) {
            let line = line.trim();
            if !line.starts_with("--") && !line.is_empty() {
                break;
            }
            if let Some(v) = line.strip_prefix("-- ced-macro:") {
                label = Some(v.trim().to_owned()).filter(|v| !v.is_empty());
            } else if let Some(v) = line.strip_prefix("-- ced-key:") {
                chord = Some(v.trim().to_owned());
            }
        }
        let Some(label) = label else { continue };
        let chord = chord.and_then(|text| match keymap::parse_chord(&text) {
            None => {
                notes.push(format!("macro {stem}: chord {text:?} does not parse"));
                None
            }
            Some(c) if taken.contains(&c) => {
                notes.push(format!("macro {stem}: chord {text} is already bound"));
                None
            }
            Some(c) => {
                taken.push(c);
                Some(text)
            }
        });
        defs.push(MacroDef { stem, label, chord, path });
    }
    defs.sort_by(|a, b| a.label.to_lowercase().cmp(&b.label.to_lowercase()).then(a.stem.cmp(&b.stem)));
    (defs, notes)
}

/// What the macro sees about the buffer it was started on (offsets at `rev`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MacroEnv {
    pub buffer: String,
    pub epoch: String,
    pub rev: u64,
    pub path: Option<String>,
    pub language: String,
    pub sel_start: usize,
    pub sel_end: usize,
}

impl MacroEnv {
    pub fn vars(&self, def: &MacroDef) -> Vec<(&'static str, String)> {
        vec![
            ("CED_BUFFER", self.buffer.clone()),
            ("CED_EPOCH", self.epoch.clone()),
            ("CED_REV", self.rev.to_string()),
            ("CED_PATH", self.path.clone().unwrap_or_default()),
            ("CED_LANGUAGE", self.language.clone()),
            ("CED_SEL_START", self.sel_start.to_string()),
            ("CED_SEL_END", self.sel_end.to_string()),
            ("CED_ORIGIN", def.origin()),
        ]
    }
}

/// One line of output, or the end of a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MacroEvent {
    Line { stem: String, text: String, stderr: bool },
    Exit { stem: String, code: Option<i32> },
    Failed { stem: String, error: String },
}

/// Start `def` and stream its output. The working directory is the buffer's
/// directory when it has a path.
pub fn spawn(def: &MacroDef, env: &MacroEnv) -> impl iced::futures::Stream<Item = MacroEvent> + Send + 'static {
    spawn_with(Path::new(MIX), def, env)
}

/// [`spawn`] with an explicit interpreter (tests).
pub fn spawn_with(mix: &Path, def: &MacroDef, env: &MacroEnv) -> impl iced::futures::Stream<Item = MacroEvent> + Send + 'static {
    let (tx, rx) = iced::futures::channel::mpsc::unbounded();
    let stem = def.stem.clone();
    if !mix.exists() {
        let _ = tx.unbounded_send(MacroEvent::Failed { stem, error: format!("{} is not installed", mix.display()) });
        return rx;
    }
    let mut command = Command::new(mix);
    command.arg(&def.path).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    command.envs(env.vars(def));
    if let Some(dir) = env.path.as_deref().and_then(|p| Path::new(p).parent()).filter(|d| d.is_dir()) {
        command.current_dir(dir);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let _ = tx.unbounded_send(MacroEvent::Failed { stem, error: format!("spawning {}: {error}", mix.display()) });
            return rx;
        }
    };
    let pipe = |reader: Box<dyn std::io::Read + Send>, stderr: bool, tx: iced::futures::channel::mpsc::UnboundedSender<MacroEvent>, stem: String| {
        std::thread::spawn(move || {
            for text in std::io::BufReader::new(reader).lines().map_while(Result::ok) {
                let _ = tx.unbounded_send(MacroEvent::Line { stem: stem.clone(), text, stderr });
            }
        })
    };
    let out = pipe(Box::new(child.stdout.take().expect("piped")), false, tx.clone(), stem.clone());
    let err = pipe(Box::new(child.stderr.take().expect("piped")), true, tx.clone(), stem.clone());
    std::thread::spawn(move || {
        let status = child.wait();
        let _ = out.join();
        let _ = err.join();
        let _ = tx.unbounded_send(match status {
            Ok(status) => MacroEvent::Exit { stem, code: status.code() },
            Err(error) => MacroEvent::Failed { stem, error: error.to_string() },
        });
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::futures::StreamExt;

    fn write(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn discovery_reads_headers_and_checks_chords() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        write(d, "upper.mix", "-- ced-macro: Uppercase selection\n-- ced-key: Ctrl+Alt+U\nprint(1)\n");
        write(d, "clash.mix", "-- ced-macro: Clash\n-- ced-key: Ctrl+S\n");
        write(d, "reserved.mix", "-- ced-macro: Reserved\n-- ced-key: Ctrl+Alt+P\n");
        write(d, "twice.mix", "-- ced-macro: Also U\n-- ced-key: Ctrl+Alt+U\n");
        write(d, "plain.mix", "print(\"no header\")\n");
        write(d, "late.mix", "print(1)\n-- ced-macro: Too late\n");
        write(d, "bad name.mix", "-- ced-macro: Spaces\n");
        write(d, "notes.txt", "-- ced-macro: Not mix\n");
        let (defs, notes) = discover_with_notes(d);
        let labels: Vec<_> = defs.iter().map(|m| m.label.as_str()).collect();
        assert_eq!(labels, ["Also U", "Clash", "Reserved", "Uppercase selection"]);
        let chord = |stem: &str| defs.iter().find(|m| m.stem == stem).unwrap().chord.clone();
        assert_eq!(chord("upper"), None, "`twice` sorts first by path and takes Ctrl+Alt+U");
        assert_eq!(chord("twice").as_deref(), Some("Ctrl+Alt+U"));
        assert_eq!(chord("clash"), None);
        assert_eq!(chord("reserved"), None);
        assert_eq!(notes.len(), 4, "{notes:?}");
        assert_eq!(defs[0].action_id(), "macro.twice");
        assert_eq!(defs[0].origin(), "agent:macro.twice");
        assert!(discover(&d.join("missing")).is_empty());
    }

    #[test]
    fn a_run_gets_the_environment_and_streams_output() {
        let dir = tempfile::tempdir().unwrap();
        // `/bin/sh` stands in for mix and runs the macro file itself: never
        // exec a file this test just wrote (ETXTBSY races other tests' forks).
        let fake = Path::new("/bin/sh");
        write(dir.path(), "m.mix", "echo \"$CED_BUFFER $CED_REV $CED_ORIGIN $CED_SEL_START-$CED_SEL_END\"\necho oops >&2\nexit 3\n");
        let def = MacroDef { stem: "m".into(), label: "M".into(), chord: None, path: dir.path().join("m.mix") };
        let env = MacroEnv {
            buffer: "b2_x".into(),
            epoch: "e".into(),
            rev: 7,
            path: None,
            language: "mix".into(),
            sel_start: 3,
            sel_end: 9,
        };
        let events: Vec<_> = iced::futures::executor::block_on(spawn_with(fake, &def, &env).collect());
        assert!(events.contains(&MacroEvent::Line { stem: "m".into(), text: "b2_x 7 agent:macro.m 3-9".into(), stderr: false }));
        assert!(events.contains(&MacroEvent::Line { stem: "m".into(), text: "oops".into(), stderr: true }));
        assert_eq!(events.last(), Some(&MacroEvent::Exit { stem: "m".into(), code: Some(3) }));
        let missing: Vec<_> = iced::futures::executor::block_on(spawn_with(Path::new("/nonexistent/mix"), &def, &env).collect());
        assert!(matches!(&missing[..], [MacroEvent::Failed { error, .. }] if error.contains("/nonexistent/mix")));
    }
}
