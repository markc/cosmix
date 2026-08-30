//! A native CTK file requester built entirely from Bevy UI widgets.
//!
//! The requester deliberately stops at path selection. Applications describe
//! a request with [`FileRequest`] and consume [`FileRequestResult`]; parsing,
//! loading, saving and atomic-write policy stay with the application.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Mutex;

use accesskit::Role;
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::message::{MessageReader, MessageWriter};
use bevy::ecs::observer::On;
use bevy::ecs::query::{Has, With};
use bevy::feathers::theme::{ThemeBackgroundColor, ThemeBorderColor, ThemeTextColor, UiTheme};
use bevy::input::ButtonInput;
use bevy::input_focus::tab_navigation::TabIndex;
use bevy::input_focus::{FocusCause, InputFocus};
use bevy::picking::events::{Click, Pointer};
use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy::tasks::IoTaskPool;
use bevy::text::{EditableText, TextEdit};
use bevy::ui::{Overflow, Pressed, Selected};
// `Button` MUST come from ui_widgets (the headless widget that emits
// `Activate` via ButtonPlugin) — `bevy::prelude::*` otherwise resolves it to
// the legacy `bevy::ui::widget::Button` marker, which never activates and
// left every requester button silently inert.
use bevy::ui_widgets::{
    Activate, ActivateOnPress, ActiveDescendant, Button, ListBox, ListItem, ScrollArea, ValueChange,
};

use crate::dialog_shell::{spawn_dialog_shell, DialogShell};
use crate::interaction::{
    activate_modal, close_active_modal, coordinator_is_top, defer_modal_despawn,
    ensure_modal_coordinator, queue_file_request, release_coordinator_capture_if_idle,
    remove_queued_file_request, set_modal_focus_scope, take_next_file_request,
    FileRequestCompatAdapter, InteractionId, InteractionPresentationSystems, ModalCoordinator,
    ModalPresenter, DIALOG_Z,
};
use crate::modal_capture::{ModalCapture, ModalCaptureSystems};
use crate::text_field::{spawn_text_field, CtkTextFieldProps};
use crate::theme::{ctk_color, tokens};

const REQUESTER_Z: i32 = 1_000;
const OVERWRITE_Z: i32 = DIALOG_Z + 10;

/// Caller-supplied identity used to correlate a result with its request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FileRequestId(pub u64);

/// What kind of filesystem object the requester should return.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileRequestMode {
    OpenFile,
    SaveFile,
    SelectDirectory,
}

/// A named extension filter. Directories remain visible under every filter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileFilter {
    pub label: String,
    pub extensions: Vec<String>,
}

impl FileFilter {
    pub fn new(
        label: impl Into<String>,
        extensions: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            label: label.into(),
            extensions: extensions
                .into_iter()
                .map(Into::into)
                .map(|extension: String| extension.trim_start_matches('.').to_ascii_lowercase())
                .collect(),
        }
    }

    fn accepts(&self, path: &Path) -> bool {
        self.extensions.is_empty()
            || path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| {
                    self.extensions
                        .iter()
                        .any(|allowed| allowed.eq_ignore_ascii_case(extension))
                })
    }
}

/// A request to show the CTK file requester.
#[derive(Message, Clone, Debug)]
pub struct FileRequest {
    pub id: FileRequestId,
    pub mode: FileRequestMode,
    pub title: String,
    pub initial_directory: Option<PathBuf>,
    pub filters: Vec<FileFilter>,
    pub suggested_name: Option<String>,
    /// Extension applied to save names without one.
    pub default_extension: Option<String>,
    /// Replace a mismatched extension as well as filling a missing one.
    pub enforce_extension: bool,
}

impl FileRequest {
    pub fn open_file(id: FileRequestId, title: impl Into<String>) -> Self {
        Self {
            id,
            mode: FileRequestMode::OpenFile,
            title: title.into(),
            initial_directory: None,
            filters: Vec::new(),
            suggested_name: None,
            default_extension: None,
            enforce_extension: false,
        }
    }

    pub fn save_file(id: FileRequestId, title: impl Into<String>) -> Self {
        Self {
            mode: FileRequestMode::SaveFile,
            ..Self::open_file(id, title)
        }
    }

    pub fn select_directory(id: FileRequestId, title: impl Into<String>) -> Self {
        Self {
            mode: FileRequestMode::SelectDirectory,
            ..Self::open_file(id, title)
        }
    }
}

/// Presenter-side request vocabulary after the legacy id has been adapted.
#[derive(Clone, Debug)]
pub(crate) struct FileRequestSpec {
    mode: FileRequestMode,
    title: String,
    initial_directory: Option<PathBuf>,
    filters: Vec<FileFilter>,
    suggested_name: Option<String>,
    default_extension: Option<String>,
    enforce_extension: bool,
}

impl From<FileRequest> for FileRequestSpec {
    fn from(request: FileRequest) -> Self {
        Self {
            mode: request.mode,
            title: request.title,
            initial_directory: request.initial_directory,
            filters: request.filters,
            suggested_name: request.suggested_name,
            default_extension: request.default_extension,
            enforce_extension: request.enforce_extension,
        }
    }
}

/// Terminal outcome of a file request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileRequestOutcome {
    Selected(Vec<PathBuf>),
    Cancelled,
    Failed(String),
}

/// Result message for a previously submitted [`FileRequest`].
#[derive(Message, Clone, Debug, PartialEq, Eq)]
pub struct FileRequestResult {
    pub id: FileRequestId,
    pub outcome: FileRequestOutcome,
}

/// Programmatically close a queued or visible file requester without
/// producing a [`FileRequestResult`].
#[derive(Message, Clone, Copy, Debug, PartialEq, Eq)]
pub struct WithdrawFileRequest(pub FileRequestId);

