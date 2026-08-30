//! The Bus file-load verbs — `app.song.load` and `app.soundfont.load` — plus
//! their shared constrained-root path authorisation. Song load drives the
//! transactional [`crate::song_load::load_one`]; soundfont load re-voices live
//! via [`crate::song_load::apply_soundfont`] (transport preserved).
//!
//! Trust model (owner-ratified): a remote caller may only load a file WITHIN
//! owner-configured song roots (and follow an embedded soundfont only within
//! soundfont roots). The local file picker is a separate, always-allowed grant
//! path. With no roots configured, every remote load is denied.

use std::path::{Component, Path, PathBuf};

use bevy::prelude::*;
use ctk::prelude::{AppPortReply, AppPortRequest, PianoRollModel};
use serde_json::json;

use crate::action::AudioIntent;
use crate::editor::SongEditor;
use crate::song_load::{load_one, SongLoadErrCode, SoundfontOutcome};
use crate::views::StatusLine;

/// The exact verbs this module answers.
pub const SONG_LOAD_VERB: &str = "app.song.load";
pub const SOUNDFONT_LOAD_VERB: &str = "app.soundfont.load";

/// The status-line provenance badge for Bus-driven document changes, so the
/// operator sees a remote agent's loads on the same feedback channel as their
/// own — mirrors `action::source_prefix` for the `Bus` source (ASCII: the UI
/// font has no `·`).
const BUS_TAG: &str = "[BUS]  ";

/// The last path component for a concise status line ("travels.mid"), falling
/// back to the whole string when there is no separator.
fn display_name(raw: &str) -> &str {
    raw.rsplit('/').find(|s| !s.is_empty()).unwrap_or(raw)
}

/// Song document formats a remote load may name (same allowlist as the picker).
const SONG_EXTS: &[&str] = &["mid", "midi", "asc", "json", "oxm"];
/// SoundFont formats a remote swap may name.
const SOUNDFONT_EXTS: &[&str] = &["sf2", "sf3"];

/// Owner-configured roots for remote (Bus) loads. Empty = remote loads denied.
#[derive(Resource, Default, Clone)]
pub struct SongBusPolicy {
    pub song_roots: Vec<PathBuf>,
    pub soundfont_roots: Vec<PathBuf>,
}

impl SongBusPolicy {
    /// From the CLI: `--song-root DIR` and `--soundfont-root DIR`, each
    /// repeatable. No `--song-root` = every remote load is denied.
    pub fn from_args(args: &[String]) -> Self {
        SongBusPolicy {
            song_roots: collect_flag(args, "--song-root"),
            soundfont_roots: collect_flag(args, "--soundfont-root"),
        }
    }
}

fn collect_flag(args: &[String], flag: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut index = 0;
    while index < args.len() {
        if args[index] == flag {
            if let Some(value) = args.get(index + 1).filter(|value| !value.starts_with("--")) {
                out.push(PathBuf::from(value));
            }
            index += 2;
        } else {
            index += 1;
        }
    }
    out
}

/// Resolve a root-relative request path within a set of authorised roots — the
/// single security implementation shared by song and soundfont loads. Rejects
/// absolute paths and any `..`, canonicalises root + candidate, then requires an
/// ancestor match, a regular file, and an allowed extension. Canonicalisation
/// resolves symlinks, so the ancestor check defeats a symlink pointing outside
/// the root as well as lexical traversal. Returns the canonical path or a denial.
fn authorize_within(
    roots: &[PathBuf],
    exts: &[&str],
    kind: &str,
    rel: &str,
) -> Result<PathBuf, String> {
    let rel_path = Path::new(rel);
    if rel_path.as_os_str().is_empty() {
        return Err("empty path".into());
    }
    if rel_path.is_absolute() {
        return Err("path must be root-relative, not absolute".into());
    }
    if rel_path
        .components()
        .any(|component| matches!(component, Component::ParentDir | Component::Prefix(_)))
    {
        return Err("path must not contain ..".into());
    }
    let ext_ok = rel_path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| exts.contains(&ext.to_ascii_lowercase().as_str()));
    if !ext_ok {
        return Err(format!(
            "unsupported {kind} extension (want {})",
            exts.join("/")
        ));
    }
    for root in roots {
        let Ok(root_canon) = root.canonicalize() else {
            continue;
        };
        let Ok(candidate) = root_canon.join(rel_path).canonicalize() else {
            continue;
        };
        if candidate.starts_with(&root_canon) && candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(format!("path is outside the authorised {kind} roots"))
}

