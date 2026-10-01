//! The `dopus` Bus port, schema `dopus.v1` — manifest and every
//! request/reply DTO, in the shape of ced's `verbs.rs` (schema `ced.v1`).
//!
//! All verbs are reachable by local and mesh callers with no authorization
//! gate (the full-mesh-access law). Success = rc 0; refusal = rc 10 with a
//! [`Refusal`] body.
//!
//! **Security posture (P3):** `dopus.action` serves the navigation, view,
//! pane and theme actions (`nav.*`, `view.*`, `theme.*`, `location.focus`, plus the selection
//! actions, which are view-ish: they move the highlight). The file
//! operations exist — keyboard + dialogs in the windowed app since P3
//! (`file.open`, `file.new-folder`, `file.rename`, `file.copy-other-pane`,
//! `file.move-other-pane`, `file.delete`) — but the Bus NEVER mutates the
//! filesystem through a file manager: every `file.*` id is pre-refused with
//! [`code::FORBIDDEN`] before the shared [`apply_action`] layer runs
//! (filemgr's rule, unchanged since P2). `dopus.open` navigates the two
//! panes.

use std::path::{Path, PathBuf};

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
    /// A permanent verb-surface boundary: file operations and file opening
    /// are keyboard-only forever — the Bus never mutates the filesystem
    /// through a file manager.
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
    /// Same ordered rows as `dopus.state.panes`; retain the pane count above.
    pub pane_states: Vec<PaneState>,
    pub config_path: Option<String>,
}

// ── state ────────────────────────────────────────────────────────────────────

/// One pane's state (`pane` 0-based; two panes in P2).
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
    /// All selected paths in visible row order; `selected` remains the focused item.
    #[serde(default)]
    pub selected_paths: Vec<String>,
    pub rows: usize,
    /// Relative or absolute, as the status line renders it.
    pub status: String,
    /// Footer text: root totals, a loading ellipsis or a root listing error.
    #[serde(default)]
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateReply {
    pub places: cosmix_dopus_core::config::SidebarConfig,
    pub properties: cosmix_dopus_core::config::SidebarConfig,
    pub panes: Vec<PaneState>,
    pub theme_scheme: String,
    pub theme_mode: String,
}

// ── action / actions.list ───────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionReq {
    pub id: String,
    #[serde(default)]
    pub pane: PaneTarget,
    #[serde(default)]
    pub args: Option<serde_json::Value>,
}

/// An omitted action target preserves active-pane behaviour.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PaneTarget {
    Left,
    Right,
    #[default]
    Active,
}

impl PaneTarget {
    fn resolve(self, core: &DopusCore) -> PaneId {
        match self {
            Self::Left => PaneId::Left,
            Self::Right => PaneId::Right,
            Self::Active => core.active(),
        }
    }
}

impl<'de> Deserialize<'de> for PaneTarget {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Target {
            Name(String),
            Index(u8),
        }
        match Target::deserialize(deserializer)? {
            Target::Name(name) => match name.as_str() {
                "left" => Ok(Self::Left),
                "right" => Ok(Self::Right),
                "active" => Ok(Self::Active),
                _ => Err(serde::de::Error::custom(
                    "pane must be left, right, active, 0 or 1",
                )),
            },
            Target::Index(0) => Ok(Self::Left),
            Target::Index(1) => Ok(Self::Right),
            Target::Index(_) => Err(serde::de::Error::custom("pane index must be 0 or 1")),
        }
    }
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
/// selection only (not persisted; see `theme.rs`).
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

// ── open (P2: first path → left pane, second → right, extras ignored) ───────

