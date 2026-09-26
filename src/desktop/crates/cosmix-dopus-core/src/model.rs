//! The semantic heart of dopus: twin-pane state, navigation, sorting, the
//! async listing/count pipeline, operation single-flight, the confirm/prompt
//! reservation book and the config settle debounce.
//!
//! Ported from src/desktop/apps/filemgr/src/browser.rs (Bevy/ctk); filemgr
//! stays untouched until retirement. Pure logic (sorting, filtering,
//! formatting, drop legality) is lifted near-verbatim with original line
//! citations; the Bevy resources/entities become plain state on
//! [`DopusCore`] and [`PaneModel`]. The only data a view needs is
//! [`DopusCore::visible_rows`].
//!
//! The core never reads the wall clock for behaviour: time-sensitive entry
//! points take `Instant`/`SystemTime` parameters so tests inject fixed times.

use std::cmp::Ordering as CmpOrdering;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Local, Utc};

use crate::config::{ConfigFile, DOpusConfig, PaneConfig, SortColumn, CURRENT_SCHEMA};
use crate::events::{ConfirmAnswer, CoreEvent, PromptKind};
use crate::ops::{FileOpKind, FileOperation};
use crate::worker::WorkerHandle;

/// Directory-count worker cap (browser.rs:317).
const DIRECTORY_COUNT_CONCURRENCY: usize = 4;

/// Config settle debounce (filemgr `ConfigPersistence`, browser.rs:90).
const CONFIG_SETTLE: Duration = Duration::from_millis(350);

/// Idle information-panel text (browser.rs:548).
const INFO_IDLE: &str = "Select a file or folder";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneId {
    Left,
    Right,
}

impl PaneId {
    pub fn index(self) -> usize {
        match self {
            Self::Left => 0,
            Self::Right => 1,
        }
    }

    /// `other_pane` (browser.rs:1922).
    pub fn other(self) -> PaneId {
        match self {
            Self::Left => Self::Right,
            Self::Right => Self::Left,
        }
    }
}

/// One row of pane data (browser.rs:459-467). `name` is the display
/// projection (control characters sanitised, browser.rs:1434-1438); `path`
/// retains the real OsStr bytes for every filesystem operation.
#[derive(Clone, Debug)]
pub struct FileEntry {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
    pub size: Option<u64>,
    pub child_count: Option<usize>,
    pub modified: Option<SystemTime>,
}

/// Back/forward navigation with the 128-entry back cap and forward cleared on
/// every new navigation (browser.rs:260-290).
#[derive(Debug, Default)]
pub struct NavigationHistory {
    pub back: Vec<PathBuf>,
    pub forward: Vec<PathBuf>,
}

impl NavigationHistory {
    fn record_new(&mut self, current: &Path, target: &Path) -> bool {
        if current == target {
            return false;
        }
        self.back.push(current.to_path_buf());
        if self.back.len() > 128 {
            self.back.remove(0);
        }
        self.forward.clear();
        true
    }

    fn back(&mut self, current: &Path) -> Option<PathBuf> {
        let target = self.back.pop()?;
        self.forward.push(current.to_path_buf());
        Some(target)
    }

    fn forward(&mut self, current: &Path) -> Option<PathBuf> {
        let target = self.forward.pop()?;
        self.back.push(current.to_path_buf());
        Some(target)
    }
}

/// One pane's plain state (browser.rs `PaneState`, 229-248, minus entities).
#[derive(Debug)]
pub struct PaneModel {
    pub path: PathBuf,
    /// This pane's current listing generation; a reply is accepted only when
    /// it matches (browser.rs:1571).
    pub generation: u64,
    /// Worker-side mirror of `generation` for count-job liveness re-checks
    /// (browser.rs `ListingInbox.generations`, 137, 1367).
    pub(crate) generation_arc: Arc<AtomicU64>,
    pub listing: bool,
    pub root: Vec<FileEntry>,
    pub children: HashMap<PathBuf, Vec<FileEntry>>,
    pub expanded: HashSet<PathBuf>,
    pub pending_children: HashSet<PathBuf>,
    pub selected: Option<PathBuf>,
    pub history: NavigationHistory,
    pub show_hidden: bool,
    pub sort: SortColumn,
    pub ascending: bool,
    pub status: String,
    /// Directory-count jobs sent but not yet replied (browser.rs
    /// `pending_counts`, 240).
    pub(crate) pending_counts: usize,
    /// A count reply changed a child count while Size sort was active;
    /// re-sort once every outstanding count has landed (browser.rs:241).
    pub(crate) count_sort_dirty: bool,
}

impl PaneModel {
    fn new(path: PathBuf, show_hidden: bool, sort: SortColumn, ascending: bool) -> Self {
        Self {
            path,
            generation: 0,
            generation_arc: Arc::new(AtomicU64::new(0)),
            listing: false,
            root: Vec::new(),
            children: HashMap::new(),
            expanded: HashSet::new(),
            pending_children: HashSet::new(),
            selected: None,
            history: NavigationHistory::default(),
            show_hidden,
            sort,
            ascending,
            status: "Loading…".into(),
            pending_counts: 0,
            count_sort_dirty: false,
        }
    }

    /// `action_selection_available` (browser.rs:251-253).
    fn action_selection_available(&self) -> bool {
        !self.listing && self.selected.is_some()
    }

    /// `action_rows_available` (browser.rs:255-257).
    fn action_rows_available(&self) -> bool {
        !self.listing && !self.root.is_empty()
    }
}

/// A queued directory-count job (browser.rs:319-324).
#[derive(Debug)]
pub(crate) struct CountJob {
    pub pane: PaneId,
    pub generation: u64,
    pub entry_path: PathBuf,
    pub show_hidden: bool,
}

/// Single-flight operation slot (browser.rs `FileActionState.pending`, 413).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OpState {
    Idle,
    Running,
}

/// What a reserved token will launch when resolved (browser.rs `ConfirmedOp`
/// 422-427 plus `NameEditKind` 454-457).
enum Reservation {
    Delete { source: PathBuf, source_pane: PaneId },
    NewFolder { parent: PathBuf, pane: PaneId },
    Rename { source: PathBuf, pane: PaneId },
}

/// Token-keyed single-flight reservation book (browser.rs
/// `FileActionState.pending_confirm`, 405-411). Tokens are minted by the core
/// and echoed back by the app; each token is consumed exactly once — by a
/// resolution, a withdrawal, or nothing at all (fail-closed).
struct ConfirmBook {
    next_token: u64,
    entries: HashMap<u64, Reservation>,
}

impl ConfirmBook {
    fn new() -> Self {
        Self {
            next_token: 1,
            entries: HashMap::new(),
        }
    }

    fn insert(&mut self, reservation: Reservation) -> u64 {
        let token = self.next_token;
        self.next_token += 1;
        self.entries.insert(token, reservation);
        token
    }
}

/// The headless twin-pane browser. All methods run on the caller's (UI)
/// thread; filesystem work happens on detached worker threads that reply
/// through the channel returned by [`DopusCore::new`].
pub struct DopusCore {
    panes: [PaneModel; 2],
    active: PaneId,
    split_ratio: f32,
    operation: OpState,
    confirms: ConfirmBook,
    /// Information-panel text. Only clock-free strings live here (idle text,
    /// operation results); the view formats selection details itself so the
    /// relative modified time uses the app's clock.
    info: String,
    last_observed: Option<DOpusConfig>,
    pending_config: Option<DOpusConfig>,
    config_dirty_since: Option<Instant>,
    config_file: Option<ConfigFile>,
    /// Global listing nonce (browser.rs `ListingInbox.nonce`, 141): every
    /// `start_listing` bumps it, so generations are unique across panes.
    listing_nonce: u64,
    count_queue: VecDeque<CountJob>,
    workers: WorkerHandle,
    /// Derived view-facing events awaiting [`DopusCore::tick`] or
    /// [`DopusCore::on_event`].
    pending: Vec<CoreEvent>,
}

impl DopusCore {
    /// Build the core from a startup config and optionally a persistence
    /// target. Both panes start listing immediately; their `ListingStarted`
    /// events are waiting in the queue for the first [`DopusCore::tick`].
    pub fn new(config: DOpusConfig, config_file: Option<ConfigFile>) -> (Self, mpsc::Receiver<CoreEvent>) {
        let (tx, rx) = mpsc::channel();
        let workers = WorkerHandle::new(tx);
        let home = home_directory();
        let left_start = configured_directory(&config.left.path, &home);
        let right_start = configured_directory(&config.right.path, &home);
        let mut core = Self {
            panes: [
                PaneModel::new(
                    left_start,
                    config.left.show_hidden,
                    config.left.sort,
                    config.left.ascending,
                ),
                PaneModel::new(
                    right_start,
                    config.right.show_hidden,
                    config.right.sort,
                    config.right.ascending,
                ),
            ],
            active: if config.active_pane == "right" {
                PaneId::Right
            } else {
                PaneId::Left
            },
            split_ratio: config.split_ratio,
            operation: OpState::Idle,
            confirms: ConfirmBook::new(),
            info: INFO_IDLE.into(),
            last_observed: None,
            pending_config: None,
            config_dirty_since: None,
            config_file,
            listing_nonce: 0,
            count_queue: VecDeque::new(),
            workers,
            pending: Vec::new(),
        };
        core.start_listing(PaneId::Left);
        core.start_listing(PaneId::Right);
        (core, rx)
    }

    // -- view projections ---------------------------------------------------

    pub fn pane(&self, pane: PaneId) -> &PaneModel {
        &self.panes[pane.index()]
    }

    pub fn active(&self) -> PaneId {
        self.active
    }

