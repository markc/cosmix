//! FileMgr's native twin-pane browser application.

use std::cmp::Ordering as CmpOrdering;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config::{
    ConfigFile, FileMgrConfig, PaneConfig, SidebarConfig, SortColumn, CURRENT_SCHEMA,
};
use crate::file_ops::{FileOpKind, FileOperation};
use crate::{action, app_port, IDENTITY};
use bevy::app::AppExit;
use bevy::asset::AssetPlugin;
use bevy::ecs::system::SystemParam;
use bevy::feathers::{
    dark_theme::create_dark_theme,
    theme::{ThemeBackgroundColor, ThemeBorderColor, ThemeTextColor, UiTheme},
};
use bevy::input::keyboard::KeyboardInput;
use bevy::input::ButtonState;
use bevy::input_focus::tab_navigation::TabIndex;
#[cfg(test)]
use bevy::input_focus::FocusCause;
use bevy::input_focus::{FocusedInput, InputFocus};
use bevy::picking::events::{Click, Pointer};
use bevy::picking::hover::Hovered;
use bevy::picking::pointer::PointerButton;
use bevy::picking::Pickable;
use bevy::prelude::*;
use bevy::tasks::IoTaskPool;
use bevy::text::{EditableText, TextCursorStyle, TextEdit};
use bevy::ui::widget::TextScroll;
use bevy::ui::{percent, px, InteractionDisabled, UiRect};
use bevy::ui_widgets::{Activate, ScrollArea};
use bevy::window::PrimaryWindow;
use chrono::{DateTime, Local, Utc};
use cosmix_actions::filemgr as action_ids;
use cosmix_actions::{ActionArgs, ActionId, ActionValue};
use ctk::icons::{
    file_icon, prepare_data_root, spawn_icon, warm_export_label_fonts, Icon, IconSet,
};
use ctk::icons::{SvgPlugin, UiSvg};
use ctk::prelude::*;
use ctk::theme::{ctk_color, tokens};

pub fn run() {
    warm_export_label_fonts();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let noded_url = app_port::parse_noded_url(&args)
        .unwrap_or_else(|error| {
            eprintln!("filemgr: {error}");
            std::process::exit(2);
        })
        .unwrap_or_else(ctk::prelude::resolve_noded_url);
    let dirs = crate::config::app_dirs();
    let asset_root = prepare_data_root(&dirs).unwrap_or_else(|error| {
        eprintln!("filemgr: {error}");
        dirs.cache()
    });
    let (config, config_file) = ConfigFile::load(&dirs);
    let theme_dir = dirs.config();
    let mut app = App::new();
    app.add_plugins(
        DefaultPlugins
            .set(AssetPlugin {
                file_path: asset_root.to_string_lossy().into_owned(),
                ..default()
            })
            .set(WindowPlugin {
                primary_window: Some(Window {
                    title: format!("{} · twin-pane file manager", IDENTITY.display_name),
                    name: Some(IDENTITY.app_id()),
                    resolution: (1440, 900).into(),
                    resizable: true,
                    ..default()
                }),
                ..default()
            }),
    )
    .add_plugins((SvgPlugin, FeathersPlugins));
    add_runtime_plugins(&mut app, Some(theme_dir), noded_url);
    app.insert_resource(StartupConfig(config))
        .insert_resource(ConfigPersistence {
            file: config_file,
            last_observed: None,
            pending: None,
            settle: Timer::from_seconds(0.35, TimerMode::Once),
        })
        .add_systems(Startup, setup)
        .run();
}

/// Add the complete non-render FileMgr runtime plugin stack.
///
/// Keeping this composition shared with the headless startup regression test
/// makes Bevy validate the same cross-plugin schedule graph used by the binary.
fn add_runtime_plugins(app: &mut App, theme_dir: Option<PathBuf>, noded_url: String) {
    app.add_plugins((
        CtkThemePlugin::new(theme_dir),
        DcsAppShellPlugin,
        ActionBridgePlugin,
        TreeViewPlugin,
        ctk::interaction::InteractionPlugin,
        DndPlugin,
        OsDndPlugin,
        action::FileMgrActionPlugin,
        BrowserPlugin,
        app_port::FileMgrAppPortPlugin::new(noded_url),
    ));
}

#[derive(Resource)]
struct StartupConfig(FileMgrConfig);

#[derive(Resource)]
struct ConfigPersistence {
    file: ConfigFile,
    last_observed: Option<FileMgrConfig>,
    pending: Option<FileMgrConfig>,
    settle: Timer,
}

struct BrowserPlugin;

impl Plugin for BrowserPlugin {
    fn build(&self, app: &mut App) {
        let (tx, rx) = mpsc::channel();
        let (operation_tx, operation_rx) = mpsc::channel();
        let generations = [Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0))];
        app.insert_resource(ListingInbox {
            tx,
            rx: Mutex::new(rx),
            nonce: AtomicU64::new(0),
            generations,
        })
        .init_resource::<DirectoryCountWork>()
        .init_resource::<ModifiedTimeRefresh>()
        .insert_resource(OperationInbox {
            tx: operation_tx,
            rx: Mutex::new(operation_rx),
        })
        .init_resource::<FileActionState>()
        .init_resource::<FileDndState>()
        .init_resource::<FocusedActivationOrigins>()
        .add_observer(on_toolbar_action)
        .add_observer(on_place_action)
        .add_observer(on_sort_action)
        .add_observer(on_nested_action_click)
        .add_observer(on_row_click)
        .add_observer(on_tree_changed)
        .add_observer(focused_action_input_adapter)
        .add_observer(focused_input_adapter)
        .add_systems(First, clear_focused_activation_origins)
        .add_systems(
            Update,
            cancel_dnd_delivery
                .before(InteractionSystems)
                .before(apply_drop_decision_results),
        )
        .add_systems(
            Update,
            (
                (
                    receive_listings,
                    receive_directory_counts,
                    dispatch_directory_counts,
                )
                    .chain(),
                receive_operations.before(resolve_file_drop),
                apply_drop_decision_results.before(on_interaction_result),
                on_interaction_result.before(resolve_file_drop),
                route_browser_actions.in_set(action::ActionRoute),
                persist_config,
                refresh_modified_times,
            ),
        )
        .add_systems(Update, resolve_file_drop.in_set(AppResolve))
        .add_systems(Update, handle_file_drop.after(DndCommit))
        .add_systems(Update, handle_positionless_file_drop.after(DndCommit))
        .add_systems(
            Update,
            consume_dnd_highlights
                .after(DndCommit)
                .before(paint_browser),
        )
        // `paint_browser` is the only writer of selection colour, and rows are
        // spawned through deferred `Commands` — `receive_listings`,
        // `receive_directory_counts` and `set_sort` (via `route_browser_actions`)
        // all reach `rebuild_pane_rows`, as do the `on_tree_changed` /
        // `on_sort_action` observers. Without these constraints the painter is
        // merely unordered against them, so a rebuilt row is not query-visible
        // until the commands flush and a retained selection renders one frame
        // with `BackgroundColor::NONE` and resting foregrounds. Ordering after
        // the producing systems makes Bevy insert the sync point that flushes
        // every command queued earlier in the frame, observers included.
        .add_systems(
            Update,
            paint_browser
                .after(receive_listings)
                .after(receive_directory_counts)
                .after(route_browser_actions),
        );
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PaneId {
    Left,
    Right,
}

impl PaneId {
    pub(crate) fn index(self) -> usize {
        match self {
            Self::Left => 0,
            Self::Right => 1,
        }
    }
}

pub(crate) struct PaneState {
    pub(crate) path: PathBuf,
    generation: u64,
    list: Entity,
    path_input: Entity,
    status_text: Entity,
    pub(crate) rows: Vec<Entity>,
    root_entries: Vec<FileEntry>,
    children: HashMap<PathBuf, Vec<FileEntry>>,
    expanded: HashSet<PathBuf>,
    pending_children: HashSet<PathBuf>,
    pending_counts: usize,
    count_sort_dirty: bool,
    pub(crate) selected: Option<PathBuf>,
    listing: bool,
    pub(crate) history: NavigationHistory,
    pub(crate) show_hidden: bool,
    pub(crate) sort: SortColumn,
    ascending: bool,
}

impl PaneState {
    pub(crate) fn action_selection_available(&self) -> bool {
        !self.listing && self.selected.is_some()
    }

    pub(crate) fn action_rows_available(&self) -> bool {
        !self.listing && !self.rows.is_empty()
    }
}

#[derive(Default)]
pub(crate) struct NavigationHistory {
    pub(crate) back: Vec<PathBuf>,
    pub(crate) forward: Vec<PathBuf>,
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

#[derive(Resource)]
pub(crate) struct BrowserState {
    pub(crate) panes: [PaneState; 2],
    pub(crate) active: PaneId,
    info_text: Entity,
    shell: Entity,
    split: Entity,
}

#[derive(Resource)]
struct ListingInbox {
    tx: mpsc::Sender<ListingReply>,
    rx: Mutex<mpsc::Receiver<ListingReply>>,
    nonce: AtomicU64,
    generations: [Arc<AtomicU64>; 2],
}

struct ListingReply {
    pane: PaneId,
    generation: u64,
    path: PathBuf,
    root: bool,
    result: Result<Vec<FileEntry>, String>,
}

const DIRECTORY_COUNT_CONCURRENCY: usize = 4;

struct DirectoryCountJob {
    pane: PaneId,
    generation: u64,
    entry_path: PathBuf,
    show_hidden: bool,
}

struct DirectoryCountReply {
    pane: PaneId,
    generation: u64,
    entry_path: PathBuf,
    count: Option<usize>,
}

#[derive(Resource)]
struct DirectoryCountWork {
    pending: VecDeque<DirectoryCountJob>,
    tx: mpsc::Sender<DirectoryCountReply>,
    rx: Mutex<mpsc::Receiver<DirectoryCountReply>>,
    in_flight: Arc<AtomicUsize>,
}

impl Default for DirectoryCountWork {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            pending: VecDeque::new(),
            tx,
            rx: Mutex::new(rx),
            in_flight: Arc::new(AtomicUsize::new(0)),
        }
    }
}

#[derive(Resource)]
struct ModifiedTimeRefresh(Timer);

impl Default for ModifiedTimeRefresh {
    fn default() -> Self {
        Self(Timer::from_seconds(60.0, TimerMode::Repeating))
    }
}

#[derive(Resource)]
struct OperationInbox {
    tx: mpsc::Sender<OperationReply>,
    rx: Mutex<mpsc::Receiver<OperationReply>>,
}

struct OperationReply {
    operation: FileOperation,
    source_pane: PaneId,
    drop_delivery: Option<DropDelivery>,
    result: Result<String, String>,
}

#[derive(Clone, Copy)]
struct DropDelivery {
    delivery_id: DeliveryId,
    action: DropAction,
}

/// The single-flight slot owned by a queued plain-drop confirmation.
///
/// This is separate from `pending`: the reservation starts at `DndCommit`,
/// before the interaction service presents the dialog, and is atomically
/// consumed when a matching result starts the real operation. The delivery id
/// therefore has exactly one owner at every point in the deferred path.
struct DropConfirmReservation {
    interaction_id: InteractionId,
    sources: Vec<PathBuf>,
    destination: PathBuf,
    source_pane: PaneId,
    delivery_id: Option<DeliveryId>,
    wayland_decision_required: bool,
}

struct PendingWaylandDropDecision {
    delivery_id: DeliveryId,
    sources: Vec<PathBuf>,
    destination: PathBuf,
    source_pane: PaneId,
    action: DropAction,
}

#[derive(Resource, Default)]
pub(crate) struct FileActionState {
    /// Operations awaiting confirmation, keyed by the interaction id the ctk
    /// service will echo back. A map (not a single slot) because the service
    /// queues concurrent modals — a second confirm must not orphan the first.
    pending_confirm: HashMap<InteractionId, ConfirmedOp>,
    pending_name_edit: HashMap<InteractionId, NameEditKind>,
    drop_confirm: Option<DropConfirmReservation>,
    pending_drop_decision: Option<PendingWaylandDropDecision>,
    pub(crate) pending: bool,
}

impl FileActionState {
    pub(crate) fn is_idle(&self) -> bool {
        !self.pending && self.drop_confirm.is_none() && self.pending_drop_decision.is_none()
    }
}

enum ConfirmedOp {
    Delete {
        source: PathBuf,
        source_pane: PaneId,
    },
}

#[derive(Resource, Default)]
struct FileDndState {
    highlighted: Option<Entity>,
    last_busy: bool,
}

#[derive(Resource, Default)]
struct FocusedActivationOrigins {
    keyboard: HashSet<Entity>,
}

impl FocusedActivationOrigins {
    fn source_for(&mut self, entity: Entity) -> Source {
        if self.keyboard.remove(&entity) {
            Source::Key
        } else {
            Source::Mouse
        }
    }
}

fn clear_focused_activation_origins(mut origins: ResMut<FocusedActivationOrigins>) {
    origins.keyboard.clear();
}

enum NameEditKind {
    NewFolder { parent: PathBuf, pane: PaneId },
    Rename { source: PathBuf, pane: PaneId },
}

#[derive(Clone)]
struct FileEntry {
    path: PathBuf,
    name: String,
    is_dir: bool,
    size: Option<u64>,
    child_count: Option<usize>,
    modified: Option<SystemTime>,
}

#[derive(Component, Clone)]
struct FileRow {
    pane: PaneId,
    path: PathBuf,
    name: String,
    is_dir: bool,
    size: Option<u64>,
    child_count: Option<usize>,
    modified: Option<SystemTime>,
    icon: Entity,
    size_text: Entity,
}

#[derive(Component)]
struct FileRowIcon;

#[derive(Component)]
struct SelectedRowTextColor {
    row: Entity,
    resting: bevy::feathers::theme::ThemeToken,
    selected: bevy::feathers::theme::ThemeToken,
}

#[derive(Component)]
struct SelectedRowSvgColor {
    row: Entity,
    resting: bevy::feathers::theme::ThemeToken,
}

#[derive(Component)]
struct ModifiedTimeText(SystemTime);

#[derive(Component)]
struct PaneSurface(PaneId);

#[derive(Component)]
struct PathInput(PaneId);

#[derive(Component)]
struct SortAction {
    pane: PaneId,
    column: SortColumn,
}

#[derive(Component)]
struct SortIndicator {
    pane: PaneId,
    column: SortColumn,
    ascending: Entity,
    descending: Entity,
}

#[derive(Component)]
struct PlaceAction(PathBuf);

#[allow(clippy::too_many_arguments)]
fn setup(
    mut commands: Commands,
    mut theme: ResMut<UiTheme>,
    mut theme_state: ResMut<ThemeState>,
    mut metrics: ResMut<CtkThemeMetrics>,
    inbox: Res<ListingInbox>,
    mut count_work: ResMut<DirectoryCountWork>,
    startup: Res<StartupConfig>,
    asset_server: Res<AssetServer>,
    actions: Res<MenuActionRegistry>,
) {
    *theme = UiTheme(create_dark_theme());
    // The cosmix theme: built-in Ocean-dark ← shared
    // ~/.config/cosmix/theme.conf.mix (desktop-wide, same scheme key the web
    // uses) ← this app's own theme.conf.mix override.
    let app_cfg = crate::config::app_dirs().config();
    let spec = resolve_app_theme(Some(&app_cfg));
    eprintln!("cosmix theme: {} {}", spec.scheme.name(), spec.mode.name());
    *metrics = spec.metrics.clone();
    apply_theme(&mut theme, &mut theme_state, &spec);
    commands.spawn(Camera2d);
    let icons = IconSet::load_with_rasters(&asset_server, crate::config::app_dirs().cache());

    let info_text = ui_text(&mut commands, "Select a file or folder", 13.0, true);
    let info_panel = commands
        .spawn(Node {
            flex_direction: FlexDirection::Column,
            row_gap: px(10),
            padding: UiRect::all(px(12)),
            ..default()
        })
        .with_child((
            Text::new("Information"),
            TextFont::from_font_size(18.0),
            ThemeTextColor(tokens::TEXT),
        ))
        .add_child(info_text)
        .id();
    let preview_panel = placeholder_panel(
        &mut commands,
        "Preview",
        "Preview providers will land in a later slice.",
    );

    let home = home_directory();
    let places_panel = spawn_places(&mut commands, &home, &icons, &theme);
    let bookmarks_panel = placeholder_panel(
        &mut commands,
        "Bookmarks",
        "Custom bookmarks will be persisted with the layout.",
    );

    let left_start = configured_directory(&startup.0.left.path, &home);
    let right_start = configured_directory(&startup.0.right.path, &home);
    let left_pane = spawn_pane(
        &mut commands,
        &icons,
        &theme,
        PaneId::Left,
        &left_start,
        startup.0.left.show_hidden,
        startup.0.left.sort,
        startup.0.left.ascending,
    );
    let right_pane = spawn_pane(
        &mut commands,
        &icons,
        &theme,
        PaneId::Right,
        &right_start,
        startup.0.right.show_hidden,
        startup.0.right.sort,
        startup.0.right.ascending,
    );
    let split = spawn_dcs_split(
        &mut commands,
        DcsSplitProps {
            first: left_pane.root,
            second: right_pane.root,
            ratio: startup.0.split_ratio,
        },
    );
    let centre = split.root;

    let toolbar = spawn_toolbar(&mut commands, &icons, &theme);
    let menus = action::menu_defs();
    if let Err(issues) = validate_menu_against_registry(&menus, actions.registry()) {
        panic!("invalid FileMgr menu/action registry contract: {issues:?}");
    }
    let menu_bar = spawn_menu_bar_with_icons(&mut commands, &menus, &icons, &theme);
    commands.entity(menu_bar).insert(ActionBridgeBar);
    let shell = spawn_dcs_app_shell_with_icons(
        &mut commands,
        DcsAppShellProps::new(DcsShellProps {
            toolbar,
            centre,
            left_panels: vec![
                DcsPanel::new("places", "Places", places_panel),
                DcsPanel::new("bookmarks", "Bookmarks", bookmarks_panel),
            ],
            right_panels: vec![
                DcsPanel::new("information", "Information", info_panel),
                DcsPanel::new("preview", "Preview", preview_panel),
            ],
            left_width: startup.0.left_sidebar.width,
            right_width: startup.0.right_sidebar.width,
            left_open: startup.0.left_sidebar.open,
            right_open: startup.0.right_sidebar.open,
            left_pinned: startup.0.left_sidebar.pinned,
            right_pinned: startup.0.right_sidebar.pinned,
            pin_breakpoint: 1040.0,
            left_controls: None,
            right_controls: None,
        })
        .with_menu_bar(menu_bar),
        &icons,
        &theme,
    );
    let shell_root = shell.dcs.root;
    let left_panel = startup.0.left_sidebar.active_panel.clone();
    let right_panel = startup.0.right_sidebar.active_panel.clone();
    commands.queue(move |world: &mut World| {
        let Some(mut state) = world.get_mut::<DcsShellState>(shell_root) else {
            return;
        };
        state.left.select_panel_id(&left_panel);
        state.right.select_panel_id(&right_panel);
    });
    let mut state = BrowserState {
        panes: [left_pane.state, right_pane.state],
        active: if startup.0.active_pane == "right" {
            PaneId::Right
        } else {
            PaneId::Left
        },
        info_text,
        shell: shell_root,
        split: split.root,
    };
    start_listing(
        PaneId::Left,
        &mut state.panes[0],
        &inbox,
        &mut count_work,
        &mut commands,
    );
    start_listing(
        PaneId::Right,
        &mut state.panes[1],
        &inbox,
        &mut count_work,
        &mut commands,
    );
    commands.insert_resource(state);
    commands.insert_resource(icons);
}

