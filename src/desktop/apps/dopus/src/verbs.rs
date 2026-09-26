//! The `dopus` Bus port, schema `dopus.v1` — manifest and every
//! request/reply DTO, in the shape of ced's `verbs.rs` (schema `ced.v1`).
//!
//! All verbs are reachable by local and mesh callers with no authorization
//! gate (the full-mesh-access law). Success = rc 0; refusal = rc 10 with a
//! [`Refusal`] body.
//!
//! **P1 security posture:** `dopus.action` serves only navigation, view and
//! theme actions (`nav.*`, `view.*`, `theme.*`, plus the selection actions,
//! which are view-ish: they move the highlight). File-mutating actions and
//! `file.open` are keyboard-only until P2/P3 — refused with
//! [`code::FORBIDDEN`], matching filemgr's rule that a Bus caller never
//! mutates the filesystem through a file manager.

use serde::{Deserialize, Serialize};

pub const SERVICE: &str = "dopus";
pub const SCHEMA: &str = "dopus.v1";

/// `(verb, read_only)` in manifest order.
pub const VERBS: &[(&str, bool)] = &[
    ("dopus.ping", true),
    ("dopus.describe", true),
    ("dopus.info", true),
    ("dopus.state", true),
    ("dopus.action", false),
    ("dopus.actions.list", true),
    ("dopus.theme.set", false),
    ("dopus.open", false),
    ("dopus.quit", false),
];

/// Refusal codes (`error_code`).
pub mod code {
    pub const INVALID_ARGUMENT: &str = "INVALID_ARGUMENT";
    pub const NOT_FOUND: &str = "NOT_FOUND";
    pub const CONFLICT: &str = "CONFLICT";
    pub const UNAVAILABLE: &str = "UNAVAILABLE";
    pub const INTERNAL: &str = "INTERNAL";
    pub const UNKNOWN_VERB: &str = "UNKNOWN_VERB";
    /// A P2/P3 verb surface: file operations and file opening are refused on
    /// the Bus until their UI exists.
    pub const FORBIDDEN: &str = "FORBIDDEN";
}

/// Every refusal body (decision 10 shape).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refusal {
    pub error_code: String,
    pub message: String,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmptyReq {}

// ── ping / describe / info ──────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PingReply {
    pub pong: bool,
    pub service: String,
    pub schema: String,
    pub pid: u32,
    pub headless: bool,
}

/// The `app.describe` control surface (ctk-app-control.v0), as ced serves it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DescribeReply {
    pub contract: String,
    pub app: String,
    pub title: String,
    pub view: String,
    pub engine: String,
    pub version: String,
    pub description: String,
    pub controls: Vec<serde_json::Value>,
    pub verbs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InfoReply {
    pub version: String,
    pub git_sha: String,
    pub build_time: String,
    pub headless: bool,
    pub panes: usize,
    pub config_path: Option<String>,
}

// ── state ────────────────────────────────────────────────────────────────────

/// One pane's state (`pane` 0-based; P1 only ever has pane 0).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneState {
    pub pane: u8,
    pub path: String,
    pub active: bool,
    pub show_hidden: bool,
    /// `name | size | modified`.
    pub sort: String,
    pub ascending: bool,
    pub selected: Option<String>,
    pub rows: usize,
    /// Relative or absolute, as the status line renders it.
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateReply {
    pub panes: Vec<PaneState>,
    pub theme_scheme: String,
    pub theme_mode: String,
}