/// Authorise a song-document path within the song roots.
pub fn authorize_song_path(policy: &SongBusPolicy, rel: &str) -> Result<PathBuf, String> {
    authorize_within(&policy.song_roots, SONG_EXTS, "song", rel)
}

/// Authorise a soundfont path within the soundfont roots.
pub fn authorize_soundfont_path(policy: &SongBusPolicy, rel: &str) -> Result<PathBuf, String> {
    authorize_within(&policy.soundfont_roots, SOUNDFONT_EXTS, "soundfont", rel)
}

/// Whether an embedded soundfont path resolves inside an authorised soundfont
/// root. A denied font is never opened; the load keeps the current font.
pub fn soundfont_authorized(policy: &SongBusPolicy, path: &Path) -> bool {
    let Ok(canon) = path.canonicalize() else {
        return false;
    };
    policy.soundfont_roots.iter().any(|root| {
        root.canonicalize()
            .map(|root_canon| canon.starts_with(&root_canon))
            .unwrap_or(false)
    })
}

fn error_reply(rc: u8, message: &str, code: &str) -> AppPortReply {
    (rc, json!({ "error": message, "code": code }).to_string())
}

/// The requested path: header `path=` wins over a JSON body `{"path": …}`
/// (parity with `app.controls.*`).
fn request_path(request: &AppPortRequest) -> Option<String> {
    request.request.headers.get("path").cloned().or_else(|| {
        serde_json::from_str::<serde_json::Value>(&request.request.body)
            .ok()
            .and_then(|body| {
                body.get("path")
                    .and_then(|path| path.as_str())
                    .map(str::to_string)
            })
    })
}