    /// The flatten projection (browser.rs `flatten_entries`, 1991-2012,
    /// depth cap 64). The only data a view needs.
    pub fn visible_rows(&self, pane: PaneId) -> Vec<VisibleRow> {
        let pane = &self.panes[pane.index()];
        let mut visible = Vec::new();
        flatten_entries(&pane.root, 0, &pane.expanded, &pane.children, &mut visible);
        visible
    }

    pub fn info(&self) -> &str {
        &self.info
    }

    /// Action availability as plain data (filemgr action.rs:114-124, extended
    /// with the fields the iced keymap layer needs). The keymap/action layer
    /// itself is not ported — the app uses `cosmix-actions` directly.
    pub fn availability(&self) -> AvailabilitySnapshot {
        let pane = &self.panes[self.active.index()];
        let rows = self.visible_rows(self.active);
        AvailabilitySnapshot {
            can_go_back: !pane.history.back.is_empty(),
            can_go_forward: !pane.history.forward.is_empty(),
            can_go_parent: pane.path.parent().is_some(),
            has_selection: pane.action_selection_available(),
            selection_is_dir: pane
                .selected
                .as_deref()
                .and_then(|selected| find_entry(pane, selected))
                .is_some_and(|entry| entry.is_dir),
            rows_available: pane.action_rows_available(),
            operation_running: self.operation == OpState::Running,
            show_hidden: pane.show_hidden,
            sort: pane.sort,
            ascending: pane.ascending,
        }
    }

    /// The session config as it would be persisted right now (browser.rs
    /// `persist_config` snapshot, 3578-3602). Derived from pane state — the
    /// config is never stored as a second copy.
    pub fn config_snapshot(&self) -> DOpusConfig {
        let pane_config = |pane: &PaneModel| PaneConfig {
            path: pane.path.clone(),
            show_hidden: pane.show_hidden,
            sort: pane.sort,
            ascending: pane.ascending,
        };
        DOpusConfig {
            schema_version: CURRENT_SCHEMA,
            left: pane_config(&self.panes[PaneId::Left.index()]),
            right: pane_config(&self.panes[PaneId::Right.index()]),
            active_pane: match self.active {
                PaneId::Left => "left",
                PaneId::Right => "right",
            }
            .into(),
            split_ratio: self.split_ratio,
        }
    }

    // -- navigation ---------------------------------------------------------

    pub fn navigate(&mut self, pane: PaneId, path: PathBuf) {
        self.navigate_new(pane, path);
    }

    pub fn go_back(&mut self) {
        let pane_id = self.active;
        let target = {
            let pane = &mut self.panes[pane_id.index()];
            match pane.history.back(&pane.path) {
                Some(target) => target,
                None => return,
            }
        };
        self.panes[pane_id.index()].path = target;
        self.start_listing(pane_id);
    }

    pub fn go_forward(&mut self) {
        let pane_id = self.active;
        let target = {
            let pane = &mut self.panes[pane_id.index()];
            match pane.history.forward(&pane.path) {
                Some(target) => target,
                None => return,
            }
        };
        self.panes[pane_id.index()].path = target;
        self.start_listing(pane_id);
    }

    /// Navigate to the parent of the active pane's directory, if any. The
    /// single source of truth for "go up" (browser.rs:3425-3438).
    pub fn go_parent(&mut self) {
        let pane_id = self.active;
        if let Some(parent) = self.panes[pane_id.index()].path.parent().map(Path::to_path_buf) {
            self.navigate_new(pane_id, parent);
        }
    }

    pub fn go_home(&mut self) {
        let pane_id = self.active;
        self.navigate_new(pane_id, home_directory());
    }

    /// `navigate_new` (browser.rs:3383-3395): record history only when the
    /// target differs, then always relist.
    fn navigate_new(&mut self, pane_id: PaneId, target: PathBuf) {
        let changed = {
            let pane = &mut self.panes[pane_id.index()];
            pane.history.record_new(&pane.path, &target)
        };
        if changed {
            self.panes[pane_id.index()].path = target;
        }
        self.start_listing(pane_id);
    }

    pub fn refresh(&mut self) {
        self.start_listing(self.active);
    }

    // -- view options -------------------------------------------------------

    /// Flip the active pane's hidden-file visibility and re-list (browser.rs
    /// 3440-3452).
    pub fn toggle_hidden(&mut self) {
        let pane_id = self.active;
        self.panes[pane_id.index()].show_hidden = !self.panes[pane_id.index()].show_hidden;
        self.start_listing(pane_id);
    }

    /// Set the active pane's sort. Same column toggles direction; a new
    /// column adopts the requested direction (browser.rs:3178-3196 — filemgr
    /// hardcoded ascending on switch; the flag keeps the core API explicit
    /// while the app passes `true` for identical behaviour).
    pub fn set_sort(&mut self, column: SortColumn, ascending: bool) {
        let pane_id = self.active;
        let pane = &mut self.panes[pane_id.index()];
        if pane.sort == column {
            pane.ascending = !pane.ascending;
        } else {
            pane.sort = column;
            pane.ascending = ascending;
        }
        sort_all_entries(pane);
        pane.count_sort_dirty = false;
    }

    pub fn set_active_pane(&mut self, pane: PaneId) {
        self.active = pane;
        self.emit(CoreEvent::InfoChanged);
    }

    pub fn switch_pane(&mut self) {
        self.set_active_pane(self.active.other());
    }

    pub fn set_split_ratio(&mut self, ratio: f32) {
        self.split_ratio = ratio;
    }

    // -- selection ----------------------------------------------------------

    /// Move the selection over [`DopusCore::visible_rows`] (browser.rs
    /// `select_relative`, 3208-3227).
    pub fn select_relative(&mut self, pane: PaneId, delta: isize) {
        let rows = self.visible_rows(pane);
        if rows.is_empty() {
            return;
        }
        let current = self.panes[pane.index()]
            .selected
            .as_ref()
            .and_then(|selected| rows.iter().position(|row| &row.entry.path == selected));
        let index = if delta > 0 {
            current.map_or(0, |index| (index + 1).min(rows.len().saturating_sub(1)))
        } else {
            current.unwrap_or(0).saturating_sub(1)
        };
        self.select_index(pane, &rows, index);
    }

    /// Jump to the first or last visible row (browser.rs `select_edge`,
    /// 3229-3241).
    pub fn select_edge(&mut self, pane: PaneId, last: bool) {
        let rows = self.visible_rows(pane);
        let index = if last {
            rows.len().checked_sub(1)
        } else if rows.is_empty() {
            None
        } else {
            Some(0)
        };
        if let Some(index) = index {
            self.select_index(pane, &rows, index);
        }
    }

    /// Direct selection (the plain-state equivalent of `select_row`,
    /// browser.rs:2867-2873: selecting a row also activates its pane).
    pub fn select_path(&mut self, pane: PaneId, path: Option<PathBuf>) {
        self.panes[pane.index()].selected = path;
        self.active = pane;
        self.emit(CoreEvent::SelectionChanged { pane });
        self.emit(CoreEvent::InfoChanged);
    }

    fn select_index(&mut self, pane: PaneId, rows: &[VisibleRow], index: usize) {
        let Some(row) = rows.get(index) else {
            return;
        };
        self.panes[pane.index()].selected = Some(row.entry.path.clone());
        self.active = pane;
        self.emit(CoreEvent::SelectionChanged { pane });
        self.emit(CoreEvent::InfoChanged);
    }

    // -- tree ---------------------------------------------------------------

    /// Expand/collapse a directory row (browser.rs `on_tree_changed`,
    /// 2427-2473). The first expansion lists children carrying the captured
    /// generation; a later reply is accepted only if that generation still
    /// governs the pane.
    pub fn toggle_expand(&mut self, pane: PaneId, path: &Path) {
        {
            let pane = &mut self.panes[pane.index()];
            // Only directories expand (browser.rs:2443-2445).
            match find_entry(pane, path) {
                Some(entry) if entry.is_dir => {}
                _ => return,
            }
            if pane.expanded.contains(path) {
                pane.expanded.remove(path);
                return;
            }
            pane.expanded.insert(path.to_path_buf());
            if pane.children.contains_key(path) {
                // Already listed: the flatten projection picks it up.
                return;
            }
        }
        self.start_child_listing(pane, path.to_path_buf());
    }

    /// `start_child_listing` (browser.rs:1403-1422): deduplicated through
    /// `pending_children`, carrying the pane's captured generation.
    fn start_child_listing(&mut self, pane_id: PaneId, path: PathBuf) {
        let (generation, show_hidden) = {
            let pane = &mut self.panes[pane_id.index()];
            if !pane.pending_children.insert(path.clone()) {
                return;
            }
            (pane.generation, pane.show_hidden)
        };
        self.workers
            .spawn_listing(pane_id, generation, path, false, show_hidden);
    }

    // -- opening & operations -----------------------------------------------

    /// Open the active pane's selection: a directory navigates, a non-directory
    /// emits [`CoreEvent::OpenFile`] — the `xdg-open` spawn stays in the app
    /// (browser.rs:3255-3281).
    pub fn open_selection(&mut self) {
        let pane_id = self.active;
        let Some(path) = self.panes[pane_id.index()].selected.clone() else {
            return;
        };
        match find_entry(&self.panes[pane_id.index()], &path) {
            Some(entry) if entry.is_dir => self.navigate_new(pane_id, path),
            Some(_) => self.emit(CoreEvent::OpenFile(path)),
            None => {}
        }
    }

    pub fn copy_selection_to_other_pane(&mut self) {
        self.transfer_selection(true);
    }

    pub fn move_selection_to_other_pane(&mut self) {
        self.transfer_selection(false);
    }