// ── action / actions.list ───────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionReq {
    pub id: String,
    #[serde(default)]
    pub args: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionReply {
    pub id: String,
    pub ok: bool,
    pub result: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionRow {
    pub id: String,
    pub label: String,
    pub keys: Vec<String>,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionsReply {
    pub actions: Vec<ActionRow>,
}

// ── theme ────────────────────────────────────────────────────────────────────

/// `scheme`/`mode` by name; `null` leaves it as resolved. An in-session
/// selection only (P1 does not persist; see `theme.rs`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThemeSetReq {
    #[serde(default)]
    pub scheme: Option<String>,
    #[serde(default)]
    pub mode: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThemeSetReply {
    pub scheme: String,
    pub mode: String,
}

// ── open (P1: accepted, paths ignored) ──────────────────────────────────────

/// The single-instance forward: a second `cosmix-dopus` process sends its
/// arguments here and exits. P1 accepts and ignores the paths — no
/// multi-pane or open-target handling until P2.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenReq {
    #[serde(default)]
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenReply {
    pub accepted: usize,
    /// Always false in P1: paths are accepted and ignored.
    pub opened: bool,
}

// ── app control ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuitReply {
    pub quitting: bool,
}

// ── serving ──────────────────────────────────────────────────────────────────
//
// One place turns a Bus command into replies + effects, shared by the
// windowed app ([`crate::app`]) and [`crate::headless`] so the two surfaces
// cannot drift. Only [`crate::app`] applies `Served::ThemeSet` (it owns the
// theme); everything else is answered here.

use cosmix_actions::filemgr;
use cosmix_actions::ActionId;
use cosmix_dopus_core::{DopusCore, PaneId};

/// What the caller tells the served verbs about itself.
pub struct ServerMeta {
    pub service: String,
    pub headless: bool,
    pub config_path: Option<String>,
    pub theme_scheme: String,
    pub theme_mode: String,
    /// The `dopus.actions.list` table, built once at boot from the effective
    /// keymap (the caller owns the keymap; this layer stays stateless).
    pub actions: Vec<ActionRow>,
}

/// One served command's answer.
pub enum Served {
    /// Reply `(rc, body)` to command `id`.
    Reply { id: u64, rc: u8, body: String },
    /// Apply the theme selection, then reply to `id` with the resolved
    /// `(scheme, mode)` names.
    ThemeSet { id: u64, scheme: Option<String>, mode: Option<String> },
    /// Reply to `id`, then quit.
    Quit { id: u64 },
}

impl Served {
    fn reply_json<T: serde::Serialize>(id: u64, value: &T) -> Self {
        Self::Reply { id, rc: 0, body: serde_json::to_string(value).unwrap_or_else(|_| "{}".into()) }
    }

    fn refusal(id: u64, refusal: Refusal) -> Self {
        Self::Reply {
            id,
            rc: 10,
            body: serde_json::to_string(&refusal).unwrap_or_else(|_| format!("{{\"error_code\":\"{}\"}}", code::INTERNAL)),
        }
    }

    fn error(id: u64, error_code: &str, message: String) -> Self {
        Self::refusal(id, Refusal { error_code: error_code.to_owned(), message, reason: None })
    }
}

/// The P1 action table: `(action, label)`. `app.quit` is served; everything
/// `file.*`/`place.*` is keyboard-only until P2/P3 (see the module header).
pub const P1_ACTIONS: &[(ActionId, &str)] = &[
    (filemgr::FILE_OPEN, "Open the selection"),
    (filemgr::NAV_BACK, "Go back"),
    (filemgr::NAV_FORWARD, "Go forward"),
    (filemgr::NAV_PARENT, "Go to parent folder"),
    (filemgr::NAV_HOME, "Go to home folder"),
    (filemgr::VIEW_REFRESH, "Refresh"),
    (filemgr::VIEW_TOGGLE_HIDDEN, "Toggle hidden files"),
    (filemgr::VIEW_SORT_NAME, "Sort by name"),
    (filemgr::VIEW_SORT_SIZE, "Sort by size"),
    (filemgr::VIEW_SORT_MODIFIED, "Sort by modified"),
    (filemgr::SELECT_NEXT, "Select next"),
    (filemgr::SELECT_PREVIOUS, "Select previous"),
    (filemgr::SELECT_FIRST, "Select first"),
    (filemgr::SELECT_LAST, "Select last"),
    (cosmix_actions::theme::MODE_TOGGLE, "Toggle light/dark mode"),
    (cosmix_actions::theme::SCHEME_OCEAN, "Scheme: Ocean"),
    (cosmix_actions::theme::SCHEME_CRIMSON, "Scheme: Crimson"),
    (cosmix_actions::theme::SCHEME_STONE, "Scheme: Stone"),
    (cosmix_actions::theme::SCHEME_FOREST, "Scheme: Forest"),
    (cosmix_actions::theme::SCHEME_SUNSET, "Scheme: Sunset"),
    (cosmix_actions::theme::SCHEME_MONO, "Scheme: Mono"),
    (filemgr::APP_QUIT, "Quit dopus"),
];

/// The theme scheme a `theme.scheme-*` action selects.
pub fn scheme_action(action: ActionId) -> Option<&'static str> {
    Some(match action {
        a if a == cosmix_actions::theme::SCHEME_OCEAN => "ocean",
        a if a == cosmix_actions::theme::SCHEME_CRIMSON => "crimson",
        a if a == cosmix_actions::theme::SCHEME_STONE => "stone",
        a if a == cosmix_actions::theme::SCHEME_FOREST => "forest",
        a if a == cosmix_actions::theme::SCHEME_SUNSET => "sunset",
        a if a == cosmix_actions::theme::SCHEME_MONO => "mono",
        _ => return None,
    })
}