#[derive(Clone, Debug)]
struct FileEntry {
    path: PathBuf,
    name: String,
    is_dir: bool,
    size: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RequesterPhase {
    Listing,
    Browsing,
    ConfirmOverwrite,
}

struct ActiveRequest {
    correlation: InteractionId,
    request: FileRequestSpec,
    directory: PathBuf,
    generation: u64,
    phase: RequesterPhase,
    show_hidden: bool,
    filter_index: usize,
    selected: Option<PathBuf>,
    pending_overwrite: Option<PathBuf>,
    root: Entity,
    default_focus: Entity,
    list: Entity,
    path_input: Entity,
    filename_input: Option<Entity>,
    status_text: Entity,
    hidden_text: Entity,
    filter_text: Entity,
    rows: Vec<Entity>,
    overwrite_prompt: Option<Entity>,
}

#[derive(Resource, Default)]
pub struct FileRequesterState {
    active: Option<ActiveRequest>,
}

struct ListingReply {
    generation: u64,
    directory: PathBuf,
    result: Result<Vec<FileEntry>, String>,
}

#[derive(Resource)]
struct ListingInbox {
    tx: Sender<ListingReply>,
    rx: Mutex<Receiver<ListingReply>>,
    /// Requester-WIDE monotonically increasing listing nonce: a stale reply
    /// from a cancelled request can never match a later request that happens
    /// to reuse the same per-request generation numbers.
    nonce: std::sync::atomic::AtomicU64,
}

impl Default for ListingInbox {
    fn default() -> Self {
        let (tx, rx) = channel();
        Self {
            tx,
            rx: Mutex::new(rx),
            nonce: std::sync::atomic::AtomicU64::new(0),
        }
    }
}

#[derive(Component)]
struct FileRequesterRoot;

#[derive(Component)]
struct FileRequesterPanel;

#[derive(Component)]
struct RequesterButton(RequesterButtonKind);

#[derive(Clone, Copy)]
enum RequesterButtonKind {
    Parent,
    Home,
    Current,
    Root,
    ToggleHidden,
    CycleFilter,
    Accept,
    Cancel,
    ConfirmOverwrite,
    CancelOverwrite,
}

#[derive(Component)]
struct RequesterPathInput;

#[derive(Component)]
struct RequesterFilenameInput;

#[derive(Component)]
struct FileRow {
    path: PathBuf,
    is_dir: bool,
}

#[derive(Component)]
struct FileRowText {
    dim: bool,
}

/// Installs the native file requester service.
pub struct FileRequesterPlugin;

/// File-request ingress. Keyboard handling deliberately remains in the
/// disjoint [`ModalCaptureSystems`] set; application keyboard systems run
/// after this set so [`ModalCapture`] reflects requests accepted this frame.
///
/// Any future [`FileRequest`] producer scheduled in `Update` must run before
/// this set or it reintroduces a same-frame modal-capture race.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct FileRequesterSystems;

impl Plugin for FileRequesterPlugin {
    fn build(&self, app: &mut App) {
        ensure_modal_coordinator(app);
        app.init_resource::<FileRequesterState>()
            .init_resource::<FileRequestCompatAdapter>()
            .init_resource::<ListingInbox>()
            .add_message::<FileRequest>()
            .add_message::<FileRequestResult>()
            .add_message::<WithdrawFileRequest>()
            .add_observer(on_requester_button)
            .add_observer(on_list_selection)
            .add_observer(on_row_click)
            .add_systems(Update, requester_keyboard.in_set(ModalCaptureSystems))
            .add_systems(
                Update,
                (receive_requests, withdraw_file_requests)
                    .chain()
                    .in_set(FileRequesterSystems)
                    .after(ModalCaptureSystems),
            )
            .add_systems(
                Update,
                present_file_request
                    .after(FileRequesterSystems)
                    .after(InteractionPresentationSystems),
            )
            .add_systems(Update, (receive_listings, update_requester_styles));
    }
}

fn receive_requests(
    mut requests: MessageReader<FileRequest>,
    mut coordinator: ResMut<ModalCoordinator>,
    mut capture: ResMut<ModalCapture>,
    mut compat: ResMut<FileRequestCompatAdapter>,
) {
    for request in requests.read().cloned() {
        // Legacy caller ids stop here. Every queued presentation receives a
        // fresh process-global InteractionId and maps back only on completion.
        queue_file_request(&mut coordinator, &mut capture, &mut compat, request);
    }
}

fn withdraw_file_requests(
    mut withdrawals: MessageReader<WithdrawFileRequest>,
    mut state: ResMut<FileRequesterState>,
    mut coordinator: ResMut<ModalCoordinator>,
    mut capture: ResMut<ModalCapture>,
    mut compat: ResMut<FileRequestCompatAdapter>,
    mut commands: Commands,
    mut focus: ResMut<InputFocus>,
) {
    for withdrawal in withdrawals.read() {
        let correlations = compat.correlations_for(withdrawal.0);
        for correlation in correlations {
            remove_queued_file_request(&mut coordinator, correlation);
            if state
                .active
                .as_ref()
                .is_some_and(|active| active.correlation == correlation)
            {
                state.active = None;
                let closed = close_active_modal(
                    &mut coordinator,
                    &mut capture,
                    ModalPresenter::FileRequester,
                    &mut commands,
                    &mut focus,
                );
                debug_assert_eq!(closed, Some(correlation));
            }
            compat.resolve(correlation);
        }
        release_coordinator_capture_if_idle(&mut coordinator, &mut capture);
    }
}

fn present_file_request(
    mut state: ResMut<FileRequesterState>,
    mut coordinator: ResMut<ModalCoordinator>,
    inbox: Res<ListingInbox>,
    mut commands: Commands,
    mut focus: ResMut<InputFocus>,
) {
    let Some(queued) = take_next_file_request(&mut coordinator) else {
        return;
    };
    let request = queued.request;
    let directory = starting_directory(&request);
    let previous_focus = focus.get();
    let mut active = spawn_requester(&mut commands, request, directory, queued.correlation);
    focus.set(active.default_focus, FocusCause::Navigated);
    start_listing(&mut active, &inbox);
    activate_modal(
        &mut coordinator,
        ModalPresenter::FileRequester,
        active.correlation,
        active.root,
        active.default_focus,
        previous_focus,
    );
    state.active = Some(active);
}

fn starting_directory(request: &FileRequestSpec) -> PathBuf {
    let requested = request
        .initial_directory
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from(std::path::MAIN_SEPARATOR.to_string()));
    if requested.is_dir() {
        requested
    } else {
        requested
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or(requested)
    }
}