    /// `transfer_selection` (browser.rs:3283-3305).
    fn transfer_selection(&mut self, copy: bool) {
        let pane_id = self.active;
        let Some(source) = self.panes[pane_id.index()].selected.clone() else {
            return;
        };
        let destination = self.panes[pane_id.other().index()].path.clone();
        let operation = if copy {
            FileOperation::copy(source, destination)
        } else {
            FileOperation::move_to(source, destination)
        };
        self.start_operation(operation, pane_id);
    }

    /// Raise the destructive-delete confirmation (browser.rs
    /// `request_delete_confirm`, 958-992). Refuses while anything is in
    /// flight; the token is resolved through [`DopusCore::confirm`].
    pub fn delete_selection(&mut self) {
        if !self.is_idle() {
            return;
        }
        let pane_id = self.active;
        let Some(source) = self.panes[pane_id.index()].selected.clone() else {
            return;
        };
        let message = format!(
            "Permanently delete this item?\n\n{}\n\nThis cannot be undone.",
            sanitise_display_path(&source)
        );
        let token = self
            .confirms
            .insert(Reservation::Delete { source, source_pane: pane_id });
        self.emit(CoreEvent::ConfirmRequested { token, message });
    }

    /// Resolve a delete confirmation. Anything but an explicit yes fails
    /// closed: the reservation is consumed and no operation runs. A token is
    /// consumed exactly once — a second resolution finds nothing and must
    /// never start a second operation.
    pub fn confirm(&mut self, token: u64, answer: ConfirmAnswer) {
        // Fail closed on unknown, stale or foreign (prompt) tokens without
        // touching them (browser.rs:1160-1171 dismiss/future-outcome law).
        match self.confirms.entries.get(&token) {
            Some(Reservation::Delete { .. }) => {}
            _ => return,
        }
        let Some(Reservation::Delete { source, source_pane }) = self.confirms.entries.remove(&token)
        else {
            unreachable!("reservation kind guarded above");
        };
        if answer == ConfirmAnswer::Yes {
            self.start_operation(FileOperation::delete(source), source_pane);
        }
    }

    /// Raise the new-folder name prompt (browser.rs `open_name_edit`,
    /// 3325-3381). Resolved through [`DopusCore::prompt_text`].
    pub fn begin_new_folder(&mut self) {
        if !self.is_idle() {
            return;
        }
        let pane = self.active;
        let parent = self.panes[pane.index()].path.clone();
        let token = self.confirms.insert(Reservation::NewFolder { parent, pane });
        self.emit(CoreEvent::PromptRequested {
            token,
            kind: PromptKind::NewFolder,
            initial: "New Folder".to_owned(),
        });
    }

    /// Raise the rename prompt. The initial text is deliberately NOT
    /// display-sanitised (browser.rs:3353-3355): it becomes the rename target
    /// when submitted, and a display projection here would silently rename an
    /// unchanged filename on Enter.
    pub fn begin_rename(&mut self) {
        if !self.is_idle() {
            return;
        }
        let pane = self.active;
        let Some(source) = self.panes[pane.index()].selected.clone() else {
            return;
        };
        let initial = source
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let token = self.confirms.insert(Reservation::Rename { source, pane });
        self.emit(CoreEvent::PromptRequested {
            token,
            kind: PromptKind::Rename,
            initial,
        });
    }

    /// Resolve a name prompt. `None` (dismissal) withdraws the reservation;
    /// `Some(name)` starts the operation — unless another operation holds the
    /// single-flight slot, in which case the reservation is still consumed,
    /// a status line explains, and nothing runs (fails exactly once).
    pub fn prompt_text(&mut self, token: u64, text: Option<String>) {
        match self.confirms.entries.get(&token) {
            Some(Reservation::NewFolder { .. } | Reservation::Rename { .. }) => {}
            // Unknown, stale or foreign (confirm) token: fail closed.
            _ => return,
        }
        let reservation = self
            .confirms
            .entries
            .remove(&token)
            .expect("reservation kind guarded above");
        // Dismissed outcomes resolve fail-closed (browser.rs:1132-1158: a
        // non-text interaction result consumes the edit and runs nothing).
        let Some(name) = text else { return };
        match reservation {
            Reservation::NewFolder { parent, pane } => {
                self.start_operation(FileOperation::new_folder(parent.join(name)), pane);
            }
            Reservation::Rename { source, pane } => {
                if let Some(parent) = source.parent().map(Path::to_path_buf) {
                    self.start_operation(FileOperation::rename(source, parent.join(name)), pane);
                }
                // No resolvable parent: the reservation is consumed and
                // nothing runs — fail closed.
            }
            Reservation::Delete { .. } => unreachable!("reservation kind guarded above"),
        }
    }

    /// `start_operation` (browser.rs:1783-1830). Single-flight: while one
    /// operation runs, new requests produce a status line, never a silent
    /// queue.
    fn start_operation(&mut self, operation: FileOperation, source_pane: PaneId) -> bool {
        if !self.is_idle() {
            self.set_status(Some(source_pane), "Another file operation is still running");
            return false;
        }
        self.operation = OpState::Running;
        let verb = match operation.kind {
            FileOpKind::Copy => "Copying",
            FileOpKind::Move => "Moving",
            FileOpKind::Delete => "Deleting",
            FileOpKind::NewFolder => "Creating",
            FileOpKind::Rename => "Renaming",
            FileOpKind::BatchCopy => "Copying batch",
            FileOpKind::BatchMove => "Moving batch",
        };
        self.set_status(
            Some(source_pane),
            &format!("{verb} {}…", operation.source.display()),
        );
        self.workers.spawn_operation(operation, source_pane);
        true
    }

    fn is_idle(&self) -> bool {
        self.operation == OpState::Idle && self.confirms.entries.is_empty()
    }

    // -- worker replies -----------------------------------------------------

    /// Re-enter a worker reply (or pass through any other event). Returns the
    /// derived view-facing events, including any queued by earlier mutators.
    /// Stale replies are validated and dropped here.
    pub fn on_event(&mut self, event: CoreEvent) -> Vec<CoreEvent> {
        match event {
            CoreEvent::ListingArrived {
                pane,
                generation,
                path,
                root,
                result,
            } => self.receive_listing(pane, generation, path, root, result),
            CoreEvent::CountArrived {
                pane,
                generation,
                path,
                count,
            } => self.receive_count(pane, generation, path, count),
            CoreEvent::OperationArrived {
                kind: _,
                source_pane,
                result,
            } => self.receive_operation(source_pane, result),
            // Mutators' outputs are already in the queue; a view event fed
            // back in passes through unchanged.
            other => self.emit(other),
        }
        self.take_events()
    }

    /// `receive_listings` (browser.rs:1544-1639).
    fn receive_listing(
        &mut self,
        pane_id: PaneId,
        generation: u64,
        path: PathBuf,
        root: bool,
        result: Result<Vec<FileEntry>, String>,
    ) {
        enum Listing {
            Ok { jobs: Vec<CountJob>, status: String },
            Err { selection_cleared: bool, status: String },
        }
        // Stale rejection (browser.rs:1571-1573): a reply is accepted ONLY
        // when BOTH the pane's generation and — for root listings — its path
        // match what is currently expected.
        let listing = {
            let pane = &mut self.panes[pane_id.index()];
            if pane.generation != generation || root && pane.path != path {
                return;
            }
            pane.pending_children.remove(&path);
            if root {
                pane.listing = false;
            }
            match result {
                Ok(mut entries) => {
                    if !root {
                        pane.count_sort_dirty |=
                            set_backing_child_count(pane, &path, Some(entries.len()))
                                && pane.sort == SortColumn::Size;
                    }
                    let jobs = entries
                        .iter()
                        .filter(|entry| entry.is_dir)
                        .map(|entry| CountJob {
                            pane: pane_id,
                            generation,
                            entry_path: entry.path.clone(),
                            show_hidden: pane.show_hidden,
                        })
                        .collect::<Vec<_>>();
                    pane.pending_counts = pane.pending_counts.saturating_add(jobs.len());
                    sort_entries(&mut entries, pane.sort, pane.ascending);
                    if root {
                        pane.root = entries;
                    } else {
                        pane.children.insert(path.clone(), entries);
                    }
                    if pane.pending_counts == 0 && pane.count_sort_dirty {
                        sort_all_entries(pane);
                        pane.count_sort_dirty = false;
                    }
                    let status = pane_summary(&pane.root);
                    pane.status = status.clone();
                    Listing::Ok { jobs, status }
                }
                Err(error) => {
                    let mut selection_cleared = false;
                    if root {
                        // clear_pane_rows (browser.rs:1352-1357): an error
                        // leaves the pane empty with no selection.
                        pane.selected = None;
                        pane.root.clear();
                        selection_cleared = true;
                    }
                    let status = sanitise_display_text(&error);
                    pane.status = status.clone();
                    Listing::Err {
                        selection_cleared,
                        status,
                    }
                }
            }
        };
        let mut events = Vec::new();
        match listing {
            Listing::Ok { jobs, status } => {
                self.count_queue.extend(jobs);
                events.push(CoreEvent::Status {
                    pane: Some(pane_id),
                    text: status,
                });
            }
            Listing::Err {
                selection_cleared,
                status,
            } => {
                if selection_cleared {
                    events.push(CoreEvent::SelectionChanged { pane: pane_id });
                }
                events.push(CoreEvent::Status {
                    pane: Some(pane_id),
                    text: status,
                });
            }
        }
        // A listing for the active pane resets the information panel
        // (browser.rs:1633-1637).
        if self.active == pane_id {
            self.info = INFO_IDLE.into();
            events.push(CoreEvent::InfoChanged);
        }
        self.pending.extend(events);
        self.dispatch_counts();
    }