struct SpawnedPane {
    root: Entity,
    state: PaneState,
}

#[allow(clippy::too_many_arguments)]
fn spawn_pane(
    commands: &mut Commands,
    icons: &IconSet,
    theme: &UiTheme,
    pane: PaneId,
    path: &Path,
    show_hidden: bool,
    sort: SortColumn,
    ascending: bool,
) -> SpawnedPane {
    let path_input = path_input(commands, pane, path);
    let header = commands
        .spawn((
            Node {
                width: percent(100),
                min_height: px(38),
                align_items: AlignItems::Center,
                padding: UiRect::axes(px(10), px(5)),
                ..default()
            },
            ThemeBackgroundColor(tokens::MASTER_PANEL),
        ))
        .add_child(path_input)
        .id();
    let columns = column_headers(commands, icons, theme, pane, sort, ascending);
    let tree = spawn_tree_view(commands);
    let list = commands
        .spawn(Node {
            width: percent(100),
            flex_grow: 1.0,
            min_height: px(0),
            flex_direction: FlexDirection::Column,
            overflow: bevy::ui::Overflow::scroll_y(),
            ..default()
        })
        .insert(ScrollArea)
        .add_child(tree)
        .id();
    let status = spawn_status_bar(commands, "Loading…");
    let root = commands
        .spawn((
            Node {
                flex_grow: 1.0,
                min_width: px(0),
                min_height: px(0),
                flex_direction: FlexDirection::Column,
                border: UiRect::all(px(1)),
                ..default()
            },
            ThemeBackgroundColor(tokens::PANEL),
            BorderColor::all(Color::NONE),
            ThemeBorderColor(tokens::BORDER),
            Outline::new(px(3), px(-3), Color::NONE),
            PaneSurface(pane),
            DropTarget,
        ))
        .add_children(&[header, columns, list, status.root])
        .id();
    SpawnedPane {
        root,
        state: PaneState {
            path: path.to_path_buf(),
            generation: 0,
            list: tree,
            path_input,
            status_text: status.text,
            rows: Vec::new(),
            root_entries: Vec::new(),
            children: HashMap::new(),
            expanded: HashSet::new(),
            pending_children: HashSet::new(),
            pending_counts: 0,
            count_sort_dirty: false,
            selected: None,
            listing: false,
            history: NavigationHistory::default(),
            show_hidden,
            sort,
            ascending,
        },
    }
}

fn spawn_toolbar(commands: &mut Commands, icons: &IconSet, theme: &UiTheme) -> Entity {
    let button = |label, icon, action| {
        ToolbarItem::Button(
            ToolbarButtonDef::new(label)
                .with_icon(icon)
                .icon_only()
                .with_action(action),
        )
    };
    spawn_toolbar_row_with_icons_aligned(
        commands,
        ToolbarAlignment::Centre,
        [
            button("Back", Icon::ArrowLeft, action_ids::NAV_BACK),
            button("Forward", Icon::ArrowRight, action_ids::NAV_FORWARD),
            button("Parent directory", Icon::ArrowUp, action_ids::NAV_PARENT),
            button("Home directory", Icon::House, action_ids::NAV_HOME),
            button("Refresh", Icon::Refresh, action_ids::VIEW_REFRESH),
            button(
                "Toggle hidden files",
                Icon::Eye,
                action_ids::VIEW_TOGGLE_HIDDEN,
            ),
        ],
        [
            button(
                "Copy selection to the other pane",
                Icon::Copy,
                action_ids::FILE_COPY,
            ),
            button(
                "Move selection to the other pane",
                Icon::MoveHorizontal,
                action_ids::FILE_MOVE,
            ),
            button("Delete selection", Icon::Trash, action_ids::FILE_DELETE),
            button("Quit FileMgr", Icon::LogOut, action_ids::APP_QUIT),
        ],
        icons,
        theme,
    )
    .root
}

fn path_input(commands: &mut Commands, pane: PaneId, path: &Path) -> Entity {
    commands
        .spawn((
            // No height constraint of any kind. `EditableText` carries
            // `visible_lines: Some(1.)`, so its measure already reports exactly one
            // line of the *effective* font and the border box tracks the theme's UI
            // font size for free.
            //
            // A `min_height`/`height` here is not the border-box floor it looks like:
            // bevy's `TextInputMeasure` resolves the node's own min/preferred height
            // and returns it as the measured *content* size (`measurement.rs`
            // `resolve_axis`: `effective = known.or(preferred.or(min)…)`), so padding
            // and border are then added on top. The authored `min_height: px(28)`
            // meant 28 of content + 8 padding + 2 border = a 38px box around a 15.6px
            // line — the second line of dead space this used to render. Nor is a fixed
            // `height: px(28)` the fix: it pins the box while the text keeps growing,
            // so a raised `body_px` overflows it.
            //
            // Likewise no `Overflow::clip()`: the renderer already derives a clip rect
            // from the node for any `TextScroll` entity (bevy_ui_render `text.rs`).
            // Adding one here would only serve to hide a future sizing regression.
            Node {
                width: percent(100),
                min_width: px(80),
                padding: UiRect::axes(px(7), px(4)),
                border: UiRect::all(px(1)),
                ..default()
            },
            EditableText::new(path.to_string_lossy()),
            TextLayout::no_wrap(),
            TextFont::from_font_size(13.0),
            ThemeTextColor(tokens::TEXT),
            TextCursorStyle::default(),
            TextScroll::default(),
            ThemeBackgroundColor(tokens::TRACK),
            BorderColor::all(Color::NONE),
            CtkTextInputFocusBorder::new(tokens::BORDER),
            TabIndex(0),
            PathInput(pane),
        ))
        .id()
}

fn column_headers(
    commands: &mut Commands,
    icons: &IconSet,
    theme: &UiTheme,
    pane: PaneId,
    active_sort: SortColumn,
    is_ascending: bool,
) -> Entity {
    let row = commands
        .spawn((
            Node {
                width: percent(100),
                min_height: px(30),
                flex_direction: FlexDirection::Row,
                align_items: AlignItems::Center,
                ..default()
            },
            ThemeBackgroundColor(tokens::MASTER_PANEL),
        ))
        .id();
    for (column, base, width) in [
        (SortColumn::Name, "Name", 58.0),
        (SortColumn::Size, "Size", 17.0),
        (SortColumn::Modified, "Modified", 25.0),
    ] {
        let label = commands
            .spawn((
                Text::new(base),
                TextFont::from_font_size(12.0),
                ThemeTextColor(tokens::TEXT_DIM),
            ))
            .id();
        let ascending = spawn_icon(
            commands,
            icons,
            theme,
            Icon::ChevronUp,
            12.0,
            tokens::TEXT_DIM,
        );
        let descending = spawn_icon(
            commands,
            icons,
            theme,
            Icon::ChevronDown,
            12.0,
            tokens::TEXT_DIM,
        );
        commands.entity(ascending).insert(Node {
            width: px(12),
            min_width: px(12),
            height: px(12),
            display: if active_sort == column && is_ascending {
                Display::Flex
            } else {
                Display::None
            },
            ..default()
        });
        commands.entity(descending).insert(Node {
            width: px(12),
            min_width: px(12),
            height: px(12),
            display: if active_sort == column && !is_ascending {
                Display::Flex
            } else {
                Display::None
            },
            ..default()
        });
        let button = commands
            .spawn((
                Node {
                    width: percent(width),
                    min_height: px(30),
                    min_width: px(0),
                    align_items: AlignItems::Center,
                    column_gap: px(4),
                    padding: UiRect::axes(px(8), px(3)),
                    ..default()
                },
                BackgroundColor(Color::NONE),
                Button,
                Hovered::default(),
                TabIndex(0),
                SortAction { pane, column },
                SortIndicator {
                    pane,
                    column,
                    ascending,
                    descending,
                },
            ))
            .add_children(&[label, ascending, descending])
            .id();
        commands.entity(row).add_child(button);
    }
    row
}

/// Raise a destructive-delete confirmation through the ctk interaction service.
/// The dialog is owned by that service; we retain only the pending intent so
/// the [`InteractionResult`] can be mapped back to an operation.
fn request_delete_confirm(
    commands: &mut Commands,
    file_actions: &mut FileActionState,
    source: PathBuf,
    source_pane: PaneId,
) {
    if !file_actions.is_idle() {
        return;
    }
    let message = format!(
        "{}\n\nThis cannot be undone.",
        sanitise_display_path(&source)
    );
    let request = InteractionRequest::confirm("Permanently delete this item?", message)
        .severity(InteractionSeverity::Danger)
        // Cancel is the Enter default and initial focus — never default a
        // destructive action (WAI: focus the least destructive first).
        .action(InteractionAction::new("cancel", "Cancel", ActionRole::Cancel).default())
        .action(InteractionAction::new(
            "delete",
            "Delete",
            ActionRole::Destructive,
        ));
    file_actions.pending_confirm.insert(
        request.id(),
        ConfirmedOp::Delete {
            source,
            source_pane,
        },
    );
    commands.write_message(action::PendingInteractionRequest(request));
}

/// Raise a drop-target Copy/Move/Cancel confirmation. The chosen action key
/// ("copy"/"move") selects the operation kind when the result arrives.
fn request_transfer_confirm(
    commands: &mut Commands,
    file_actions: &mut FileActionState,
    sources: Vec<PathBuf>,
    destination: PathBuf,
    source_pane: PaneId,
    delivery_id: Option<DeliveryId>,
    wayland_decision_required: bool,
) -> bool {
    if !file_actions.is_idle() || sources.is_empty() {
        return false;
    }
    let source_summary = if sources.len() == 1 {
        sanitise_display_path(&sources[0])
    } else {
        let preview = sources
            .iter()
            .take(4)
            .map(|path| sanitise_display_path(path))
            .collect::<Vec<_>>()
            .join("\n");
        let remainder = sources.len().saturating_sub(4);
        if remainder == 0 {
            format!("{} items:\n{preview}", sources.len())
        } else {
            format!("{} items:\n{preview}\n…and {remainder} more", sources.len())
        }
    };
    let message = format!(
        "{source_summary}\n\nto\n\n{}",
        sanitise_display_path(&destination)
    );
    let request = InteractionRequest::confirm("Drop file or folder", message)
        .action(InteractionAction::new("cancel", "Cancel", ActionRole::Cancel).default())
        .action(InteractionAction::new(
            "copy",
            "Copy",
            ActionRole::Auxiliary,
        ))
        .action(InteractionAction::new(
            "move",
            "Move",
            ActionRole::Auxiliary,
        ));
    file_actions.drop_confirm = Some(DropConfirmReservation {
        interaction_id: request.id(),
        sources,
        destination,
        source_pane,
        delivery_id,
        wayland_decision_required,
    });
    commands.write_message(action::PendingInteractionRequest(request));
    true
}

/// Consume interaction results and run the confirmed operation, if any.
fn on_interaction_result(
    mut results: MessageReader<InteractionResult>,
    browser: Res<BrowserState>,
    operation_inbox: Res<OperationInbox>,
    mut file_actions: ResMut<FileActionState>,
    mut statuses: Query<&mut StatusText>,
    mut drop_completions: MessageWriter<DropComplete>,
    mut drop_decisions: MessageWriter<DropDecision>,
) {
    for result in results.read() {
        if file_actions
            .drop_confirm
            .as_ref()
            .is_some_and(|reservation| reservation.interaction_id == result.id)
        {
            let reservation = file_actions
                .drop_confirm
                .take()
                .expect("matching drop-confirm reservation checked above");
            let DropConfirmReservation {
                sources,
                destination,
                source_pane,
                delivery_id,
                wayland_decision_required,
                ..
            } = reservation;
            let action = match result.outcome.action_key() {
                Some("copy") => Some(DropAction::Copy),
                Some("move") => Some(DropAction::Move),
                _ => None,
            };
            if wayland_decision_required {
                let delivery_id = delivery_id.expect("Wayland Ask reservation has a delivery id");
                drop_decisions.write(DropDecision {
                    delivery_id,
                    decision: match action {
                        Some(DropAction::Copy) => DropDecisionKind::Copy,
                        Some(DropAction::Move) => DropDecisionKind::Move,
                        Some(DropAction::Ask) => unreachable!("Ask cannot resolve to Ask"),
                        None => DropDecisionKind::Dismissed,
                    },
                });
                if let Some(action) = action {
                    file_actions.pending_drop_decision = Some(PendingWaylandDropDecision {
                        delivery_id,
                        sources,
                        destination,
                        source_pane,
                        action,
                    });
                } else {
                    complete_drop_failed(&mut drop_completions, delivery_id);
                }
                continue;
            }
            let started = action.is_some_and(|action| {
                transfer_operation(action, sources, destination).is_ok_and(|operation| {
                    start_operation(
                        operation,
                        source_pane,
                        &browser,
                        &operation_inbox,
                        &mut file_actions,
                        &mut statuses,
                        delivery_id.map(|delivery_id| DropDelivery {
                            delivery_id,
                            action,
                        }),
                    )
                })
            });
            if !started {
                if let Some(delivery_id) = delivery_id {
                    complete_drop_failed(&mut drop_completions, delivery_id);
                }
            }
            continue;
        }
        if let Some(edit) = file_actions.pending_name_edit.remove(&result.id) {
            let InteractionOutcome::Resolved(InteractionValue::Text(name)) = &result.outcome else {
                continue;
            };
            let resolved = match edit {
                NameEditKind::NewFolder { parent, pane } => {
                    Some((FileOperation::new_folder(parent.join(name)), pane))
                }
                NameEditKind::Rename { source, pane } => source.parent().map(|parent| {
                    (
                        FileOperation::rename(source.clone(), parent.join(name)),
                        pane,
                    )
                }),
            };
            if let Some((operation, pane)) = resolved {
                start_operation(
                    operation,
                    pane,
                    &browser,
                    &operation_inbox,
                    &mut file_actions,
                    &mut statuses,
                    None,
                );
            }
            continue;
        }
        let Some(op) = file_actions.pending_confirm.remove(&result.id) else {
            continue;
        };
        // Dismissed, cancelled and future outcomes resolve fail-closed.
        let key = result.outcome.action_key().unwrap_or("cancel");
        let resolved: Option<(FileOperation, PaneId)> = match op {
            ConfirmedOp::Delete {
                source,
                source_pane,
            } if key == "delete" => Some((FileOperation::delete(source), source_pane)),
            ConfirmedOp::Delete { .. } => None,
        };
        if let Some((operation, source_pane)) = resolved {
            start_operation(
                operation,
                source_pane,
                &browser,
                &operation_inbox,
                &mut file_actions,
                &mut statuses,
                None,
            );
        }
    }
}