fn spawn_requester(
    commands: &mut Commands,
    request: FileRequestSpec,
    directory: PathBuf,
    correlation: InteractionId,
) -> ActiveRequest {
    let shell = spawn_dialog_shell(
        commands,
        DialogShell::new(&request.title, Role::Dialog, tokens::BORDER, REQUESTER_Z)
            .size(percent(58), px(360), px(810))
            .height(percent(60), px(360)),
    );
    commands.entity(shell.root).insert(FileRequesterRoot);
    commands.entity(shell.panel).insert(FileRequesterPanel);

    let path_field = spawn_text_field(
        commands,
        CtkTextFieldProps::new(&*directory.to_string_lossy(), "Directory path"),
    );
    commands.entity(path_field.input).insert(RequesterPathInput);
    let path_input = path_field.input;
    let parent = requester_button(commands, "Up", RequesterButtonKind::Parent);
    let path_row = commands
        .spawn(Node {
            width: percent(100),
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Center,
            column_gap: px(6),
            ..default()
        })
        .add_children(&[parent, path_field.root])
        .id();

    let home = requester_button(commands, "Home", RequesterButtonKind::Home);
    let current = requester_button(commands, "Current", RequesterButtonKind::Current);
    let filesystem_root = requester_button(commands, "Root", RequesterButtonKind::Root);
    let (hidden, hidden_text) =
        requester_button_parts(commands, "Hidden: off", RequesterButtonKind::ToggleHidden);

    let sidebar = commands
        .spawn((
            Node {
                width: px(145),
                min_width: px(145),
                flex_direction: FlexDirection::Column,
                row_gap: px(5),
                ..default()
            },
            BackgroundColor(Color::NONE),
        ))
        .add_children(&[home, current, filesystem_root, hidden])
        .id();

    let list = commands
        .spawn((
            Node {
                flex_grow: 1.0,
                min_width: px(0),
                min_height: px(0),
                flex_direction: FlexDirection::Column,
                overflow: Overflow::scroll_y(),
                padding: UiRect::all(px(3)),
                border: UiRect::all(px(1)),
                ..default()
            },
            ThemeBackgroundColor(tokens::SURFACE),
            BorderColor::all(Color::NONE),
            ThemeBorderColor(tokens::CONTROL),
            ListBox,
            ScrollArea,
            TabIndex(0),
        ))
        .id();
    let main_row = commands
        .spawn(Node {
            width: percent(100),
            flex_grow: 1.0,
            min_height: px(0),
            flex_direction: FlexDirection::Row,
            column_gap: px(8),
            ..default()
        })
        .add_children(&[sidebar, list])
        .id();

    let status_text = text(commands, "Loading...", 13.0, true);
    let filename_field = (request.mode == FileRequestMode::SaveFile).then(|| {
        let field = spawn_text_field(
            commands,
            CtkTextFieldProps::new(
                request.suggested_name.as_deref().unwrap_or_default(),
                "File name",
            ),
        );
        commands.entity(field.input).insert(RequesterFilenameInput);
        field
    });
    let filename_input = filename_field.map(|field| field.input);
    let mut filename_children = Vec::new();
    if let Some(field) = filename_field {
        filename_children.push(text(commands, "File name", 13.0, true));
        filename_children.push(field.root);
    }
    let filename_row = commands
        .spawn(Node {
            width: percent(100),
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Center,
            column_gap: px(8),
            ..default()
        })
        .add_children(&filename_children)
        .id();

    let (filter_button, filter_text) = requester_button_parts(
        commands,
        &filter_label(&request, 0),
        RequesterButtonKind::CycleFilter,
    );
    let cancel = requester_button(commands, "Cancel", RequesterButtonKind::Cancel);
    let accept_label = match request.mode {
        FileRequestMode::OpenFile => "Open",
        FileRequestMode::SaveFile => "Save",
        FileRequestMode::SelectDirectory => "Select",
    };
    let accept = requester_button(commands, accept_label, RequesterButtonKind::Accept);
    let bottom_row = commands
        .spawn(Node {
            width: percent(100),
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Center,
            justify_content: JustifyContent::SpaceBetween,
            column_gap: px(8),
            ..default()
        })
        .add_child(filter_button)
        .id();

    commands.entity(shell.body).add_children(&[
        path_row,
        main_row,
        status_text,
        filename_row,
        bottom_row,
    ]);
    commands
        .entity(shell.actions)
        .add_children(&[cancel, accept]);
    let default_focus = filename_input.unwrap_or(list);

    ActiveRequest {
        correlation,
        request,
        directory,
        generation: 0,
        phase: RequesterPhase::Listing,
        show_hidden: false,
        filter_index: 0,
        selected: None,
        pending_overwrite: None,
        root: shell.root,
        default_focus,
        list,
        path_input,
        filename_input,
        status_text,
        hidden_text,
        filter_text,
        rows: Vec::new(),
        overwrite_prompt: None,
    }
}

fn filter_label(request: &FileRequestSpec, index: usize) -> String {
    request
        .filters
        .get(index)
        .map(|filter| format!("Type: {}", filter.label))
        .unwrap_or_else(|| "Type: All files".to_string())
}

fn requester_button(commands: &mut Commands, label: &str, kind: RequesterButtonKind) -> Entity {
    requester_button_parts(commands, label, kind).0
}

fn requester_button_parts(
    commands: &mut Commands,
    label: &str,
    kind: RequesterButtonKind,
) -> (Entity, Entity) {
    let label_entity = text(commands, label, 13.0, false);
    let button = commands
        .spawn((
            Node {
                min_width: px(72),
                min_height: px(30),
                padding: UiRect::axes(px(9), px(5)),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                border_radius: BorderRadius::all(px(4)),
                ..default()
            },
            ThemeBackgroundColor(tokens::CONTROL),
            Button,
            // Fire Activate on press-down (the proven ctk `action_button`
            // pattern) — the default press→release→click path only emits
            // Activate while `Pressed` still holds, which the release handler
            // can clear first, so nav buttons silently did nothing.
            ActivateOnPress,
            Hovered::default(),
            TabIndex(0),
            RequesterButton(kind),
        ))
        .add_child(label_entity)
        .id();
    (button, label_entity)
}

fn text(commands: &mut Commands, value: &str, size: f32, dim: bool) -> Entity {
    commands
        .spawn((
            Text::new(value),
            TextFont::from_font_size(size),
            ThemeTextColor(if dim { tokens::TEXT_DIM } else { tokens::TEXT }),
        ))
        .id()
}

fn start_listing(active: &mut ActiveRequest, inbox: &ListingInbox) {
    active.generation = inbox
        .nonce
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        + 1;
    active.phase = RequesterPhase::Listing;
    active.selected = None;
    let generation = active.generation;
    let directory = active.directory.clone();
    let filter = active.request.filters.get(active.filter_index).cloned();
    let show_hidden = active.show_hidden;
    let tx = inbox.tx.clone();
    IoTaskPool::get()
        .spawn(async move {
            let result = read_directory(&directory, filter.as_ref(), show_hidden);
            let _ = tx.send(ListingReply {
                generation,
                directory,
                result,
            });
        })
        .detach();
}