    /// `receive_directory_counts` (browser.rs:1675-1759).
    fn receive_count(
        &mut self,
        pane_id: PaneId,
        generation: u64,
        path: PathBuf,
        count: Option<usize>,
    ) {
        let pane = &mut self.panes[pane_id.index()];
        if pane.generation != generation {
            return;
        }
        pane.pending_counts = pane.pending_counts.saturating_sub(1);
        pane.count_sort_dirty |=
            set_backing_child_count(pane, &path, count) && pane.sort == SortColumn::Size;
        // A count for the active pane's selected row repaints the information
        // panel (browser.rs `count_reply_repaints_information`, 1753-1759).
        let repaints_info =
            self.active == pane_id && pane.selected.as_deref() == Some(path.as_path());
        if pane.pending_counts == 0 && pane.count_sort_dirty {
            sort_all_entries(pane);
            pane.count_sort_dirty = false;
        }
        if repaints_info {
            self.emit(CoreEvent::InfoChanged);
        }
        self.dispatch_counts();
    }

    /// `receive_operations` (browser.rs:1832-1894). After EVERY operation
    /// reply — success OR failure — BOTH panes relist exactly once: a batch
    /// stops at its first runtime error with earlier items already
    /// transferred, and a failed move or delete can also have mutated the
    /// tree before failing, so a failure is not evidence that the panes still
    /// match the disk. One relist per reply either way — never one per batch
    /// item.
    fn receive_operation(&mut self, source_pane: PaneId, result: Result<String, String>) {
        self.operation = OpState::Idle;
        match result {
            Ok(message) => {
                self.info = sanitise_display_text(&message);
                self.emit(CoreEvent::InfoChanged);
            }
            Err(error) => {
                // Error text lands on the source pane's status line.
                self.set_status(Some(source_pane), &error);
            }
        }
        for pane_id in [PaneId::Left, PaneId::Right] {
            self.start_listing(pane_id);
        }
        self.emit(CoreEvent::RefreshAll);
    }

    /// `dispatch_directory_counts` (browser.rs:1641-1673): fill the worker
    /// cap from the queue, re-checking the pane's live generation at dispatch.
    fn dispatch_counts(&mut self) {
        while self.workers.count_in_flight.load(AtomicOrdering::Acquire) < DIRECTORY_COUNT_CONCURRENCY
        {
            let Some(job) = self.count_queue.pop_front() else {
                break;
            };
            let live_generation = Arc::clone(&self.workers.generations[job.pane.index()]);
            if live_generation.load(AtomicOrdering::Acquire) != job.generation {
                continue;
            }
            self.workers.spawn_count(job);
        }
    }

    // -- frame pump ---------------------------------------------------------

    /// Per-frame work: count dispatch plus the config settle debounce
    /// (browser.rs `persist_config`, 3564-3622). Returns the derived
    /// view-facing events queued so far.
    pub fn tick(&mut self, now: Instant) -> Vec<CoreEvent> {
        self.dispatch_counts();
        let snapshot = self.config_snapshot();
        if self.last_observed.as_ref() != Some(&snapshot) {
            self.last_observed = Some(snapshot.clone());
            self.pending_config = Some(snapshot);
            self.config_dirty_since = Some(now);
        }
        if let Some(since) = self.config_dirty_since {
            if now.duration_since(since) >= CONFIG_SETTLE {
                self.config_dirty_since = None;
                let mut save_error = None;
                if let Some(snapshot) = self.pending_config.take() {
                    if let Some(file) = &self.config_file {
                        if let Err(error) = file.save(&snapshot) {
                            save_error = Some(error);
                        }
                    }
                    self.emit(CoreEvent::ConfigSettled(snapshot));
                }
                if let Some(error) = save_error {
                    self.set_status(None, &error);
                }
            }
        }
        self.take_events()
    }

    // -- internals ----------------------------------------------------------

    /// `start_listing` (browser.rs:1359-1397). Bumps the global nonce and the
    /// pane's generation (both the u64 and the worker-side Arc), purges the
    /// pane's queued count jobs, clears rows/children/expansion, and spawns
    /// the listing.
    fn start_listing(&mut self, pane_id: PaneId) {
        self.listing_nonce += 1;
        let generation = self.listing_nonce;
        let (path, show_hidden) = {
            let pane = &mut self.panes[pane_id.index()];
            pane.generation = generation;
            // Every queued job for this pane belongs to a generation
            // superseded by the listing just started. Purge it immediately
            // even when all workers are occupied, so repeated refreshes
            // cannot accumulate directory-sized stale batches behind the
            // four in-flight jobs (browser.rs:1368-1372).
            self.count_queue.retain(|job| job.pane != pane_id);
            // clear_pane_rows (browser.rs:1352-1357).
            pane.selected = None;
            pane.listing = true;
            pane.root.clear();
            pane.children.clear();
            pane.expanded.clear();
            pane.pending_children.clear();
            pane.pending_counts = 0;
            pane.count_sort_dirty = false;
            (pane.path.clone(), pane.show_hidden)
        };
        self.workers
            .store_generation(pane_id, generation);
        self.workers
            .spawn_listing(pane_id, generation, path, true, show_hidden);
        self.emit(CoreEvent::ListingStarted { pane: pane_id });
        self.emit(CoreEvent::SelectionChanged { pane: pane_id });
    }

    fn set_status(&mut self, pane: Option<PaneId>, message: &str) {
        // Status text is presentation-only. File-operation errors retain raw
        // paths internally, but controls must not create forged status lines
        // (browser.rs:1909-1920).
        let text = sanitise_display_text(message);
        if let Some(pane) = pane {
            self.panes[pane.index()].status = text.clone();
        }
        self.emit(CoreEvent::Status { pane, text });
    }

    fn emit(&mut self, event: CoreEvent) {
        self.pending.push(event);
    }

    fn take_events(&mut self) -> Vec<CoreEvent> {
        std::mem::take(&mut self.pending)
    }
}

/// One flattened, depth-annotated row (browser.rs `VisibleEntry`, 1939-1943).
#[derive(Clone, Debug)]
pub struct VisibleRow {
    pub entry: FileEntry,
    pub depth: usize,
}

/// `flatten_entries` (browser.rs:1991-2012). Depth is capped at 64.
fn flatten_entries(
    entries: &[FileEntry],
    depth: usize,
    expanded: &HashSet<PathBuf>,
    children: &HashMap<PathBuf, Vec<FileEntry>>,
    output: &mut Vec<VisibleRow>,
) {
    if depth > 64 {
        return;
    }
    for entry in entries {
        output.push(VisibleRow {
            entry: entry.clone(),
            depth,
        });
        if entry.is_dir && expanded.contains(&entry.path) {
            if let Some(entries) = children.get(&entry.path) {
                flatten_entries(entries, depth + 1, expanded, children, output);
            }
        }
    }
}

/// Find an entry by path across the pane's root listing and expanded children.
fn find_entry<'a>(pane: &'a PaneModel, path: &Path) -> Option<&'a FileEntry> {
    pane.root
        .iter()
        .find(|entry| entry.path == path)
        .or_else(|| pane.children.values().flatten().find(|entry| entry.path == path))
}

/// `set_backing_child_count` (browser.rs:1761-1774): update the entry's child
/// count wherever it lives, reporting whether anything changed.
fn set_backing_child_count(pane: &mut PaneModel, path: &Path, count: Option<usize>) -> bool {
    for entry in pane
        .root
        .iter_mut()
        .chain(pane.children.values_mut().flatten())
    {
        if entry.path == path {
            let changed = entry.child_count != count;
            entry.child_count = count;
            return changed;
        }
    }
    false
}

fn sort_all_entries(pane: &mut PaneModel) {
    sort_entries(&mut pane.root, pane.sort, pane.ascending);
    for entries in pane.children.values_mut() {
        sort_entries(entries, pane.sort, pane.ascending);
    }
}

/// `entry_visible` (browser.rs:1478-1480): the dot-prefix rule.
pub fn entry_visible(name: &str, show_hidden: bool) -> bool {
    show_hidden || !name.starts_with('.')
}

/// `sanitise_display_text` (browser.rs:1482-1492): control characters become
/// U+FFFD in the DISPLAY PROJECTION ONLY — real OsStr bytes are kept for
/// operations, and rename input is deliberately NOT sanitised.
pub fn sanitise_display_text(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_control() {
                '\u{fffd}'
            } else {
                character
            }
        })
        .collect()
}

/// `sanitise_display_path` (browser.rs:1494-1498): sanitise the complete
/// rendered path, not only its final component — Unix permits control
/// characters in every ancestor directory name.
pub fn sanitise_display_path(path: &Path) -> String {
    sanitise_display_text(&path.to_string_lossy())
}

/// `read_directory` (browser.rs:1424-1456).
pub fn read_directory(directory: &Path, show_hidden: bool) -> Result<Vec<FileEntry>, String> {
    let read = std::fs::read_dir(directory)
        .map_err(|error| format!("{}: {error}", sanitise_display_path(directory)))?;
    let mut entries = Vec::new();
    for entry in read.flatten() {
        let raw_name = entry.file_name();
        let lossy_name = raw_name.to_string_lossy();
        if !entry_visible(&lossy_name, show_hidden) {
            continue;
        }
        // Unix permits control characters, including hard line breaks, in a
        // filename. Replace them only in the display projection so a name can
        // never turn a no-wrap row into multiple lines; `entry.path()` below
        // retains the real OsString bytes for every filesystem operation.
        let name = sanitise_display_text(&lossy_name);
        let path = entry.path();
        let metadata = entry.metadata().ok();
        let is_dir = metadata.as_ref().is_some_and(std::fs::Metadata::is_dir);
        let size = metadata
            .as_ref()
            .filter(|_| !is_dir)
            .map(std::fs::Metadata::len);
        entries.push(FileEntry {
            path,
            name,
            is_dir,
            size,
            child_count: None,
            modified: metadata.and_then(|metadata| metadata.modified().ok()),
        });
    }
    Ok(entries)
}