/// The `app.song.load` verb handler. The Bus bridge only forwards a CORRELATED
/// request (an id-less send is dropped before dispatch), so every invocation
/// can answer. The load runs synchronously through the shared `load_one`, so
/// `rc=0` means the document is committed and the RT bank is submitted with the
/// stop-at-zero barrier guaranteed for its first block.
///
/// Accepted cost: the parse→build→submit runs inline on the Bevy thread, so a
/// remote peer repeatedly loading a large AUTHORISED file adds per-call frame
/// latency (`rc=11` bounds concurrent overlap, not sequential repeats). This is
/// acceptable because the file must live under an owner-configured root; a
/// deferred/off-thread build is a later optimisation if it ever bites.
pub fn song_load_verb(
    In(request): In<AppPortRequest>,
    policy: Res<SongBusPolicy>,
    editor: Option<ResMut<SongEditor>>,
    model: Option<ResMut<PianoRollModel>>,
    mut audio: MessageWriter<AudioIntent>,
    mut status: Option<ResMut<StatusLine>>,
) -> AppPortReply {
    let Some(raw) = request_path(&request) else {
        return error_reply(10, "missing path", "path_missing");
    };

    let path = match authorize_song_path(&policy, &raw) {
        Ok(path) => path,
        Err(reason) => {
            warn!(from = request.request.from.as_str(), path = raw.as_str(), %reason, "app.song.load denied");
            if let Some(status) = status.as_deref_mut() {
                status.error(format!("{BUS_TAG}Load denied: {}", display_name(&raw)));
            }
            return error_reply(10, &reason, "path_denied");
        }
    };

    let (Some(mut editor), Some(mut model)) = (editor, model) else {
        return error_reply(10, "no song document in this session", "no_document");
    };

    let policy_snapshot = policy.clone();
    let result = load_one(&mut editor, &mut model, &path, |font| {
        soundfont_authorized(&policy_snapshot, font)
    });

    match result {
        Ok(ok) => {
            audio.write(AudioIntent::Reset);
            let soundfont = match ok.soundfont {
                SoundfontOutcome::Loaded => "loaded",
                SoundfontOutcome::KeptCurrent => "kept_current",
            };
            info!(from = request.request.from.as_str(), path = %path.display(), "app.song.load ok");
            if let Some(status) = status.as_deref_mut() {
                match ok.warnings.first() {
                    Some(warning) => status.error(format!(
                        "{BUS_TAG}Loaded {} — {warning}",
                        display_name(&raw)
                    )),
                    None => status.ok(format!("{BUS_TAG}Loaded {}", display_name(&raw))),
                }
            }
            (
                0,
                json!({
                    "path": raw,
                    "loaded": true,
                    "soundfont": soundfont,
                    "transport": {
                        "target": "stopped",
                        "position": 0.0,
                        "barrier": "before-first-render",
                    },
                    "warnings": ok.warnings,
                })
                .to_string(),
            )
        }
        Err(error) => {
            let (rc, code) = match error.code {
                SongLoadErrCode::Load => (10, "load"),
                SongLoadErrCode::Busy => (11, "busy"),
            };
            // The internal message can carry the resolved ABSOLUTE on-disk path
            // (it begins with an owner song-root) — the operator log gets that
            // detail, but the remote caller gets only the relative path it sent,
            // never the server's filesystem layout.
            warn!(from = request.request.from.as_str(), path = %path.display(), code, detail = error.message.as_str(), "app.song.load failed");
            let public = match error.code {
                SongLoadErrCode::Load => format!("could not load {raw}"),
                SongLoadErrCode::Busy => "song load is busy; retry".to_string(),
            };
            if let Some(status) = status.as_deref_mut() {
                status.error(format!("{BUS_TAG}{public}"));
            }
            error_reply(rc, &public, code)
        }
    }
}