fn read_directory(
    directory: &Path,
    filter: Option<&FileFilter>,
    show_hidden: bool,
) -> Result<Vec<FileEntry>, String> {
    let read = std::fs::read_dir(directory)
        .map_err(|error| format!("{}: {error}", directory.display()))?;
    let mut entries = Vec::new();
    for entry in read {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        let metadata = entry.metadata().ok();
        let is_dir = metadata.as_ref().is_some_and(std::fs::Metadata::is_dir);
        if !is_dir && filter.is_some_and(|filter| !filter.accepts(&path)) {
            continue;
        }
        entries.push(FileEntry {
            path,
            name,
            is_dir,
            size: metadata.as_ref().map(std::fs::Metadata::len),
        });
    }
    entries.sort_by(|left, right| {
        right
            .is_dir
            .cmp(&left.is_dir)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(entries)
}

fn receive_listings(
    mut state: ResMut<FileRequesterState>,
    inbox: Res<ListingInbox>,
    theme: Res<UiTheme>,
    mut commands: Commands,
    mut texts: Query<&mut Text>,
    mut inputs: Query<&mut EditableText>,
) {
    let mut replies = Vec::new();
    let rx = inbox.rx.lock().expect("file requester inbox poisoned");
    while let Ok(reply) = rx.try_recv() {
        replies.push(reply);
    }
    drop(rx);

    let Some(active) = state.active.as_mut() else {
        return;
    };
    for reply in replies {
        if reply.generation != active.generation || reply.directory != active.directory {
            continue;
        }
        // The ListBox stores an accessibility pointer (`ActiveDescendant`) at
        // the clicked row ON THE LISTBOX ENTITY — that reference OUTLIVES the
        // rows we are about to despawn, so a later consumer (a11y/keyboard)
        // would dereference a dead entity and panic on the next command
        // flush. Reset it before the row set changes.
        commands.entity(active.list).insert(ActiveDescendant(None));
        for row in active.rows.drain(..) {
            commands.entity(row).try_despawn();
        }
        match reply.result {
            Ok(entries) => {
                for entry in entries {
                    let row = spawn_file_row(&mut commands, &entry, &theme);
                    commands.entity(active.list).add_child(row);
                    active.rows.push(row);
                }
                active.phase = RequesterPhase::Browsing;
                if let Ok(mut status) = texts.get_mut(active.status_text) {
                    status.0 = format!("{} items", active.rows.len());
                }
            }
            Err(error) => {
                active.phase = RequesterPhase::Browsing;
                if let Ok(mut status) = texts.get_mut(active.status_text) {
                    status.0 = error;
                }
            }
        }
        if let Ok(mut input) = inputs.get_mut(active.path_input) {
            input
                .editor_mut()
                .set_text(&active.directory.to_string_lossy());
            input.queue_edit(TextEdit::TextEnd(false));
        }
    }
}

fn spawn_file_row(commands: &mut Commands, entry: &FileEntry, theme: &UiTheme) -> Entity {
    let name = text(
        commands,
        &format!("{}{}", entry.name, if entry.is_dir { "/" } else { "" }),
        13.0,
        false,
    );
    let size = text(
        commands,
        &entry
            .size
            .filter(|_| !entry.is_dir)
            .map(format_size)
            .unwrap_or_default(),
        12.0,
        true,
    );
    commands.entity(name).remove::<ThemeTextColor>().insert((
        TextColor(ctk_color(theme, &tokens::TEXT)),
        FileRowText { dim: false },
    ));
    commands.entity(size).remove::<ThemeTextColor>().insert((
        TextColor(ctk_color(theme, &tokens::TEXT_DIM)),
        FileRowText { dim: true },
    ));
    commands
        .spawn((
            Node {
                width: percent(100),
                min_height: px(28),
                flex_direction: FlexDirection::Row,
                justify_content: JustifyContent::SpaceBetween,
                align_items: AlignItems::Center,
                padding: UiRect::axes(px(7), px(3)),
                ..default()
            },
            BackgroundColor(Color::NONE),
            Hovered::default(),
            ListItem,
            FileRow {
                path: entry.path.clone(),
                is_dir: entry.is_dir,
            },
        ))
        .add_children(&[name, size])
        .id()
}

fn format_size(bytes: u64) -> String {
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

fn on_list_selection(
    changed: On<ValueChange<Entity>>,
    mut state: ResMut<FileRequesterState>,
    rows: Query<&FileRow>,
    mut commands: Commands,
    mut inputs: Query<&mut EditableText>,
) {
    let Some(active) = state.active.as_mut() else {
        return;
    };
    if changed.source != active.list {
        return;
    }
    if active.phase != RequesterPhase::Browsing {
        return;
    }
    let Ok(row) = rows.get(changed.value) else {
        return;
    };
    active.selected = Some(row.path.clone());
    if active.request.mode == FileRequestMode::SaveFile && !row.is_dir {
        if let Some(filename_input) = active.filename_input {
            if let Ok(mut input) = inputs.get_mut(filename_input) {
                if let Some(name) = row.path.file_name().and_then(|name| name.to_str()) {
                    input.editor_mut().set_text(name);
                    input.queue_edit(TextEdit::TextEnd(false));
                }
            }
        }
    }
    // Fallible: a row can be despawned between the selection event and this
    // observer's command flush (rapid navigate-after-select).
    for entity in &active.rows {
        if *entity == changed.value {
            commands.entity(*entity).try_insert(Selected);
        } else {
            commands.entity(*entity).try_remove::<Selected>();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn on_row_click(
    mut click: On<Pointer<Click>>,
    mut state: ResMut<FileRequesterState>,
    mut coordinator: ResMut<ModalCoordinator>,
    mut capture: ResMut<ModalCapture>,
    mut compat: ResMut<FileRequestCompatAdapter>,
    rows: Query<&FileRow>,
    parents: Query<&ChildOf>,
    inbox: Res<ListingInbox>,
    mut results: MessageWriter<FileRequestResult>,
    mut commands: Commands,
    mut focus: ResMut<InputFocus>,
) {
    if click.entity != click.original_event_target() || click.count < 2 {
        return;
    }
    let mut entity = click.original_event_target();
    let row = loop {
        if let Ok(row) = rows.get(entity) {
            break Some((row.path.clone(), row.is_dir));
        }
        match parents.get(entity) {
            Ok(parent) => entity = parent.parent(),
            Err(_) => break None,
        }
    };
    let Some((path, is_dir)) = row else {
        return;
    };
    let Some(active) = state.active.as_mut() else {
        return;
    };
    // Browsing only: a double-click on a STALE row during a listing must
    // not open a file from the previous directory/filter.
    if active.phase != RequesterPhase::Browsing {
        return;
    }
    // We are handling this double-click ourselves — stop it bubbling to the
    // ListBox's own click handler. Otherwise, on a file double-click that
    // finishes the request, `listbox_on_row_click` would queue an
    // `ActiveDescendant` insert onto the listbox that `finish_request` is
    // despawning THIS frame, panicking on the command flush.
    click.propagate(false);
    if is_dir {
        active.directory = path;
        start_listing(active, &inbox);
    } else if active.request.mode == FileRequestMode::OpenFile {
        finish_request(
            &mut state,
            &mut coordinator,
            &mut capture,
            &mut compat,
            FileRequestOutcome::Selected(vec![path]),
            &mut results,
            &mut commands,
            &mut focus,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn on_requester_button(
    activated: On<Activate>,
    buttons: Query<&RequesterButton>,
    mut state: ResMut<FileRequesterState>,
    mut coordinator: ResMut<ModalCoordinator>,
    mut capture: ResMut<ModalCapture>,
    mut compat: ResMut<FileRequestCompatAdapter>,
    inbox: Res<ListingInbox>,
    mut results: MessageWriter<FileRequestResult>,
    mut commands: Commands,
    mut focus: ResMut<InputFocus>,
    inputs: Query<&EditableText>,
    mut texts: Query<&mut Text>,
) {
    let Ok(button) = buttons.get(activated.entity) else {
        return;
    };
    if state
        .active
        .as_ref()
        .is_some_and(|active| active.phase == RequesterPhase::ConfirmOverwrite)
        && !matches!(
            button.0,
            RequesterButtonKind::ConfirmOverwrite | RequesterButtonKind::CancelOverwrite
        )
    {
        return;
    }
    match button.0 {
        RequesterButtonKind::Parent => {
            if let Some(active) = state.active.as_mut() {
                if let Some(parent) = active.directory.parent() {
                    active.directory = parent.to_path_buf();
                    start_listing(active, &inbox);
                }
            }
        }
        RequesterButtonKind::Home => navigate_known(&mut state, home_directory(), &inbox),
        RequesterButtonKind::Current => {
            navigate_known(&mut state, std::env::current_dir().ok(), &inbox)
        }
        RequesterButtonKind::Root => {
            let root = state
                .active
                .as_ref()
                .map(|active| filesystem_root(&active.directory));
            navigate_known(&mut state, root, &inbox);
        }
        RequesterButtonKind::ToggleHidden => {
            if let Some(active) = state.active.as_mut() {
                active.show_hidden = !active.show_hidden;
                if let Ok(mut label) = texts.get_mut(active.hidden_text) {
                    label.0 = if active.show_hidden {
                        "Hidden: on".into()
                    } else {
                        "Hidden: off".into()
                    };
                }
                start_listing(active, &inbox);
            }
        }
        RequesterButtonKind::CycleFilter => {
            if let Some(active) = state.active.as_mut() {
                if active.request.filters.len() > 1 {
                    active.filter_index = (active.filter_index + 1) % active.request.filters.len();
                    if let Ok(mut label) = texts.get_mut(active.filter_text) {
                        label.0 = filter_label(&active.request, active.filter_index);
                    }
                    start_listing(active, &inbox);
                }
            }
        }
        RequesterButtonKind::Accept => accept_request(
            &mut state,
            &mut coordinator,
            &mut capture,
            &mut compat,
            &inbox,
            &inputs,
            &mut results,
            &mut commands,
            &mut focus,
            &mut texts,
        ),
        RequesterButtonKind::Cancel => finish_request(
            &mut state,
            &mut coordinator,
            &mut capture,
            &mut compat,
            FileRequestOutcome::Cancelled,
            &mut results,
            &mut commands,
            &mut focus,
        ),
        RequesterButtonKind::ConfirmOverwrite => {
            let outcome = state
                .active
                .as_mut()
                .and_then(|active| active.pending_overwrite.take())
                .map(|path| FileRequestOutcome::Selected(vec![path]));
            if let Some(outcome) = outcome {
                finish_request(
                    &mut state,
                    &mut coordinator,
                    &mut capture,
                    &mut compat,
                    outcome,
                    &mut results,
                    &mut commands,
                    &mut focus,
                );
            }
        }
        RequesterButtonKind::CancelOverwrite => {
            if let Some(active) = state.active.as_mut() {
                if let Some(prompt) = active.overwrite_prompt.take() {
                    defer_modal_despawn(&mut coordinator, prompt);
                }
                active.pending_overwrite = None;
                active.phase = RequesterPhase::Browsing;
                set_modal_focus_scope(
                    &mut coordinator,
                    ModalPresenter::FileRequester,
                    active.root,
                    active.default_focus,
                );
                restore_requester_focus(active, &mut focus);
            }
        }
    }
}

fn navigate_known(
    state: &mut FileRequesterState,
    directory: Option<PathBuf>,
    inbox: &ListingInbox,
) {
    let (Some(active), Some(directory)) = (state.active.as_mut(), directory) else {
        return;
    };
    active.directory = directory;
    start_listing(active, inbox);
}

fn home_directory() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn filesystem_root(path: &Path) -> PathBuf {
    path.ancestors()
        .last()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(std::path::MAIN_SEPARATOR.to_string()))
}

#[allow(clippy::too_many_arguments)]
fn requester_keyboard(
    keys: Res<ButtonInput<KeyCode>>,
    editable: Query<(), With<EditableText>>,
    path_inputs: Query<(), With<RequesterPathInput>>,
    mut state: ResMut<FileRequesterState>,
    mut coordinator: ResMut<ModalCoordinator>,
    mut capture: ResMut<ModalCapture>,
    mut compat: ResMut<FileRequestCompatAdapter>,
    inbox: Res<ListingInbox>,
    inputs: Query<&EditableText>,
    mut results: MessageWriter<FileRequestResult>,
    mut commands: Commands,
    mut focus: ResMut<InputFocus>,
    mut texts: Query<&mut Text>,
) {
    if !coordinator_is_top(&coordinator, &capture, ModalPresenter::FileRequester) {
        return;
    }
    if keys.just_pressed(KeyCode::Escape) {
        if state
            .active
            .as_ref()
            .is_some_and(|active| active.phase == RequesterPhase::ConfirmOverwrite)
        {
            if let Some(active) = state.active.as_mut() {
                if let Some(prompt) = active.overwrite_prompt.take() {
                    defer_modal_despawn(&mut coordinator, prompt);
                }
                active.pending_overwrite = None;
                active.phase = RequesterPhase::Browsing;
                set_modal_focus_scope(
                    &mut coordinator,
                    ModalPresenter::FileRequester,
                    active.root,
                    active.default_focus,
                );
                restore_requester_focus(active, &mut focus);
            }
            return;
        }
        finish_request(
            &mut state,
            &mut coordinator,
            &mut capture,
            &mut compat,
            FileRequestOutcome::Cancelled,
            &mut results,
            &mut commands,
            &mut focus,
        );
        return;
    }
    if state
        .active
        .as_ref()
        .is_some_and(|active| active.phase == RequesterPhase::ConfirmOverwrite)
    {
        return;
    }
    let focused = focus.get();
    if keys.just_pressed(KeyCode::Enter) {
        if let Some(entity) = focused {
            if path_inputs.contains(entity) {
                if let Ok(input) = inputs.get(entity) {
                    let path = PathBuf::from(input.value().to_string());
                    if path.is_dir() {
                        if let Some(active) = state.active.as_mut() {
                            active.directory = path;
                            start_listing(active, &inbox);
                        }
                    }
                }
                return;
            }
        }
        accept_request(
            &mut state,
            &mut coordinator,
            &mut capture,
            &mut compat,
            &inbox,
            &inputs,
            &mut results,
            &mut commands,
            &mut focus,
            &mut texts,
        );
        return;
    }
    if keys.just_pressed(KeyCode::Backspace)
        && focused.is_none_or(|entity| !editable.contains(entity))
    {
        if let Some(active) = state.active.as_mut() {
            if let Some(parent) = active.directory.parent() {
                active.directory = parent.to_path_buf();
                start_listing(active, &inbox);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn accept_request(
    state: &mut FileRequesterState,
    coordinator: &mut ModalCoordinator,
    capture: &mut ModalCapture,
    compat: &mut FileRequestCompatAdapter,
    inbox: &ListingInbox,
    inputs: &Query<&EditableText>,
    results: &mut MessageWriter<FileRequestResult>,
    commands: &mut Commands,
    focus: &mut InputFocus,
    texts: &mut Query<&mut Text>,
) {
    let Some(active) = state.active.as_mut() else {
        return;
    };
    if active.phase != RequesterPhase::Browsing {
        return;
    }
    let path = match active.request.mode {
        FileRequestMode::OpenFile => match active.selected.clone() {
            Some(path) if path.is_file() => path,
            Some(path) if path.is_dir() => {
                active.directory = path;
                start_listing(active, inbox);
                return;
            }
            _ => return,
        },
        FileRequestMode::SelectDirectory => active
            .selected
            .clone()
            .filter(|path| path.is_dir())
            .unwrap_or_else(|| active.directory.clone()),
        FileRequestMode::SaveFile => {
            let Some(filename_input) = active.filename_input else {
                return;
            };
            let Ok(filename) = inputs.get(filename_input) else {
                return;
            };
            let name = filename.value().to_string();
            if name.trim().is_empty() {
                return;
            }
            corrected_save_path(&active.directory, &name, &active.request)
        }
    };

    if active.request.mode == FileRequestMode::SaveFile {
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() => {
                active.pending_overwrite = Some(path.clone());
                active.phase = RequesterPhase::ConfirmOverwrite;
                let (prompt, cancel) = spawn_overwrite_prompt(commands, &path);
                commands.entity(active.root).add_child(prompt);
                active.overwrite_prompt = Some(prompt);
                set_modal_focus_scope(coordinator, ModalPresenter::FileRequester, prompt, cancel);
                focus.set(cancel, FocusCause::Navigated);
                return;
            }
            Ok(_) => {
                // Symlinks (incl. dangling), directories, FIFOs: never
                // silently replaced and never written through.
                if let Ok(mut status) = texts.get_mut(active.status_text) {
                    status.0 = format!("{} exists and is not a regular file", path.display());
                }
                return;
            }
            // Only definitive NotFound proves the path is free; any other
            // probe error (permissions, I/O, stale handle) must refuse
            // rather than bypass the overwrite/non-regular protection.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                if let Ok(mut status) = texts.get_mut(active.status_text) {
                    status.0 = format!("{}: {error}", path.display());
                }
                return;
            }
        }
    }
    finish_request(
        state,
        coordinator,
        capture,
        compat,
        FileRequestOutcome::Selected(vec![path]),
        results,
        commands,
        focus,
    );
}

fn corrected_save_path(directory: &Path, name: &str, request: &FileRequestSpec) -> PathBuf {
    let mut path = directory.join(name);
    if let Some(extension) = request.default_extension.as_deref() {
        let matches = path
            .extension()
            .and_then(|current| current.to_str())
            .is_some_and(|current| current.eq_ignore_ascii_case(extension));
        if path.extension().is_none() || request.enforce_extension && !matches {
            path.set_extension(extension.trim_start_matches('.'));
        }
    }
    path
}

fn spawn_overwrite_prompt(commands: &mut Commands, path: &Path) -> (Entity, Entity) {
    let shell = spawn_dialog_shell(
        commands,
        DialogShell::new(
            "Replace existing file?",
            Role::AlertDialog,
            tokens::CONTROL_ACTIVE,
            OVERWRITE_Z,
        )
        .size(percent(44), px(360), px(640)),
    );
    let message = text(
        commands,
        &format!("{} already exists.", path.display()),
        13.0,
        true,
    );
    let cancel = requester_button(
        commands,
        "Keep existing",
        RequesterButtonKind::CancelOverwrite,
    );
    let replace = requester_button(commands, "Replace", RequesterButtonKind::ConfirmOverwrite);
    commands.entity(shell.body).add_child(message);
    commands
        .entity(shell.actions)
        .add_children(&[cancel, replace]);
    (shell.root, cancel)
}

fn restore_requester_focus(active: &ActiveRequest, focus: &mut InputFocus) {
    focus.set(active.default_focus, FocusCause::Navigated);
}

#[allow(clippy::too_many_arguments)]
fn finish_request(
    state: &mut FileRequesterState,
    coordinator: &mut ModalCoordinator,
    capture: &mut ModalCapture,
    compat: &mut FileRequestCompatAdapter,
    outcome: FileRequestOutcome,
    results: &mut MessageWriter<FileRequestResult>,
    commands: &mut Commands,
    focus: &mut InputFocus,
) {
    let Some(active) = state.active.take() else {
        return;
    };
    let correlation = close_active_modal(
        coordinator,
        capture,
        ModalPresenter::FileRequester,
        commands,
        focus,
    );
    debug_assert_eq!(correlation, Some(active.correlation));
    let Some(id) = compat.resolve(active.correlation) else {
        debug_assert!(false, "file request compatibility mapping was missing");
        return;
    };
    results.write(FileRequestResult { id, outcome });
}

#[allow(clippy::type_complexity)]
fn update_requester_styles(
    theme: Res<UiTheme>,
    // The three &mut BackgroundColor queries must be PROVABLY disjoint
    // (B0001 panics at system init otherwise) — the markers never coexist,
    // but the scheduler needs the Without bounds to know that.
    mut panels: Query<
        (&mut BackgroundColor, &mut BorderColor),
        (
            With<FileRequesterPanel>,
            Without<RequesterButton>,
            Without<FileRow>,
        ),
    >,
    mut buttons: Query<
        (&Hovered, Has<Pressed>, &mut BackgroundColor),
        (With<RequesterButton>, Without<FileRow>),
    >,
    mut rows: Query<(&Hovered, Has<Selected>, &Children, &mut BackgroundColor), With<FileRow>>,
    mut labels: Query<(&FileRowText, &mut TextColor)>,
) {
    for (mut background, mut border) in &mut panels {
        background.0 = ctk_color(&theme, &tokens::PANEL);
        border.set_all(ctk_color(&theme, &tokens::BORDER));
    }
    for (hovered, pressed, mut background) in &mut buttons {
        background.0 = if pressed {
            ctk_color(&theme, &tokens::CONTROL_ACTIVE)
        } else if hovered.get() {
            ctk_color(&theme, &tokens::THUMB)
        } else {
            ctk_color(&theme, &tokens::CONTROL)
        };
    }
    for (hovered, selected, children, mut background) in &mut rows {
        background.0 = if selected {
            ctk_color(&theme, &tokens::ROW_SELECTED)
        } else if hovered.get() {
            ctk_color(&theme, &tokens::ROW_HOVER)
        } else {
            Color::NONE
        };
        for child in children {
            let Ok((managed, mut colour)) = labels.get_mut(*child) else {
                continue;
            };
            let token = match (selected, managed.dim) {
                (true, true) => &tokens::ROW_SELECTED_TEXT_DIM,
                (true, false) => &tokens::ROW_SELECTED_TEXT,
                (false, true) => &tokens::TEXT_DIM,
                (false, false) => &tokens::TEXT,
            };
            colour.0 = ctk_color(&theme, token);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::app::TaskPoolPlugin;
    use bevy::ecs::message::Messages;
    use std::fs;

    #[derive(Resource, Default)]
    struct BoardRuns(usize);

    fn requester_closed(capture: Res<ModalCapture>) -> bool {
        !capture.is_captured()
    }

    fn board_probe(mut runs: ResMut<BoardRuns>) {
        runs.0 += 1;
    }

    fn temp_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "ctk-file-requester-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn listing_is_filtered_hidden_and_directory_first() {
        let root = temp_dir("listing");
        fs::create_dir(root.join("Zebra")).unwrap();
        fs::create_dir(root.join("alpha")).unwrap();
        fs::write(root.join("song.mid"), b"midi").unwrap();
        fs::write(root.join("notes.txt"), b"text").unwrap();
        fs::write(root.join(".hidden.mid"), b"hidden").unwrap();

        let filter = FileFilter::new("Songs", ["mid", "midi"]);
        let entries = read_directory(&root, Some(&filter), false).unwrap();
        let names: Vec<_> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, ["alpha", "Zebra", "song.mid"]);

        let visible = read_directory(&root, Some(&filter), true).unwrap();
        assert!(visible.iter().any(|entry| entry.name == ".hidden.mid"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn save_extension_policy_is_explicit() {
        let directory = Path::new("base");
        let mut request = FileRequest::save_file(FileRequestId(1), "Save");
        request.default_extension = Some("mix".into());
        let mut request = FileRequestSpec::from(request);
        assert_eq!(
            corrected_save_path(directory, "session", &request),
            PathBuf::from("base/session.mix")
        );
        assert_eq!(
            corrected_save_path(directory, "session.txt", &request),
            PathBuf::from("base/session.txt")
        );
        request.enforce_extension = true;
        assert_eq!(
            corrected_save_path(directory, "session.txt", &request),
            PathBuf::from("base/session.mix")
        );
    }

    #[test]
    fn filter_is_case_insensitive() {
        let filter = FileFilter::new("SoundFonts", ["sf2", "sf3"]);
        assert!(filter.accepts(Path::new("ORCHESTRA.SF2")));
        assert!(!filter.accepts(Path::new("orchestra.wav")));
    }

    fn seed_active_request(app: &mut App, directory: PathBuf, id: FileRequestId) {
        let world = app.world_mut();
        let root = world.spawn_empty().id();
        let list = world.spawn_empty().id();
        let path_input = world.spawn(EditableText::new("")).id();
        let status_text = world.spawn(Text::new("")).id();
        let hidden_text = world.spawn(Text::new("Hidden: off")).id();
        let filter_text = world.spawn(Text::new("Type: MIDI")).id();
        let correlation = InteractionId::next();
        world
            .resource_mut::<FileRequestCompatAdapter>()
            .register(correlation, id);
        world.resource_scope(|world, mut coordinator: Mut<ModalCoordinator>| {
            let mut capture = world.resource_mut::<ModalCapture>();
            crate::interaction::acquire_coordinator_capture(&mut coordinator, &mut capture);
        });
        let previous_focus = world.resource::<InputFocus>().get();
        activate_modal(
            &mut world.resource_mut::<ModalCoordinator>(),
            ModalPresenter::FileRequester,
            correlation,
            root,
            list,
            previous_focus,
        );
        let mut state = world.resource_mut::<FileRequesterState>();
        state.active = Some(ActiveRequest {
            correlation,
            request: FileRequestSpec::from(FileRequest {
                filters: vec![
                    FileFilter::new("MIDI", ["mid"]),
                    FileFilter::new("Audio", ["wav"]),
                ],
                ..FileRequest::open_file(id, "Open")
            }),
            directory,
            generation: 0,
            phase: RequesterPhase::Browsing,
            show_hidden: false,
            filter_index: 0,
            selected: None,
            pending_overwrite: None,
            root,
            default_focus: list,
            list,
            path_input,
            filename_input: None,
            status_text,
            hidden_text,
            filter_text,
            rows: Vec::new(),
            overwrite_prompt: None,
        });
    }

    fn action_test_app() -> App {
        let mut app = App::new();
        app.add_plugins(TaskPoolPlugin::default())
            .add_plugins(FileRequesterPlugin);
        app.finish();
        app.cleanup();
        app
    }

    fn scheduled_test_app() -> App {
        let mut app = App::new();
        app.add_plugins(TaskPoolPlugin::default())
            .init_resource::<InputFocus>()
            .init_resource::<ButtonInput<KeyCode>>()
            .init_resource::<UiTheme>()
            .init_resource::<BoardRuns>()
            .add_plugins(FileRequesterPlugin)
            .add_systems(
                Update,
                board_probe
                    .after(ModalCaptureSystems)
                    .run_if(requester_closed),
            );
        app.finish();
        app.cleanup();
        app
    }

    #[test]
    fn escape_close_latch_suppresses_board_input_through_update() {
        let root = temp_dir("close-latch");
        let mut app = scheduled_test_app();
        seed_active_request(&mut app, root.clone(), FileRequestId(81));
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::Escape);

        app.update();

        let state = app.world().resource::<FileRequesterState>();
        assert!(state.active.is_none());
        assert!(
            !app.world().resource::<ModalCapture>().is_captured(),
            "Last clears the shared close latch after Update"
        );
        assert_eq!(app.world().resource::<BoardRuns>().0, 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn queued_request_remains_captured_when_active_request_closes() {
        let root = temp_dir("queued-capture");
        let mut app = scheduled_test_app();
        for id in [FileRequestId(91), FileRequestId(92)] {
            let mut request = FileRequest::open_file(id, "Open");
            request.initial_directory = Some(root.clone());
            app.world_mut().write_message(request);
        }

        app.update();
        {
            let state = app.world().resource::<FileRequesterState>();
            let correlation = state.active.as_ref().unwrap().correlation;
            assert_eq!(
                app.world()
                    .resource::<FileRequestCompatAdapter>()
                    .legacy_id(correlation),
                Some(FileRequestId(91))
            );
            assert_eq!(app.world().resource::<ModalCoordinator>().queued_len(), 1);
            assert!(app.world().resource::<ModalCapture>().is_captured());
        }
        // Opening ingestion deliberately runs in FileRequesterSystems, after
        // ModalCaptureSystems. Measure only the active→queued handoff here.
        app.world_mut().resource_mut::<BoardRuns>().0 = 0;

        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::Escape);
        app.update();

        let state = app.world().resource::<FileRequesterState>();
        let correlation = state.active.as_ref().unwrap().correlation;
        assert_eq!(
            app.world()
                .resource::<FileRequestCompatAdapter>()
                .legacy_id(correlation),
            Some(FileRequestId(92))
        );
        assert_eq!(app.world().resource::<ModalCoordinator>().queued_len(), 0);
        assert!(app.world().resource::<ModalCapture>().is_captured());
        assert_eq!(
            app.world().resource::<BoardRuns>().0,
            0,
            "capture never opened a board-input gap between queued requests"
        );
        fs::remove_dir_all(root).unwrap();
    }

    fn trigger_button(app: &mut App, kind: RequesterButtonKind) {
        let button = app.world_mut().spawn(RequesterButton(kind)).id();
        app.world_mut().trigger(Activate { entity: button });
        app.world_mut().flush();
    }

    #[test]
    fn activate_event_reaches_cancel_and_emits_result() {
        let root = temp_dir("cancel-action");
        let mut app = action_test_app();
        seed_active_request(&mut app, root.clone(), FileRequestId(77));
        trigger_button(&mut app, RequesterButtonKind::Cancel);
        assert!(app
            .world()
            .resource::<FileRequesterState>()
            .active
            .is_none());
        let messages = app.world().resource::<Messages<FileRequestResult>>();
        let mut cursor = messages.get_cursor();
        let results: Vec<_> = cursor.read(messages).cloned().collect();
        assert_eq!(
            results,
            [FileRequestResult {
                id: FileRequestId(77),
                outcome: FileRequestOutcome::Cancelled,
            }]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn withdrawal_closes_active_request_without_emitting_a_result() {
        let root = temp_dir("withdraw-active");
        let mut app = scheduled_test_app();
        seed_active_request(&mut app, root.clone(), FileRequestId(78));

        app.world_mut()
            .write_message(WithdrawFileRequest(FileRequestId(78)));
        app.update();

        assert!(app
            .world()
            .resource::<FileRequesterState>()
            .active
            .is_none());
        assert!(!app.world().resource::<ModalCoordinator>().is_active());
        assert!(!app.world().resource::<ModalCapture>().is_captured());
        assert!(app
            .world()
            .resource::<FileRequestCompatAdapter>()
            .correlations_for(FileRequestId(78))
            .is_empty());
        let messages = app.world().resource::<Messages<FileRequestResult>>();
        let mut cursor = messages.get_cursor();
        assert_eq!(cursor.read(messages).count(), 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn withdrawal_removes_queued_request_without_disturbing_active_request() {
        let root = temp_dir("withdraw-queued");
        let mut app = scheduled_test_app();
        for id in [FileRequestId(93), FileRequestId(94)] {
            let mut request = FileRequest::open_file(id, "Open");
            request.initial_directory = Some(root.clone());
            app.world_mut().write_message(request);
        }
        app.update();
        assert_eq!(app.world().resource::<ModalCoordinator>().queued_len(), 1);

        app.world_mut()
            .write_message(WithdrawFileRequest(FileRequestId(94)));
        app.update();

        assert!(app
            .world()
            .resource::<FileRequesterState>()
            .active
            .is_some());
        assert_eq!(app.world().resource::<ModalCoordinator>().queued_len(), 0);
        assert!(app.world().resource::<ModalCapture>().is_captured());
        assert!(app
            .world()
            .resource::<FileRequestCompatAdapter>()
            .correlations_for(FileRequestId(94))
            .is_empty());
        let messages = app.world().resource::<Messages<FileRequestResult>>();
        let mut cursor = messages.get_cursor();
        assert_eq!(cursor.read(messages).count(), 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn requester_close_restores_a_live_invocation_focus() {
        let root = temp_dir("restore-focus");
        let mut app = action_test_app();
        let invoker = app.world_mut().spawn_empty().id();
        app.world_mut()
            .resource_mut::<InputFocus>()
            .set(invoker, FocusCause::Navigated);
        seed_active_request(&mut app, root.clone(), FileRequestId(77));

        trigger_button(&mut app, RequesterButtonKind::Cancel);

        assert_eq!(app.world().resource::<InputFocus>().get(), Some(invoker));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn requester_close_clears_a_despawned_invocation_focus() {
        let root = temp_dir("dead-restore-focus");
        let mut app = action_test_app();
        let invoker = app.world_mut().spawn_empty().id();
        app.world_mut()
            .resource_mut::<InputFocus>()
            .set(invoker, FocusCause::Navigated);
        seed_active_request(&mut app, root.clone(), FileRequestId(77));
        app.world_mut().despawn(invoker);

        trigger_button(&mut app, RequesterButtonKind::Cancel);

        assert_eq!(app.world().resource::<InputFocus>().get(), None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn navigation_hidden_and_filter_actions_mutate_the_live_request() {
        let root = temp_dir("navigation-actions");
        let child = root.join("one").join("two");
        fs::create_dir_all(&child).unwrap();
        let mut app = action_test_app();
        seed_active_request(&mut app, child.clone(), FileRequestId(77));

        trigger_button(&mut app, RequesterButtonKind::Parent);
        {
            let mut state = app.world_mut().resource_mut::<FileRequesterState>();
            let active = state.active.as_mut().unwrap();
            assert_eq!(active.directory, root.join("one"));
            active.phase = RequesterPhase::Browsing;
        }
        trigger_button(&mut app, RequesterButtonKind::ToggleHidden);
        {
            let mut state = app.world_mut().resource_mut::<FileRequesterState>();
            let active = state.active.as_mut().unwrap();
            assert!(active.show_hidden);
            active.phase = RequesterPhase::Browsing;
        }
        trigger_button(&mut app, RequesterButtonKind::CycleFilter);
        let state = app.world().resource::<FileRequesterState>();
        let active = state.active.as_ref().unwrap();
        assert_eq!(active.filter_index, 1);
        assert_eq!(
            app.world().get::<Text>(active.hidden_text).unwrap().0,
            "Hidden: on"
        );
        assert_eq!(
            app.world().get::<Text>(active.filter_text).unwrap().0,
            "Type: Audio"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn requester_root_is_a_modal_tab_group() {
        // The requester captures the keyboard while open, so its root must be a
        // modal TabGroup: Tab cycles only within the dialog's own controls and
        // can never escape into a non-modal board TabGroup beneath it (parity
        // with the settings modal, which already does this).
        use bevy::ecs::world::CommandQueue;
        use bevy::input_focus::tab_navigation::TabGroup;
        let mut world = World::new();
        let mut queue = CommandQueue::default();
        let active = {
            let mut commands = Commands::new(&mut queue, &world);
            spawn_requester(
                &mut commands,
                FileRequest::open_file(FileRequestId(1), "Open").into(),
                PathBuf::from("/tmp"),
                InteractionId::next(),
            )
        };
        queue.apply(&mut world);
        let group = world
            .get::<TabGroup>(active.root)
            .expect("requester root carries a TabGroup");
        assert!(group.modal, "requester TabGroup must be modal");
    }

    #[test]
    fn overwrite_prompt_is_its_own_modal_tab_group() {
        // The overwrite prompt is a child of the requester root but must be its
        // own modal group so Tab is confined to Keep/Replace while it is up —
        // bevy skips nested tab-groups when gathering the requester group, so
        // the prompt's controls are not mixed into the requester's Tab order.
        use bevy::ecs::world::CommandQueue;
        use bevy::input_focus::tab_navigation::TabGroup;
        let mut world = World::new();
        let mut queue = CommandQueue::default();
        let (overlay, _cancel) = {
            let mut commands = Commands::new(&mut queue, &world);
            spawn_overwrite_prompt(&mut commands, Path::new("/tmp/song.mid"))
        };
        queue.apply(&mut world);
        let group = world
            .get::<TabGroup>(overlay)
            .expect("overwrite prompt overlay carries a TabGroup");
        assert!(group.modal, "overwrite prompt TabGroup must be modal");
    }
}