/// `count_directory_entries` (browser.rs:1458-1476): same filter as
/// `read_directory`, abortable per entry through `cancelled`.
pub fn count_directory_entries(
    directory: &Path,
    show_hidden: bool,
    mut cancelled: impl FnMut() -> bool,
) -> Option<usize> {
    let read = std::fs::read_dir(directory).ok()?;
    let mut count = 0;
    for entry in read {
        if cancelled() {
            return None;
        }
        let Ok(entry) = entry else {
            continue;
        };
        let name = entry.file_name();
        count += usize::from(entry_visible(&name.to_string_lossy(), show_hidden));
    }
    Some(count)
}

/// `compare_known` (browser.rs:1500-1514): known values sort by direction,
/// unknowns always last.
fn compare_known<T: Ord>(left: Option<T>, right: Option<T>, ascending: bool) -> CmpOrdering {
    match (left, right) {
        (Some(left), Some(right)) => {
            let ordering = left.cmp(&right);
            if ascending {
                ordering
            } else {
                ordering.reverse()
            }
        }
        (Some(_), None) => CmpOrdering::Less,
        (None, Some(_)) => CmpOrdering::Greater,
        (None, None) => CmpOrdering::Equal,
    }
}

/// `sort_entries` (browser.rs:1516-1542): directories first regardless of
/// direction; Name case-insensitive; Size for directories by known child
/// counts with unknowns last; Modified `Option<SystemTime>` None-last;
/// raw-name tie-break.
pub fn sort_entries(entries: &mut [FileEntry], column: SortColumn, ascending: bool) {
    entries.sort_by(|left, right| {
        let directory_order = right.is_dir.cmp(&left.is_dir);
        if directory_order != CmpOrdering::Equal {
            return directory_order;
        }
        let primary = match column {
            SortColumn::Name => {
                let ordering = left.name.to_lowercase().cmp(&right.name.to_lowercase());
                if ascending {
                    ordering
                } else {
                    ordering.reverse()
                }
            }
            SortColumn::Size => {
                if left.is_dir {
                    compare_known(left.child_count, right.child_count, ascending)
                } else {
                    compare_known(left.size, right.size, ascending)
                }
            }
            SortColumn::Modified => compare_known(left.modified, right.modified, ascending),
        };
        primary.then_with(|| left.name.cmp(&right.name))
    });
}

/// `home_directory` (browser.rs:3643-3647).
pub fn home_directory() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// `configured_directory` (browser.rs:3649-3656): a configured path that no
/// longer exists falls back to home rather than erroring at startup.
fn configured_directory(value: &Path, fallback: &Path) -> PathBuf {
    if value.is_dir() {
        value.to_path_buf()
    } else {
        fallback.to_path_buf()
    }
}

/// `filesystem_root` (browser.rs:3658-3663).
fn filesystem_root(path: &Path) -> PathBuf {
    path.ancestors()
        .last()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// The Places list (browser.rs `spawn_places` data, 1267-1284): Home, the
/// filesystem root, then the XDG user directories that exist.
pub fn places(home: &Path) -> Vec<(&'static str, PathBuf)> {
    let mut places = vec![
        ("Home", home.to_path_buf()),
        ("Filesystem", filesystem_root(home)),
    ];
    for name in [
        "Desktop",
        "Documents",
        "Downloads",
        "Music",
        "Pictures",
        "Videos",
    ] {
        let path = home.join(name);
        if path.is_dir() {
            places.push((name, path));
        }
    }
    places
}

// -- formatters (browser.rs:3665-3785) ------------------------------------

/// `format_size` (browser.rs:3665-3677): binary and compact.
pub fn format_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let bytes = bytes as f64;
    if bytes < KIB {
        format!("{} B", bytes as u64)
    } else if bytes < KIB * KIB {
        format!("{:.1} KiB", bytes / KIB)
    } else if bytes < KIB * KIB * KIB {
        format!("{:.1} MiB", bytes / (KIB * KIB))
    } else {
        format!("{:.1} GiB", bytes / (KIB * KIB * KIB))
    }
}

/// `format_child_count` (browser.rs:3679-3683).
pub fn format_child_count(count: Option<usize>) -> String {
    count
        .map(|count| format!("{count} {}", if count == 1 { "item" } else { "items" }))
        .unwrap_or_default()
}

/// `format_file_info` (browser.rs:3685-3708), restructured from the Bevy
/// `FileRow` component to [`FileEntry`].
pub fn format_file_info(entry: &FileEntry, now: SystemTime) -> String {
    let (quantity_label, quantity) = if entry.is_dir {
        (
            "Contents",
            entry
                .child_count
                .map(|count| format!("{count} {}", if count == 1 { "item" } else { "items" }))
                .unwrap_or_else(|| "—".into()),
        )
    } else {
        (
            "Size",
            entry.size.map(format_size).unwrap_or_else(|| "—".into()),
        )
    };
    format!(
        "{}\nType: {}\n{quantity_label}: {quantity}\nModified: {}\n\n{}",
        entry.name,
        if entry.is_dir { "Folder" } else { "File" },
        entry
            .modified
            .map(|modified| format_modified_at(modified, now))
            .unwrap_or_else(|| "—".into()),
        sanitise_display_path(&entry.path)
    )
}

/// `pane_summary` (browser.rs:3710-3726).
pub fn pane_summary(entries: &[FileEntry]) -> String {
    let folders = entries.iter().filter(|entry| entry.is_dir).count();
    let files = entries.len().saturating_sub(folders);
    let bytes = entries
        .iter()
        .filter(|entry| !entry.is_dir)
        .filter_map(|entry| entry.size)
        .sum();
    format!(
        "{} {}, {} {} ({})",
        folders,
        if folders == 1 { "folder" } else { "folders" },
        files,
        if files == 1 { "file" } else { "files" },
        format_size(bytes)
    )
}

/// `format_modified_at` (browser.rs:3728-3732): relative with an absolute
/// fallback.
pub fn format_modified_at(modified: SystemTime, now: SystemTime) -> String {
    format_modified_at_with(modified, now, |modified| {
        format_absolute_system_time(modified).unwrap_or_else(|| "—".into())
    })
}

/// `format_absolute_system_time` (browser.rs:3734-3737).
fn format_absolute_system_time(modified: SystemTime) -> Option<String> {
    let utc = system_time_to_utc(modified)?;
    Some(format_absolute_datetime(utc.with_timezone(&Local)))
}

/// `system_time_to_utc` (browser.rs:3739-3760): `None` outside chrono's
/// range instead of panicking.
fn system_time_to_utc(time: SystemTime) -> Option<DateTime<Utc>> {
    let (seconds, nanoseconds) = match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => (
            i64::try_from(duration.as_secs()).ok()?,
            duration.subsec_nanos(),
        ),
        Err(error) => {
            let duration = error.duration();
            let seconds = i64::try_from(duration.as_secs()).ok()?;
            let nanoseconds = duration.subsec_nanos();
            if nanoseconds == 0 {
                (seconds.checked_neg()?, 0)
            } else {
                (
                    seconds.checked_neg()?.checked_sub(1)?,
                    1_000_000_000 - nanoseconds,
                )
            }
        }
    };
    DateTime::<Utc>::from_timestamp(seconds, nanoseconds)
}

/// `format_absolute_datetime` (browser.rs:3762-3768).
fn format_absolute_datetime<Tz>(modified: DateTime<Tz>) -> String
where
    Tz: chrono::TimeZone,
    Tz::Offset: std::fmt::Display,
{
    modified.format("%d/%m/%y at %-I:%M %P").to_string()
}

/// `format_modified_at_with` (browser.rs:3770-3784): boundaries are injected
/// through `now` and the absolute renderer is a parameter so tests can pin it.
pub fn format_modified_at_with(
    modified: SystemTime,
    now: SystemTime,
    absolute: impl FnOnce(SystemTime) -> String,
) -> String {
    match now.duration_since(modified) {
        Ok(age) if age < Duration::from_secs(60) => "now".into(),
        Ok(age) if age < Duration::from_secs(3600) => format!("{}m ago", age.as_secs() / 60),
        Ok(age) if age < Duration::from_secs(86_400) => format!("{}h ago", age.as_secs() / 3600),
        Ok(age) if age < Duration::from_secs(7 * 86_400) => {
            format!("{}d ago", age.as_secs() / 86_400)
        }
        Ok(_) | Err(_) => absolute(modified),
    }
}

// -- drop legality (browser.rs:2661-2732) ----------------------------------
//
// Ported now, while fresh, for a later drag-and-drop arc; v1 has no OS DnD.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropAction {
    Copy,
    Move,
    Ask,
}

/// The keyboard modifiers a drop carries (the plain-data stand-in for Bevy's
/// `Modifiers`, browser.rs:2705-2713).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DropModifiers {
    pub control: bool,
    pub shift: bool,
}

/// The allowed-actions mask (the plain-data stand-in for ctk's `ActionMask`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DropActionMask {
    copy: bool,
    move_: bool,
    ask: bool,
}

impl DropActionMask {
    pub const NONE: Self = Self {
        copy: false,
        move_: false,
        ask: false,
    };
    pub const ALL: Self = Self {
        copy: true,
        move_: true,
        ask: true,
    };

    pub const fn contains(self, action: DropAction) -> bool {
        match action {
            DropAction::Copy => self.copy,
            DropAction::Move => self.move_,
            DropAction::Ask => self.ask,
        }
    }
}

/// `file_drop_actions` (browser.rs:2661-2667).
pub fn file_drop_actions(source: &Path, destination: &Path, busy: bool) -> DropActionMask {
    if busy || !drop_destination_is_distinct(source, destination) {
        DropActionMask::NONE
    } else {
        DropActionMask::ALL
    }
}