fn cancel_dnd_delivery(
    mut cancellations: MessageReader<DndDeliveryCancelled>,
    mut file_actions: ResMut<FileActionState>,
    mut withdrawals: MessageWriter<WithdrawInteraction>,
) {
    for cancellation in cancellations.read() {
        if file_actions
            .drop_confirm
            .as_ref()
            .is_some_and(|reservation| reservation.delivery_id == Some(cancellation.delivery_id))
        {
            let reservation = file_actions
                .drop_confirm
                .take()
                .expect("matching drop-confirm reservation checked above");
            withdrawals.write(WithdrawInteraction(reservation.interaction_id));
        }
        if file_actions
            .pending_drop_decision
            .as_ref()
            .is_some_and(|pending| pending.delivery_id == cancellation.delivery_id)
        {
            file_actions.pending_drop_decision = None;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_drop_decision_results(
    mut results: MessageReader<DropDecisionResult>,
    browser: Res<BrowserState>,
    operation_inbox: Res<OperationInbox>,
    mut file_actions: ResMut<FileActionState>,
    mut statuses: Query<&mut StatusText>,
    mut completions: MessageWriter<DropComplete>,
) {
    for result in results.read() {
        if !file_actions
            .pending_drop_decision
            .as_ref()
            .is_some_and(|pending| pending.delivery_id == result.delivery_id)
        {
            continue;
        }
        let pending = file_actions
            .pending_drop_decision
            .take()
            .expect("matching Wayland drop decision checked above");
        match &result.status {
            DropDecisionStatus::Accepted => {
                let delivery = DropDelivery {
                    delivery_id: pending.delivery_id,
                    action: pending.action,
                };
                let started =
                    transfer_operation(pending.action, pending.sources, pending.destination)
                        .is_ok_and(|operation| {
                            start_operation(
                                operation,
                                pending.source_pane,
                                &browser,
                                &operation_inbox,
                                &mut file_actions,
                                &mut statuses,
                                Some(delivery),
                            )
                        });
                if !started {
                    complete_drop_failed(&mut completions, pending.delivery_id);
                }
            }
            DropDecisionStatus::Rejected(reason) => set_status(
                &browser,
                pending.source_pane,
                &format!("Drop action rejected: {reason}"),
                &mut statuses,
            ),
        }
    }
}

fn spawn_places(commands: &mut Commands, home: &Path, icons: &IconSet, theme: &UiTheme) -> Entity {
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
    let root = commands
        .spawn(Node {
            flex_direction: FlexDirection::Column,
            row_gap: px(2),
            padding: UiRect::all(px(6)),
            ..default()
        })
        .id();
    for (name, path) in places {
        let icon_kind = if name == "Home" {
            Icon::House
        } else if name == "Downloads" {
            Icon::Download
        } else if name == "Music" {
            Icon::Music
        } else if name == "Filesystem" {
            Icon::HardDrive
        } else {
            Icon::Folder
        };
        let icon = spawn_icon(commands, icons, theme, icon_kind, 16.0, tokens::TEXT_DIM);
        let label = ui_text(commands, name, 13.0, false);
        let row = commands
            .spawn((
                Node {
                    width: percent(100),
                    min_height: px(26),
                    column_gap: px(7),
                    align_items: AlignItems::Center,
                    padding: UiRect::axes(px(8), px(4)),
                    border_radius: BorderRadius::all(px(3)),
                    ..default()
                },
                BackgroundColor(Color::NONE),
                Button,
                Hovered::default(),
                TabIndex(0),
                PlaceAction(path),
            ))
            .add_children(&[icon, label])
            .id();
        commands.entity(root).add_child(row);
    }
    root
}

fn placeholder_panel(commands: &mut Commands, title: &str, body: &str) -> Entity {
    commands
        .spawn(Node {
            flex_direction: FlexDirection::Column,
            row_gap: px(9),
            padding: UiRect::all(px(12)),
            ..default()
        })
        .with_child((
            Text::new(title.to_string()),
            TextFont::from_font_size(18.0),
            ThemeTextColor(tokens::TEXT),
        ))
        .with_child((
            Text::new(body.to_string()),
            TextFont::from_font_size(13.0),
            ThemeTextColor(tokens::TEXT_DIM),
        ))
        .id()
}

fn clear_pane_rows(commands: &mut Commands, pane: &mut PaneState) {
    for row in pane.rows.drain(..) {
        commands.entity(row).despawn();
    }
    pane.selected = None;
}

fn start_listing(
    pane_id: PaneId,
    pane: &mut PaneState,
    inbox: &ListingInbox,
    count_work: &mut DirectoryCountWork,
    commands: &mut Commands,
) {
    pane.generation = inbox.nonce.fetch_add(1, Ordering::Relaxed) + 1;
    inbox.generations[pane_id.index()].store(pane.generation, Ordering::Release);
    // Every queued job for this pane belongs to a generation superseded by the
    // listing just started. Purge it immediately even when all workers are
    // occupied, so repeated refreshes cannot accumulate directory-sized stale
    // batches behind the four in-flight jobs.
    purge_pending_directory_counts(count_work, pane_id);
    clear_pane_rows(commands, pane);
    pane.listing = true;
    pane.root_entries.clear();
    pane.children.clear();
    pane.expanded.clear();
    pane.pending_children.clear();
    pane.pending_counts = 0;
    pane.count_sort_dirty = false;
    let generation = pane.generation;
    let path = pane.path.clone();
    let show_hidden = pane.show_hidden;
    let tx = inbox.tx.clone();
    IoTaskPool::get()
        .spawn(async move {
            let result = read_directory(&path, show_hidden);
            let _ = tx.send(ListingReply {
                pane: pane_id,
                generation,
                path,
                root: true,
                result,
            });
        })
        .detach();
}

fn purge_pending_directory_counts(work: &mut DirectoryCountWork, pane: PaneId) {
    work.pending.retain(|job| job.pane != pane);
}

fn start_child_listing(pane_id: PaneId, pane: &mut PaneState, path: PathBuf, inbox: &ListingInbox) {
    if !pane.pending_children.insert(path.clone()) {
        return;
    }
    let generation = pane.generation;
    let show_hidden = pane.show_hidden;
    let tx = inbox.tx.clone();
    IoTaskPool::get()
        .spawn(async move {
            let result = read_directory(&path, show_hidden);
            let _ = tx.send(ListingReply {
                pane: pane_id,
                generation,
                path,
                root: false,
                result,
            });
        })
        .detach();
}

fn read_directory(directory: &Path, show_hidden: bool) -> Result<Vec<FileEntry>, String> {
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

fn count_directory_entries(
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

fn entry_visible(name: &str, show_hidden: bool) -> bool {
    show_hidden || !name.starts_with('.')
}

fn sanitise_display_text(text: &str) -> String {
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

fn sanitise_display_path(path: &Path) -> String {
    // Sanitise the complete rendered path, not only its final component:
    // Unix permits control characters in every ancestor directory name.
    sanitise_display_text(&path.to_string_lossy())
}

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

fn sort_entries(entries: &mut [FileEntry], column: SortColumn, ascending: bool) {
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

#[allow(clippy::too_many_arguments)]
fn receive_listings(
    mut commands: Commands,
    inbox: Res<ListingInbox>,
    mut count_work: ResMut<DirectoryCountWork>,
    mut browser: ResMut<BrowserState>,
    mut texts: Query<&mut Text>,
    mut statuses: Query<&mut StatusText>,
    mut inputs: Query<&mut EditableText>,
    icons: Res<IconSet>,
    theme: Res<UiTheme>,
    windows: Query<&Window, With<PrimaryWindow>>,
) {
    let export_icon_scale = windows
        .single()
        .map_or(1, |window| export_icon_buffer_scale(window.scale_factor()));
    let mut replies = Vec::new();
    let rx = inbox.rx.lock().expect("FileMgr listing inbox poisoned");
    while let Ok(reply) = rx.try_recv() {
        replies.push(reply);
    }
    drop(rx);

    for reply in replies {
        let clear_info = browser.active == reply.pane;
        let info_text = browser.info_text;
        let pane = &mut browser.panes[reply.pane.index()];
        if pane.generation != reply.generation || reply.root && pane.path != reply.path {
            continue;
        }
        pane.pending_children.remove(&reply.path);
        if reply.root {
            pane.listing = false;
            if let Ok(mut input) = inputs.get_mut(pane.path_input) {
                input.editor_mut().set_text(&pane.path.to_string_lossy());
                input.queue_edit(TextEdit::TextEnd(false));
            }
        }
        match reply.result {
            Ok(mut entries) => {
                if !reply.root {
                    pane.count_sort_dirty |=
                        set_backing_child_count(pane, &reply.path, Some(entries.len()))
                            && pane.sort == SortColumn::Size;
                }
                let jobs = entries
                    .iter()
                    .filter(|entry| entry.is_dir)
                    .map(|entry| DirectoryCountJob {
                        pane: reply.pane,
                        generation: reply.generation,
                        entry_path: entry.path.clone(),
                        show_hidden: pane.show_hidden,
                    })
                    .collect::<Vec<_>>();
                pane.pending_counts = pane.pending_counts.saturating_add(jobs.len());
                count_work.pending.extend(jobs);
                sort_entries(&mut entries, pane.sort, pane.ascending);
                if reply.root {
                    pane.root_entries = entries;
                } else {
                    pane.children.insert(reply.path.clone(), entries);
                }
                if pane.pending_counts == 0 && pane.count_sort_dirty {
                    sort_all_entries(pane);
                    pane.count_sort_dirty = false;
                }
                rebuild_pane_rows(
                    &mut commands,
                    reply.pane,
                    pane,
                    &icons,
                    &theme,
                    export_icon_scale,
                );
                if let Ok(mut status) = statuses.get_mut(pane.status_text) {
                    status.set(pane_summary(&pane.root_entries));
                }
            }
            Err(error) => {
                if reply.root {
                    clear_pane_rows(&mut commands, pane);
                    pane.root_entries.clear();
                }
                if let Ok(mut status) = statuses.get_mut(pane.status_text) {
                    status.set(sanitise_display_text(&error));
                }
            }
        }
        if clear_info {
            if let Ok(mut info) = texts.get_mut(info_text) {
                info.0 = "Select a file or folder".into();
            }
        }
    }
}

fn dispatch_directory_counts(mut work: ResMut<DirectoryCountWork>, inbox: Res<ListingInbox>) {
    while work.in_flight.load(Ordering::Acquire) < DIRECTORY_COUNT_CONCURRENCY {
        let Some(job) = work.pending.pop_front() else {
            break;
        };
        let live_generation = Arc::clone(&inbox.generations[job.pane.index()]);
        if live_generation.load(Ordering::Acquire) != job.generation {
            continue;
        }

        let tx = work.tx.clone();
        let in_flight = Arc::clone(&work.in_flight);
        in_flight.fetch_add(1, Ordering::AcqRel);
        IoTaskPool::get()
            .spawn(async move {
                if live_generation.load(Ordering::Acquire) == job.generation {
                    let count = count_directory_entries(&job.entry_path, job.show_hidden, || {
                        live_generation.load(Ordering::Acquire) != job.generation
                    });
                    if live_generation.load(Ordering::Acquire) == job.generation {
                        let _ = tx.send(DirectoryCountReply {
                            pane: job.pane,
                            generation: job.generation,
                            entry_path: job.entry_path,
                            count,
                        });
                    }
                }
                in_flight.fetch_sub(1, Ordering::AcqRel);
            })
            .detach();
    }
}

#[allow(clippy::too_many_arguments)]
fn receive_directory_counts(
    mut commands: Commands,
    work: Res<DirectoryCountWork>,
    mut browser: ResMut<BrowserState>,
    mut rows: Query<&mut FileRow>,
    mut texts: Query<&mut Text>,
    icons: Res<IconSet>,
    theme: Res<UiTheme>,
    windows: Query<&Window, With<PrimaryWindow>>,
) {
    let export_icon_scale = windows
        .single()
        .map_or(1, |window| export_icon_buffer_scale(window.scale_factor()));
    let replies = {
        let rx = work
            .rx
            .lock()
            .expect("FileMgr directory-count inbox poisoned");
        rx.try_iter().collect::<Vec<_>>()
    };

    for reply in replies {
        let reply_is_for_active_pane = browser.active == reply.pane;
        let info_text = browser.info_text;
        let pane = &mut browser.panes[reply.pane.index()];
        if pane.generation != reply.generation {
            continue;
        }
        pane.pending_counts = pane.pending_counts.saturating_sub(1);
        pane.count_sort_dirty |= set_backing_child_count(pane, &reply.entry_path, reply.count)
            && pane.sort == SortColumn::Size;

        let mut selected_info = None;
        for entity in &pane.rows {
            let Ok(mut row) = rows.get_mut(*entity) else {
                continue;
            };
            if row.path != reply.entry_path {
                continue;
            }
            row.child_count = reply.count;
            let count_text = format_child_count(reply.count);
            if let Ok(mut text) = texts.get_mut(row.size_text) {
                if text.0 != count_text {
                    text.0 = count_text;
                }
            }
            if count_reply_repaints_information(
                reply_is_for_active_pane,
                pane.selected.as_deref(),
                &row.path,
            ) {
                selected_info = Some(format_file_info(&row, SystemTime::now()));
            }
            break;
        }
        if let Some(selected_info) = selected_info {
            if let Ok(mut info) = texts.get_mut(info_text) {
                info.0 = selected_info;
            }
        }

        if pane.pending_counts == 0 && pane.count_sort_dirty {
            sort_all_entries(pane);
            pane.count_sort_dirty = false;
            rebuild_pane_rows(
                &mut commands,
                reply.pane,
                pane,
                &icons,
                &theme,
                export_icon_scale,
            );
        }
    }
}

fn count_reply_repaints_information(
    reply_is_for_active_pane: bool,
    selected: Option<&Path>,
    reply_path: &Path,
) -> bool {
    reply_is_for_active_pane && selected == Some(reply_path)
}

fn set_backing_child_count(pane: &mut PaneState, path: &Path, count: Option<usize>) -> bool {
    for entry in pane
        .root_entries
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

fn sort_all_entries(pane: &mut PaneState) {
    sort_entries(&mut pane.root_entries, pane.sort, pane.ascending);
    for entries in pane.children.values_mut() {
        sort_entries(entries, pane.sort, pane.ascending);
    }
}

fn start_operation(
    operation: FileOperation,
    source_pane: PaneId,
    browser: &BrowserState,
    inbox: &OperationInbox,
    file_actions: &mut FileActionState,
    statuses: &mut Query<&mut StatusText>,
    drop_delivery: Option<DropDelivery>,
) -> bool {
    if !file_actions.is_idle() {
        set_status(
            browser,
            source_pane,
            "Another file operation is still running",
            statuses,
        );
        return false;
    }
    file_actions.pending = true;
    let verb = match operation.kind {
        FileOpKind::Copy => "Copying",
        FileOpKind::Move => "Moving",
        FileOpKind::Delete => "Deleting",
        FileOpKind::NewFolder => "Creating",
        FileOpKind::Rename => "Renaming",
        FileOpKind::BatchCopy => "Copying batch",
        FileOpKind::BatchMove => "Moving batch",
    };
    set_status(
        browser,
        source_pane,
        &format!("{verb} {}…", operation.source.display()),
        statuses,
    );
    let tx = inbox.tx.clone();
    IoTaskPool::get()
        .spawn(async move {
            let result = operation.execute();
            let _ = tx.send(OperationReply {
                operation,
                source_pane,
                drop_delivery,
                result,
            });
        })
        .detach();
    true
}

#[allow(clippy::too_many_arguments)]
fn receive_operations(
    mut commands: Commands,
    inbox: Res<OperationInbox>,
    listing_inbox: Res<ListingInbox>,
    mut count_work: ResMut<DirectoryCountWork>,
    mut browser: ResMut<BrowserState>,
    mut file_actions: ResMut<FileActionState>,
    mut texts: Query<&mut Text>,
    mut statuses: Query<&mut StatusText>,
) {
    let mut replies = Vec::new();
    let rx = inbox.rx.lock().expect("FileMgr operation inbox poisoned");
    while let Ok(reply) = rx.try_recv() {
        replies.push(reply);
    }
    drop(rx);

    for reply in replies {
        file_actions.pending = false;
        if let Some(completion) = operation_drop_completion(&reply) {
            commands.write_message(completion);
        }
        match reply.result {
            Ok(message) => {
                if let Ok(mut info) = texts.get_mut(browser.info_text) {
                    info.0 = sanitise_display_text(&message);
                }
            }
            Err(error) => set_status(
                &browser,
                reply.source_pane,
                &format!(
                    "{} failed: {error}",
                    match reply.operation.kind {
                        FileOpKind::Copy => "Copy",
                        FileOpKind::Move => "Move",
                        FileOpKind::Delete => "Delete",
                        FileOpKind::NewFolder => "Create folder",
                        FileOpKind::Rename => "Rename",
                        FileOpKind::BatchCopy => "Batch copy",
                        FileOpKind::BatchMove => "Batch move",
                    }
                ),
                &mut statuses,
            ),
        }
        // Relist on failure as well as success. A batch stops at its first
        // runtime error with earlier items already transferred, and a failed
        // move or delete can also have mutated the tree before failing, so a
        // failure is not evidence that the panes still match the disk. One
        // relist per reply either way — never one per batch item.
        for pane_id in [PaneId::Left, PaneId::Right] {
            let pane = &mut browser.panes[pane_id.index()];
            start_listing(
                pane_id,
                pane,
                &listing_inbox,
                &mut count_work,
                &mut commands,
            );
        }
    }
}

fn operation_drop_completion(reply: &OperationReply) -> Option<DropComplete> {
    let delivery = reply.drop_delivery?;
    Some(DropComplete {
        delivery_id: delivery.delivery_id,
        outcome: if reply.result.is_ok() {
            DropOutcome::Completed(delivery.action)
        } else {
            DropOutcome::Failed
        },
    })
}

fn set_status(
    browser: &BrowserState,
    pane: PaneId,
    message: &str,
    statuses: &mut Query<&mut StatusText>,
) {
    if let Ok(mut status) = statuses.get_mut(browser.panes[pane.index()].status_text) {
        // Status text is presentation-only. File-operation errors retain raw
        // paths internally, but controls must not create forged status lines.
        status.set(sanitise_display_text(message));
    }
}

fn other_pane(pane: PaneId) -> PaneId {
    match pane {
        PaneId::Left => PaneId::Right,
        PaneId::Right => PaneId::Left,
    }
}

#[cfg(test)]
fn is_current_listing(
    expected_generation: u64,
    expected_path: &Path,
    reply_generation: u64,
    reply_path: &Path,
) -> bool {
    expected_generation == reply_generation && expected_path == reply_path
}

#[derive(Clone)]
struct VisibleEntry {
    entry: FileEntry,
    depth: usize,
}

const EXPORT_ICON_LOGICAL_SIZE: u32 = 40;

fn export_icon_buffer_scale(scale_factor: f32) -> i32 {
    if !scale_factor.is_finite() {
        return 1;
    }
    scale_factor.ceil().max(1.0) as i32
}

fn rebuild_pane_rows(
    commands: &mut Commands,
    pane_id: PaneId,
    pane: &mut PaneState,
    icons: &IconSet,
    theme: &UiTheme,
    export_icon_scale: i32,
) {
    for row in pane.rows.drain(..) {
        commands.entity(row).despawn();
    }
    let mut visible = Vec::new();
    flatten_entries(
        &pane.root_entries,
        0,
        &pane.expanded,
        &pane.children,
        &mut visible,
    );
    for item in visible {
        let path = item.entry.path.clone();
        let row = spawn_file_row(
            commands,
            icons,
            theme,
            pane_id,
            item.entry,
            item.depth,
            pane.list,
            pane.expanded.contains(&path),
            export_icon_scale,
        );
        commands.entity(pane.list).add_child(row);
        pane.rows.push(row);
    }
}

fn flatten_entries(
    entries: &[FileEntry],
    depth: usize,
    expanded: &HashSet<PathBuf>,
    children: &HashMap<PathBuf, Vec<FileEntry>>,
    output: &mut Vec<VisibleEntry>,
) {
    if depth > 64 {
        return;
    }
    for entry in entries {
        output.push(VisibleEntry {
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

#[allow(clippy::too_many_arguments)]
fn spawn_file_row(
    commands: &mut Commands,
    icons: &IconSet,
    theme: &UiTheme,
    pane: PaneId,
    entry: FileEntry,
    depth: usize,
    tree: Entity,
    expanded: bool,
    export_icon_scale: i32,
) -> Entity {
    let size = if entry.is_dir {
        format_child_count(entry.child_count)
    } else {
        entry.size.map(format_size).unwrap_or_default()
    };
    let modified = entry
        .modified
        .map(|modified| format_modified_at(modified, SystemTime::now()))
        .unwrap_or_default();
    let row = commands
        .spawn((
            Node {
                width: percent(100),
                min_height: px(24),
                flex_shrink: 0.0,
                ..default()
            },
            BackgroundColor(Color::NONE),
            Outline::new(px(2), px(-2), Color::NONE),
            Button,
            Hovered::default(),
            if entry.is_dir {
                TreeItem::branch(tree, None, expanded)
            } else {
                TreeItem::leaf(tree, None)
            },
        ))
        .id();
    let disclosure = if entry.is_dir {
        let collapsed = spawn_icon(
            commands,
            icons,
            theme,
            Icon::ChevronRight,
            13.0,
            tokens::TEXT_DIM,
        );
        let opened = spawn_icon(
            commands,
            icons,
            theme,
            Icon::ChevronDown,
            13.0,
            tokens::TEXT_DIM,
        );
        commands
            .entity(collapsed)
            .remove::<ThemeSvgColor>()
            .insert(SelectedRowSvgColor {
                row,
                resting: tokens::TEXT_DIM,
            });
        commands
            .entity(opened)
            .remove::<ThemeSvgColor>()
            .insert(SelectedRowSvgColor {
                row,
                resting: tokens::TEXT_DIM,
            });
        commands.entity(collapsed).insert((
            Node {
                width: px(13),
                min_width: px(13),
                height: px(13),
                display: if expanded {
                    Display::None
                } else {
                    Display::Flex
                },
                ..default()
            },
            Pickable::IGNORE,
        ));
        commands.entity(opened).insert((
            Node {
                width: px(13),
                min_width: px(13),
                height: px(13),
                display: if expanded {
                    Display::Flex
                } else {
                    Display::None
                },
                ..default()
            },
            Pickable::IGNORE,
        ));
        spawn_tree_disclosure_with_icons(commands, row, depth, true, expanded, collapsed, opened)
    } else {
        let disclosure = spawn_tree_disclosure(commands, row, depth, false, false);
        commands.entity(disclosure).insert(Pickable::IGNORE);
        disclosure
    };
    let icon_resting = if entry.is_dir {
        tokens::CONTROL_ACTIVE
    } else {
        tokens::TEXT_DIM
    };
    let icon = spawn_icon(
        commands,
        icons,
        theme,
        file_icon(&entry.path, entry.is_dir, expanded),
        16.0,
        icon_resting.clone(),
    );
    commands.entity(icon).remove::<ThemeSvgColor>().insert((
        FileRowIcon,
        Pickable::IGNORE,
        SelectedRowSvgColor {
            row,
            resting: icon_resting,
        },
    ));
    let icon_kind = file_icon(&entry.path, entry.is_dir, expanded);
    let export_icon = icons.raster(icon_kind, EXPORT_ICON_LOGICAL_SIZE, export_icon_scale);
    let ghost_icon = icons.handle(icon_kind);
    let ghost_icon_colour = ctk_color(theme, &tokens::TEXT_DIM);
    let ghost_name = entry.name.clone();
    let ghost = GhostBuilder::new(move |root, commands| {
        commands
            .entity(root)
            .insert(ThemeBackgroundColor(tokens::MASTER_PANEL.clone()));
        let icon = commands
            .spawn((
                Node {
                    width: px(16),
                    min_width: px(16),
                    height: px(16),
                    ..default()
                },
                UiSvg(ghost_icon.clone()),
                SvgColor(ghost_icon_colour),
                ThemeSvgColor(tokens::TEXT_DIM.clone()),
                Pickable::IGNORE,
            ))
            .id();
        let name = commands
            .spawn((
                Text::new(ghost_name.clone()),
                TextFont::from_font_size(12.0),
                ThemeTextColor(tokens::TEXT.clone()),
                TextLayout::no_wrap(),
                Pickable::IGNORE,
            ))
            .id();
        commands.entity(root).add_children(&[icon, name]);
    });
    let source = DragSource::new(DragPayload::Paths(vec![entry.path.clone()]), ghost)
        .with_export_label(entry.name.clone());
    let source = match export_icon {
        Ok(icon) => source.with_export_icon(icon),
        Err(_) => source,
    };
    let layout = row_layout(
        commands,
        row,
        disclosure,
        icon,
        [&entry.name, &size, &modified],
    );
    if let Some(modified) = entry.modified {
        commands
            .entity(layout.modified_text)
            .insert(ModifiedTimeText(modified));
    }
    commands.entity(row).insert((
        FileRow {
            pane,
            path: entry.path.clone(),
            name: entry.name.clone(),
            is_dir: entry.is_dir,
            size: entry.size,
            child_count: entry.child_count,
            modified: entry.modified,
            icon,
            size_text: layout.size_text,
        },
        source,
    ));
    if entry.is_dir {
        commands.entity(row).insert(DropTarget);
    }
    commands.entity(row).add_child(layout.root);
    row
}

struct RowLayoutEntities {
    root: Entity,
    size_text: Entity,
    modified_text: Entity,
}

fn row_layout(
    commands: &mut Commands,
    accessibility_target: Entity,
    disclosure: Entity,
    icon: Entity,
    values: [&str; 3],
) -> RowLayoutEntities {
    let sizes = [0.58, 0.17, 0.25];
    let row = commands
        .spawn((
            Node {
                width: percent(100),
                min_height: px(23),
                flex_direction: FlexDirection::Row,
                align_items: AlignItems::Center,
                ..default()
            },
            BackgroundColor(Color::NONE),
            Pickable::IGNORE,
        ))
        .id();
    let mut text_entities = Vec::with_capacity(values.len());
    for (index, value) in values.into_iter().enumerate() {
        let text = ui_text(commands, value, 12.0, index > 0);
        commands.entity(text).remove::<ThemeTextColor>().insert((
            Pickable::IGNORE,
            SelectedRowTextColor {
                row: accessibility_target,
                resting: if index > 0 {
                    tokens::TEXT_DIM
                } else {
                    tokens::TEXT
                },
                selected: if index > 0 {
                    tokens::ROW_SELECTED_TEXT_DIM
                } else {
                    tokens::ROW_SELECTED_TEXT
                },
            },
        ));
        let cell = commands
            .spawn((
                Node {
                    width: percent(sizes[index] * 100.0),
                    min_width: px(0),
                    padding: if index == 0 {
                        UiRect::axes(px(3), px(1))
                    } else {
                        UiRect::axes(px(7), px(1))
                    },
                    column_gap: px(5),
                    align_items: AlignItems::Center,
                    overflow: bevy::ui::Overflow::clip(),
                    ..default()
                },
                Pickable::IGNORE,
            ))
            .id();
        if index == 0 {
            let width_host = commands
                .spawn((
                    Node {
                        flex_grow: 1.0,
                        min_width: px(0),
                        align_items: AlignItems::Center,
                        overflow: bevy::ui::Overflow::clip(),
                        ..default()
                    },
                    Pickable::IGNORE,
                ))
                .add_child(text)
                .id();
            commands.entity(text).insert(
                MiddleElideText::new(value, width_host)
                    .with_accessibility_target(accessibility_target),
            );
            commands
                .entity(cell)
                .add_children(&[disclosure, icon, width_host]);
        } else {
            commands.entity(cell).add_child(text);
        }
        text_entities.push(text);
        commands.entity(row).add_child(cell);
    }
    RowLayoutEntities {
        root: row,
        size_text: text_entities[1],
        modified_text: text_entities[2],
    }
}

fn on_toolbar_action(
    activated: On<Activate>,
    actions: Query<&ToolbarActionButton>,
    capture: Res<ModalCapture>,
    mut origins: ResMut<FocusedActivationOrigins>,
    focus: Res<InputFocus>,
    mut commands: Commands,
) {
    if capture.is_captured() {
        return;
    }
    let Ok(action) = actions.get(activated.entity) else {
        return;
    };
    let source = origins.source_for(activated.entity);
    commands.write_message(ActionRequest {
        action: action.0,
        source,
        args: ActionArgs::new(),
        invocation_focus: focus.get(),
    });
}

#[allow(clippy::type_complexity)]
fn on_nested_action_click(
    mut click: On<Pointer<Click>>,
    actions: Query<
        (),
        Or<(
            With<ToolbarActionButton>,
            With<PlaceAction>,
            With<SortAction>,
        )>,
    >,
    parents: Query<&ChildOf>,
    mut commands: Commands,
) {
    if click.button != PointerButton::Primary {
        return;
    }
    // Walk up from the hit target to the nearest action ancestor. The direct
    // target (a click on the button's own padding, not a child) is just the
    // zeroth hop — handled by the same branch, so it activates like any other.
    let mut entity = click.original_event_target();
    loop {
        if actions.contains(entity) {
            click.propagate(false);
            commands.trigger(Activate { entity });
            return;
        }
        let Ok(parent) = parents.get(entity) else {
            return;
        };
        entity = parent.parent();
    }
}

fn on_place_action(
    activated: On<Activate>,
    places: Query<&PlaceAction>,
    capture: Res<ModalCapture>,
    mut origins: ResMut<FocusedActivationOrigins>,
    focus: Res<InputFocus>,
    mut commands: Commands,
) {
    if capture.is_captured() {
        return;
    }
    let Ok(place) = places.get(activated.entity) else {
        return;
    };
    let source = origins.source_for(activated.entity);
    let mut args = ActionArgs::new();
    args.insert(
        "path".to_owned(),
        ActionValue::String(place.0.to_string_lossy().into_owned()),
    );
    commands.write_message(ActionRequest {
        action: action_ids::PLACE_OPEN,
        source,
        args,
        invocation_focus: focus.get(),
    });
}

fn on_sort_action(
    activated: On<Activate>,
    actions: Query<&SortAction>,
    capture: Res<ModalCapture>,
    mut origins: ResMut<FocusedActivationOrigins>,
    mut browser: ResMut<BrowserState>,
    focus: Res<InputFocus>,
    mut commands: Commands,
) {
    if capture.is_captured() {
        return;
    }
    let Ok(action) = actions.get(activated.entity) else {
        return;
    };
    let source = origins.source_for(activated.entity);
    let id = match action.column {
        SortColumn::Name => action_ids::VIEW_SORT_NAME,
        SortColumn::Size => action_ids::VIEW_SORT_SIZE,
        SortColumn::Modified => action_ids::VIEW_SORT_MODIFIED,
    };
    browser.active = action.pane;
    commands.write_message(ActionRequest {
        action: id,
        source,
        args: ActionArgs::new(),
        invocation_focus: focus.get(),
    });
}

#[allow(clippy::too_many_arguments)]
fn on_tree_changed(
    changed: On<TreeViewChanged>,
    rows: Query<&FileRow>,
    mut browser: ResMut<BrowserState>,
    inbox: Res<ListingInbox>,
    icons: Res<IconSet>,
    theme: Res<UiTheme>,
    mut svg_icons: Query<&mut UiSvg, With<FileRowIcon>>,
    mut statuses: Query<&mut StatusText>,
    windows: Query<&Window, With<PrimaryWindow>>,
    mut commands: Commands,
) {
    let Ok(row) = rows.get(changed.item) else {
        return;
    };
    let row = row.clone();
    if !row.is_dir {
        return;
    }
    let pane = &mut browser.panes[row.pane.index()];
    if changed.expanded {
        pane.expanded.insert(row.path.clone());
    } else {
        pane.expanded.remove(&row.path);
    }
    if let Ok(mut icon) = svg_icons.get_mut(row.icon) {
        icon.0 = icons.handle(file_icon(&row.path, true, changed.expanded));
    }
    if !changed.expanded || pane.children.contains_key(&row.path) {
        let export_icon_scale = windows
            .single()
            .map_or(1, |window| export_icon_buffer_scale(window.scale_factor()));
        rebuild_pane_rows(
            &mut commands,
            row.pane,
            pane,
            &icons,
            &theme,
            export_icon_scale,
        );
        if let Ok(mut status) = statuses.get_mut(pane.status_text) {
            status.set(pane_summary(&pane.root_entries));
        }
    } else {
        start_child_listing(row.pane, pane, row.path, &inbox);
    }
}

#[allow(clippy::too_many_arguments)]
fn on_row_click(
    mut click: On<Pointer<Click>>,
    rows: Query<&FileRow>,
    disclosures: Query<(), With<TreeDisclosure>>,
    surfaces: Query<&PaneSurface>,
    parents: Query<&ChildOf>,
    mut browser: ResMut<BrowserState>,
    inbox: Res<ListingInbox>,
    mut count_work: ResMut<DirectoryCountWork>,
    icons: Res<IconSet>,
    theme: Res<UiTheme>,
    windows: Query<&Window, With<PrimaryWindow>>,
    focus: Res<InputFocus>,
    dnd_session: Res<DragSession>,
    mut texts: Query<&mut Text>,
    mut commands: Commands,
) {
    if click.entity != click.original_event_target() {
        return;
    }
    if !matches!(
        click.button,
        PointerButton::Primary | PointerButton::Secondary
    ) {
        return;
    }
    let mut entity = click.original_event_target();
    let mut pane_hit = None;
    let row = loop {
        if disclosures.contains(entity) && click.button == PointerButton::Primary {
            return;
        }
        if let Ok(row) = rows.get(entity) {
            break Some((entity, row));
        }
        if let Ok(surface) = surfaces.get(entity) {
            pane_hit = Some(surface.0);
            break None;
        }
        match parents.get(entity) {
            Ok(parent) => entity = parent.parent(),
            Err(_) => break None,
        }
    };
    let Some((row_entity, row)) = row else {
        if let Some(pane) = pane_hit {
            browser.active = pane;
        }
        return;
    };
    if browser.panes[row.pane.index()].listing {
        return;
    }
    if dnd_click_is_blocked(row_entity, &dnd_session) {
        return;
    }
    select_row(&mut browser, row, &mut texts);
    if click.button == PointerButton::Secondary {
        click.propagate(false);
        let position = windows
            .single()
            .map_or(click.pointer_location.position, |window| {
                Vec2::new(
                    click
                        .pointer_location
                        .position
                        .x
                        .clamp(4.0, (window.width() - 220.0).max(4.0)),
                    click
                        .pointer_location
                        .position
                        .y
                        .clamp(4.0, (window.height() - 110.0).max(4.0)),
                )
            });
        let items = action::context_menu_defs();
        spawn_context_menu_with_icons(&mut commands, &items, position, focus.get(), &icons, &theme);
    } else if click.count >= 2 && row.is_dir {
        let pane = &mut browser.panes[row.pane.index()];
        navigate_new(
            row.pane,
            pane,
            row.path.clone(),
            &inbox,
            &mut count_work,
            &mut commands,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn resolve_file_drop(
    mut proposals: MessageReader<AcceptanceProposal>,
    mut acceptances: MessageWriter<DropAcceptance>,
    sources: Query<&DragSource>,
    rows: Query<&FileRow>,
    surfaces: Query<&PaneSurface>,
    browser: Res<BrowserState>,
    file_actions: Res<FileActionState>,
    mut dnd_state: ResMut<FileDndState>,
    mut session: ResMut<DragSession>,
) {
    let busy = !file_actions.is_idle();
    if dnd_state.last_busy != busy {
        dnd_state.last_busy = busy;
        // Busy is a stable boolean transition, not a per-frame value. CTK
        // bounds repeated stale post-release answers and fails the drop closed
        // if application churn keeps invalidating this candidate.
        session.invalidate_acceptance();
    }

    for proposal in proposals.read() {
        let allowed_actions = proposed_file_drop(
            proposal, &sources, &rows, &surfaces, &browser, busy,
        )
        .map_or(ActionMask::NONE, |(source, destination)| {
            source.map_or(ActionMask::ALL, |source| {
                file_drop_actions(source, destination, busy)
            })
        });
        acceptances.write(DropAcceptance {
            proposal_id: proposal.proposal_id,
            revision: proposal.revision,
            allowed_actions,
            preferred: requested_drop_action(proposal.modifiers),
        });
    }
}

fn proposed_file_drop<'a>(
    proposal: &AcceptanceProposal,
    sources: &'a Query<&DragSource>,
    rows: &'a Query<&FileRow>,
    surfaces: &Query<&PaneSurface>,
    browser: &'a BrowserState,
    pending: bool,
) -> Option<(Option<&'a Path>, &'a Path)> {
    if pending || !matches!(proposal.payload_summary, PayloadSummary::Paths { .. }) {
        return None;
    }
    let destination = drop_destination(proposal.target, rows, surfaces, browser)?.as_path();
    match proposal.origin {
        DndOrigin::Internal(source_entity) => {
            let source_row = rows.get(source_entity).ok()?;
            let DragPayload::Paths(paths) = sources.get(source_entity).ok()?.payload() else {
                return None;
            };
            let [source] = paths.as_slice() else {
                return None;
            };
            if source != &source_row.path {
                return None;
            }
            Some((Some(source.as_path()), destination))
        }
        DndOrigin::External(_) => Some((None, destination)),
    }
}

fn drop_destination<'a>(
    target: Entity,
    rows: &'a Query<&FileRow>,
    surfaces: &Query<&PaneSurface>,
    browser: &'a BrowserState,
) -> Option<&'a PathBuf> {
    if let Ok(row) = rows.get(target) {
        return row.is_dir.then_some(&row.path);
    }
    let pane = surfaces.get(target).ok()?.0;
    Some(&browser.panes[pane.index()].path)
}

fn drop_destination_with_pane(
    target: Entity,
    rows: &Query<&FileRow>,
    surfaces: &Query<&PaneSurface>,
    browser: &BrowserState,
) -> Option<(PathBuf, PaneId)> {
    if let Ok(row) = rows.get(target) {
        return row.is_dir.then(|| (row.path.clone(), row.pane));
    }
    let pane = surfaces.get(target).ok()?.0;
    Some((browser.panes[pane.index()].path.clone(), pane))
}

fn file_drop_actions(source: &Path, destination: &Path, busy: bool) -> ActionMask {
    if busy || !drop_destination_is_distinct(source, destination) {
        ActionMask::NONE
    } else {
        ActionMask::COPY | ActionMask::MOVE | ActionMask::ASK
    }
}

fn file_drop_actions_batch(sources: &[PathBuf], destination: &Path, busy: bool) -> ActionMask {
    if sources.is_empty()
        || sources
            .iter()
            .any(|source| !drop_destination_is_distinct(source, destination))
    {
        ActionMask::NONE
    } else {
        file_drop_actions(&sources[0], destination, busy)
    }
}

/// Resolves the existing source and destination once each and compares
/// filesystem identity, not lexical spelling. A source symlink is an entry to
/// copy/move, never a directory root: `symlink_metadata` deliberately prevents
/// its target from participating in the descendant test.
fn drop_destination_is_distinct(source: &Path, destination: &Path) -> bool {
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

fn requested_drop_action(modifiers: Modifiers) -> DropAction {
    if modifiers.control {
        DropAction::Copy
    } else if modifiers.shift {
        DropAction::Move
    } else {
        DropAction::Ask
    }
}

fn transfer_operation(
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

#[allow(clippy::too_many_arguments)]
fn handle_file_drop(
    mut drops: MessageReader<DndDrop>,
    rows: Query<&FileRow>,
    surfaces: Query<&PaneSurface>,
    browser: Res<BrowserState>,
    operation_inbox: Res<OperationInbox>,
    mut file_actions: ResMut<FileActionState>,
    mut statuses: Query<&mut StatusText>,
    mut completions: MessageWriter<DropComplete>,
    mut commands: Commands,
) {
    for dropped in drops.read() {
        let (sources, source_pane) = match (&dropped.origin, &dropped.payload) {
            (DndOrigin::Internal(entity), DragPayload::Paths(paths)) => {
                let Ok(row) = rows.get(*entity) else {
                    complete_drop_failed(&mut completions, dropped.delivery_id);
                    continue;
                };
                let [path] = paths.as_slice() else {
                    complete_drop_failed(&mut completions, dropped.delivery_id);
                    continue;
                };
                if path != &row.path {
                    complete_drop_failed(&mut completions, dropped.delivery_id);
                    continue;
                }
                (vec![path.clone()], Some(row.pane))
            }
            (DndOrigin::External(_), DragPayload::Paths(paths)) if !paths.is_empty() => {
                (paths.clone(), None)
            }
            _ => {
                complete_drop_failed(&mut completions, dropped.delivery_id);
                continue;
            }
        };
        let Some((destination, destination_pane)) =
            drop_destination_with_pane(dropped.target, &rows, &surfaces, &browser)
        else {
            complete_drop_failed(&mut completions, dropped.delivery_id);
            continue;
        };
        let allowed = file_drop_actions_batch(&sources, &destination, !file_actions.is_idle());
        if !allowed.contains(dropped.action) {
            complete_drop_failed(&mut completions, dropped.delivery_id);
            continue;
        }

        match dropped.action {
            DropAction::Ask => {
                if !request_transfer_confirm(
                    &mut commands,
                    &mut file_actions,
                    sources,
                    destination,
                    source_pane.unwrap_or(destination_pane),
                    Some(dropped.delivery_id),
                    dropped.decision_requirement == DropDecisionRequirement::Wayland,
                ) {
                    complete_drop_failed(&mut completions, dropped.delivery_id);
                }
            }
            action @ (DropAction::Copy | DropAction::Move) => {
                let delivery = DropDelivery {
                    delivery_id: dropped.delivery_id,
                    action,
                };
                let Ok(operation) = transfer_operation(action, sources, destination) else {
                    complete_drop_failed(&mut completions, dropped.delivery_id);
                    continue;
                };
                let started = start_operation(
                    operation,
                    source_pane.unwrap_or(destination_pane),
                    &browser,
                    &operation_inbox,
                    &mut file_actions,
                    &mut statuses,
                    Some(delivery),
                );
                if !started {
                    complete_drop_failed(&mut completions, dropped.delivery_id);
                }
            }
        }
    }
}

fn complete_drop_failed(completions: &mut MessageWriter<DropComplete>, delivery_id: DeliveryId) {
    completions.write(DropComplete {
        delivery_id,
        outcome: DropOutcome::Failed,
    });
}

fn handle_positionless_file_drop(
    mut drops: MessageReader<PositionlessFileDrop>,
    browser: Res<BrowserState>,
    mut file_actions: ResMut<FileActionState>,
    mut commands: Commands,
) {
    for dropped in drops.read() {
        let pane = browser.active;
        let destination = browser.panes[pane.index()].path.clone();
        // Native X11 exposes no XDND position through Bevy. The active pane is
        // therefore the only honest destination; "pane under cursor" would be
        // an invented coordinate.
        let _ = request_transfer_confirm(
            &mut commands,
            &mut file_actions,
            dropped.paths.clone(),
            destination,
            pane,
            None,
            false,
        );
    }
}

fn consume_dnd_highlights(
    mut transitions: MessageReader<DndHighlightChanged>,
    mut state: ResMut<FileDndState>,
) {
    for transition in transitions.read() {
        if transition.highlighted {
            state.highlighted = Some(transition.target);
        } else if state.highlighted == Some(transition.target) {
            state.highlighted = None;
        }
    }
}

fn select_row(browser: &mut BrowserState, row: &FileRow, texts: &mut Query<&mut Text>) {
    browser.active = row.pane;
    browser.panes[row.pane.index()].selected = Some(row.path.clone());
    if let Ok(mut info) = texts.get_mut(browser.info_text) {
        info.0 = format_file_info(row, SystemTime::now());
    }
}

fn refresh_modified_times(
    time: Res<Time>,
    mut refresh: ResMut<ModifiedTimeRefresh>,
    browser: Res<BrowserState>,
    rows: Query<&FileRow>,
    mut texts: Query<(&mut Text, Option<&ModifiedTimeText>)>,
) {
    refresh.0.tick(time.delta());
    if !refresh.0.just_finished() {
        return;
    }

    let now = SystemTime::now();
    for (mut text, modified) in &mut texts {
        let Some(modified) = modified else {
            continue;
        };
        let rendered = format_modified_at(modified.0, now);
        if text.0 != rendered {
            text.0 = rendered;
        }
    }

    let pane = &browser.panes[browser.active.index()];
    let selected = pane.selected.as_ref().and_then(|selected| {
        pane.rows
            .iter()
            .filter_map(|entity| rows.get(*entity).ok())
            .find(|row| &row.path == selected)
    });
    if let Some(row) = selected {
        if let Ok((mut info, _)) = texts.get_mut(browser.info_text) {
            let rendered = format_file_info(row, now);
            if info.0 != rendered {
                info.0 = rendered;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn focused_input_adapter(
    mut event: On<FocusedInput<KeyboardInput>>,
    path_markers: Query<&PathInput>,
    editables: Query<(), With<EditableText>>,
    mut inputs: Query<&mut EditableText>,
    mut browser: ResMut<BrowserState>,
    inbox: Res<ListingInbox>,
    mut count_work: ResMut<DirectoryCountWork>,
    mut statuses: Query<&mut StatusText>,
    capture: Res<ModalCapture>,
    mut commands: Commands,
) {
    let entity = event.focused_entity;
    if capture.is_captured() {
        event.propagate(false);
        return;
    }
    if let Ok(path_marker) = path_markers.get(entity) {
        event.propagate(false);
        if event.input.state == ButtonState::Pressed && !event.input.repeat {
            if event.input.key_code == KeyCode::Enter {
                let target = inputs
                    .get(entity)
                    .ok()
                    .map(|input| PathBuf::from(input.value().to_string()));
                if let Some(target) = target {
                    browser.active = path_marker.0;
                    let pane = &mut browser.panes[path_marker.0.index()];
                    if target.is_dir() {
                        navigate_new(
                            path_marker.0,
                            pane,
                            target,
                            &inbox,
                            &mut count_work,
                            &mut commands,
                        );
                    } else if let Ok(mut status) = statuses.get_mut(pane.status_text) {
                        status.set(format!(
                            "Not a directory: {}",
                            sanitise_display_path(&target)
                        ));
                    }
                }
            } else if event.input.key_code == KeyCode::Escape {
                let pane = &browser.panes[path_marker.0.index()];
                if let Ok(mut input) = inputs.get_mut(entity) {
                    input.editor_mut().set_text(&pane.path.to_string_lossy());
                    input.queue_edit(TextEdit::TextEnd(false));
                }
            }
        }
        return;
    }
    if editables.contains(entity) {
        event.propagate(false);
    }
}

#[allow(clippy::type_complexity)]
fn focused_action_input_adapter(
    mut event: On<FocusedInput<KeyboardInput>>,
    action_controls: Query<
        Has<InteractionDisabled>,
        Or<(
            With<ToolbarActionButton>,
            With<PlaceAction>,
            With<SortAction>,
        )>,
    >,
    capture: Res<ModalCapture>,
    mut origins: ResMut<FocusedActivationOrigins>,
) {
    let disabled = action_controls.get(event.focused_entity).ok();
    let focused_action = disabled.is_some();
    if focused_action && capture.is_captured() {
        // Always swallow the semantic key before it reaches the window
        // resolver. Under capture the Activate producer is independently
        // fail-closed, so Bevy's same-target Button observer cannot publish.
        event.propagate(false);
        return;
    }
    if focused_action {
        // Bevy's ButtonPlugin is the sole Enter/Space activation authority for
        // focused buttons. The resulting Activate observer publishes exactly
        // one semantic ActionRequest; this adapter only prevents the key from
        // also reaching the window keymap.
        event.propagate(false);
        if disabled == Some(false)
            && event.input.state == ButtonState::Pressed
            && !event.input.repeat
            && matches!(event.input.key_code, KeyCode::Enter | KeyCode::Space)
        {
            origins.keyboard.insert(event.focused_entity);
        }
    }
}

#[derive(SystemParam)]
struct BrowserActionParams<'w, 's> {
    requests: MessageReader<'w, 's, ActionRequest>,
    registry: Res<'w, MenuActionRegistry>,
    browser: ResMut<'w, BrowserState>,
    listing: Res<'w, ListingInbox>,
    count_work: ResMut<'w, DirectoryCountWork>,
    operations: Res<'w, OperationInbox>,
    file_actions: ResMut<'w, FileActionState>,
    rows: Query<'w, 's, &'static FileRow>,
    texts: Query<'w, 's, &'static mut Text>,
    statuses: Query<'w, 's, &'static mut StatusText>,
    icons: Res<'w, IconSet>,
    theme: Res<'w, UiTheme>,
    windows: Query<'w, 's, &'static Window, With<PrimaryWindow>>,
    exit: MessageWriter<'w, AppExit>,
    commands: Commands<'w, 's>,
}

fn route_browser_actions(mut params: BrowserActionParams) {
    let requests: Vec<_> = params.requests.read().cloned().collect();
    for request in &requests {
        if !is_browser_action(request.action) {
            continue;
        }
        if let Err(error) = params.registry.registry().validate_invocation_from(
            request.action,
            &request.args,
            action::registry_source(request.source),
        ) {
            eprintln!("filemgr: action {} rejected: {error}", request.action);
            continue;
        }
        let pane_id = params.browser.active;
        match request.action {
            action_ids::APP_QUIT => {
                params.exit.write(AppExit::Success);
            }
            action_ids::NAV_SWITCH_PANE => {
                params.browser.active = other_pane(pane_id);
            }
            action_ids::NAV_BACK => navigate_back(
                pane_id,
                &mut params.browser.panes[pane_id.index()],
                &params.listing,
                &mut params.count_work,
                &mut params.commands,
            ),
            action_ids::NAV_FORWARD => navigate_forward(
                pane_id,
                &mut params.browser.panes[pane_id.index()],
                &params.listing,
                &mut params.count_work,
                &mut params.commands,
            ),
            action_ids::NAV_PARENT => navigate_parent(
                pane_id,
                &mut params.browser.panes[pane_id.index()],
                &params.listing,
                &mut params.count_work,
                &mut params.commands,
            ),
            action_ids::NAV_HOME => navigate_new(
                pane_id,
                &mut params.browser.panes[pane_id.index()],
                home_directory(),
                &params.listing,
                &mut params.count_work,
                &mut params.commands,
            ),
            action_ids::VIEW_REFRESH => start_listing(
                pane_id,
                &mut params.browser.panes[pane_id.index()],
                &params.listing,
                &mut params.count_work,
                &mut params.commands,
            ),
            action_ids::VIEW_TOGGLE_HIDDEN => toggle_hidden(
                pane_id,
                &mut params.browser.panes[pane_id.index()],
                &params.listing,
                &mut params.count_work,
                &mut params.commands,
            ),
            action_ids::VIEW_SORT_NAME => set_sort(
                pane_id,
                &mut params.browser.panes[pane_id.index()],
                SortColumn::Name,
                &params.icons,
                &params.theme,
                params
                    .windows
                    .single()
                    .map_or(1, |window| export_icon_buffer_scale(window.scale_factor())),
                &mut params.commands,
            ),
            action_ids::VIEW_SORT_SIZE => set_sort(
                pane_id,
                &mut params.browser.panes[pane_id.index()],
                SortColumn::Size,
                &params.icons,
                &params.theme,
                params
                    .windows
                    .single()
                    .map_or(1, |window| export_icon_buffer_scale(window.scale_factor())),
                &mut params.commands,
            ),
            action_ids::VIEW_SORT_MODIFIED => set_sort(
                pane_id,
                &mut params.browser.panes[pane_id.index()],
                SortColumn::Modified,
                &params.icons,
                &params.theme,
                params
                    .windows
                    .single()
                    .map_or(1, |window| export_icon_buffer_scale(window.scale_factor())),
                &mut params.commands,
            ),
            action_ids::SELECT_NEXT => select_relative(&mut params, 1),
            action_ids::SELECT_PREVIOUS => select_relative(&mut params, -1),
            action_ids::SELECT_FIRST => select_edge(&mut params, false),
            action_ids::SELECT_LAST => select_edge(&mut params, true),
            action_ids::FILE_OPEN => open_selection(&mut params),
            action_ids::FILE_COPY | action_ids::FILE_MOVE => {
                transfer_selection(&mut params, request.action)
            }
            action_ids::FILE_DELETE => delete_selection(&mut params),
            action_ids::FILE_NEW_FOLDER => open_name_edit(
                &mut params,
                NameEditRequest::NewFolder,
                request.invocation_focus,
            ),
            action_ids::FILE_RENAME => open_name_edit(
                &mut params,
                NameEditRequest::Rename,
                request.invocation_focus,
            ),
            action_ids::PLACE_OPEN => {
                let Some(ActionValue::String(path)) = request.args.get("path") else {
                    continue;
                };
                let target = PathBuf::from(path);
                if target.is_dir() {
                    navigate_new(
                        pane_id,
                        &mut params.browser.panes[pane_id.index()],
                        target,
                        &params.listing,
                        &mut params.count_work,
                        &mut params.commands,
                    );
                }
            }
            _ => {}
        }
    }
}

fn is_browser_action(action: ActionId) -> bool {
    action_ids::DEFAULT_KEYMAP_ACTION_IDS.contains(&action) || action == action_ids::PLACE_OPEN
}

fn set_sort(
    pane_id: PaneId,
    pane: &mut PaneState,
    column: SortColumn,
    icons: &IconSet,
    theme: &UiTheme,
    export_icon_scale: i32,
    commands: &mut Commands,
) {
    if pane.sort == column {
        pane.ascending = !pane.ascending;
    } else {
        pane.sort = column;
        pane.ascending = true;
    }
    sort_all_entries(pane);
    pane.count_sort_dirty = false;
    rebuild_pane_rows(commands, pane_id, pane, icons, theme, export_icon_scale);
}

fn selected_row(params: &BrowserActionParams) -> Option<FileRow> {
    let pane = &params.browser.panes[params.browser.active.index()];
    let selected = pane.selected.as_ref()?;
    pane.rows
        .iter()
        .filter_map(|entity| params.rows.get(*entity).ok())
        .find(|row| &row.path == selected)
        .cloned()
}

fn select_relative(params: &mut BrowserActionParams, delta: isize) {
    let pane_id = params.browser.active;
    let pane = &params.browser.panes[pane_id.index()];
    let current = pane.selected.as_ref().and_then(|selected| {
        pane.rows.iter().position(|entity| {
            params
                .rows
                .get(*entity)
                .is_ok_and(|row| &row.path == selected)
        })
    });
    let index = if delta > 0 {
        current.map_or(0, |index| {
            (index + 1).min(pane.rows.len().saturating_sub(1))
        })
    } else {
        current.unwrap_or(0).saturating_sub(1)
    };
    select_index(params, index);
}

fn select_edge(params: &mut BrowserActionParams, last: bool) {
    let pane = &params.browser.panes[params.browser.active.index()];
    let index = if last {
        pane.rows.len().checked_sub(1)
    } else if pane.rows.is_empty() {
        None
    } else {
        Some(0)
    };
    if let Some(index) = index {
        select_index(params, index);
    }
}

fn select_index(params: &mut BrowserActionParams, index: usize) {
    let pane = &params.browser.panes[params.browser.active.index()];
    let row = pane
        .rows
        .get(index)
        .and_then(|entity| params.rows.get(*entity).ok())
        .cloned();
    if let Some(row) = row {
        select_row(&mut params.browser, &row, &mut params.texts);
    }
}

fn open_selection(params: &mut BrowserActionParams) {
    let Some(row) = selected_row(params) else {
        return;
    };
    let pane_id = row.pane;
    select_row(&mut params.browser, &row, &mut params.texts);
    if row.is_dir {
        navigate_new(
            pane_id,
            &mut params.browser.panes[pane_id.index()],
            row.path,
            &params.listing,
            &mut params.count_work,
            &mut params.commands,
        );
    } else if let Err(error) = std::process::Command::new("xdg-open")
        .arg(&row.path)
        .spawn()
    {
        set_status(
            &params.browser,
            pane_id,
            &format!("Opening {}: {error}", row.path.display()),
            &mut params.statuses,
        );
    }
}

fn transfer_selection(params: &mut BrowserActionParams, action: ActionId) {
    let pane_id = params.browser.active;
    let Some(source) = params.browser.panes[pane_id.index()].selected.clone() else {
        return;
    };
    let destination = params.browser.panes[other_pane(pane_id).index()]
        .path
        .clone();
    let operation = if action == action_ids::FILE_COPY {
        FileOperation::copy(source, destination)
    } else {
        FileOperation::move_to(source, destination)
    };
    start_operation(
        operation,
        pane_id,
        &params.browser,
        &params.operations,
        &mut params.file_actions,
        &mut params.statuses,
        None,
    );
}

fn delete_selection(params: &mut BrowserActionParams) {
    let pane_id = params.browser.active;
    let Some(source) = params.browser.panes[pane_id.index()].selected.clone() else {
        return;
    };
    request_delete_confirm(
        &mut params.commands,
        &mut params.file_actions,
        source,
        pane_id,
    );
}

enum NameEditRequest {
    NewFolder,
    Rename,
}

fn open_name_edit(
    params: &mut BrowserActionParams,
    request: NameEditRequest,
    invocation_focus: Option<Entity>,
) {
    if !params.file_actions.is_idle() || !params.file_actions.pending_name_edit.is_empty() {
        return;
    }
    let pane = params.browser.active;
    let (title, message, initial, kind, select_all) = match request {
        NameEditRequest::NewFolder => (
            "New folder",
            "Enter a name for the new folder.",
            "New Folder".to_owned(),
            NameEditKind::NewFolder {
                parent: params.browser.panes[pane.index()].path.clone(),
                pane,
            },
            false,
        ),
        NameEditRequest::Rename => {
            let Some(source) = params.browser.panes[pane.index()].selected.clone() else {
                return;
            };
            let initial = source
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            // Deliberately leave control characters unsanitised: this field
            // becomes the rename target when submitted. A display projection
            // here would silently rename an unchanged filename on Enter.
            (
                "Rename",
                "Enter a new name.",
                initial,
                NameEditKind::Rename { source, pane },
                true,
            )
        }
    };
    let mut prompt = InteractionRequest::prompt(title, message)
        .initial_text(initial)
        .validator(TextValidator::new(validate_filename));
    if let Some(invoker) = invocation_focus {
        prompt = prompt.invoked_by(invoker);
    }
    if select_all {
        prompt = prompt.select_all();
    }
    params
        .file_actions
        .pending_name_edit
        .insert(prompt.id(), kind);
    params
        .commands
        .write_message(action::PendingInteractionRequest(prompt));
}

fn navigate_new(
    pane_id: PaneId,
    pane: &mut PaneState,
    target: PathBuf,
    inbox: &ListingInbox,
    count_work: &mut DirectoryCountWork,
    commands: &mut Commands,
) {
    if pane.history.record_new(&pane.path, &target) {
        pane.path = target;
    }
    start_listing(pane_id, pane, inbox, count_work, commands);
}

fn navigate_back(
    pane_id: PaneId,
    pane: &mut PaneState,
    inbox: &ListingInbox,
    count_work: &mut DirectoryCountWork,
    commands: &mut Commands,
) {
    let Some(target) = pane.history.back(&pane.path) else {
        return;
    };
    pane.path = target;
    start_listing(pane_id, pane, inbox, count_work, commands);
}

fn navigate_forward(
    pane_id: PaneId,
    pane: &mut PaneState,
    inbox: &ListingInbox,
    count_work: &mut DirectoryCountWork,
    commands: &mut Commands,
) {
    let Some(target) = pane.history.forward(&pane.path) else {
        return;
    };
    pane.path = target;
    start_listing(pane_id, pane, inbox, count_work, commands);
}

/// Navigate to the parent of the pane's current directory, if any. The single
/// source of truth for "go up", shared by the toolbar Parent action and the
/// Backspace shortcut so the two can't drift.
fn navigate_parent(
    pane_id: PaneId,
    pane: &mut PaneState,
    inbox: &ListingInbox,
    count_work: &mut DirectoryCountWork,
    commands: &mut Commands,
) {
    if let Some(parent) = pane.path.parent().map(Path::to_path_buf) {
        navigate_new(pane_id, pane, parent, inbox, count_work, commands);
    }
}

/// Flip the pane's hidden-file visibility and re-list. The single source of
/// truth for "toggle hidden", shared by the toolbar ToggleHidden action and the
/// Ctrl+H shortcut.
fn toggle_hidden(
    pane_id: PaneId,
    pane: &mut PaneState,
    inbox: &ListingInbox,
    count_work: &mut DirectoryCountWork,
    commands: &mut Commands,
) {
    pane.show_hidden = !pane.show_hidden;
    start_listing(pane_id, pane, inbox, count_work, commands);
}

#[allow(clippy::too_many_arguments)]
fn paint_browser(
    browser: Res<BrowserState>,
    file_actions: Res<FileActionState>,
    dnd: Res<FileDndState>,
    theme: Res<UiTheme>,
    mut rows: Query<
        (
            Entity,
            &FileRow,
            &Hovered,
            &mut BackgroundColor,
            &mut Outline,
        ),
        Without<PaneSurface>,
    >,
    mut panes: Query<(Entity, &PaneSurface, &mut BorderColor, &mut Outline), Without<FileRow>>,
    row_lookup: Query<&FileRow>,
    mut row_texts: Query<(&SelectedRowTextColor, &mut TextColor)>,
    mut row_icons: Query<(&SelectedRowSvgColor, &mut SvgColor)>,
    sort_indicators: Query<&SortIndicator>,
    mut nodes: Query<&mut Node>,
) {
    for (entity, row, hovered, mut colour, mut outline) in &mut rows {
        let selected = browser.panes[row.pane.index()].selected.as_ref() == Some(&row.path);
        let highlighted = file_actions.is_idle() && dnd.highlighted == Some(entity);
        let want = if selected {
            ctk_color(&theme, &tokens::ROW_SELECTED)
        } else if hovered.get() {
            ctk_color(&theme, &tokens::ROW_HOVER)
        } else {
            Color::NONE
        };
        if colour.0 != want {
            colour.0 = want;
        }
        let outline_colour = if highlighted {
            ctk_color(&theme, &tokens::METER_AMBER)
        } else {
            Color::NONE
        };
        if outline.color != outline_colour {
            outline.color = outline_colour;
        }
    }
    for (managed, mut colour) in &mut row_texts {
        let Ok(row) = row_lookup.get(managed.row) else {
            continue;
        };
        let selected = browser.panes[row.pane.index()].selected.as_ref() == Some(&row.path);
        let token = if selected {
            &managed.selected
        } else {
            &managed.resting
        };
        let want = ctk_color(&theme, token);
        if colour.0 != want {
            colour.0 = want;
        }
    }
    for (managed, mut colour) in &mut row_icons {
        let Ok(row) = row_lookup.get(managed.row) else {
            continue;
        };
        let selected = browser.panes[row.pane.index()].selected.as_ref() == Some(&row.path);
        let token = if selected {
            &tokens::ROW_SELECTED_TEXT
        } else {
            &managed.resting
        };
        let want = ctk_color(&theme, token);
        if colour.0 != want {
            colour.0 = want;
        }
    }
    for (entity, pane, mut border, mut outline) in &mut panes {
        let highlighted = file_actions.is_idle() && dnd.highlighted == Some(entity);
        let want = ctk_color(&theme, &pane_border_token(pane.0, browser.active));
        if border.top != want {
            *border = BorderColor::all(want);
        }
        let outline_colour = if highlighted {
            ctk_color(&theme, &tokens::METER_AMBER)
        } else {
            Color::NONE
        };
        if outline.color != outline_colour {
            outline.color = outline_colour;
        }
    }
    for indicator in &sort_indicators {
        let pane = &browser.panes[indicator.pane.index()];
        let active = pane.sort == indicator.column;
        if let Ok(mut node) = nodes.get_mut(indicator.ascending) {
            node.display = if active && pane.ascending {
                Display::Flex
            } else {
                Display::None
            };
        }
        if let Ok(mut node) = nodes.get_mut(indicator.descending) {
            node.display = if active && !pane.ascending {
                Display::Flex
            } else {
                Display::None
            };
        }
    }
}

fn persist_config(
    time: Res<Time>,
    browser: Option<Res<BrowserState>>,
    shells: Query<&DcsShellState>,
    splits: Query<&DcsSplitState>,
    mut persistence: ResMut<ConfigPersistence>,
) {
    let Some(browser) = browser else { return };
    let Ok(shell) = shells.get(browser.shell) else {
        return;
    };
    let Ok(split) = splits.get(browser.split) else {
        return;
    };
    let pane_config = |pane: &PaneState| PaneConfig {
        path: pane.path.to_string_lossy().into_owned(),
        show_hidden: pane.show_hidden,
        sort: pane.sort,
        ascending: pane.ascending,
    };
    let sidebar_config = |sidebar: &DcsSidebarState, fallback: &str| SidebarConfig {
        open: sidebar.open,
        pinned: sidebar.pin_preference,
        width: sidebar.width(),
        active_panel: sidebar.active_panel_id().unwrap_or(fallback).to_string(),
    };
    let snapshot = FileMgrConfig {
        schema_version: CURRENT_SCHEMA,
        left: pane_config(&browser.panes[PaneId::Left.index()]),
        right: pane_config(&browser.panes[PaneId::Right.index()]),
        active_pane: match browser.active {
            PaneId::Left => "left",
            PaneId::Right => "right",
        }
        .into(),
        split_ratio: split.ratio(),
        left_sidebar: sidebar_config(&shell.left, "places"),
        right_sidebar: sidebar_config(&shell.right, "information"),
    };
    if persistence.last_observed.as_ref() != Some(&snapshot) {
        persistence.last_observed = Some(snapshot.clone());
        persistence.pending = Some(snapshot);
        persistence.settle.reset();
    }
    if persistence.pending.is_none() {
        return;
    }
    persistence.settle.tick(time.delta());
    if !persistence.settle.just_finished() {
        return;
    }
    let snapshot = persistence
        .pending
        .take()
        .expect("pending FileMgr config checked above");
    if let Err(error) = persistence.file.save(&snapshot) {
        eprintln!("filemgr: {error}");
    }
}

fn pane_border_token(pane: PaneId, active: PaneId) -> bevy::feathers::theme::ThemeToken {
    if pane == active {
        tokens::TEXT_DIM
    } else {
        tokens::BORDER
    }
}

fn ui_text(commands: &mut Commands, value: &str, size: f32, dim: bool) -> Entity {
    commands
        .spawn((
            Text::new(value.to_string()),
            TextFont::from_font_size(size),
            ThemeTextColor(if dim { tokens::TEXT_DIM } else { tokens::TEXT }),
            TextLayout::no_wrap(),
        ))
        .id()
}

fn home_directory() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn configured_directory(value: &str, fallback: &Path) -> PathBuf {
    let configured = PathBuf::from(value);
    if configured.is_dir() {
        configured
    } else {
        fallback.to_path_buf()
    }
}

fn filesystem_root(path: &Path) -> PathBuf {
    path.ancestors()
        .last()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("/"))
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

fn format_child_count(count: Option<usize>) -> String {
    count
        .map(|count| format!("{count} {}", if count == 1 { "item" } else { "items" }))
        .unwrap_or_default()
}

fn format_file_info(row: &FileRow, now: SystemTime) -> String {
    let (quantity_label, quantity) = if row.is_dir {
        (
            "Contents",
            row.child_count
                .map(|count| format!("{count} {}", if count == 1 { "item" } else { "items" }))
                .unwrap_or_else(|| "—".into()),
        )
    } else {
        (
            "Size",
            row.size.map(format_size).unwrap_or_else(|| "—".into()),
        )
    };
    format!(
        "{}\nType: {}\n{quantity_label}: {quantity}\nModified: {}\n\n{}",
        row.name,
        if row.is_dir { "Folder" } else { "File" },
        row.modified
            .map(|modified| format_modified_at(modified, now))
            .unwrap_or_else(|| "—".into()),
        sanitise_display_path(&row.path)
    )
}

fn pane_summary(entries: &[FileEntry]) -> String {
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

fn format_modified_at(modified: SystemTime, now: SystemTime) -> String {
    format_modified_at_with(modified, now, |modified| {
        format_absolute_system_time(modified).unwrap_or_else(|| "—".into())
    })
}

fn format_absolute_system_time(modified: SystemTime) -> Option<String> {
    let utc = system_time_to_utc(modified)?;
    Some(format_absolute_datetime(utc.with_timezone(&Local)))
}

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

fn format_absolute_datetime<Tz>(modified: DateTime<Tz>) -> String
where
    Tz: chrono::TimeZone,
    Tz::Offset: std::fmt::Display,
{
    modified.format("%d/%m/%y at %-I:%M %P").to_string()
}

fn format_modified_at_with(
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

#[cfg(test)]
mod tests {
    use super::*;

    fn layout_test_app(scale_factor: f32) -> App {
        use bevy::camera::{CameraPlugin, ComputedCameraValues, RenderTargetInfo, Viewport};
        use bevy::image::TextureAtlasPlugin;
        use bevy::input::InputPlugin;
        use bevy::render::mesh::MeshPlugin;
        use bevy::text::TextPlugin;
        use bevy::ui::UiPlugin;
        use bevy::{
            asset::AssetPlugin, image::ImagePlugin, picking::PickingPlugin,
            transform::TransformPlugin,
        };

        const LOGICAL_WIDTH: f32 = 400.0;
        const LOGICAL_HEIGHT: f32 = 100.0;
        let target_width = (LOGICAL_WIDTH * scale_factor) as u32;
        let target_height = (LOGICAL_HEIGHT * scale_factor) as u32;
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_plugins(AssetPlugin::default())
            .add_plugins(TransformPlugin)
            .add_plugins(CameraPlugin)
            .add_plugins(ImagePlugin::default())
            .add_plugins(TextureAtlasPlugin)
            .add_plugins(MeshPlugin)
            .add_plugins(InputPlugin)
            .add_plugins(PickingPlugin)
            .add_plugins(TextPlugin)
            .add_plugins(UiPlugin);
        app.world_mut().spawn((
            Camera2d,
            Camera {
                computed: ComputedCameraValues {
                    target_info: Some(RenderTargetInfo {
                        physical_size: UVec2::new(target_width, target_height),
                        scale_factor,
                    }),
                    ..default()
                },
                viewport: Some(Viewport {
                    physical_size: UVec2::new(target_width, target_height),
                    ..default()
                }),
                ..default()
            },
        ));
        app.finish();
        app.cleanup();
        app
    }

    #[test]
    fn path_input_is_one_line_tall_and_tracks_the_ui_font() {
        use bevy::ecs::world::CommandQueue;
        use bevy::text::TextLayoutInfo;

        // Padding (4 top + 4 bottom) and border (1 + 1), authored in `path_input`.
        const CHROME_PX: f32 = 10.0;

        // 13.0 is what `path_input` authors; 18.0 stands in for an operator raising
        // the theme's `body_px`, which ctk reconciles by rewriting `TextFont` in
        // place. The box has to follow the font — that is what a fixed height
        // (`height: px(28)`) gets wrong, and it is invisible at 13.0 alone.
        for (scale_factor, font_px) in [
            (1.0_f32, 13.0_f32),
            (1.5, 13.0),
            (2.0, 13.0),
            (1.0, 18.0),
            (1.5, 18.0),
            (2.0, 18.0),
        ] {
            let mut app = layout_test_app(scale_factor);
            let mut queue = CommandQueue::default();
            let input = {
                let mut commands = Commands::new(&mut queue, app.world());
                path_input(&mut commands, PaneId::Left, Path::new("/a/path"))
            };
            queue.apply(app.world_mut());
            app.world_mut()
                .entity_mut(input)
                .insert(TextFont::from_font_size(font_px));
            let header = app
                .world_mut()
                .spawn(Node {
                    width: percent(100),
                    min_height: px(38),
                    align_items: AlignItems::Center,
                    padding: UiRect::axes(px(10), px(5)),
                    ..default()
                })
                .add_child(input)
                .id();
            app.world_mut()
                .spawn(Node {
                    width: px(400),
                    height: px(100),
                    flex_direction: FlexDirection::Column,
                    ..default()
                })
                .add_child(header);

            app.world_mut().run_schedule(PostUpdate);

            let world = app.world();
            let node = world.get::<ComputedNode>(input).unwrap();
            let layout = world.get::<TextLayoutInfo>(input).unwrap();

            // `LineHeight::default()` is `RelativeToFont(1.2)`, and bevy's
            // `TextInputMeasure` ceils its result.
            let one_line = (1.2 * font_px * scale_factor).ceil();
            assert_eq!(
                node.content_box().height(),
                one_line,
                "scale {scale_factor} font {font_px}: the content box is exactly one line"
            );
            assert_eq!(
                node.size().y,
                one_line + CHROME_PX * scale_factor,
                "scale {scale_factor} font {font_px}: the border box is that line plus chrome"
            );
            assert!(
                layout.size.y <= node.content_box().height(),
                "scale {scale_factor} font {font_px}: the laid-out text fits without clipping"
            );
            assert!(
                world.get::<ComputedNode>(header).unwrap().size().y >= node.size().y,
                "scale {scale_factor} font {font_px}: the header absorbs the input"
            );
            // Not `Overflow::clip()`: the renderer already clips any `TextScroll`
            // entity, so a clip here would only hide a sizing regression like the one
            // this test exists to catch.
            assert_eq!(
                world.get::<Node>(input).unwrap().overflow,
                bevy::ui::Overflow::visible()
            );
            let editable = world.get::<EditableText>(input).unwrap();
            assert_eq!(editable.visible_lines, Some(1.0));
            assert!(!editable.allow_newlines);
            assert!(world.get::<CtkTextInputFocusBorder>(input).is_some());
            assert!(world.get::<ThemeBorderColor>(input).is_none());
        }
    }

    #[test]
    fn selected_row_paints_every_foreground_and_restores_each_resting_token() {
        use bevy::ecs::world::CommandQueue;

        let panel = Color::srgb(0.02, 0.03, 0.04);
        let text = Color::srgb(0.9, 0.91, 0.92);
        let text_dim = Color::srgb(0.5, 0.51, 0.52);
        let active = Color::srgb(0.2, 0.7, 0.8);
        let selected_background = Color::srgb(0.6, 0.61, 0.62);
        // Distinct from `panel`: the selected foreground is its own contrast-
        // checked token now, so a test that reused the panel colour could not
        // tell the two apart.
        let selected_text = Color::srgb(0.99, 0.98, 0.97);
        let selected_text_dim = Color::srgb(0.72, 0.71, 0.70);
        let changed_selected_text = Color::srgb(0.08, 0.09, 0.1);
        let changed_selected_text_dim = Color::srgb(0.22, 0.23, 0.24);
        let changed_dim = Color::srgb(0.4, 0.41, 0.42);
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_plugins(AssetPlugin::default())
            .init_asset::<ctk::icons::SvgFile>();
        let mut theme = UiTheme::default();
        theme.set_color("ctk.panel", panel);
        theme.set_color("ctk.text", text);
        theme.set_color("ctk.text.dim", text_dim);
        theme.set_color("ctk.control.active", active);
        theme.set_color("ctk.row.selected", selected_background);
        theme.set_color("ctk.row.selected.text", selected_text);
        theme.set_color("ctk.row.selected.text.dim", selected_text_dim);
        theme.set_color("ctk.row.hover", Color::NONE);
        theme.set_color("ctk.meter.amber", Color::NONE);
        let icons = IconSet::load(app.world().resource::<AssetServer>());
        let tree = app.world_mut().spawn_empty().id();
        let folder_path = PathBuf::from("/fixture/folder");
        let file_path = PathBuf::from("/fixture/file.txt");
        let (folder_row, file_row) = {
            let mut queue = CommandQueue::default();
            let rows = {
                let mut commands = Commands::new(&mut queue, app.world());
                (
                    spawn_file_row(
                        &mut commands,
                        &icons,
                        &theme,
                        PaneId::Left,
                        FileEntry {
                            path: folder_path.clone(),
                            name: "folder".into(),
                            is_dir: true,
                            size: None,
                            child_count: Some(3),
                            modified: Some(UNIX_EPOCH),
                        },
                        0,
                        tree,
                        false,
                        1,
                    ),
                    spawn_file_row(
                        &mut commands,
                        &icons,
                        &theme,
                        PaneId::Left,
                        FileEntry {
                            path: file_path.clone(),
                            name: "file.txt".into(),
                            is_dir: false,
                            size: Some(42),
                            child_count: None,
                            modified: Some(UNIX_EPOCH),
                        },
                        0,
                        tree,
                        false,
                        1,
                    ),
                )
            };
            queue.apply(app.world_mut());
            rows
        };
        let placeholder = Entity::PLACEHOLDER;
        app.insert_resource(theme)
            .insert_resource(BrowserState {
                panes: [
                    pane_fixture(vec![folder_row, file_row], Some(folder_path.clone()), false),
                    pane_fixture(Vec::new(), None, false),
                ],
                active: PaneId::Left,
                info_text: placeholder,
                shell: placeholder,
                split: placeholder,
            })
            .init_resource::<FileActionState>()
            .init_resource::<FileDndState>()
            .add_systems(Update, paint_browser);

        let mut text_query = app.world_mut().query::<(Entity, &SelectedRowTextColor)>();
        let text_entities = text_query
            .iter(app.world())
            .map(|(entity, managed)| {
                (
                    entity,
                    managed.row,
                    managed.resting.clone(),
                    managed.selected.clone(),
                )
            })
            .collect::<Vec<_>>();
        let mut icon_query = app.world_mut().query::<(Entity, &SelectedRowSvgColor)>();
        let icon_entities = icon_query
            .iter(app.world())
            .map(|(entity, managed)| (entity, managed.row, managed.resting.clone()))
            .collect::<Vec<_>>();
        assert_eq!(text_entities.len(), 6);
        assert_eq!(icon_entities.len(), 4);
        assert_eq!(
            text_entities
                .iter()
                .filter(|(_, _, token, _)| token == &tokens::TEXT)
                .count(),
            2
        );
        assert_eq!(
            text_entities
                .iter()
                .filter(|(_, _, token, _)| token == &tokens::TEXT_DIM)
                .count(),
            4
        );
        assert_eq!(
            text_entities
                .iter()
                .filter(|(_, _, _, token)| token == &tokens::ROW_SELECTED_TEXT)
                .count(),
            2
        );
        assert_eq!(
            text_entities
                .iter()
                .filter(|(_, _, _, token)| token == &tokens::ROW_SELECTED_TEXT_DIM)
                .count(),
            4
        );
        assert_eq!(
            icon_entities
                .iter()
                .filter(|(_, _, token)| token == &tokens::TEXT_DIM)
                .count(),
            3
        );
        assert_eq!(
            icon_entities
                .iter()
                .filter(|(_, _, token)| token == &tokens::CONTROL_ACTIVE)
                .count(),
            1
        );
        assert_eq!(
            text_entities
                .iter()
                .filter(|(entity, _, _, _)| app.world().get::<MiddleElideText>(*entity).is_some())
                .count(),
            2
        );
        for (entity, _, _, _) in &text_entities {
            assert!(app.world().get::<ThemeTextColor>(*entity).is_none());
        }
        for (entity, _, _) in &icon_entities {
            assert!(app.world().get::<ThemeSvgColor>(*entity).is_none());
        }

        app.update();
        assert_eq!(
            app.world().get::<BackgroundColor>(folder_row).unwrap().0,
            selected_background
        );
        for (entity, row, resting, selected) in &text_entities {
            let want = if *row == folder_row {
                app.world().resource::<UiTheme>().color(selected)
            } else {
                app.world().resource::<UiTheme>().color(resting)
            };
            assert_eq!(app.world().get::<TextColor>(*entity).unwrap().0, want);
        }
        for (entity, row, token) in &icon_entities {
            let want = if *row == folder_row {
                selected_text
            } else {
                app.world().resource::<UiTheme>().color(token)
            };
            assert_eq!(app.world().get::<SvgColor>(*entity).unwrap().0, want);
        }

        app.world_mut().resource_mut::<BrowserState>().panes[0].selected = Some(file_path.clone());
        app.update();
        assert_eq!(
            app.world().get::<BackgroundColor>(file_row).unwrap().0,
            selected_background
        );
        for (entity, row, resting, selected) in &text_entities {
            let want = if *row == file_row {
                app.world().resource::<UiTheme>().color(selected)
            } else {
                app.world().resource::<UiTheme>().color(resting)
            };
            assert_eq!(app.world().get::<TextColor>(*entity).unwrap().0, want);
        }
        for (entity, row, token) in &icon_entities {
            let want = if *row == file_row {
                selected_text
            } else {
                app.world().resource::<UiTheme>().color(token)
            };
            assert_eq!(app.world().get::<SvgColor>(*entity).unwrap().0, want);
        }

        app.world_mut()
            .resource_mut::<UiTheme>()
            .set_color("ctk.row.selected.text", changed_selected_text);
        app.world_mut()
            .resource_mut::<UiTheme>()
            .set_color("ctk.row.selected.text.dim", changed_selected_text_dim);
        app.update();
        for (entity, row, resting, selected) in &text_entities {
            let want = if *row == file_row {
                app.world().resource::<UiTheme>().color(selected)
            } else {
                app.world().resource::<UiTheme>().color(resting)
            };
            assert_eq!(app.world().get::<TextColor>(*entity).unwrap().0, want);
        }
        for (entity, row, token) in &icon_entities {
            let want = if *row == file_row {
                changed_selected_text
            } else {
                app.world().resource::<UiTheme>().color(token)
            };
            assert_eq!(app.world().get::<SvgColor>(*entity).unwrap().0, want);
        }

        app.world_mut().resource_mut::<BrowserState>().panes[0].selected = None;
        app.world_mut()
            .resource_mut::<UiTheme>()
            .set_color("ctk.text.dim", changed_dim);
        app.update();
        for (entity, _, resting, _) in &text_entities {
            assert_eq!(
                app.world().get::<TextColor>(*entity).unwrap().0,
                app.world().resource::<UiTheme>().color(resting)
            );
        }
        for (entity, _, token) in &icon_entities {
            assert_eq!(
                app.world().get::<SvgColor>(*entity).unwrap().0,
                app.world().resource::<UiTheme>().color(token)
            );
        }
    }

    /// Rows are spawned through deferred `Commands`, so a relist that retains the
    /// selection must not leave the reborn row unpainted for a frame. This drives
    /// the real `receive_listings` → `paint_browser` pair in one schedule and
    /// asserts the paint lands in the *same* `update()` that spawned the rows.
    #[test]
    fn a_relisted_row_is_painted_in_the_frame_its_rebuild_lands() {
        let panel = Color::srgb(0.02, 0.03, 0.04);
        let selected_background = Color::srgb(0.6, 0.61, 0.62);
        let selected_text = Color::srgb(0.99, 0.98, 0.97);
        let selected_text_dim = Color::srgb(0.72, 0.71, 0.70);
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_plugins(AssetPlugin::default())
            .init_asset::<ctk::icons::SvgFile>();
        let mut theme = UiTheme::default();
        theme.set_color("ctk.panel", panel);
        theme.set_color("ctk.text", Color::srgb(0.9, 0.91, 0.92));
        theme.set_color("ctk.text.dim", Color::srgb(0.5, 0.51, 0.52));
        theme.set_color("ctk.control.active", Color::srgb(0.2, 0.7, 0.8));
        theme.set_color("ctk.row.selected", selected_background);
        theme.set_color("ctk.row.selected.text", selected_text);
        theme.set_color("ctk.row.selected.text.dim", selected_text_dim);
        theme.set_color("ctk.row.hover", Color::NONE);
        theme.set_color("ctk.meter.amber", Color::NONE);
        let icons = IconSet::load(app.world().resource::<AssetServer>());

        let (tx, rx) = mpsc::channel();
        let list = app.world_mut().spawn_empty().id();
        let placeholder = Entity::PLACEHOLDER;
        let selected_path = PathBuf::from("/fixture/kept.txt");
        let mut pane = pane_fixture(Vec::new(), Some(selected_path.clone()), true);
        pane.list = list;

        app.insert_resource(theme)
            .insert_resource(ListingInbox {
                tx: tx.clone(),
                rx: Mutex::new(rx),
                nonce: AtomicU64::new(0),
                generations: [Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0))],
            })
            .insert_resource(BrowserState {
                panes: [pane, pane_fixture(Vec::new(), None, false)],
                active: PaneId::Left,
                info_text: placeholder,
                shell: placeholder,
                split: placeholder,
            })
            .insert_resource(icons)
            .init_resource::<DirectoryCountWork>()
            .init_resource::<FileActionState>()
            .init_resource::<FileDndState>()
            // The same constraint the plugin applies. `route_browser_actions` is
            // omitted only because it needs the action-message plumbing; the
            // mechanism under test is the sync point a `.after()` inserts between
            // a `Commands`-producing system and the painter, and `receive_listings`
            // exercises it identically.
            .add_systems(Update, paint_browser.after(receive_listings))
            .add_systems(Update, receive_listings);

        tx.send(ListingReply {
            pane: PaneId::Left,
            generation: 0,
            path: PathBuf::from("/fixture"),
            root: true,
            result: Ok(vec![FileEntry {
                path: selected_path.clone(),
                name: "kept.txt".into(),
                is_dir: false,
                size: Some(42),
                child_count: None,
                modified: Some(UNIX_EPOCH),
            }]),
        })
        .expect("listing inbox closed");

        app.update();

        let rows = app.world().resource::<BrowserState>().panes[0].rows.clone();
        assert_eq!(rows.len(), 1, "the relist should have rebuilt one row");
        let row = rows[0];
        assert_eq!(
            app.world().get::<BackgroundColor>(row).unwrap().0,
            selected_background,
            "the retained selection must be painted in the frame the row was rebuilt"
        );

        let mut text_query = app.world_mut().query::<(Entity, &SelectedRowTextColor)>();
        let texts = text_query
            .iter(app.world())
            .filter(|(_, managed)| managed.row == row)
            .map(|(entity, managed)| (entity, managed.selected.clone()))
            .collect::<Vec<_>>();
        let mut icon_query = app.world_mut().query::<(Entity, &SelectedRowSvgColor)>();
        let svgs = icon_query
            .iter(app.world())
            .filter(|(_, managed)| managed.row == row)
            .map(|(entity, _)| entity)
            .collect::<Vec<_>>();
        assert!(!texts.is_empty() && !svgs.is_empty());
        assert_eq!(
            texts
                .iter()
                .filter(|(_, token)| token == &tokens::ROW_SELECTED_TEXT)
                .count(),
            1,
            "the relisted name must use the main knockout token"
        );
        assert_eq!(
            texts
                .iter()
                .filter(|(_, token)| token == &tokens::ROW_SELECTED_TEXT_DIM)
                .count(),
            2,
            "the relisted size and modified columns must use the dim knockout token"
        );
        for (entity, token) in texts {
            assert_eq!(
                app.world().get::<TextColor>(entity).unwrap().0,
                app.world().resource::<UiTheme>().color(&token)
            );
        }
        for entity in svgs {
            assert_eq!(
                app.world().get::<SvgColor>(entity).unwrap().0,
                selected_text
            );
        }
    }

    struct DropTestRoot(PathBuf);

    impl DropTestRoot {
        fn new(name: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "cosmix-filemgr-dnd-{name}-{}-{nonce}",
                std::process::id()
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for DropTestRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn drag_icon_scale_ceil_avoids_fractional_hidpi_upscaling() {
        assert_eq!(export_icon_buffer_scale(1.0), 1);
        assert_eq!(export_icon_buffer_scale(1.5), 2);
        assert_eq!(export_icon_buffer_scale(2.0), 2);
        assert_eq!(export_icon_buffer_scale(f32::NAN), 1);
    }

    #[test]
    fn active_pane_border_matches_dim_status_text() {
        assert_eq!(
            pane_border_token(PaneId::Left, PaneId::Left),
            tokens::TEXT_DIM
        );
        assert_eq!(
            pane_border_token(PaneId::Right, PaneId::Left),
            tokens::BORDER
        );
    }

    #[test]
    fn file_drop_rejects_same_directory() {
        let root = DropTestRoot::new("same-directory");
        let source = root.0.join("source.txt");
        std::fs::write(&source, b"source").unwrap();
        assert_eq!(file_drop_actions(&source, &root.0, false), ActionMask::NONE);
    }

    #[test]
    fn file_drop_rejects_directory_self_and_descendants() {
        let root = DropTestRoot::new("self-descendant");
        let source = root.0.join("source");
        let child = source.join("child");
        let destination = root.0.join("destination");
        std::fs::create_dir_all(&child).unwrap();
        std::fs::create_dir(&destination).unwrap();
        assert_eq!(file_drop_actions(&source, &source, false), ActionMask::NONE);
        assert_eq!(file_drop_actions(&source, &child, false), ActionMask::NONE);
        assert_eq!(
            file_drop_actions(&source, &destination, false),
            ActionMask::ALL
        );
    }

    #[test]
    fn file_drop_rejects_while_an_operation_is_pending() {
        let root = DropTestRoot::new("pending");
        let source = root.0.join("source.txt");
        let destination = root.0.join("destination");
        std::fs::write(&source, b"source").unwrap();
        std::fs::create_dir(&destination).unwrap();
        assert_eq!(
            file_drop_actions(&source, &destination, true),
            ActionMask::NONE
        );
    }

    #[cfg(unix)]
    #[test]
    fn file_drop_containment_resolves_dotdot_and_symlink_identity() {
        use std::os::unix::fs::symlink;

        let root = DropTestRoot::new("canonical-containment");
        let source = root.0.join("source");
        let child = source.join("child");
        let elsewhere = root.0.join("elsewhere");
        let other = root.0.join("other");
        std::fs::create_dir_all(&child).unwrap();
        std::fs::create_dir(&elsewhere).unwrap();
        std::fs::create_dir(&other).unwrap();

        let dotdot_inside = other.join("..").join("source").join("child");
        assert_eq!(
            file_drop_actions(&source, &dotdot_inside, false),
            ActionMask::NONE
        );

        let lexical_descendant_but_distinct = source.join("..").join("elsewhere");
        assert_eq!(
            file_drop_actions(&source, &lexical_descendant_but_distinct, false),
            ActionMask::ALL
        );

        let alias_inside = root.0.join("alias-inside");
        symlink(&child, &alias_inside).unwrap();
        assert_eq!(
            file_drop_actions(&source, &alias_inside, false),
            ActionMask::NONE
        );

        let source_link = root.0.join("source-link");
        symlink(&source, &source_link).unwrap();
        assert_eq!(
            file_drop_actions(&source_link, &child, false),
            ActionMask::ALL,
            "a source symlink is moved/copied as a link, not as its directory target"
        );
    }

    #[test]
    fn file_drop_modifiers_map_to_kde_actions() {
        assert_eq!(requested_drop_action(Modifiers::default()), DropAction::Ask);
        assert_eq!(
            requested_drop_action(Modifiers {
                control: true,
                ..default()
            }),
            DropAction::Copy
        );
        assert_eq!(
            requested_drop_action(Modifiers {
                shift: true,
                ..default()
            }),
            DropAction::Move
        );
        assert_eq!(
            requested_drop_action(Modifiers {
                control: true,
                shift: true,
                ..default()
            }),
            DropAction::Copy
        );
    }

    #[test]
    fn commit_trusts_any_action_in_the_negotiated_mask() {
        let root = DropTestRoot::new("negotiated-action");
        let source = root.0.join("source.txt");
        let destination = root.0.join("destination");
        std::fs::write(&source, b"source").unwrap();
        std::fs::create_dir(&destination).unwrap();

        let allowed = file_drop_actions(&source, &destination, false);
        assert_eq!(requested_drop_action(Modifiers::default()), DropAction::Ask);
        assert!(allowed.contains(DropAction::Move));
    }

    #[test]
    fn drag_start_on_filename_text_arms_the_row_source() {
        use bevy::camera::NormalizedRenderTarget;
        use bevy::picking::backend::HitData;
        use bevy::picking::events::DragStart;
        use bevy::picking::pointer::{Location, PointerId};
        use bevy::window::WindowRef;

        #[derive(Resource, Default)]
        struct SeenDragTargets(Vec<Entity>);

        let mut app = App::new();
        app.add_plugins(DndPlugin)
            .init_resource::<SeenDragTargets>()
            .add_observer(
                |drag: On<Pointer<DragStart>>, mut seen: ResMut<SeenDragTargets>| {
                    seen.0.push(drag.entity);
                },
            );
        let window = app.world_mut().spawn(Window::default()).id();
        let row = app
            .world_mut()
            .spawn(DragSource::new(
                DragPayload::Paths(vec![PathBuf::from("/source")]),
                GhostBuilder::empty(),
            ))
            .id();
        let content = app.world_mut().spawn(Pickable::IGNORE).id();
        let cell = app.world_mut().spawn(Pickable::IGNORE).id();
        let filename = app
            .world_mut()
            .spawn((Text::new("source"), Pickable::IGNORE))
            .id();
        app.world_mut().entity_mut(row).add_child(content);
        app.world_mut().entity_mut(content).add_child(cell);
        app.world_mut().entity_mut(cell).add_child(filename);
        app.world_mut().flush();
        assert_eq!(
            app.world().get::<ChildOf>(filename).map(ChildOf::parent),
            Some(cell)
        );
        assert_eq!(
            app.world().get::<ChildOf>(cell).map(ChildOf::parent),
            Some(content)
        );
        assert_eq!(
            app.world().get::<ChildOf>(content).map(ChildOf::parent),
            Some(row)
        );

        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            Location {
                target: NormalizedRenderTarget::Window(
                    WindowRef::Entity(window).normalize(None).unwrap(),
                ),
                position: Vec2::ZERO,
            },
            DragStart {
                button: PointerButton::Primary,
                hit: HitData::new(Entity::PLACEHOLDER, 0.0, None, None),
            },
            filename,
        ));

        assert_eq!(
            app.world().resource::<SeenDragTargets>().0,
            vec![filename, cell, content, row, window]
        );
        let session = app.world().resource::<DragSession>();
        assert_eq!(session.phase(), Some(DragPhase::Armed));
        assert_eq!(session.source(), Some(row));
    }

    fn dnd_delivery_id_fixture() -> DeliveryId {
        use bevy::camera::NormalizedRenderTarget;
        use bevy::picking::backend::HitData;
        use bevy::picking::events::{Drag, DragDrop, DragEnd, DragStart};
        use bevy::picking::hover::HoverMap;
        use bevy::picking::pointer::{Location, PointerId};
        use bevy::window::WindowRef;

        fn accept(
            mut proposals: MessageReader<AcceptanceProposal>,
            mut acceptances: MessageWriter<DropAcceptance>,
        ) {
            for proposal in proposals.read() {
                acceptances.write(DropAcceptance {
                    proposal_id: proposal.proposal_id,
                    revision: proposal.revision,
                    allowed_actions: ActionMask::MOVE,
                    preferred: DropAction::Move,
                });
            }
        }

        let mut app = App::new();
        app.add_plugins(DndPlugin)
            .add_systems(Update, accept.in_set(AppResolve));
        let window = app.world_mut().spawn_empty().id();
        let source = app
            .world_mut()
            .spawn(DragSource::new(
                DragPayload::Paths(vec![PathBuf::from("/source")]),
                GhostBuilder::empty(),
            ))
            .id();
        let target = app.world_mut().spawn(DropTarget).id();
        let location = |position| Location {
            target: NormalizedRenderTarget::Window(
                WindowRef::Entity(window).normalize(None).unwrap(),
            ),
            position,
        };
        let hit = || HitData::new(Entity::PLACEHOLDER, 0.0, None, None);

        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            location(Vec2::ZERO),
            DragStart {
                button: PointerButton::Primary,
                hit: hit(),
            },
            source,
        ));
        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            location(Vec2::new(DRAG_THRESHOLD_PX, 0.0)),
            Drag {
                button: PointerButton::Primary,
                distance: Vec2::new(DRAG_THRESHOLD_PX, 0.0),
                delta: Vec2::new(DRAG_THRESHOLD_PX, 0.0),
            },
            source,
        ));
        app.world_mut().flush();
        app.world_mut()
            .resource_mut::<HoverMap>()
            .entry(PointerId::Mouse)
            .or_default()
            .insert(target, hit());
        app.update();

        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            location(Vec2::new(DRAG_THRESHOLD_PX, 0.0)),
            DragDrop {
                button: PointerButton::Primary,
                dropped: source,
                hit: hit(),
            },
            target,
        ));
        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            location(Vec2::new(DRAG_THRESHOLD_PX, 0.0)),
            DragEnd {
                button: PointerButton::Primary,
                distance: Vec2::new(DRAG_THRESHOLD_PX, 0.0),
            },
            source,
        ));
        app.update();

        let drops = app.world().resource::<Messages<DndDrop>>();
        let mut cursor = drops.get_cursor();
        cursor.read(drops).last().unwrap().delivery_id
    }

    #[test]
    fn drop_completion_keeps_delivery_id_until_real_operation_outcome() {
        let delivery_id = dnd_delivery_id_fixture();
        let operation =
            FileOperation::move_to(PathBuf::from("/source"), PathBuf::from("/destination"));
        let success = OperationReply {
            operation: operation.clone(),
            source_pane: PaneId::Left,
            drop_delivery: Some(DropDelivery {
                delivery_id,
                action: DropAction::Move,
            }),
            result: Ok("moved".into()),
        };
        assert_eq!(
            operation_drop_completion(&success),
            Some(DropComplete {
                delivery_id,
                outcome: DropOutcome::Completed(DropAction::Move),
            })
        );

        let failed = OperationReply {
            operation,
            source_pane: PaneId::Left,
            drop_delivery: Some(DropDelivery {
                delivery_id,
                action: DropAction::Move,
            }),
            result: Err("failed".into()),
        };
        assert_eq!(
            operation_drop_completion(&failed),
            Some(DropComplete {
                delivery_id,
                outcome: DropOutcome::Failed,
            })
        );
    }

    #[test]
    fn invalid_drop_confirmation_reservation_fails_exactly_once() {
        let delivery_id = dnd_delivery_id_fixture();
        let interaction_id = InteractionId::next();
        let (operation_tx, operation_rx) = mpsc::channel();
        let placeholder = Entity::PLACEHOLDER;
        let mut app = App::new();
        app.add_message::<InteractionResult>()
            .add_message::<DropComplete>()
            .add_message::<DropDecision>()
            .insert_resource(OperationInbox {
                tx: operation_tx,
                rx: Mutex::new(operation_rx),
            })
            .insert_resource(BrowserState {
                panes: [
                    pane_fixture(Vec::new(), None, false),
                    pane_fixture(Vec::new(), None, false),
                ],
                active: PaneId::Left,
                info_text: placeholder,
                shell: placeholder,
                split: placeholder,
            })
            .insert_resource(FileActionState {
                drop_confirm: Some(DropConfirmReservation {
                    interaction_id,
                    sources: vec![PathBuf::from("/source")],
                    destination: PathBuf::from("/destination"),
                    source_pane: PaneId::Left,
                    delivery_id: Some(delivery_id),
                    wayland_decision_required: false,
                }),
                // Model an invalid reservation: another operation somehow
                // acquired the running slot before the dialog result.
                pending: true,
                ..default()
            })
            .add_systems(Update, on_interaction_result);
        for _ in 0..2 {
            app.world_mut().write_message(InteractionResult {
                id: interaction_id,
                outcome: InteractionOutcome::Action("copy".into()),
            });
        }

        app.update();

        let completions = app.world().resource::<Messages<DropComplete>>();
        let mut cursor = completions.get_cursor();
        assert_eq!(
            cursor.read(completions).copied().collect::<Vec<_>>(),
            vec![DropComplete {
                delivery_id,
                outcome: DropOutcome::Failed,
            }]
        );
        assert!(app
            .world()
            .resource::<FileActionState>()
            .drop_confirm
            .is_none());
    }

    #[test]
    fn dead_delivery_with_open_confirmation_withdraws_without_operation_or_completion() {
        use bevy::app::TaskPoolPlugin;

        let delivery_id = dnd_delivery_id_fixture();
        let request = InteractionRequest::confirm("Drop", "Confirm transfer")
            .action(InteractionAction::new(
                "cancel",
                "Cancel",
                ActionRole::Cancel,
            ))
            .action(InteractionAction::new(
                "move",
                "Move",
                ActionRole::Auxiliary,
            ));
        let interaction_id = request.id();
        let mut app = App::new();
        app.add_plugins(TaskPoolPlugin::default())
            .init_resource::<ButtonInput<KeyCode>>()
            .add_plugins(ctk::interaction::InteractionPlugin)
            .add_message::<DndDeliveryCancelled>()
            .add_message::<DropComplete>()
            .insert_resource(FileActionState {
                drop_confirm: Some(DropConfirmReservation {
                    interaction_id,
                    sources: vec![PathBuf::from("/source")],
                    destination: PathBuf::from("/destination"),
                    source_pane: PaneId::Left,
                    delivery_id: Some(delivery_id),
                    wayland_decision_required: true,
                }),
                ..default()
            })
            .add_systems(Update, cancel_dnd_delivery.before(InteractionSystems));
        app.finish();
        app.cleanup();

        app.world_mut().write_message(request);
        app.update();
        assert!(app.world().resource::<ModalCoordinator>().is_active());

        app.world_mut()
            .write_message(DndDeliveryCancelled { delivery_id });
        app.update();

        let file_actions = app.world().resource::<FileActionState>();
        assert!(file_actions.drop_confirm.is_none());
        assert!(!file_actions.pending);
        assert!(file_actions.pending_drop_decision.is_none());
        assert!(!app.world().resource::<ModalCoordinator>().is_active());
        let completions = app.world().resource::<Messages<DropComplete>>();
        let mut cursor = completions.get_cursor();
        assert_eq!(cursor.read(completions).count(), 0);
    }

    #[test]
    fn wayland_ask_operation_waits_for_bridge_decision_acceptance() {
        use bevy::app::TaskPoolPlugin;

        let delivery_id = dnd_delivery_id_fixture();
        let interaction_id = InteractionId::next();
        let (operation_tx, operation_rx) = mpsc::channel();
        let mut app = App::new();
        app.add_plugins(TaskPoolPlugin::default());
        let left_status = app.world_mut().spawn(StatusText::new("Ready")).id();
        let right_status = app.world_mut().spawn(StatusText::new("Ready")).id();
        let mut left = pane_fixture(Vec::new(), None, false);
        left.status_text = left_status;
        let mut right = pane_fixture(Vec::new(), None, false);
        right.status_text = right_status;
        app.add_message::<InteractionResult>()
            .add_message::<DropDecision>()
            .add_message::<DropDecisionResult>()
            .add_message::<DropComplete>()
            .insert_resource(OperationInbox {
                tx: operation_tx,
                rx: Mutex::new(operation_rx),
            })
            .insert_resource(BrowserState {
                panes: [left, right],
                active: PaneId::Left,
                info_text: Entity::PLACEHOLDER,
                shell: Entity::PLACEHOLDER,
                split: Entity::PLACEHOLDER,
            })
            .insert_resource(FileActionState {
                drop_confirm: Some(DropConfirmReservation {
                    interaction_id,
                    sources: vec![PathBuf::from("/definitely-missing-ctk-dnd-source")],
                    destination: PathBuf::from("/destination"),
                    source_pane: PaneId::Left,
                    delivery_id: Some(delivery_id),
                    wayland_decision_required: true,
                }),
                ..default()
            })
            .add_systems(
                Update,
                (apply_drop_decision_results, on_interaction_result).chain(),
            );
        app.finish();
        app.cleanup();

        app.world_mut().write_message(InteractionResult {
            id: interaction_id,
            outcome: InteractionOutcome::Action("move".into()),
        });
        app.update();

        let file_actions = app.world().resource::<FileActionState>();
        assert!(!file_actions.pending);
        assert!(file_actions.pending_drop_decision.is_some());
        let decisions = app.world().resource::<Messages<DropDecision>>();
        let mut decision_cursor = decisions.get_cursor();
        assert_eq!(
            decision_cursor.read(decisions).copied().collect::<Vec<_>>(),
            vec![DropDecision {
                delivery_id,
                decision: DropDecisionKind::Move,
            }]
        );

        app.world_mut().write_message(DropDecisionResult {
            delivery_id,
            status: DropDecisionStatus::Rejected(
                "FinalActionNotOffered { action: Move, source_actions: COPY | ASK }".into(),
            ),
        });
        app.update();

        assert!(!app.world().resource::<FileActionState>().pending);
        assert!(app
            .world()
            .get::<StatusText>(left_status)
            .is_some_and(|status| status.0.contains("FinalActionNotOffered")));
        let completions = app.world().resource::<Messages<DropComplete>>();
        let mut cursor = completions.get_cursor();
        assert_eq!(cursor.read(completions).count(), 0);

        let accepted_interaction_id = InteractionId::next();
        app.world_mut()
            .resource_mut::<FileActionState>()
            .drop_confirm = Some(DropConfirmReservation {
            interaction_id: accepted_interaction_id,
            sources: vec![PathBuf::from("/definitely-missing-ctk-dnd-source")],
            destination: PathBuf::from("/destination"),
            source_pane: PaneId::Left,
            delivery_id: Some(delivery_id),
            wayland_decision_required: true,
        });
        app.world_mut().write_message(InteractionResult {
            id: accepted_interaction_id,
            outcome: InteractionOutcome::Action("move".into()),
        });
        app.update();
        assert!(!app.world().resource::<FileActionState>().pending);

        app.world_mut().write_message(DropDecisionResult {
            delivery_id,
            status: DropDecisionStatus::Accepted,
        });
        app.update();

        assert!(app.world().resource::<FileActionState>().pending);
        assert!(app
            .world()
            .resource::<FileActionState>()
            .pending_drop_decision
            .is_none());
    }

    fn pane_fixture(rows: Vec<Entity>, selected: Option<PathBuf>, listing: bool) -> PaneState {
        PaneState {
            path: PathBuf::from("/fixture"),
            generation: 0,
            list: Entity::PLACEHOLDER,
            path_input: Entity::PLACEHOLDER,
            status_text: Entity::PLACEHOLDER,
            rows,
            root_entries: Vec::new(),
            children: HashMap::new(),
            expanded: HashSet::new(),
            pending_children: HashSet::new(),
            pending_counts: 0,
            count_sort_dirty: false,
            selected,
            listing,
            history: NavigationHistory::default(),
            show_hidden: false,
            sort: SortColumn::Name,
            ascending: true,
        }
    }

    #[test]
    fn directory_sort_puts_folders_first_then_names_case_insensitively() {
        let mut entries = vec![
            FileEntry {
                path: "z".into(),
                name: "z".into(),
                is_dir: false,
                size: None,
                child_count: None,
                modified: None,
            },
            FileEntry {
                path: "B".into(),
                name: "B".into(),
                is_dir: true,
                size: None,
                child_count: None,
                modified: None,
            },
            FileEntry {
                path: "a".into(),
                name: "a".into(),
                is_dir: true,
                size: None,
                child_count: None,
                modified: None,
            },
        ];
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
            FileEntry {
                path: "small".into(),
                name: "small".into(),
                is_dir: false,
                size: Some(10),
                child_count: None,
                modified: None,
            },
            FileEntry {
                path: "folder".into(),
                name: "folder".into(),
                is_dir: true,
                size: None,
                child_count: Some(4),
                modified: None,
            },
            FileEntry {
                path: "large".into(),
                name: "large".into(),
                is_dir: false,
                size: Some(100),
                child_count: None,
                modified: None,
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
            FileEntry {
                path: "unknown".into(),
                name: "unknown".into(),
                is_dir: true,
                size: None,
                child_count: None,
                modified: None,
            },
            FileEntry {
                path: "large".into(),
                name: "large".into(),
                is_dir: true,
                size: None,
                child_count: Some(12),
                modified: None,
            },
            FileEntry {
                path: "small".into(),
                name: "small".into(),
                is_dir: true,
                size: None,
                child_count: Some(2),
                modified: None,
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
        let mut pane = pane_fixture(Vec::new(), None, false);
        pane.sort = SortColumn::Size;
        pane.root_entries = vec![
            FileEntry {
                path: "/fixture/growing".into(),
                name: "growing".into(),
                is_dir: true,
                size: None,
                child_count: Some(2),
                modified: None,
            },
            FileEntry {
                path: "/fixture/steady".into(),
                name: "steady".into(),
                is_dir: true,
                size: None,
                child_count: Some(10),
                modified: None,
            },
        ];
        sort_all_entries(&mut pane);
        assert_eq!(pane.root_entries[0].name, "growing");

        assert!(set_backing_child_count(
            &mut pane,
            Path::new("/fixture/growing"),
            Some(102)
        ));
        sort_all_entries(&mut pane);

        assert_eq!(pane.root_entries[0].name, "steady");
        assert!(!set_backing_child_count(
            &mut pane,
            Path::new("/fixture/growing"),
            Some(102)
        ));
    }

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
        let row = FileRow {
            pane: PaneId::Left,
            path: "/fixture/folder".into(),
            name: "folder".into(),
            is_dir: true,
            size: Some(4096),
            child_count: Some(15),
            modified: None,
            icon: Entity::PLACEHOLDER,
            size_text: Entity::PLACEHOLDER,
        };

        let info = format_file_info(&row, SystemTime::UNIX_EPOCH);
        assert!(info.contains("\nContents: 15 items\n"));
        assert!(!info.contains("\nSize: "));
    }

    #[test]
    fn information_panel_sanitises_controls_in_the_complete_display_path() {
        let row = FileRow {
            pane: PaneId::Left,
            path: "/fixture\nancestor/short\nname.md".into(),
            name: "short\u{fffd}name.md".into(),
            is_dir: false,
            size: Some(12),
            child_count: None,
            modified: None,
            icon: Entity::PLACEHOLDER,
            size_text: Entity::PLACEHOLDER,
        };

        let info = format_file_info(&row, SystemTime::UNIX_EPOCH);

        assert!(info.ends_with("/fixture\u{fffd}ancestor/short\u{fffd}name.md"));
        assert!(!info.contains("fixture\nancestor"));
        assert!(!info.contains("short\nname.md"));
        assert_eq!(info.matches('\n').count(), 5);
    }

    #[test]
    fn count_reply_repaints_information_only_for_the_active_selected_row() {
        let selected = Path::new("/fixture/folder");

        assert!(count_reply_repaints_information(
            true,
            Some(selected),
            selected
        ));
        assert!(!count_reply_repaints_information(
            false,
            Some(selected),
            selected
        ));
        assert!(!count_reply_repaints_information(
            true,
            Some(selected),
            Path::new("/fixture/other")
        ));
    }

    #[test]
    fn pane_summary_matches_dolphin_style_counts() {
        let entries = vec![
            FileEntry {
                path: "folder".into(),
                name: "folder".into(),
                is_dir: true,
                size: None,
                child_count: Some(2),
                modified: None,
            },
            FileEntry {
                path: "file".into(),
                name: "file".into(),
                is_dir: false,
                size: Some(1536),
                child_count: None,
                modified: None,
            },
        ];
        assert_eq!(pane_summary(&entries), "1 folder, 1 file (1.5 KiB)");
    }

    #[test]
    fn dotfiles_follow_the_per_pane_hidden_setting() {
        assert!(!entry_visible(".git", false));
        assert!(entry_visible(".git", true));
        assert!(entry_visible("music", false));
    }

    #[test]
    fn child_count_uses_the_listing_filter_and_honours_cancellation() {
        let root = DropTestRoot::new("child-count");
        std::fs::File::create(root.0.join("visible")).unwrap();
        std::fs::File::create(root.0.join(".hidden")).unwrap();

        assert_eq!(count_directory_entries(&root.0, false, || false), Some(1));
        assert_eq!(count_directory_entries(&root.0, true, || false), Some(2));
        assert_eq!(count_directory_entries(&root.0, true, || true), None);
    }

    #[test]
    fn a_new_listing_purges_only_that_panes_queued_counts() {
        let mut work = DirectoryCountWork::default();
        for (pane, generation) in [(PaneId::Left, 1), (PaneId::Right, 2), (PaneId::Left, 3)] {
            work.pending.push_back(DirectoryCountJob {
                pane,
                generation,
                entry_path: PathBuf::from(format!("/{generation}")),
                show_hidden: false,
            });
        }

        purge_pending_directory_counts(&mut work, PaneId::Left);

        assert_eq!(work.pending.len(), 1);
        assert_eq!(work.pending[0].pane, PaneId::Right);
    }

    #[test]
    fn read_directory_sanitises_display_controls_but_keeps_the_real_path() {
        let root = DropTestRoot::new("display-controls");
        let real_name = "short\nname\t.md";
        std::fs::File::create(root.0.join(real_name)).unwrap();

        let entries = read_directory(&root.0, true).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "short\u{fffd}name\u{fffd}.md");
        assert_eq!(
            entries[0].path.file_name().unwrap().to_string_lossy(),
            real_name
        );
    }

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

    #[test]
    fn stale_listing_requires_both_matching_generation_and_path() {
        let path = Path::new("/music");
        assert!(is_current_listing(4, path, 4, path));
        assert!(!is_current_listing(4, path, 3, path));
        assert!(!is_current_listing(4, path, 4, Path::new("/other")));
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
    fn toolbar_action_component_carries_canonical_id() {
        assert_eq!(
            ToolbarActionButton(action_ids::VIEW_REFRESH).0,
            action_ids::VIEW_REFRESH
        );
    }

    #[test]
    fn navigation_start_clears_rows_selection_and_loading_disables_actions() {
        use bevy::ecs::world::CommandQueue;

        let mut world = World::new();
        let old_row = world.spawn_empty().id();
        let mut pane = pane_fixture(vec![old_row], Some(PathBuf::from("/fixture/old")), false);
        assert!(pane.action_selection_available());
        assert!(pane.action_rows_available());

        let mut queue = CommandQueue::default();
        {
            let mut commands = Commands::new(&mut queue, &world);
            clear_pane_rows(&mut commands, &mut pane);
        }
        pane.listing = true;
        assert!(pane.rows.is_empty());
        assert!(pane.selected.is_none());
        assert!(!pane.action_selection_available());
        assert!(!pane.action_rows_available());
        queue.apply(&mut world);
        assert!(world.get_entity(old_row).is_err());
    }

    #[derive(Resource, Default)]
    struct SeenButtonActions(Vec<(ActionId, Source)>);

    fn collect_button_actions(
        mut requests: MessageReader<ActionRequest>,
        mut seen: ResMut<SeenButtonActions>,
    ) {
        seen.0.extend(
            requests
                .read()
                .map(|request| (request.action, request.source)),
        );
    }

    fn focused_button_app(captured: bool) -> (App, Entity, Entity) {
        use bevy::input_focus::{InputDispatchPlugin, InputFocusPlugin};
        use bevy::ui_widgets::{Button, ButtonPlugin};

        let mut app = App::new();
        app.add_plugins(bevy::input::InputPlugin)
            .add_message::<ActionRequest>()
            .add_plugins((InputFocusPlugin, InputDispatchPlugin, ButtonPlugin))
            .init_resource::<FileActionState>()
            .init_resource::<FocusedActivationOrigins>()
            .init_resource::<ModalCapture>()
            .init_resource::<SeenButtonActions>()
            .add_observer(focused_action_input_adapter)
            .add_observer(on_toolbar_action)
            .add_systems(First, clear_focused_activation_origins)
            .add_systems(Update, collect_button_actions);
        let window = app
            .world_mut()
            .spawn((Window::default(), PrimaryWindow))
            .id();
        let button = app
            .world_mut()
            .spawn((Button, ToolbarActionButton(action_ids::VIEW_REFRESH)))
            .id();
        app.world_mut()
            .resource_mut::<InputFocus>()
            .set(button, FocusCause::Navigated);
        if captured {
            app.world_mut().resource_mut::<ModalCapture>().acquire(
                ModalCaptureOwner {
                    kind: "test-modal",
                    entity: None,
                },
                ModalCaptureLayer(1),
            );
        }
        (app, window, button)
    }

    fn press_focused_key(app: &mut App, window: Entity, key_code: KeyCode) {
        use bevy::input::keyboard::{Key as LogicalKey, NativeKey};

        app.world_mut().write_message(KeyboardInput {
            key_code,
            logical_key: LogicalKey::Unidentified(NativeKey::Unidentified),
            state: ButtonState::Pressed,
            text: None,
            repeat: false,
            window,
        });
        app.update();
    }

    #[test]
    fn real_button_plugin_focused_activation_publishes_exactly_once() {
        let (mut app, window, _button) = focused_button_app(false);
        press_focused_key(&mut app, window, KeyCode::Enter);
        assert_eq!(
            app.world().resource::<SeenButtonActions>().0,
            [(action_ids::VIEW_REFRESH, Source::Key)]
        );
    }

    #[test]
    fn non_keyboard_activation_keeps_mouse_provenance() {
        let (mut app, _window, button) = focused_button_app(false);
        app.world_mut().trigger(Activate { entity: button });
        app.update();
        assert_eq!(
            app.world().resource::<SeenButtonActions>().0,
            [(action_ids::VIEW_REFRESH, Source::Mouse)]
        );
    }

    #[test]
    fn modal_capture_blocks_action_activation() {
        let (mut app, window, _button) = focused_button_app(true);
        press_focused_key(&mut app, window, KeyCode::Enter);
        assert!(
            app.world().resource::<SeenButtonActions>().0.is_empty(),
            "focused controls must not publish actions through an active modal"
        );
    }

    // --- on_nested_action_click pointer routing -----------------------------

    /// Records whether nested toolbar clicks resolve to their action owner.
    #[derive(Resource, Default)]
    struct ClickActivations(Vec<bool>);

    fn click_app() -> App {
        let mut app = App::new();
        app.init_resource::<ClickActivations>();
        app.add_observer(on_nested_action_click);
        app.add_observer(
            |activated: On<Activate>,
             actions: Query<&ToolbarActionButton>,
             mut log: ResMut<ClickActivations>| {
                log.0.push(actions.get(activated.entity).is_ok());
            },
        );
        app
    }

    fn primary_click(app: &mut App, target: Entity) {
        use bevy::camera::NormalizedRenderTarget;
        use bevy::picking::backend::HitData;
        use bevy::picking::pointer::{Location, PointerId};
        use bevy::window::{Window, WindowRef};
        use std::time::Duration;
        // The pointer's render target is a real window entity, distinct from the
        // clicked `target`, so propagation terminates faithfully instead of
        // looping back through the pick target.
        let window = app.world_mut().spawn(Window::default()).id();
        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            Location {
                target: NormalizedRenderTarget::Window(
                    WindowRef::Entity(window).normalize(None).unwrap(),
                ),
                position: Vec2::ZERO,
            },
            Click {
                button: PointerButton::Primary,
                hit: HitData::new(Entity::PLACEHOLDER, 0.0, None, None),
                duration: Duration::ZERO,
                count: 1,
            },
            target,
        ));
        app.world_mut().flush();
    }

    #[test]
    fn direct_target_action_click_activates_once() {
        // A click landing on the action entity's own area (no icon/text child
        // as the hit target) must still activate exactly once — the padding
        // dead-zone the early direct-target return used to leave dead.
        let mut app = click_app();
        let button = app
            .world_mut()
            .spawn(ToolbarActionButton(action_ids::VIEW_REFRESH))
            .id();
        primary_click(&mut app, button);
        assert_eq!(app.world().resource::<ClickActivations>().0, [true]);
    }

    #[test]
    fn real_runtime_plugin_stack_initialises_headless() {
        let mut app = App::new();
        app.set_error_handler(bevy::ecs::error::ignore)
            .add_plugins(MinimalPlugins);
        add_runtime_plugins(
            &mut app,
            None,
            "ws://127.0.0.1:1/filemgr-schedule-test".to_owned(),
        );
        app.finish();
        app.cleanup();

        // The Phase-5 startup regression was raised while Bevy built Update,
        // before any window interaction. One headless update exercises that
        // same complete runtime-plugin schedule graph.
        app.update();
    }
}