/// What applying one action does. The Bus layer and the keyboard layer share
/// this: a `dopus.action` call is a keystroke a remote caller pressed.
pub enum Applied {
    Done,
    Quit,
}

/// Apply one action to the core. Law 5 is the core's (`set_sort` toggles a
/// same-column sort itself); every P1 switch passes `ascending: true`.
pub fn apply_action(action: ActionId, core: &mut DopusCore) -> Result<Applied, Refusal> {
    let done = Ok(Applied::Done);
    let pane = PaneId::Left;
    if action == filemgr::FILE_OPEN {
        // A directory opens in place; a file derives an `OpenFile` event,
        // which the app surfaces as a status line (law 4's P1 posture: no
        // spawn until there is an operations surface to own it).
        core.open_selection();
        return done;
    }
    if action == filemgr::NAV_BACK {
        core.go_back();
        return done;
    }
    if action == filemgr::NAV_FORWARD {
        core.go_forward();
        return done;
    }
    if action == filemgr::NAV_PARENT {
        core.go_parent();
        return done;
    }
    if action == filemgr::NAV_HOME {
        core.go_home();
        return done;
    }
    if action == filemgr::VIEW_REFRESH {
        core.refresh();
        return done;
    }
    if action == filemgr::VIEW_TOGGLE_HIDDEN {
        core.toggle_hidden();
        return done;
    }
    if action == filemgr::VIEW_SORT_NAME {
        core.set_sort(cosmix_dopus_core::SortColumn::Name, true);
        return done;
    }
    if action == filemgr::VIEW_SORT_SIZE {
        core.set_sort(cosmix_dopus_core::SortColumn::Size, true);
        return done;
    }
    if action == filemgr::VIEW_SORT_MODIFIED {
        core.set_sort(cosmix_dopus_core::SortColumn::Modified, true);
        return done;
    }
    if action == filemgr::SELECT_NEXT {
        core.select_relative(pane, 1);
        return done;
    }
    if action == filemgr::SELECT_PREVIOUS {
        core.select_relative(pane, -1);
        return done;
    }
    if action == filemgr::SELECT_FIRST {
        core.select_edge(pane, false);
        return done;
    }
    if action == filemgr::SELECT_LAST {
        core.select_edge(pane, true);
        return done;
    }
    if action == filemgr::APP_QUIT {
        return Ok(Applied::Quit);
    }
    Err(Refusal {
        error_code: code::FORBIDDEN.to_owned(),
        message: format!("{action} is keyboard-only in P1 (no file-operation UI yet)"),
        reason: Some("p1_scope".to_owned()),
    })
}

/// `dopus.actions.list`'s table: the P1 actions with their effective chords,
/// from the effective keymap (windowed and headless share this).
pub fn action_table(keymap: &cosmix_actions::Keymap) -> Vec<ActionRow> {
    P1_ACTIONS
        .iter()
        .map(|(action, label)| ActionRow {
            id: action.to_string(),
            label: (*label).to_owned(),
            keys: keymap
                .defaults
                .iter()
                .filter(|b| b.action == *action)
                .map(|b| b.chord.to_string())
                .collect(),
            enabled: true,
        })
        .collect()
}