/// `file_drop_actions_batch` (browser.rs:2669-2679).
pub fn file_drop_actions_batch(sources: &[PathBuf], destination: &Path, busy: bool) -> DropActionMask {
    if sources.is_empty()
        || sources
            .iter()
            .any(|source| !drop_destination_is_distinct(source, destination))
    {
        DropActionMask::NONE
    } else {
        file_drop_actions(&sources[0], destination, busy)
    }
}

/// `drop_destination_is_distinct` (browser.rs:2685-2703). Resolves the
/// existing source and destination once each and compares filesystem
/// identity, not lexical spelling. A source symlink is an entry to copy/move,
/// never a directory root: `symlink_metadata` deliberately prevents its
/// target from participating in the descendant test.
pub fn drop_destination_is_distinct(source: &Path, destination: &Path) -> bool {
    let Ok(source_metadata) = std::fs::symlink_metadata(source) else {
        return false;
    };
    let Ok(destination) = destination.canonicalize() else {
        return false;
    };
    if source_metadata.is_dir() && !source_metadata.file_type().is_symlink() {
        let Ok(source) = source.canonicalize() else {
            return false;
        };
        source.parent() != Some(destination.as_path()) && !destination.starts_with(source)
    } else {
        source
            .parent()
            .and_then(|parent| parent.canonicalize().ok())
            .is_some_and(|parent| parent != destination)
    }
}

/// `requested_drop_action` (browser.rs:2705-2713): the KDE convention —
/// Ctrl copies, Shift moves, anything else asks.
pub fn requested_drop_action(modifiers: DropModifiers) -> DropAction {
    if modifiers.control {
        DropAction::Copy
    } else if modifiers.shift {
        DropAction::Move
    } else {
        DropAction::Ask
    }
}

/// `transfer_operation` (browser.rs:2715-2732).
pub fn transfer_operation(
    action: DropAction,
    sources: Vec<PathBuf>,
    destination: PathBuf,
) -> Result<FileOperation, String> {
    let [source] = sources.as_slice() else {
        return match action {
            DropAction::Copy => FileOperation::copy_batch(sources, destination),
            DropAction::Move => FileOperation::move_batch(sources, destination),
            DropAction::Ask => unreachable!("Ask requires transfer confirmation"),
        };
    };
    Ok(match action {
        DropAction::Copy => FileOperation::copy(source.clone(), destination),
        DropAction::Move => FileOperation::move_to(source.clone(), destination),
        DropAction::Ask => unreachable!("Ask requires transfer confirmation"),
    })
}