/// The single-instance forward: a second `cosmix-dopus` process sends its
/// arguments here and exits. P2 applies the paths: the first navigates the
/// left pane, the second the right, extras are logged and ignored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenReq {
    #[serde(default)]
    pub paths: Vec<String>,
    /// Omission preserves positional left/right forwarding. An explicit
    /// target takes exactly one path and does not change the active pane.
    #[serde(default)]
    pub pane: Option<PaneTarget>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenReply {
    pub accepted: usize,
    /// True when at least one path landed in a pane.
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
// cannot drift. The window applies theme and location-focus effects;
// headless refuses them because there is no window to act on.

use cosmix_actions::ActionId;
use cosmix_actions::filemgr;
use cosmix_dopus_core::{DopusCore, PaneId};

/// What the caller tells the served verbs about itself.
pub struct ServerMeta {
    pub service: String,
    pub headless: bool,
    /// The window can focus a location editor (no modal or shutdown).
    /// Headless always refuses focus, regardless of this flag.
    pub location_focus_available: bool,
    pub config_path: Option<String>,
    /// The RESOLVED scheme/mode names, reported by `dopus.state`. A headless
    /// caller paints nothing and resolves no theme: it passes empty strings
    /// (and `dopus.state` reports them empty on purpose).
    pub theme_scheme: String,
    pub theme_mode: String,
    /// The `dopus.actions.list` table, built once at boot from the effective
    /// keymap (the caller owns the keymap; this layer stays stateless).
    pub actions: Vec<ActionRow>,
}

/// One served command's answer.
pub enum Served {
    ToggleSidebar {
        id: u64,
        sidebar: cosmix_dopus_core::config::Sidebar,
        action: String,
    },
    /// Reply `(rc, body)` to command `id`.
    Reply { id: u64, rc: u8, body: String },
    /// Apply the theme selection, then reply to `id` with the resolved
    /// `(scheme, mode)` names.
    ThemeSet {
        id: u64,
        scheme: Option<String>,
        mode: Option<String>,
    },
    /// Perform the theme selection of a `dopus.action theme.*` call: the
    /// windowed twin of [`Served::ThemeSet`] (mode-toggle resolves against
    /// the live selection; headless refuses UNAVAILABLE).
    ThemeAction { id: u64, action: ThemeAction },
    /// Focus the requested location bar, then acknowledge the action.
    LocationFocus { id: u64, pane: PaneId },
    /// Reply to `id`, then quit.
    Quit { id: u64 },
}

impl Served {
    fn reply_json<T: serde::Serialize>(id: u64, value: &T) -> Self {
        Self::Reply {
            id,
            rc: 0,
            body: serde_json::to_string(value).unwrap_or_else(|_| "{}".into()),
        }
    }

    fn refusal(id: u64, refusal: Refusal) -> Self {
        Self::Reply {
            id,
            rc: 10,
            body: serde_json::to_string(&refusal)
                .unwrap_or_else(|_| format!("{{\"error_code\":\"{}\"}}", code::INTERNAL)),
        }
    }

    fn error(id: u64, error_code: &str, message: String) -> Self {
        Self::refusal(
            id,
            Refusal {
                error_code: error_code.to_owned(),
                message,
                reason: None,
            },
        )
    }
}

/// The action table: `(action, label)`. `app.quit` is served; the `file.*`
/// operations are keyboard-only (the Bus arm refuses every `file.*` — see
/// the module header), so they appear here for the keyboard path and
/// `dopus.actions.list` only; their `enabled` flag is per-call from the
/// core's availability ([`apply_availability`]).
pub const ACTIONS: &[(ActionId, &str)] = &[
    (cosmix_actions::view::TOGGLE_PLACES, "Show or hide Places"),
    (
        cosmix_actions::view::TOGGLE_PROPERTIES,
        "Show or hide Properties",
    ),
    (cosmix_actions::location::FOCUS, "Focus the location bar"),
    (filemgr::FILE_OPEN, "Open the selection"),
    (filemgr::FILE_NEW_FOLDER, "New folder"),
    (filemgr::FILE_RENAME, "Rename the selection"),
    (filemgr::FILE_COPY, "Copy the selection to the other pane"),
    (filemgr::FILE_MOVE, "Move the selection to the other pane"),
    (filemgr::FILE_DELETE, "Delete the selection"),
    (filemgr::NAV_BACK, "Go back"),
    (filemgr::NAV_FORWARD, "Go forward"),
    (filemgr::NAV_PARENT, "Go to parent folder"),
    (filemgr::NAV_HOME, "Go to home folder"),
    (filemgr::NAV_SWITCH_PANE, "Switch active pane"),
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

/// The theme selection one of the `theme.*` actions performs. The windowed
/// app resolves it against its live theme (the same performer the
/// `dopus.theme.set` verb uses); headless refuses it — a theme with nothing
/// to paint is a lie.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThemeAction {
    /// A `theme.scheme-*` action: the scheme by its `dopus.theme.set` name.
    Scheme(String),
    /// `theme.mode-toggle`: light/dark, resolved against the live selection.
    ModeToggle,
}

/// The theme selection a `theme.*` action performs, if it is one.
fn theme_action(action: ActionId) -> Option<ThemeAction> {
    if action == cosmix_actions::theme::MODE_TOGGLE {
        return Some(ThemeAction::ModeToggle);
    }
    scheme_action(action).map(|name| ThemeAction::Scheme(name.to_owned()))
}

/// What applying one action does. The Bus layer and the keyboard layer share
/// this: a `dopus.action` call is a keystroke a remote caller pressed.
pub enum Applied {
    ToggleSidebar(cosmix_dopus_core::config::Sidebar),
    Done,
    /// The window focuses this pane's location editor; headless refuses.
    LocationFocus(PaneId),
    /// A `theme.*` action: the windowed app performs the selection
    /// ([`Served::ThemeAction`]); headless refuses it.
    Theme(ThemeAction),
    Quit,
}

/// A keyboard invocation of an action the availability gate has taken off
/// the table: a no-op WITH an explanation (the status line the keyboard
/// path makes of it), never a silent nothing. `UNAVAILABLE`: the action is
/// real, the current state just cannot take it.
fn gated(message: &str) -> Refusal {
    Refusal {
        error_code: code::UNAVAILABLE.to_owned(),
        message: message.to_owned(),
        reason: Some("not_offered".to_owned()),
    }
}

/// Whether the core already holds a NewFolder or Rename reservation (its
/// `begin_*` verbs return silently then — browser.rs's single name edit).
/// Both callers refuse up front instead: [`gated`] forbids propagating a
/// silent nothing as `Ok(Done)` (the 200 ms flush window would surface it
/// as a hollow success).
fn name_edit_pending(core: &DopusCore) -> bool {
    core.outstanding_reservations().iter().any(|(_, kind)| {
        matches!(
            kind,
            cosmix_dopus_core::ReservationKind::NewFolder
                | cosmix_dopus_core::ReservationKind::Rename
        )
    })
}

/// Apply one keyboard action to the core's active pane; the pane headers
/// activate their pane first (the app calls `set_active_pane` before these).
/// Law 5 is the core's (`set_sort` toggles a same-column sort itself); every
/// column switch passes `ascending: true`.
///
/// The `file.*` arms are keyboard-only (the Bus pre-refuses every `file.*`
/// id before this layer — see the module header): each checks the core's
/// [`AvailabilitySnapshot`] first and refuses with [`gated`] when the table
/// would show the row disabled, mirroring what the core itself refuses
/// (single-flight: "Another file operation is still running"; selection:
/// nothing to act on).
pub fn apply_action(action: ActionId, core: &mut DopusCore) -> Result<Applied, Refusal> {
    let pane = core.active();
    apply_action_in(action, core, pane)
}

/// Apply pane-local navigation, view and selection actions directly to the
/// target. Global actions (switch-pane, theme, quit) retain their meaning.
pub fn apply_action_in(
    action: ActionId,
    core: &mut DopusCore,
    pane: PaneId,
) -> Result<Applied, Refusal> {
    if action == cosmix_actions::view::TOGGLE_PLACES {
        return Ok(Applied::ToggleSidebar(
            cosmix_dopus_core::config::Sidebar::Places,
        ));
    }
    if action == cosmix_actions::view::TOGGLE_PROPERTIES {
        return Ok(Applied::ToggleSidebar(
            cosmix_dopus_core::config::Sidebar::Properties,
        ));
    }
    let done = Ok(Applied::Done);
    if action == cosmix_actions::location::FOCUS {
        return Ok(Applied::LocationFocus(pane));
    }
    let availability = core.availability();
    let busy = || gated("A file operation is still running");
    if action == filemgr::FILE_OPEN {
        // A directory opens in place; a non-directory derives an `OpenFile`
        // event, which the app serves by spawning `xdg-open` (law 4).
        if !availability.has_selection {
            return Err(gated("Nothing is selected"));
        }
        core.open_selection();
        return done;
    }
    if action == filemgr::FILE_NEW_FOLDER {
        if availability.operation_running {
            return Err(busy());
        }
        if name_edit_pending(core) {
            return Err(gated("A name edit is already pending"));
        }
        core.begin_new_folder();
        return done;
    }
    if action == filemgr::FILE_RENAME {
        if !availability.has_selection {
            return Err(gated("Nothing is selected"));
        }
        if availability.selection_count != 1 {
            return Err(gated("Select one item to rename"));
        }
        if availability.operation_running {
            return Err(busy());
        }
        if name_edit_pending(core) {
            return Err(gated("A name edit is already pending"));
        }
        core.begin_rename();
        return done;
    }
    if action == filemgr::FILE_COPY {
        if !availability.has_selection {
            return Err(gated("Nothing is selected"));
        }
        if availability.operation_running {
            return Err(busy());
        }
        core.copy_selection_to_other_pane();
        return done;
    }
    if action == filemgr::FILE_MOVE {
        if !availability.has_selection {
            return Err(gated("Nothing is selected"));
        }
        if availability.operation_running {
            return Err(busy());
        }
        core.move_selection_to_other_pane();
        return done;
    }
    if action == filemgr::FILE_DELETE {
        if !availability.has_selection {
            return Err(gated("Nothing is selected"));
        }
        if availability.operation_running {
            return Err(busy());
        }
        core.delete_selection();
        return done;
    }
    if action == filemgr::NAV_BACK {
        core.go_back_in(pane);
        return done;
    }
    if action == filemgr::NAV_FORWARD {
        core.go_forward_in(pane);
        return done;
    }
    if action == filemgr::NAV_PARENT {
        core.go_parent_in(pane);
        return done;
    }
    if action == filemgr::NAV_HOME {
        core.go_home_in(pane);
        return done;
    }
    if action == filemgr::NAV_SWITCH_PANE {
        core.switch_pane();
        return done;
    }
    if action == filemgr::VIEW_REFRESH {
        core.refresh_in(pane);
        return done;
    }
    if action == filemgr::VIEW_TOGGLE_HIDDEN {
        core.toggle_hidden_in(pane);
        return done;
    }
    if action == filemgr::VIEW_SORT_NAME {
        core.set_sort_in(pane, cosmix_dopus_core::SortColumn::Name, true);
        return done;
    }
    if action == filemgr::VIEW_SORT_SIZE {
        core.set_sort_in(pane, cosmix_dopus_core::SortColumn::Size, true);
        return done;
    }
    if action == filemgr::VIEW_SORT_MODIFIED {
        core.set_sort_in(pane, cosmix_dopus_core::SortColumn::Modified, true);
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
    if let Some(theme) = theme_action(action) {
        // The module header and `dopus.actions.list` promise these; the
        // windowed app performs the selection (what `dopus.theme.set`
        // takes), headless refuses it — no painter, no theme.
        return Ok(Applied::Theme(theme));
    }
    // Every advertised ACTIONS entry is handled above (file.* is refused
    // separately at Bus ingress). A shared FileMgr id not advertised by
    // dopus is UNAVAILABLE here; an id nothing defines is INVALID_ARGUMENT.
    let known = filemgr::MENU_ACTION_IDS.contains(&action)
        || filemgr::DEFAULT_KEYMAP_ACTION_IDS.contains(&action)
        || ACTIONS.iter().any(|(known, _)| *known == action);
    Err(Refusal {
        error_code: if known {
            code::UNAVAILABLE.to_owned()
        } else {
            code::INVALID_ARGUMENT.to_owned()
        },
        message: if known {
            format!("{action} is not implemented by dopus")
        } else {
            format!("{action} is not a dopus action")
        },
        reason: known.then(|| "not_implemented".to_owned()),
    })
}

/// Refresh the table's `enabled` flags from the core's live availability
/// (P3): a `file.*` row the current state cannot take reads as disabled —
/// no selection, or an operation holding the single-flight slot — and the
/// keyboard path agrees ([`apply_action`] refuses with the same verdict).
/// Non-file rows are enabled here; [`serve_command`] additionally gates
/// location.focus on window availability. The table is built once at boot
/// and its flags are refreshed for every `dopus.actions.list` reply.
pub fn apply_availability(
    actions: &mut [ActionRow],
    availability: &cosmix_dopus_core::AvailabilitySnapshot,
) {
    for row in actions {
        let selection = availability.has_selection;
        let idle = !availability.operation_running;
        row.enabled = match row.id.as_str() {
            "file.open" => selection,
            "file.new-folder" => idle,
            "file.rename" => selection && idle && availability.selection_count == 1,
            "file.copy-other-pane" | "file.move-other-pane" | "file.delete" => selection && idle,
            _ => true,
        };
    }
}

/// `dopus.actions.list`'s table: the served actions with their effective
/// chords, from the effective keymap (windowed and headless share this).
pub fn action_table(keymap: &cosmix_actions::Keymap) -> Vec<ActionRow> {
    ACTIONS
        .iter()
        .map(|(action, label)| ActionRow {
            id: action.to_string(),
            label: (*label).to_owned(),
            keys: keymap
                .effective_bindings()
                .filter(|b| b.action == *action)
                .map(|b| b.chord.to_string())
                .collect(),
            enabled: true,
        })
        .collect()
}

/// A FILE path navigated as a pane directory lands on its PARENT —
/// `/etc/passwd` opens `/etc`, not a permanent error status plus a persisted
/// file path the pane can never list. Selecting the file itself is P3.
pub fn navigable(path: PathBuf) -> PathBuf {
    if path.exists() && !path.is_dir() {
        return path.parent().map(Path::to_path_buf).unwrap_or(path);
    }
    path
}

/// Apply forwarded `dopus.open` PATHs to the panes: the first navigates the
/// left pane, the second the right, extras are logged and ignored (the
/// P2 open contract — the same path the windowed startup and the Bus verb
/// take, so the two surfaces cannot drift). Relative paths resolve against
/// THIS process's cwd here — the chokepoint every Bus/open entry funnels
/// through, so the verb, argv and forward cannot drift (main.rs absolutises
/// its argv too; a harmless double). A file path lands on its parent
/// ([`navigable`]).
pub fn apply_open_paths(core: &mut DopusCore, paths: &[String]) {
    for (index, raw) in paths.iter().enumerate() {
        let pane = match index {
            0 => PaneId::Left,
            1 => PaneId::Right,
            _ => {
                tracing::info!("dopus.open: ignoring extra path {} ({raw})", index + 1);
                continue;
            }
        };
        core.navigate(pane, open_path(raw));
    }
}

fn open_path(raw: &str) -> PathBuf {
    let expanded = crate::dirs::expand_tilde(raw);
    let absolute = if expanded.is_relative() {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(expanded),
            Err(_) => expanded,
        }
    } else {
        expanded
    };
    navigable(absolute)
}

/// One pane's `dopus.state` row.
fn pane_state(core: &DopusCore, pane_id: PaneId) -> PaneState {
    let pane = core.pane(pane_id);
    PaneState {
        pane: pane_id.index() as u8,
        path: cosmix_dopus_core::sanitise_display_path(&pane.path),
        active: core.active() == pane_id,
        show_hidden: pane.show_hidden,
        sort: match pane.sort {
            cosmix_dopus_core::SortColumn::Name => "name",
            cosmix_dopus_core::SortColumn::Size => "size",
            cosmix_dopus_core::SortColumn::Modified => "modified",
        }
        .to_owned(),
        ascending: pane.ascending,
        selected: pane
            .selected
            .as_ref()
            .map(|p| cosmix_dopus_core::sanitise_display_path(p)),
        selected_paths: core
            .selected_paths(pane_id)
            .iter()
            .map(|path| cosmix_dopus_core::sanitise_display_path(path))
            .collect(),
        rows: core.visible_rows(pane_id).len(),
        status: pane.status.clone(),
        summary: pane.footer_summary(),
    }
}

/// Serve one Bus command. Never panics, never leaves a command unanswered:
/// every arm ends in a `Served`.
pub fn serve_command(
    command: &crate::bus::Command,
    core: &mut DopusCore,
    meta: &ServerMeta,
    info: &cosmix_buildinfo::BuildInfo,
) -> Vec<Served> {
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
                description: "the CosMix twin-pane file manager (P4: plain Places and Properties panels; file operations via keyboard and dialogs; file.* stays Bus-forbidden)".to_owned(),
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
                panes: 2,
                pane_states: vec![pane_state(core, PaneId::Left), pane_state(core, PaneId::Right)],
                config_path: meta.config_path.clone(),
            },
        )],
        "dopus.state" => {
            let state = StateReply {
                places: core.sidebar(cosmix_dopus_core::config::Sidebar::Places),
                properties: core.sidebar(cosmix_dopus_core::config::Sidebar::Properties),
                panes: vec![pane_state(core, PaneId::Left), pane_state(core, PaneId::Right)],
                theme_scheme: meta.theme_scheme.clone(),
                theme_mode: meta.theme_mode.clone(),
            };
            vec![Served::reply_json(command.id, &state)]
        }
        "dopus.action" => match serde_json::from_str::<ActionReq>(&command.body) {
            Ok(req) => {
                let pane = req.pane.resolve(core);
                match ActionId::intern(&req.id) {
                    // The Bus never opens (or otherwise touches) files: `file.*`
                    // is keyboard-only, even though `apply_action` serves the
                    // keyboard arms (a directory in place, a file via `xdg-open`,
                    // the confirm/prompt dialogs). Matching filemgr's rule — a
                    // remote caller never mutates the filesystem through a file
                    // manager.
                    Ok(action) if action.as_str().starts_with("file.") => vec![Served::error(
                        command.id,
                        code::FORBIDDEN,
                        format!("{action} is keyboard-only — the Bus never mutates the filesystem through a file manager"),
                    )],
                    Ok(action) => match apply_action_in(action, core, pane) {
                        Ok(Applied::ToggleSidebar(_)) if meta.headless || !meta.location_focus_available => vec![Served::refusal(command.id, Refusal {
                            error_code: code::UNAVAILABLE.to_owned(),
                            message: "sidebar toggles need an available window".to_owned(),
                            reason: Some(if meta.headless { "headless" } else { "window_busy" }.to_owned()),
                        })],
                        Ok(Applied::ToggleSidebar(sidebar)) => vec![Served::ToggleSidebar { id: command.id, sidebar, action: req.id }],
                        Ok(Applied::LocationFocus(_)) if meta.headless || !meta.location_focus_available => {
                            vec![Served::refusal(command.id, Refusal {
                                error_code: code::UNAVAILABLE.to_owned(),
                                message: "location focus needs an available window editor".to_owned(),
                                reason: Some(if meta.headless { "headless" } else { "window_busy" }.to_owned()),
                            })]
                        }
                        Ok(Applied::LocationFocus(pane)) => vec![Served::LocationFocus { id: command.id, pane }],
                        Ok(Applied::Done) => vec![Served::reply_json(
                            command.id,
                            &ActionReply { id: req.id, ok: true, result: None },
                        )],
                        // The theme.* actions: UNAVAILABLE on headless (the
                        // theme.set pre-refusal's wording — the vocabulary is
                        // real, the painter is not), performed windowed.
                        Ok(Applied::Theme(_)) if meta.headless => vec![Served::refusal(
                            command.id,
                            Refusal {
                                error_code: code::UNAVAILABLE.to_owned(),
                                message: "theme selection needs the windowed app (headless paints nothing)".to_owned(),
                                reason: Some("headless".to_owned()),
                            },
                        )],
                        Ok(Applied::Theme(theme)) => vec![Served::ThemeAction { id: command.id, action: theme }],
                        Ok(Applied::Quit) => vec![
                            Served::reply_json(command.id, &QuitReply { quitting: true }),
                            Served::Quit { id: command.id },
                        ],
                        Err(refusal) => vec![Served::refusal(command.id, refusal)],
                    },
                    Err(error) => vec![Served::error(command.id, code::INVALID_ARGUMENT, format!("action id {:?}: {error}", req.id))],
                }
            },
            Err(error) => vec![Served::error(command.id, code::INVALID_ARGUMENT, format!("body: {error}"))],
        },
        "dopus.actions.list" => {
            // `enabled` is per-frame from the core's availability (P3): the
            // table this call returns reflects what the keyboard could do
            // right now.
            let mut actions = meta.actions.clone();
            apply_availability(&mut actions, &core.availability());
            for row in &mut actions {
                if [cosmix_actions::location::FOCUS.as_str(), cosmix_actions::view::TOGGLE_PLACES.as_str(), cosmix_actions::view::TOGGLE_PROPERTIES.as_str()].contains(&row.id.as_str()) {
                    row.enabled = !meta.headless && meta.location_focus_available;
                }
            }
            vec![Served::reply_json(command.id, &ActionsReply { actions })]
        }
        "dopus.theme.set" => match serde_json::from_str::<ThemeSetReq>(&command.body) {
            Ok(req) => vec![Served::ThemeSet { id: command.id, scheme: req.scheme, mode: req.mode }],
            Err(error) => vec![Served::error(command.id, code::INVALID_ARGUMENT, format!("body: {error}"))],
        },
        "dopus.open" => match serde_json::from_str::<OpenReq>(&command.body) {
            Ok(req) => {
                if let Some(target) = req.pane {
                    if req.paths.len() != 1 {
                        return vec![Served::error(command.id, code::INVALID_ARGUMENT,
                            "pane-targeted open requires exactly one path".to_owned())];
                    }
                    let pane = target.resolve(core);
                    core.navigate(pane, open_path(&req.paths[0]));
                } else {
                    apply_open_paths(core, &req.paths);
                }
                vec![Served::reply_json(
                    command.id,
                    &OpenReply { accepted: req.paths.len(), opened: !req.paths.is_empty() },
                )]
            }
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

#[cfg(test)]
mod summary_tests {
    use super::*;
    use cosmix_dopus_core::{CoreEvent, DOpusConfig, FileEntry};

    #[test]
    fn pane_summaries_follow_their_root_listing_independent_of_focus() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = DOpusConfig::default();
        config.left.path = dir.path().to_owned();
        config.right.path = dir.path().to_owned();
        let (mut core, _rx) = DopusCore::new(config, None);
        for (id, entries) in [
            (
                PaneId::Left,
                vec![("folder", true, None), ("file", false, Some(1536))],
            ),
            (
                PaneId::Right,
                vec![("one", false, Some(1024)), ("two", false, Some(1024))],
            ),
        ] {
            core.on_event(CoreEvent::ListingArrived {
                pane: id,
                generation: core.pane(id).generation,
                path: dir.path().to_owned(),
                root: true,
                result: Ok(entries
                    .into_iter()
                    .map(|(name, is_dir, size)| FileEntry {
                        path: dir.path().join(name),
                        name: name.into(),
                        is_dir,
                        size,
                        child_count: is_dir.then_some(99),
                        modified: None,
                    })
                    .collect()),
            });
        }
        for active in [PaneId::Left, PaneId::Right] {
            core.set_active_pane(active);
            for (id, expected) in [
                (PaneId::Left, "1 folder, 1 file (1.5 KiB)"),
                (PaneId::Right, "0 folders, 2 files (2.0 KiB)"),
            ] {
                let state = pane_state(&core, id);
                assert_eq!(state.summary, expected);
                assert_eq!(
                    state.summary,
                    cosmix_dopus_core::pane_summary(&core.pane(id).root)
                );
                assert_eq!(state.status, core.pane(id).status);
            }
        }
        core.on_event(CoreEvent::ListingArrived {
            pane: PaneId::Right,
            generation: core.pane(PaneId::Right).generation,
            path: dir.path().to_owned(),
            root: true,
            result: Ok(Vec::new()),
        });
        assert_eq!(
            pane_state(&core, PaneId::Right).summary,
            "0 folders, 0 files (0 B)"
        );
        assert_eq!(
            pane_state(&core, PaneId::Left).summary,
            "1 folder, 1 file (1.5 KiB)"
        );
    }
}