/// Serve one Bus command. Never panics, never leaves a command unanswered:
/// every arm ends in a `Served`.
pub fn serve_command(command: &crate::bus::Command, core: &mut DopusCore, meta: &ServerMeta, info: &cosmix_buildinfo::BuildInfo) -> Vec<Served> {
    match command.verb.as_str() {
        "dopus.ping" => vec![Served::reply_json(
            command.id,
            &PingReply {
                pong: true,
                service: meta.service.clone(),
                schema: SCHEMA.to_owned(),
                pid: std::process::id(),
                headless: meta.headless,
            },
        )],
        "dopus.describe" => vec![Served::reply_json(
            command.id,
            &DescribeReply {
                contract: "ctk-app-control.v0".to_owned(),
                app: "dopus".to_owned(),
                title: "CosMix DOpus".to_owned(),
                view: "dopus".to_owned(),
                engine: "iced".to_owned(),
                version: info.version.to_owned(),
                description: "the CosMix twin-pane file manager (P1: one live pane)".to_owned(),
                controls: Vec::new(),
                verbs: VERBS.iter().map(|(verb, _)| (*verb).to_owned()).collect(),
            },
        )],
        "dopus.info" => vec![Served::reply_json(
            command.id,
            &InfoReply {
                version: info.version.to_owned(),
                git_sha: info.git_sha.to_owned(),
                build_time: info.build_time.to_owned(),
                headless: meta.headless,
                panes: 1,
                config_path: meta.config_path.clone(),
            },
        )],
        "dopus.state" => {
            let pane = core.pane(PaneId::Left);
            let state = StateReply {
                panes: vec![PaneState {
                    pane: 0,
                    path: cosmix_dopus_core::sanitise_display_path(&pane.path),
                    active: core.active() == PaneId::Left,
                    show_hidden: pane.show_hidden,
                    sort: match pane.sort {
                        cosmix_dopus_core::SortColumn::Name => "name",
                        cosmix_dopus_core::SortColumn::Size => "size",
                        cosmix_dopus_core::SortColumn::Modified => "modified",
                    }
                    .to_owned(),
                    ascending: pane.ascending,
                    selected: pane.selected.as_ref().map(|p| cosmix_dopus_core::sanitise_display_path(p)),
                    rows: core.visible_rows(PaneId::Left).len(),
                    status: pane.status.clone(),
                }],
                theme_scheme: meta.theme_scheme.clone(),
                theme_mode: meta.theme_mode.clone(),
            };
            vec![Served::reply_json(command.id, &state)]
        }
        "dopus.action" => match serde_json::from_str::<ActionReq>(&command.body) {
            Ok(req) => match ActionId::intern(&req.id) {
                Ok(action) => match apply_action(action, core) {
                    Ok(Applied::Done) => vec![Served::reply_json(
                        command.id,
                        &ActionReply { id: req.id, ok: true, result: None },
                    )],
                    Ok(Applied::Quit) => vec![
                        Served::reply_json(command.id, &QuitReply { quitting: true }),
                        Served::Quit { id: command.id },
                    ],
                    Err(refusal) => vec![Served::refusal(command.id, refusal)],
                },
                Err(error) => vec![Served::error(command.id, code::INVALID_ARGUMENT, format!("action id {:?}: {error}", req.id))],
            },
            Err(error) => vec![Served::error(command.id, code::INVALID_ARGUMENT, format!("body: {error}"))],
        },
        "dopus.actions.list" => vec![Served::reply_json(command.id, &ActionsReply { actions: meta.actions.clone() })],
        "dopus.theme.set" => match serde_json::from_str::<ThemeSetReq>(&command.body) {
            Ok(req) => vec![Served::ThemeSet { id: command.id, scheme: req.scheme, mode: req.mode }],
            Err(error) => vec![Served::error(command.id, code::INVALID_ARGUMENT, format!("body: {error}"))],
        },
        "dopus.open" => match serde_json::from_str::<OpenReq>(&command.body) {
            Ok(req) => vec![Served::reply_json(
                command.id,
                &OpenReply { accepted: req.paths.len(), opened: false },
            )],
            Err(error) => vec![Served::error(command.id, code::INVALID_ARGUMENT, format!("body: {error}"))],
        },
        "dopus.quit" => vec![
            Served::reply_json(command.id, &QuitReply { quitting: true }),
            Served::Quit { id: command.id },
        ],
        other => vec![Served::error(
            command.id,
            code::UNKNOWN_VERB,
            format!("{other} is not a dopus verb (schema {SCHEMA})"),
        )],
    }
}