/// Action availability as plain data (filemgr action.rs:114-124, extended per
/// the dopus keymap contract).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AvailabilitySnapshot {
    pub can_go_back: bool,
    pub can_go_forward: bool,
    pub can_go_parent: bool,
    pub has_selection: bool,
    pub selection_is_dir: bool,
    pub rows_available: bool,
    pub operation_running: bool,
    pub show_hidden: bool,
    pub sort: SortColumn,
    pub ascending: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now_instant() -> Instant {
        Instant::now()
    }

    /// A core rooted at a temp directory with `left/` and `right/` panes.
    fn core_fixture() -> (tempfile::TempDir, DopusCore, mpsc::Receiver<CoreEvent>) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("left")).unwrap();
        std::fs::create_dir(dir.path().join("right")).unwrap();
        let config = DOpusConfig {
            left: PaneConfig {
                path: dir.path().join("left"),
                ..Default::default()
            },
            right: PaneConfig {
                path: dir.path().join("right"),
                ..Default::default()
            },
            ..Default::default()
        };
        let (core, rx) = DopusCore::new(config, None);
        (dir, core, rx)
    }

    fn pane_fixture() -> PaneModel {
        PaneModel::new(PathBuf::from("/fixture"), false, SortColumn::Name, true)
    }

    fn entry(name: &str, is_dir: bool) -> FileEntry {
        FileEntry {
            path: PathBuf::from(name),
            name: name.into(),
            is_dir,
            size: None,
            child_count: None,
            modified: None,
        }
    }

    // -- sorting (browser.rs tests ~:4952-5113) ------------------------------

    #[test]
    fn directory_sort_puts_folders_first_then_names_case_insensitively() {
        let mut entries = vec![entry("z", false), entry("B", true), entry("a", true)];
        sort_entries(&mut entries, SortColumn::Name, true);
        assert_eq!(
            entries
                .into_iter()
                .map(|entry| entry.name)
                .collect::<Vec<_>>(),
            ["a", "B", "z"]
        );
    }

    #[test]
    fn descending_size_sort_keeps_directories_first() {
        let mut entries = vec![
            {
                let mut entry = entry("small", false);
                entry.size = Some(10);
                entry
            },
            {
                let mut entry = entry("folder", true);
                entry.child_count = Some(4);
                entry
            },
            {
                let mut entry = entry("large", false);
                entry.size = Some(100);
                entry
            },
        ];
        sort_entries(&mut entries, SortColumn::Size, false);
        assert_eq!(
            entries
                .into_iter()
                .map(|entry| entry.name)
                .collect::<Vec<_>>(),
            ["folder", "large", "small"]
        );
    }

    #[test]
    fn folder_size_sort_uses_known_child_counts_and_leaves_unknowns_last() {
        let mut entries = vec![
            entry("unknown", true),
            {
                let mut entry = entry("large", true);
                entry.child_count = Some(12);
                entry
            },
            {
                let mut entry = entry("small", true);
                entry.child_count = Some(2);
                entry
            },
        ];

        sort_entries(&mut entries, SortColumn::Size, true);
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["small", "large", "unknown"]
        );
        sort_entries(&mut entries, SortColumn::Size, false);
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["large", "small", "unknown"]
        );
    }

    #[test]
    fn a_changed_child_listing_count_reorders_size_sorted_folders() {
        let mut pane = pane_fixture();
        pane.sort = SortColumn::Size;
        pane.root = vec![
            {
                let mut entry = entry("growing", true);
                entry.path = PathBuf::from("/fixture/growing");
                entry.child_count = Some(2);
                entry
            },
            {
                let mut entry = entry("steady", true);
                entry.path = PathBuf::from("/fixture/steady");
                entry.child_count = Some(10);
                entry
            },
        ];
        sort_all_entries(&mut pane);
        assert_eq!(pane.root[0].name, "growing");

        assert!(set_backing_child_count(
            &mut pane,
            Path::new("/fixture/growing"),
            Some(102)
        ));
        sort_all_entries(&mut pane);

        assert_eq!(pane.root[0].name, "steady");
        assert!(!set_backing_child_count(
            &mut pane,
            Path::new("/fixture/growing"),
            Some(102)
        ));
    }

    // -- format family (browser.rs tests ~:5115-5208) ------------------------

    #[test]
    fn size_format_is_binary_and_compact() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1536), "1.5 KiB");
        assert_eq!(format_child_count(Some(1)), "1 item");
        assert_eq!(format_child_count(Some(15)), "15 items");
        assert_eq!(format_child_count(None), "");
    }

    #[test]
    fn folder_information_reports_contents_instead_of_inode_size() {
        let mut folder = entry("/fixture/folder", true);
        folder.size = Some(4096);
        folder.child_count = Some(15);

        let info = format_file_info(&folder, SystemTime::UNIX_EPOCH);
        assert!(info.contains("\nContents: 15 items\n"));
        assert!(!info.contains("\nSize: "));
    }

    #[test]
    fn information_panel_sanitises_controls_in_the_complete_display_path() {
        let entry = FileEntry {
            path: "/fixture\nancestor/short\nname.md".into(),
            name: "short\u{fffd}name.md".into(),
            is_dir: false,
            size: Some(12),
            child_count: None,
            modified: None,
        };

        let info = format_file_info(&entry, SystemTime::UNIX_EPOCH);

        assert!(info.ends_with("/fixture\u{fffd}ancestor/short\u{fffd}name.md"));
        assert!(!info.contains("fixture\nancestor"));
        assert!(!info.contains("short\nname.md"));
        assert_eq!(info.matches('\n').count(), 5);
    }

    #[test]
    fn pane_summary_matches_dolphin_style_counts() {
        let mut folder = entry("folder", true);
        folder.child_count = Some(2);
        let mut file = entry("file", false);
        file.size = Some(1536);
        let entries = vec![folder, file];
        assert_eq!(pane_summary(&entries), "1 folder, 1 file (1.5 KiB)");
    }

    // -- visibility, counts, sanitisation (browser.rs ~:5209-5258) -----------

    #[test]
    fn dotfiles_follow_the_per_pane_hidden_setting() {
        assert!(!entry_visible(".git", false));
        assert!(entry_visible(".git", true));
        assert!(entry_visible("music", false));
    }

    #[test]
    fn child_count_uses_the_listing_filter_and_honours_cancellation() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::File::create(dir.path().join("visible")).unwrap();
        std::fs::File::create(dir.path().join(".hidden")).unwrap();

        assert_eq!(
            count_directory_entries(dir.path(), false, || false),
            Some(1)
        );
        assert_eq!(count_directory_entries(dir.path(), true, || false), Some(2));
        assert_eq!(count_directory_entries(dir.path(), true, || true), None);
    }

    #[test]
    fn a_new_listing_purges_only_that_panes_queued_counts() {
        let (_dir, mut core, _rx) = core_fixture();
        core.tick(now_instant());
        for (pane, generation) in [(PaneId::Left, 1), (PaneId::Right, 2), (PaneId::Left, 3)] {
            core.count_queue.push_back(CountJob {
                pane,
                generation,
                entry_path: PathBuf::from(format!("/{generation}")),
                show_hidden: false,
            });
        }

        // A relist of the left pane must purge exactly its queued jobs
        // (browser.rs:1399-1401).
        core.refresh();

        assert_eq!(core.count_queue.len(), 1);
        assert_eq!(core.count_queue[0].pane, PaneId::Right);
    }

    #[test]
    fn read_directory_sanitises_display_controls_but_keeps_the_real_path() {
        let dir = tempfile::tempdir().unwrap();
        let real_name = "short\nname\t.md";
        std::fs::File::create(dir.path().join(real_name)).unwrap();

        let entries = read_directory(dir.path(), true).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "short\u{fffd}name\u{fffd}.md");
        assert_eq!(
            entries[0].path.file_name().unwrap().to_string_lossy(),
            real_name
        );
    }

    // -- chrono boundaries (browser.rs ~:5260-5306) ---------------------------

    #[test]
    fn modified_time_boundaries_use_the_injected_clock() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let relative = |age| {
            format_modified_at_with(now - Duration::from_secs(age), now, |_| "absolute".into())
        };

        assert_eq!(relative(0), "now");
        assert_eq!(relative(59), "now");
        assert_eq!(relative(60), "1m ago");
        assert_eq!(relative(3_599), "59m ago");
        assert_eq!(relative(3_600), "1h ago");
        assert_eq!(relative(86_399), "23h ago");
        assert_eq!(relative(86_400), "1d ago");
        assert_eq!(relative(7 * 86_400 - 1), "6d ago");
        assert_eq!(relative(7 * 86_400), "absolute");
        assert_eq!(
            format_modified_at_with(now + Duration::from_secs(1), now, |_| "absolute".into()),
            "absolute"
        );
    }

    #[test]
    fn absolute_modified_time_uses_the_dolphin_style_local_format() {
        use chrono::TimeZone;

        let timezone = chrono::FixedOffset::east_opt(10 * 60 * 60).unwrap();
        let modified = timezone
            .with_ymd_and_hms(2025, 11, 25, 11, 20, 0)
            .single()
            .unwrap();

        assert_eq!(format_absolute_datetime(modified), "25/11/25 at 11:20 am");
    }

    #[test]
    fn out_of_chrono_range_modified_time_falls_back_without_panicking() {
        let out_of_range_seconds = u64::try_from(DateTime::<Utc>::MAX_UTC.timestamp())
            .unwrap()
            .saturating_add(86_400);
        let modified = UNIX_EPOCH
            .checked_add(Duration::from_secs(out_of_range_seconds))
            .unwrap();

        assert!(system_time_to_utc(modified).is_none());
        assert_eq!(format_modified_at(modified, UNIX_EPOCH), "—");
    }

    // -- stale rejection + history (browser.rs ~:5309-5325) -------------------

    #[test]
    fn stale_listing_requires_both_matching_generation_and_path() {
        let (_dir, mut core, _rx) = core_fixture();
        core.tick(now_instant());

        core.navigate(PaneId::Left, PathBuf::from("/music"));
        let generation = core.pane(PaneId::Left).generation;

        // Both match: accepted.
        let events = core.on_event(CoreEvent::ListingArrived {
            pane: PaneId::Left,
            generation,
            path: PathBuf::from("/music"),
            root: true,
            result: Ok(vec![]),
        });
        assert!(!events.is_empty(), "the matching reply is accepted");
        assert!(!core.pane(PaneId::Left).listing);

        // Generation mismatch: rejected.
        core.navigate(PaneId::Left, PathBuf::from("/music"));
        let generation = core.pane(PaneId::Left).generation;
        core.on_event(CoreEvent::ListingArrived {
            pane: PaneId::Left,
            generation: generation + 1,
            path: PathBuf::from("/music"),
            root: true,
            result: Ok(vec![entry("/music/x", false)]),
        });
        assert!(
            core.pane(PaneId::Left).listing,
            "a stale generation must not complete the listing"
        );
        assert!(core.pane(PaneId::Left).root.is_empty());

        // Path mismatch (root replies): rejected.
        core.on_event(CoreEvent::ListingArrived {
            pane: PaneId::Left,
            generation,
            path: PathBuf::from("/other"),
            root: true,
            result: Ok(vec![entry("/other/x", false)]),
        });
        assert!(core.pane(PaneId::Left).root.is_empty());
    }

    /// The port-restructured interaction the new test pins: a lazy child
    /// listing captured one generation, then navigation supersedes it — the
    /// late children reply must be rejected outright.
    #[test]
    fn late_children_reply_after_navigation_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("folder");
        std::fs::create_dir(&folder).unwrap();
        // Root the left pane at the temp root itself.
        let config = DOpusConfig {
            left: PaneConfig {
                path: dir.path().to_path_buf(),
                ..Default::default()
            },
            right: PaneConfig {
                path: dir.path().join("right"),
                ..Default::default()
            },
            ..Default::default()
        };
        let (mut core, _rx) = DopusCore::new(config, None);
        core.tick(now_instant());

        let generation = core.pane(PaneId::Left).generation;
        core.on_event(CoreEvent::ListingArrived {
            pane: PaneId::Left,
            generation,
            path: dir.path().to_path_buf(),
            root: true,
            result: Ok(vec![{
                let mut entry = entry(&folder.to_string_lossy(), true);
                entry.path = folder.clone();
                entry.name = "folder".into();
                entry
            }]),
        });

        // Expand: a child listing starts, carrying the captured generation.
        core.toggle_expand(PaneId::Left, &folder);
        assert!(core.pane(PaneId::Left).pending_children.contains(&folder));
        let child_generation = core.pane(PaneId::Left).generation;

        // Navigate away (into the folder itself): the pane's generation moves on.
        core.navigate(PaneId::Left, folder.clone());
        let new_generation = core.pane(PaneId::Left).generation;
        assert_ne!(child_generation, new_generation);
        // Flush the navigate-emitted events so the assertions below see only
        // what the late reply itself derives.
        let _ = core.tick(now_instant());

        // The late children reply, addressed to the superseded generation,
        // must be rejected: not merged, not even touched.
        let events = core.on_event(CoreEvent::ListingArrived {
            pane: PaneId::Left,
            generation: child_generation,
            path: folder.clone(),
            root: false,
            result: Ok(vec![entry("late.txt", false)]),
        });
        assert!(events.is_empty(), "the late reply must be dropped silently");
        assert!(!core.pane(PaneId::Left).children.contains_key(&folder));
        assert!(
            !core.pane(PaneId::Left).pending_children.contains(&folder),
            "a rejected reply must not disturb the new generation's state"
        );
    }

    #[test]
    fn new_navigation_clears_forward_history() {
        let mut history = NavigationHistory::default();
        assert!(history.record_new(Path::new("/a"), Path::new("/b")));
        assert_eq!(history.back(Path::new("/b")), Some(PathBuf::from("/a")));
        assert_eq!(history.forward(Path::new("/a")), Some(PathBuf::from("/b")));
        assert_eq!(history.back(Path::new("/b")), Some(PathBuf::from("/a")));
        assert!(history.record_new(Path::new("/a"), Path::new("/c")));
        assert_eq!(history.forward(Path::new("/c")), None);
    }

    #[test]
    fn navigation_start_clears_selection_and_disables_actions() {
        let (_dir, mut core, _rx) = core_fixture();
        core.tick(now_instant());
        // Land the startup listing so the pane is no longer `listing` —
        // `has_selection` is gated on it (`action_selection_available`,
        // browser.rs:236-240), and selection only ever happens on listed rows.
        let generation = core.pane(PaneId::Left).generation;
        core.on_event(CoreEvent::ListingArrived {
            pane: PaneId::Left,
            generation,
            path: core.pane(PaneId::Left).path.clone(),
            root: true,
            result: Ok(vec![]),
        });

        core.select_path(PaneId::Left, Some(PathBuf::from("/fixture/old")));
        assert!(core.availability().has_selection);

        core.navigate(PaneId::Left, core.pane(PaneId::Left).path.clone());

        let availability = core.availability();
        assert!(core.pane(PaneId::Left).listing);
        assert!(core.pane(PaneId::Left).selected.is_none());
        assert!(!availability.has_selection);
        assert!(!availability.rows_available);
    }

    // -- drop-legality quartet (browser.rs tests ~:4365-4483) -----------------

    #[test]
    fn file_drop_rejects_same_directory() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.txt");
        std::fs::write(&source, b"source").unwrap();
        assert_eq!(
            file_drop_actions(&source, dir.path(), false),
            DropActionMask::NONE
        );
    }

    #[test]
    fn file_drop_rejects_directory_self_and_descendants() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let child = source.join("child");
        let destination = dir.path().join("destination");
        std::fs::create_dir_all(&child).unwrap();
        std::fs::create_dir(&destination).unwrap();
        assert_eq!(
            file_drop_actions(&source, &source, false),
            DropActionMask::NONE
        );
        assert_eq!(
            file_drop_actions(&source, &child, false),
            DropActionMask::NONE
        );
        assert_eq!(
            file_drop_actions(&source, &destination, false),
            DropActionMask::ALL
        );
    }

    #[test]
    fn file_drop_rejects_while_an_operation_is_pending() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.txt");
        let destination = dir.path().join("destination");
        std::fs::write(&source, b"source").unwrap();
        std::fs::create_dir(&destination).unwrap();
        assert_eq!(
            file_drop_actions(&source, &destination, true),
            DropActionMask::NONE
        );
    }

    #[cfg(unix)]
    #[test]
    fn file_drop_containment_resolves_dotdot_and_symlink_identity() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let child = source.join("child");
        let elsewhere = dir.path().join("elsewhere");
        let other = dir.path().join("other");
        std::fs::create_dir_all(&child).unwrap();
        std::fs::create_dir(&elsewhere).unwrap();
        std::fs::create_dir(&other).unwrap();

        let dotdot_inside = other.join("..").join("source").join("child");
        assert_eq!(
            file_drop_actions(&source, &dotdot_inside, false),
            DropActionMask::NONE
        );

        let lexical_descendant_but_distinct = source.join("..").join("elsewhere");
        assert_eq!(
            file_drop_actions(&source, &lexical_descendant_but_distinct, false),
            DropActionMask::ALL
        );

        let alias_inside = dir.path().join("alias-inside");
        std::os::unix::fs::symlink(&child, &alias_inside).unwrap();
        assert_eq!(
            file_drop_actions(&source, &alias_inside, false),
            DropActionMask::NONE
        );

        let source_link = dir.path().join("source-link");
        std::os::unix::fs::symlink(&source, &source_link).unwrap();
        assert_eq!(
            file_drop_actions(&source_link, &child, false),
            DropActionMask::ALL,
            "a source symlink is moved/copied as a link, not as its directory target"
        );
    }

    #[test]
    fn file_drop_modifiers_map_to_kde_actions() {
        assert_eq!(
            requested_drop_action(DropModifiers::default()),
            DropAction::Ask
        );
        assert_eq!(
            requested_drop_action(DropModifiers {
                control: true,
                ..Default::default()
            }),
            DropAction::Copy
        );
        assert_eq!(
            requested_drop_action(DropModifiers {
                shift: true,
                ..Default::default()
            }),
            DropAction::Move
        );
        assert_eq!(
            requested_drop_action(DropModifiers {
                control: true,
                shift: true,
            }),
            DropAction::Copy
        );
    }

    #[test]
    fn commit_trusts_any_action_in_the_negotiated_mask() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.txt");
        let destination = dir.path().join("destination");
        std::fs::write(&source, b"source").unwrap();
        std::fs::create_dir(&destination).unwrap();

        let allowed = file_drop_actions(&source, &destination, false);
        assert_eq!(
            requested_drop_action(DropModifiers::default()),
            DropAction::Ask
        );
        assert!(allowed.contains(DropAction::Move));
    }

    // -- reservation state machine (browser.rs:387-427 + tests ~:4653-4927,
    //    restructured from ctk interaction results to token-keyed calls) -----

    fn confirm_token(core: &mut DopusCore) -> u64 {
        core.select_path(PaneId::Left, Some(PathBuf::from("/fixture/target")));
        core.delete_selection();
        let events = core.tick(now_instant());
        events
            .iter()
            .find_map(|event| match event {
                CoreEvent::ConfirmRequested { token, .. } => Some(*token),
                _ => None,
            })
            .expect("delete_selection must raise a ConfirmRequested")
    }

    #[test]
    fn a_confirmed_delete_starts_one_operation_and_consumes_the_token() {
        let (_dir, mut core, _rx) = core_fixture();
        core.tick(now_instant());
        let token = confirm_token(&mut core);

        core.confirm(token, ConfirmAnswer::Yes);
        assert!(core.availability().operation_running);

        // The token is consumed: a replayed resolution must be a no-op —
        // the reservation fails exactly once, never twice (browser.rs test
        // ~:4693, restructured).
        core.confirm(token, ConfirmAnswer::Yes);
        core.confirm(token, ConfirmAnswer::No);

        // And while the operation runs, a new delete request is refused
        // rather than queued (browser.rs:967-969).
        let before = core.pending.len();
        core.delete_selection();
        assert_eq!(core.pending.len(), before);
        assert!(core.availability().operation_running);
    }

    #[test]
    fn dismissed_or_unknown_confirm_fails_closed() {
        let (_dir, mut core, _rx) = core_fixture();
        core.tick(now_instant());

        // Unknown token: nothing happens at all.
        core.confirm(4242, ConfirmAnswer::Yes);
        assert!(!core.availability().operation_running);

        let token = confirm_token(&mut core);
        core.confirm(token, ConfirmAnswer::No);
        assert!(!core.availability().operation_running);

        // Consumed by the dismissal: a later yes finds nothing.
        core.confirm(token, ConfirmAnswer::Yes);
        assert!(!core.availability().operation_running);
    }

    #[test]
    fn prompt_dismissal_withdraws_and_text_applies_on_rename() {
        let (_dir, mut core, _rx) = core_fixture();
        core.tick(now_instant());
        core.select_path(PaneId::Left, Some(PathBuf::from("/fixture/old name.txt")));

        core.begin_rename();
        let events = core.tick(now_instant());
        let (token, initial) = events
            .iter()
            .find_map(|event| match event {
                CoreEvent::PromptRequested { token, initial, .. } => Some((*token, initial.clone())),
                _ => None,
            })
            .expect("begin_rename must raise a PromptRequested");
        assert_eq!(initial, "old name.txt");

        // Dismissal withdraws: nothing runs, and the dead token stays dead.
        core.prompt_text(token, None);
        assert!(!core.availability().operation_running);
        core.prompt_text(token, Some("new name.txt".into()));
        assert!(!core.availability().operation_running);

        // A fresh reservation applies the text as the rename target.
        core.begin_rename();
        let events = core.tick(now_instant());
        let token = events
            .iter()
            .find_map(|event| match event {
                CoreEvent::PromptRequested { token, .. } => Some(*token),
                _ => None,
            })
            .unwrap();
        core.prompt_text(token, Some("new name.txt".into()));
        assert!(core.availability().operation_running);
    }

    #[test]
    fn cross_kind_and_busy_reservations_fail_closed() {
        let (_dir, mut core, _rx) = core_fixture();
        core.tick(now_instant());

        // A confirm token is not a prompt token and vice versa.
        let confirm_id = confirm_token(&mut core);
        core.prompt_text(confirm_id, Some("whatever".into()));
        assert!(!core.availability().operation_running);
        core.confirm(confirm_id, ConfirmAnswer::Yes);

        // While that operation runs, prompts and confirms are refused.
        core.begin_new_folder();
        core.begin_rename();
        core.delete_selection();
        let events = core.tick(now_instant());
        assert!(
            !events.iter().any(|event| matches!(
                event,
                CoreEvent::PromptRequested { .. } | CoreEvent::ConfirmRequested { .. }
            )),
            "nothing new may be reserved while an operation is in flight"
        );
    }

    #[test]
    fn second_operation_while_running_reports_instead_of_queueing() {
        let (_dir, mut core, _rx) = core_fixture();
        core.tick(now_instant());
        core.select_path(PaneId::Left, Some(PathBuf::from("/fixture/a.txt")));
        core.copy_selection_to_other_pane();
        assert!(core.availability().operation_running);

        // Single-flight (browser.rs:1792-1800): a second request produces a
        // status line, never a silent queue or a second running operation.
        core.copy_selection_to_other_pane();
        assert!(core.pane(PaneId::Left).status.contains("Another file operation is still running"));
        assert!(core.availability().operation_running);
    }

    // -- operation replies (browser.rs:1879-1894) -----------------------------

    #[test]
    fn every_operation_reply_relists_both_panes_once() {
        let (_dir, mut core, _rx) = core_fixture();
        core.tick(now_instant());

        let events = core.on_event(CoreEvent::OperationArrived {
            kind: FileOpKind::Copy,
            source_pane: PaneId::Left,
            result: Ok("Copied /a to /b".into()),
        });
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, CoreEvent::ListingStarted { .. }))
                .count(),
            2,
            "success relists both panes exactly once"
        );
        assert!(matches!(events.last(), Some(CoreEvent::RefreshAll)));
        assert!(!core.availability().operation_running);

        // Failure relists too: the tree may have been mutated before failing.
        let events = core.on_event(CoreEvent::OperationArrived {
            kind: FileOpKind::Delete,
            source_pane: PaneId::Right,
            result: Err("deleting /a: permission denied".into()),
        });
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, CoreEvent::ListingStarted { .. }))
                .count(),
            2,
            "failure relists both panes exactly once"
        );
    }

    // -- config settle debounce (browser.rs:3564-3622) -------------------------

    #[test]
    fn config_settles_after_the_debounce_and_only_once_per_change() {
        let (_dir, mut core, _rx) = core_fixture();
        let t0 = now_instant();
        core.tick(t0); // baseline observation

        core.set_split_ratio(0.7);
        let events = core.tick(t0 + Duration::from_millis(100));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, CoreEvent::ConfigSettled(_))),
            "the debounce has not elapsed"
        );

        let events = core.tick(t0 + Duration::from_millis(600));
        match events.iter().find(|event| matches!(event, CoreEvent::ConfigSettled(_))) {
            Some(CoreEvent::ConfigSettled(config)) => assert_eq!(config.split_ratio, 0.7),
            other => panic!("expected ConfigSettled, got {other:?}"),
        }

        // No change, no settle.
        let events = core.tick(t0 + Duration::from_millis(1_200));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, CoreEvent::ConfigSettled(_))),
            "an unchanged config must not settle twice"
        );
    }

    #[test]
    fn config_snapshot_is_derived_from_pane_state() {
        let (_dir, core, _rx) = core_fixture();
        let snapshot = core.config_snapshot();
        assert_eq!(snapshot.schema_version, CURRENT_SCHEMA);
        assert_eq!(snapshot.left.path, _dir.path().join("left"));
        assert_eq!(snapshot.right.path, _dir.path().join("right"));
        assert_eq!(snapshot.active_pane, "left");
        assert_eq!(snapshot.split_ratio, 0.5);
    }

    // -- places (browser.rs:1267-1284) ----------------------------------------

    #[test]
    fn places_list_home_filesystem_and_existing_xdg_directories() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join("Documents")).unwrap();
        std::fs::File::create(home.path().join("Music")).unwrap(); // a file, not a place

        let places = places(home.path());
        let names: Vec<_> = places.iter().map(|(name, _)| *name).collect();
        assert_eq!(names, ["Home", "Filesystem", "Documents"]);
        assert_eq!(places[1].1, PathBuf::from("/"));
    }
}