/// The `app.soundfont.load` verb: swap the active soundfont for one inside the
/// authorised soundfont roots. The re-voice ships as an EDIT, so playback is
/// preserved — the font changes live. No transport reset.
pub fn soundfont_load_verb(
    In(request): In<AppPortRequest>,
    policy: Res<SongBusPolicy>,
    editor: Option<ResMut<SongEditor>>,
    model: Option<ResMut<PianoRollModel>>,
    mut status: Option<ResMut<StatusLine>>,
) -> AppPortReply {
    let Some(raw) = request_path(&request) else {
        return error_reply(10, "missing path", "path_missing");
    };
    let path = match authorize_soundfont_path(&policy, &raw) {
        Ok(path) => path,
        Err(reason) => {
            warn!(from = request.request.from.as_str(), path = raw.as_str(), %reason, "app.soundfont.load denied");
            if let Some(status) = status.as_deref_mut() {
                status.error(format!("{BUS_TAG}SoundFont denied: {}", display_name(&raw)));
            }
            return error_reply(10, &reason, "path_denied");
        }
    };
    let (Some(mut editor), Some(mut model)) = (editor, model) else {
        return error_reply(10, "no song document in this session", "no_document");
    };
    match crate::song_load::apply_soundfont(&mut editor, &mut model, &path) {
        Ok(()) => {
            info!(from = request.request.from.as_str(), path = %path.display(), "app.soundfont.load ok");
            if let Some(status) = status.as_deref_mut() {
                status.ok(format!("{BUS_TAG}SoundFont: {}", display_name(&raw)));
            }
            (0, json!({ "path": raw, "loaded": true }).to_string())
        }
        Err(detail) => {
            // The internal message can carry the absolute path — log it, but
            // return only the caller's relative path.
            warn!(from = request.request.from.as_str(), path = %path.display(), detail = detail.as_str(), "app.soundfont.load failed");
            if let Some(status) = status.as_deref_mut() {
                status.error(format!(
                    "{BUS_TAG}could not load soundfont {}",
                    display_name(&raw)
                ));
            }
            error_reply(10, &format!("could not load soundfont {raw}"), "load")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("studio-song-bus-{}-{}", std::process::id(), name));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn authorises_a_song_inside_a_root() {
        let root = temp_root("ok");
        let song = root.join("tune.json");
        cosmix_song::Song::new("t").save_to_file(&song).unwrap();
        let policy = SongBusPolicy {
            song_roots: vec![root.clone()],
            soundfont_roots: vec![],
        };
        assert_eq!(
            authorize_song_path(&policy, "tune.json").unwrap(),
            song.canonicalize().unwrap()
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn rejects_traversal_absolute_and_bad_extension() {
        let root = temp_root("deny");
        cosmix_song::Song::new("t")
            .save_to_file(root.join("tune.json"))
            .unwrap();
        let policy = SongBusPolicy {
            song_roots: vec![root.clone()],
            soundfont_roots: vec![],
        };
        assert!(authorize_song_path(&policy, "../etc/passwd.json").is_err());
        assert!(authorize_song_path(&policy, "/etc/passwd.json").is_err());
        assert!(authorize_song_path(&policy, "tune.exe").is_err());
        assert!(authorize_song_path(&policy, "missing.json").is_err());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn no_roots_denies_everything() {
        let policy = SongBusPolicy::default();
        assert!(authorize_song_path(&policy, "anything.json").is_err());
    }

    #[test]
    #[cfg(unix)]
    fn rejects_a_symlink_inside_a_root_pointing_outside() {
        // The load-bearing defense: a symlink INSIDE a song-root that points
        // outside it must be denied — canonicalize resolves the link, and the
        // component-wise ancestor check then fails. (Lexical `../` never appears.)
        let root = temp_root("symlink-root");
        let outside = temp_root("symlink-outside");
        cosmix_song::Song::new("secret")
            .save_to_file(outside.join("secret.json"))
            .unwrap();
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
        let policy = SongBusPolicy {
            song_roots: vec![root.clone()],
            soundfont_roots: vec![],
        };
        assert!(
            authorize_song_path(&policy, "escape/secret.json").is_err(),
            "a symlink escaping the root must be denied"
        );
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&outside).ok();
    }

    fn run_verb_system<M>(
        app: &mut App,
        command: &str,
        system: impl IntoSystem<In<AppPortRequest>, AppPortReply, M> + 'static,
        body: &str,
    ) -> AppPortReply {
        use ctk::prelude::InboundRequest;
        use std::collections::BTreeMap;
        let system = app.world_mut().register_system(system);
        let request = AppPortRequest {
            request: InboundRequest {
                connection_generation: 1,
                from: "peer.mesh".into(),
                command: command.into(),
                body: body.into(),
                headers: BTreeMap::new(),
                reply_id: Some("1".into()),
            },
            app_name: "studio-bevy-1".into(),
        };
        app.world_mut().run_system_with(system, request).unwrap()
    }

    fn run_verb(app: &mut App, body: &str) -> AppPortReply {
        run_verb_system(app, SONG_LOAD_VERB, song_load_verb, body)
    }

    fn verb_app(policy: SongBusPolicy, editor: crate::editor::SongEditor) -> App {
        let mut app = App::new();
        app.add_message::<crate::action::AudioIntent>();
        app.insert_resource(policy);
        app.insert_resource(editor);
        app.init_resource::<PianoRollModel>();
        app
    }

    #[test]
    fn verb_loads_an_authorised_song_and_replies_ok() {
        let root = temp_root("verb-ok");
        cosmix_song::Song::new("loaded via bus")
            .save_to_file(root.join("tune.json"))
            .unwrap();
        let mut app = verb_app(
            SongBusPolicy {
                song_roots: vec![root.clone()],
                soundfont_roots: vec![],
            },
            crate::editor::test_editor("old song"),
        );

        let (rc, body) = run_verb(&mut app, r#"{"path":"tune.json"}"#);

        assert_eq!(rc, 0, "authorised load replies rc 0: {body}");
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["loaded"], true);
        assert_eq!(parsed["transport"]["target"], "stopped");
        assert_eq!(parsed["transport"]["barrier"], "before-first-render");
        assert_eq!(
            app.world().resource::<SongEditor>().song().name,
            "loaded via bus",
            "the document committed"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn verb_denies_a_path_outside_the_roots() {
        let root = temp_root("verb-deny");
        let mut app = verb_app(
            SongBusPolicy {
                song_roots: vec![root.clone()],
                soundfont_roots: vec![],
            },
            crate::editor::test_editor("untouched"),
        );

        let (rc, body) = run_verb(&mut app, r#"{"path":"../secret.json"}"#);

        assert_eq!(rc, 10);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["code"],
            "path_denied"
        );
        assert_eq!(
            app.world().resource::<SongEditor>().song().name,
            "untouched",
            "a denied load never touches the document"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn verb_reports_busy_when_the_swap_ring_is_full() {
        let root = temp_root("verb-busy");
        cosmix_song::Song::new("attempted")
            .save_to_file(root.join("tune.json"))
            .unwrap();
        let mut editor = crate::editor::test_editor("old song");
        for _ in 0..2 {
            let bank = editor
                .build_bank(&cosmix_song::Song::new("filler"), None)
                .unwrap();
            editor.submit_bank(bank).unwrap();
        }
        let mut app = verb_app(
            SongBusPolicy {
                song_roots: vec![root.clone()],
                soundfont_roots: vec![],
            },
            editor,
        );

        let (rc, body) = run_verb(&mut app, r#"{"path":"tune.json"}"#);

        assert_eq!(rc, 11, "a full ring is a retryable busy: {body}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["code"],
            "busy"
        );
        assert_eq!(
            app.world().resource::<SongEditor>().song().name,
            "old song",
            "a busy load never commits"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn soundfont_authorisation_honours_roots_and_extension() {
        let root = temp_root("sf-auth");
        std::fs::write(root.join("bank.sf2"), b"stub").unwrap();
        let policy = SongBusPolicy {
            song_roots: vec![],
            soundfont_roots: vec![root.clone()],
        };
        assert!(authorize_soundfont_path(&policy, "bank.sf2")
            .unwrap()
            .is_file());
        assert!(authorize_soundfont_path(&policy, "bank.mid").is_err()); // wrong ext
        assert!(authorize_soundfont_path(&policy, "../bank.sf2").is_err()); // traversal
        assert!(authorize_soundfont_path(&policy, "/etc/x.sf2").is_err()); // absolute
                                                                           // No soundfont roots configured → every remote font denied.
        assert!(authorize_soundfont_path(&SongBusPolicy::default(), "bank.sf2").is_err());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn soundfont_verb_denies_a_path_outside_the_roots() {
        let root = temp_root("sf-verb-deny");
        let mut app = verb_app(
            SongBusPolicy {
                song_roots: vec![],
                soundfont_roots: vec![root.clone()],
            },
            crate::editor::test_editor("untouched"),
        );
        let (rc, body) = run_verb_system(
            &mut app,
            SOUNDFONT_LOAD_VERB,
            soundfont_load_verb,
            r#"{"path":"../evil.sf2"}"#,
        );
        assert_eq!(rc, 10);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["code"],
            "path_denied"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn soundfont_gate_honours_roots() {
        let root = temp_root("sf");
        let font = root.join("bank.sf2");
        std::fs::write(&font, b"not really a soundfont").unwrap();
        let policy = SongBusPolicy {
            song_roots: vec![],
            soundfont_roots: vec![root.clone()],
        };
        assert!(soundfont_authorized(&policy, &font));
        assert!(!soundfont_authorized(
            &policy,
            Path::new("/usr/share/soundfonts/other.sf2")
        ));
        // With no soundfont roots, every font is denied.
        assert!(!soundfont_authorized(&SongBusPolicy::default(), &font));
        std::fs::remove_dir_all(&root).ok();
    }
}
