//! FileMgr's registered action vocabulary, keymap resolver and theme actions.
//!
//! Input follows the proven desktop ordering:
//! `ActionProduce -> AppPortSystems -> ActionRoute -> ActionApply`. Focused
//! editors and controls consume their local keys before the window-level
//! normaliser sees them. Every remaining semantic key is resolved through the
//! packaged/user `.mix` keymap with live focus and modal context.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use bevy::input::keyboard::{KeyboardFocusLost, KeyboardInput};
use bevy::input_focus::{FocusedInput, InputFocus};
use bevy::prelude::*;
use bevy::text::EditableText;
use bevy::window::{Window, WindowFocused};
use cosmix_actions::filemgr as ids;
use cosmix_actions::theme as theme_ids;
use cosmix_actions::{
    load_keymap, parse_keymap, resolve, resolve_timeout, ActionArg, ActionArgKind, ActionId,
    ActionMeta, ActionRegistry, ActionSource, ActionSources, ArgsSchema, FocusContext,
    InteractiveAction, Keymap, RegistryError, ResolveDiagnostic, ResolveState, Resolved, Tick,
    FILEMGR_DEFAULT_KEYMAP_MIX,
};
use ctk::key_input::EventKeyState;
use ctk::prelude::{
    resolve_app_theme_with_selection, ActionRequest, ApplyTheme, Icon, InteractionRequest,
    MenuActionRegistry, MenuDef, MenuItemDef, MenuItemMarker, MenuKeymap, MenuPresentation,
    ModalCapture, Mode, Scheme, Source, ThemeState, ThemeWriteCompleted, ThemeWriteRequest,
};

use crate::browser::{BrowserState, FileActionState};
use crate::config::SortColumn;

const KEYMAP_FILE: &str = "keymap.conf.mix";
const MAX_DEFERRED_INPUTS: usize = 64;
const MAX_DEFERRED_FRAMES: u64 = 8;

/// Raw action production for this frame.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ActionProduce;
/// Semantic routing and application-owned side effects.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ActionRoute;
/// Final stage retained for the cross-app app-port ordering contract.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ActionApply;

/// Absolute theme selection queued by direct ingress such as `app.theme.set`.
///
/// The ActionRoute reducer applies these selections before folding relative
/// theme actions from the ordinary action bus, then performs one live apply
/// and one persistence request for the frame's final selection.
#[derive(Message, Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ThemeSelectionRequest {
    pub(crate) scheme: Scheme,
    pub(crate) mode: Mode,
}

/// App-local interaction ingress.
///
/// Producers may write this from observers or scheduled systems. The sole CTK
/// [`InteractionRequest`] publisher drains it before keyboard preflight; any
/// producer that runs later is therefore deferred to the next frame.
#[derive(Message)]
pub(crate) struct PendingInteractionRequest(pub(crate) InteractionRequest);

#[derive(Debug, Clone, Copy)]
struct QueuedKeyInput {
    raw: cosmix_actions::RawInput,
    enqueued_frame: u64,
}

#[derive(Resource, Default)]
struct ShortcutInputQueue {
    pending: VecDeque<QueuedKeyInput>,
    frame: u64,
}

impl ShortcutInputQueue {
    fn push(&mut self, raw: cosmix_actions::RawInput) {
        if self.pending.len() == MAX_DEFERRED_INPUTS {
            self.pending.pop_front();
            warn!("filemgr shortcut queue full; dropped oldest input");
        }
        self.pending.push_back(QueuedKeyInput {
            raw,
            enqueued_frame: self.frame,
        });
    }

    fn begin_frame(&mut self) {
        self.frame = self.frame.saturating_add(1);
        let oldest = self.frame.saturating_sub(MAX_DEFERRED_FRAMES);
        let before = self.pending.len();
        self.pending.retain(|input| input.enqueued_frame >= oldest);
        let dropped = before - self.pending.len();
        if dropped > 0 {
            warn!("filemgr shortcut queue dropped {dropped} starved input(s)");
        }
    }
}

#[derive(Resource)]
struct FileMgrKeymap {
    keymap: Keymap,
    state: ResolveState,
    revision: u64,
    custom_path: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct AvailabilitySnapshot {
    selection: bool,
    rows: bool,
    idle: bool,
    back: bool,
    forward: bool,
    parent: bool,
    show_hidden: bool,
    sort: SortColumn,
}

#[derive(Default)]
struct AvailabilityFlags {
    selection: AtomicBool,
    rows: AtomicBool,
    idle: AtomicBool,
    back: AtomicBool,
    forward: AtomicBool,
    parent: AtomicBool,
}

#[derive(Resource, Clone, Default)]
struct FileMgrActionAvailability {
    flags: Arc<AvailabilityFlags>,
    snapshot: AvailabilitySnapshot,
    initialised: bool,
    presentation_revision: u64,
    theme_revision: u64,
}

#[derive(Resource, Default)]
struct CaptureCandidateThisFrame {
    external_inflight: bool,
}

#[derive(bevy::ecs::system::SystemParam)]
struct ResolutionFocus<'w, 's> {
    capture: Res<'w, ModalCapture>,
    focus: Res<'w, InputFocus>,
    editables: Query<'w, 's, (), With<EditableText>>,
}

impl ResolutionFocus<'_, '_> {
    fn context(&self) -> FocusContext {
        FocusContext {
            focused_editable: self
                .focus
                .get()
                .is_some_and(|entity| self.editables.contains(entity)),
            modal_scope: self.capture.top_owner().map(|owner| owner.kind.to_owned()),
            focus_tags: Default::default(),
        }
    }
}

/// Install FileMgr's registry, default/custom keymap and resolver pipeline.
pub(crate) struct FileMgrActionPlugin;

impl Plugin for FileMgrActionPlugin {
    fn build(&self, app: &mut App) {
        let custom_path = ctk::app_dirs::AppDirs::resolve(crate::IDENTITY.slug)
            .map(|dirs| dirs.config().join(KEYMAP_FILE));
        let keymap = load_effective_keymap(custom_path.as_deref()).unwrap_or_else(|error| {
            eprintln!("filemgr: {error}; using packaged keymap");
            packaged_keymap()
        });
        let availability = FileMgrActionAvailability::default();
        let registry = build_action_registry(&availability);

        app.configure_sets(Update, (ActionProduce, ActionRoute, ActionApply).chain())
            .configure_sets(
                Update,
                ActionProduce.after(ctk::prelude::ModalCaptureSystems),
            )
            .configure_sets(Update, ctk::prelude::InteractionSystems.after(ActionRoute))
            .init_resource::<InputFocus>()
            .init_resource::<ModalCapture>()
            .init_resource::<Time>()
            .init_resource::<MenuPresentation>()
            .init_resource::<EventKeyState>()
            .init_resource::<ShortcutInputQueue>()
            .init_resource::<CaptureCandidateThisFrame>()
            .insert_resource(MenuKeymap::new(1, keymap.clone()))
            .insert_resource(FileMgrKeymap {
                keymap,
                state: ResolveState::default(),
                revision: 1,
                custom_path,
            })
            .insert_resource(availability)
            .insert_resource(MenuActionRegistry::new(registry))
            .add_message::<WindowFocused>()
            .add_message::<KeyboardFocusLost>()
            .add_message::<ActionRequest>()
            .add_message::<ThemeSelectionRequest>()
            .add_message::<PendingInteractionRequest>()
            .add_observer(capture_unconsumed_key_input)
            .add_systems(First, begin_shortcut_frame)
            .add_systems(
                PreUpdate,
                reset_keyboard_state_on_focus_lost
                    .after(bevy::input::InputSystems)
                    .after(bevy::input_focus::InputFocusSystems::Dispatch),
            )
            .add_systems(
                Update,
                (
                    reload_keymap_on_focus,
                    sync_action_availability,
                    note_external_capture_requests,
                    keyboard_actions,
                )
                    .chain()
                    .in_set(ActionProduce),
            )
            .add_systems(Update, route_theme_actions.in_set(ActionRoute))
            // All app-local interaction producers converge here. Failures are
            // intentionally before the publisher; observer/action-route writes
            // that arrive after it remain unread until the next frame.
            .add_systems(
                Update,
                (report_theme_write_failures, publish_interaction_requests)
                    .chain()
                    .before(ActionProduce),
            );
    }
}

fn begin_shortcut_frame(
    mut queue: ResMut<ShortcutInputQueue>,
    mut capture: ResMut<CaptureCandidateThisFrame>,
) {
    queue.begin_frame();
    capture.external_inflight = false;
}

fn capture_unconsumed_key_input(
    input: On<FocusedInput<KeyboardInput>>,
    windows: Query<(), With<Window>>,
    mut event_keys: ResMut<EventKeyState>,
    mut queue: ResMut<ShortcutInputQueue>,
) {
    if let Some(raw) = normalise_window_key(
        &input.input,
        windows.contains(input.focused_entity),
        &mut event_keys,
    ) {
        queue.push(raw);
    }
}

fn normalise_window_key(
    input: &KeyboardInput,
    reached_window: bool,
    event_keys: &mut EventKeyState,
) -> Option<cosmix_actions::RawInput> {
    // Fold modifiers at the event's exact position in the dispatched stream,
    // even when a focused child consumed the semantic key before it reached
    // the window. Otherwise a consumed Ctrl release leaves the next shortcut
    // carrying stale modifiers.
    let raw = event_keys.normalise(input);
    if reached_window {
        raw
    } else {
        None
    }
}

fn reset_keyboard_state_on_focus_lost(
    mut lost: MessageReader<KeyboardFocusLost>,
    mut event_keys: ResMut<EventKeyState>,
    mut queue: ResMut<ShortcutInputQueue>,
    mut keymap: ResMut<FileMgrKeymap>,
) {
    if lost.read().next().is_none() {
        return;
    }
    event_keys.reset();
    queue.pending.clear();
    keymap.state.cancel();
}

fn capture_establishing_action(action: ActionId) -> bool {
    matches!(
        action,
        ids::FILE_NEW_FOLDER | ids::FILE_RENAME | ids::FILE_DELETE
    )
}

fn note_external_capture_requests(
    mut requests: MessageReader<ActionRequest>,
    mut interactions: MessageReader<InteractionRequest>,
    registry: Res<MenuActionRegistry>,
    mut capture: ResMut<CaptureCandidateThisFrame>,
) {
    capture.external_inflight = interactions.read().next().is_some()
        || requests.read().any(|request| {
            request.source != Source::Key
                && capture_establishing_action(request.action)
                && registry.registry().is_enabled(request.action) == Some(true)
        });
}

fn keyboard_actions(
    focus: ResolutionFocus,
    time: Res<Time>,
    capture: Res<CaptureCandidateThisFrame>,
    mut queue: ResMut<ShortcutInputQueue>,
    mut keymap: ResMut<FileMgrKeymap>,
    registry: Res<MenuActionRegistry>,
    mut out: MessageWriter<ActionRequest>,
) {
    let FileMgrKeymap {
        keymap: active_keymap,
        state: resolve_state,
        ..
    } = &mut *keymap;
    let now = Tick(time.elapsed().as_millis().try_into().unwrap_or(u64::MAX));
    if capture.external_inflight || focus.capture.is_captured() {
        if resolve_state.is_pending() {
            let context = focus.context();
            let resolved = resolve_timeout(&context, active_keymap, resolve_state, now);
            report_diagnostics(resolved.diagnostics);
        }
        return;
    }

    let mut emitted_in_batch = false;
    while let Some(input) = queue.pending.pop_front() {
        let context = focus.context();
        let resolved = resolve(input.raw, &context, active_keymap, resolve_state, now);
        let result = emit_resolved(
            resolved,
            &registry,
            &mut out,
            focus.focus.get(),
            emitted_in_batch,
        );
        if result.requeue {
            resolve_state.cancel();
            queue.pending.push_front(input);
            return;
        }
        emitted_in_batch |= result.emitted;
        if result.stop {
            return;
        }
    }
    if resolve_state.is_pending() {
        let context = focus.context();
        let resolved = resolve_timeout(&context, active_keymap, resolve_state, now);
        let _ = emit_resolved(resolved, &registry, &mut out, focus.focus.get(), false);
    }
}

#[derive(Default)]
struct EmitResult {
    emitted: bool,
    stop: bool,
    requeue: bool,
}

fn emit_resolved(
    resolved: Resolved,
    registry: &MenuActionRegistry,
    out: &mut MessageWriter<ActionRequest>,
    invocation_focus: Option<Entity>,
    defer_disabled: bool,
) -> EmitResult {
    report_diagnostics(resolved.diagnostics);
    let action_count = resolved.actions.len();
    let mut result = EmitResult::default();
    for (index, action) in resolved.actions.into_iter().enumerate() {
        let args = Default::default();
        if let Err(error) =
            registry
                .registry()
                .validate_invocation_from(action, &args, ActionSource::Key)
        {
            if defer_disabled && matches!(error, RegistryError::Disabled(_)) {
                result.requeue = true;
                return result;
            }
            eprintln!("filemgr: shortcut action {action} rejected: {error}");
            continue;
        }
        out.write(ActionRequest {
            action,
            source: Source::Key,
            args,
            invocation_focus,
        });
        result.emitted = true;
        if capture_establishing_action(action) {
            result.stop = true;
            result.requeue = index + 1 < action_count;
            return result;
        }
    }
    result
}

fn report_diagnostics(diagnostics: Vec<ResolveDiagnostic>) {
    for diagnostic in diagnostics {
        eprintln!("filemgr: keymap resolution diagnostic: {diagnostic:?}");
    }
}

fn packaged_keymap() -> Keymap {
    parse_keymap(FILEMGR_DEFAULT_KEYMAP_MIX).expect("checked-in FileMgr keymap must stay valid")
}

fn load_effective_keymap(custom_path: Option<&Path>) -> Result<Keymap, String> {
    let mut keymap = packaged_keymap();
    let Some(path) = custom_path else {
        return Ok(keymap);
    };
    if !path.exists() {
        return Ok(keymap);
    }
    let custom = load_keymap(path)
        .map_err(|error| format!("loading FileMgr keymap overlay {}: {error}", path.display()))?;
    keymap.chord_timeout_ms = custom.chord_timeout_ms;
    keymap.custom = custom.custom;
    keymap
        .validate()
        .map_err(|error| format!("invalid FileMgr keymap overlay {}: {error}", path.display()))?;
    Ok(keymap)
}

fn reload_keymap_on_focus(
    mut focused: MessageReader<WindowFocused>,
    mut state: ResMut<FileMgrKeymap>,
    mut menu: ResMut<MenuKeymap>,
) {
    if !focused.read().any(|event| event.focused) {
        return;
    }
    let reloaded = match load_effective_keymap(state.custom_path.as_deref()) {
        Ok(keymap) => keymap,
        Err(error) => {
            eprintln!("filemgr: {error}; keeping current keymap");
            return;
        }
    };
    if reloaded == state.keymap {
        return;
    }
    state.revision = state.revision.saturating_add(1);
    state.keymap = reloaded.clone();
    state.state.cancel();
    menu.replace(state.revision, reloaded);
}

#[allow(clippy::too_many_arguments)]
fn register(
    registry: &mut ActionRegistry,
    id: ActionId,
    label: &str,
    category: &str,
    icon_name: &str,
    args_schema: ArgsSchema,
    interactive: bool,
    allowed_sources: ActionSources,
    enabled: Arc<dyn Fn() -> bool + Send + Sync>,
) {
    registry
        .register(
            ActionMeta {
                id,
                label: label.to_owned(),
                args_schema,
                category: Some(category.to_owned()),
                icon_name: Some(icon_name.to_owned()),
                description: None,
                interactive: interactive.then_some(InteractiveAction { direct_verb: None }),
                allowed_sources,
            },
            Arc::new(|_| Ok(())),
            enabled,
        )
        .expect("FileMgr action metadata is static, bounded and unique");
}

fn flag(
    flags: &Arc<AvailabilityFlags>,
    predicate: impl Fn(&AvailabilityFlags) -> bool + Send + Sync + 'static,
) -> Arc<dyn Fn() -> bool + Send + Sync> {
    let flags = Arc::clone(flags);
    Arc::new(move || predicate(&flags))
}

fn build_action_registry(availability: &FileMgrActionAvailability) -> ActionRegistry {
    let mut registry = ActionRegistry::new();
    let always = || Arc::new(|| true) as Arc<dyn Fn() -> bool + Send + Sync>;
    let flags = &availability.flags;
    let empty = ArgsSchema::default();

    for (id, label, icon, interactive) in [
        (ids::FILE_OPEN, "Open", "folder-open", true),
        (ids::FILE_NEW_FOLDER, "New Folder", "folder", true),
        (ids::FILE_RENAME, "Rename", "file-text", true),
        (ids::FILE_COPY, "Copy to Other Pane", "copy", false),
        (
            ids::FILE_MOVE,
            "Move to Other Pane",
            "move-horizontal",
            false,
        ),
        (ids::FILE_DELETE, "Delete", "trash", true),
    ] {
        let enabled = match id {
            ids::FILE_NEW_FOLDER => flag(flags, |f| f.idle.load(Ordering::Relaxed)),
            _ => flag(flags, |f| {
                f.selection.load(Ordering::Relaxed) && f.idle.load(Ordering::Relaxed)
            }),
        };
        register(
            &mut registry,
            id,
            label,
            "file",
            icon,
            empty.clone(),
            interactive,
            ActionSources::default(),
            enabled,
        );
    }
    register(
        &mut registry,
        ids::APP_QUIT,
        "Quit",
        "application",
        "log-out",
        empty.clone(),
        false,
        ActionSources::default(),
        always(),
    );

    for (id, label, icon, enabled) in [
        (
            ids::NAV_BACK,
            "Back",
            "arrow-left",
            flag(flags, |f| f.back.load(Ordering::Relaxed)),
        ),
        (
            ids::NAV_FORWARD,
            "Forward",
            "arrow-right",
            flag(flags, |f| f.forward.load(Ordering::Relaxed)),
        ),
        (
            ids::NAV_PARENT,
            "Parent Folder",
            "arrow-up",
            flag(flags, |f| f.parent.load(Ordering::Relaxed)),
        ),
        (ids::NAV_HOME, "Home", "house", always()),
        (ids::NAV_SWITCH_PANE, "Switch Pane", "panel-left", always()),
    ] {
        register(
            &mut registry,
            id,
            label,
            "navigation",
            icon,
            empty.clone(),
            false,
            ActionSources::BUS,
            enabled,
        );
    }

    for (id, label, icon) in [
        (ids::VIEW_REFRESH, "Refresh", "refresh"),
        (ids::VIEW_TOGGLE_HIDDEN, "Show Hidden Files", "eye"),
        (ids::VIEW_SORT_NAME, "Sort by Name", "list"),
        (ids::VIEW_SORT_SIZE, "Sort by Size", "list"),
        (ids::VIEW_SORT_MODIFIED, "Sort by Modified", "list"),
    ] {
        register(
            &mut registry,
            id,
            label,
            "view",
            icon,
            empty.clone(),
            false,
            ActionSources::BUS,
            always(),
        );
    }

    for (id, label) in [
        (ids::SELECT_NEXT, "Select Next"),
        (ids::SELECT_PREVIOUS, "Select Previous"),
        (ids::SELECT_FIRST, "Select First"),
        (ids::SELECT_LAST, "Select Last"),
    ] {
        register(
            &mut registry,
            id,
            label,
            "selection",
            "list",
            empty.clone(),
            false,
            ActionSources::default(),
            flag(flags, |f| f.rows.load(Ordering::Relaxed)),
        );
    }
    register(
        &mut registry,
        ids::PLACE_OPEN,
        "Open Place",
        "places",
        "folder-open",
        ArgsSchema {
            fields: vec![ActionArg {
                name: "path".to_owned(),
                kind: ActionArgKind::String,
                required: true,
                description: Some("Local absolute place path".to_owned()),
            }],
            allow_extra: false,
        },
        false,
        ActionSources::default(),
        always(),
    );

    for (id, label) in [
        (theme_ids::MODE_TOGGLE, "Dark Mode"),
        (theme_ids::SCHEME_OCEAN, "Ocean"),
        (theme_ids::SCHEME_CRIMSON, "Crimson"),
        (theme_ids::SCHEME_STONE, "Stone"),
        (theme_ids::SCHEME_FOREST, "Forest"),
        (theme_ids::SCHEME_SUNSET, "Sunset"),
        (theme_ids::SCHEME_MONO, "Mono"),
    ] {
        register(
            &mut registry,
            id,
            label,
            "theme",
            "grid",
            empty.clone(),
            false,
            ActionSources::BUS,
            always(),
        );
    }
    registry
}

fn sync_action_availability(
    browser: Option<Res<BrowserState>>,
    file_actions: Res<FileActionState>,
    theme: Res<ThemeState>,
    mut availability: ResMut<FileMgrActionAvailability>,
    mut registry: ResMut<MenuActionRegistry>,
    mut presentation: ResMut<MenuPresentation>,
) {
    let snapshot = browser
        .as_deref()
        .map_or_else(AvailabilitySnapshot::default, |browser| {
            let pane = &browser.panes[browser.active.index()];
            AvailabilitySnapshot {
                selection: pane.action_selection_available(),
                rows: pane.action_rows_available(),
                idle: file_actions.is_idle(),
                back: !pane.history.back.is_empty(),
                forward: !pane.history.forward.is_empty(),
                parent: pane.path.parent().is_some(),
                show_hidden: pane.show_hidden,
                sort: pane.sort,
            }
        });
    let availability_changed = !availability.initialised || snapshot != availability.snapshot;
    let theme_changed = availability.theme_revision != theme.revision;
    if !availability_changed && !theme_changed {
        return;
    }
    if availability_changed {
        availability.snapshot = snapshot;
        availability.initialised = true;
        availability
            .flags
            .selection
            .store(snapshot.selection, Ordering::Relaxed);
        availability
            .flags
            .rows
            .store(snapshot.rows, Ordering::Relaxed);
        availability
            .flags
            .idle
            .store(snapshot.idle, Ordering::Relaxed);
        availability
            .flags
            .back
            .store(snapshot.back, Ordering::Relaxed);
        availability
            .flags
            .forward
            .store(snapshot.forward, Ordering::Relaxed);
        availability
            .flags
            .parent
            .store(snapshot.parent, Ordering::Relaxed);
        registry.mark_enabled_changed();
    }
    availability.theme_revision = theme.revision;
    availability.presentation_revision = availability.presentation_revision.saturating_add(1);
    let mut markers = Vec::with_capacity(4);
    if snapshot.show_hidden {
        markers.push((ids::VIEW_TOGGLE_HIDDEN, MenuItemMarker::Checked));
    }
    markers.push((
        match snapshot.sort {
            SortColumn::Name => ids::VIEW_SORT_NAME,
            SortColumn::Size => ids::VIEW_SORT_SIZE,
            SortColumn::Modified => ids::VIEW_SORT_MODIFIED,
        },
        MenuItemMarker::Radio,
    ));
    if theme.mode == Mode::Dark {
        markers.push((theme_ids::MODE_TOGGLE, MenuItemMarker::Checked));
    }
    markers.push((scheme_action(theme.scheme), MenuItemMarker::Radio));
    *presentation = MenuPresentation::from_registry(
        availability.presentation_revision,
        registry.registry(),
        markers,
    );
}

/// Map CTK request provenance into the registry's source vocabulary.
pub(crate) const fn registry_source(source: Source) -> ActionSource {
    match source {
        Source::Key => ActionSource::Key,
        Source::Mouse => ActionSource::Mouse,
        Source::Menu => ActionSource::Menu,
        Source::Bus => ActionSource::Bus,
        Source::Midi => ActionSource::Midi,
        Source::Osc => ActionSource::Osc,
    }
}

fn route_theme_actions(
    mut selections: MessageReader<ThemeSelectionRequest>,
    mut requests: MessageReader<ActionRequest>,
    registry: Res<MenuActionRegistry>,
    state: Res<ThemeState>,
    mut apply: MessageWriter<ApplyTheme>,
    mut persist: MessageWriter<ThemeWriteRequest>,
) {
    let mut working = (state.scheme, state.mode);
    let mut changed = false;
    for selection in selections.read() {
        working = (selection.scheme, selection.mode);
        changed = true;
    }
    for request in requests.read() {
        let Some(next) = theme_selection(request.action, working) else {
            continue;
        };
        if let Err(error) = registry.registry().validate_invocation_from(
            request.action,
            &request.args,
            registry_source(request.source),
        ) {
            eprintln!("filemgr: theme action {} rejected: {error}", request.action);
            continue;
        }
        working = next;
        changed = true;
    }
    if changed {
        queue_theme_change(working.0, working.1, &mut apply, &mut persist);
    }
}

fn theme_selection(action: ActionId, current: (Scheme, Mode)) -> Option<(Scheme, Mode)> {
    let (current_scheme, current_mode) = current;
    let mode = if action == theme_ids::MODE_TOGGLE {
        return Some((
            current_scheme,
            if current_mode == Mode::Dark {
                Mode::Light
            } else {
                Mode::Dark
            },
        ));
    } else {
        current_mode
    };
    let scheme = match action {
        theme_ids::SCHEME_OCEAN => Scheme::Ocean,
        theme_ids::SCHEME_CRIMSON => Scheme::Crimson,
        theme_ids::SCHEME_STONE => Scheme::Stone,
        theme_ids::SCHEME_FOREST => Scheme::Forest,
        theme_ids::SCHEME_SUNSET => Scheme::Sunset,
        theme_ids::SCHEME_MONO => Scheme::Mono,
        _ => return None,
    };
    Some((scheme, mode))
}

fn scheme_action(scheme: Scheme) -> ActionId {
    match scheme {
        Scheme::Ocean => theme_ids::SCHEME_OCEAN,
        Scheme::Crimson => theme_ids::SCHEME_CRIMSON,
        Scheme::Stone => theme_ids::SCHEME_STONE,
        Scheme::Forest => theme_ids::SCHEME_FOREST,
        Scheme::Sunset => theme_ids::SCHEME_SUNSET,
        Scheme::Mono => theme_ids::SCHEME_MONO,
    }
}

/// Queue the shared live-apply and persistence path used by actions and Bus.
pub(crate) fn queue_theme_change(
    scheme: Scheme,
    mode: Mode,
    apply: &mut MessageWriter<ApplyTheme>,
    persist: &mut MessageWriter<ThemeWriteRequest>,
) {
    let app_dirs = crate::config::app_dirs();
    apply.write(ApplyTheme(resolve_app_theme_with_selection(
        Some(app_dirs.config().as_path()),
        scheme,
        mode,
    )));
    persist.write(ThemeWriteRequest::shared(scheme, mode));
}

fn report_theme_write_failures(
    mut completed: MessageReader<ThemeWriteCompleted>,
    mut interactions: MessageWriter<PendingInteractionRequest>,
) {
    for result in completed.read() {
        let Err(error) = &result.result else {
            continue;
        };
        eprintln!("filemgr: saving theme selection failed: {error}");
        interactions.write(PendingInteractionRequest(InteractionRequest::message(
            "Theme preference was not saved",
            error.clone(),
        )));
    }
}

fn publish_interaction_requests(
    mut pending: MessageReader<PendingInteractionRequest>,
    mut interactions: MessageWriter<InteractionRequest>,
) {
    for request in pending.read() {
        interactions.write(request.0.clone());
    }
}

/// FileMgr's menu bar declarations. Accelerator text comes only from MenuKeymap.
pub(crate) fn menu_defs() -> Vec<MenuDef> {
    vec![
        MenuDef {
            label: "File".into(),
            items: vec![
                MenuItemDef::new(ids::FILE_OPEN.as_str(), "Open").with_icon(Icon::FolderOpen),
                MenuItemDef::new(ids::FILE_NEW_FOLDER.as_str(), "New Folder…")
                    .with_icon(Icon::Folder),
                MenuItemDef::new(ids::FILE_RENAME.as_str(), "Rename…").with_icon(Icon::FileText),
                MenuItemDef::new(ids::FILE_COPY.as_str(), "Copy to Other Pane")
                    .with_icon(Icon::Copy),
                MenuItemDef::new(ids::FILE_MOVE.as_str(), "Move to Other Pane")
                    .with_icon(Icon::MoveHorizontal),
                MenuItemDef::new(ids::FILE_DELETE.as_str(), "Delete…").with_icon(Icon::Trash),
                MenuItemDef::new(ids::APP_QUIT.as_str(), "Quit").with_icon(Icon::LogOut),
            ],
        },
        MenuDef {
            label: "Navigate".into(),
            items: vec![
                MenuItemDef::new(ids::NAV_BACK.as_str(), "Back").with_icon(Icon::ArrowLeft),
                MenuItemDef::new(ids::NAV_FORWARD.as_str(), "Forward").with_icon(Icon::ArrowRight),
                MenuItemDef::new(ids::NAV_PARENT.as_str(), "Parent Folder")
                    .with_icon(Icon::ArrowUp),
                MenuItemDef::new(ids::NAV_HOME.as_str(), "Home").with_icon(Icon::House),
                MenuItemDef::new(ids::NAV_SWITCH_PANE.as_str(), "Switch Pane")
                    .with_icon(Icon::PanelRight),
            ],
        },
        MenuDef {
            label: "View".into(),
            items: vec![
                MenuItemDef::new(ids::VIEW_REFRESH.as_str(), "Refresh").with_icon(Icon::Refresh),
                MenuItemDef::new(ids::VIEW_TOGGLE_HIDDEN.as_str(), "Show Hidden Files")
                    .with_icon(Icon::Eye),
                MenuItemDef::new(ids::VIEW_SORT_NAME.as_str(), "Sort by Name")
                    .with_icon(Icon::List),
                MenuItemDef::new(ids::VIEW_SORT_SIZE.as_str(), "Sort by Size")
                    .with_icon(Icon::List),
                MenuItemDef::new(ids::VIEW_SORT_MODIFIED.as_str(), "Sort by Modified")
                    .with_icon(Icon::List),
            ],
        },
        MenuDef {
            label: "Themes".into(),
            items: vec![
                MenuItemDef::new(theme_ids::MODE_TOGGLE.as_str(), "Dark Mode").with_icon(Icon::Eye),
                MenuItemDef::new(theme_ids::SCHEME_OCEAN.as_str(), "Ocean").with_icon(Icon::Grid),
                MenuItemDef::new(theme_ids::SCHEME_CRIMSON.as_str(), "Crimson")
                    .with_icon(Icon::Grid),
                MenuItemDef::new(theme_ids::SCHEME_STONE.as_str(), "Stone").with_icon(Icon::Grid),
                MenuItemDef::new(theme_ids::SCHEME_FOREST.as_str(), "Forest").with_icon(Icon::Grid),
                MenuItemDef::new(theme_ids::SCHEME_SUNSET.as_str(), "Sunset").with_icon(Icon::Grid),
                MenuItemDef::new(theme_ids::SCHEME_MONO.as_str(), "Mono").with_icon(Icon::List),
            ],
        },
    ]
}

/// File-row context actions use the same canonical definitions and registry
/// presentation as the menu bar.
pub(crate) fn context_menu_defs() -> Vec<MenuItemDef> {
    vec![
        MenuItemDef::new(ids::FILE_OPEN.as_str(), "Open").with_icon(Icon::FolderOpen),
        MenuItemDef::new(ids::FILE_RENAME.as_str(), "Rename…").with_icon(Icon::FileText),
        MenuItemDef::new(ids::FILE_COPY.as_str(), "Copy to Other Pane").with_icon(Icon::Copy),
        MenuItemDef::new(ids::FILE_MOVE.as_str(), "Move to Other Pane")
            .with_icon(Icon::MoveHorizontal),
        MenuItemDef::new(ids::FILE_DELETE.as_str(), "Delete…").with_icon(Icon::Trash),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::input::keyboard::{Key as LogicalKey, NativeKey};
    use bevy::input::ButtonState;

    fn keyboard_input(key_code: KeyCode, state: ButtonState) -> KeyboardInput {
        KeyboardInput {
            key_code,
            logical_key: LogicalKey::Unidentified(NativeKey::Unidentified),
            state,
            text: None,
            repeat: false,
            window: Entity::PLACEHOLDER,
        }
    }

    fn modal_timing_app() -> App {
        let mut app = App::new();
        app.set_error_handler(bevy::ecs::error::ignore)
            .add_plugins(MinimalPlugins)
            .init_resource::<FileActionState>()
            .init_resource::<ThemeState>()
            .add_message::<ThemeWriteCompleted>()
            .add_plugins((ctk::interaction::InteractionPlugin, FileMgrActionPlugin));
        app.finish();
        app.cleanup();
        app
    }

    fn queue_theme_toggle(app: &mut App) {
        app.world_mut().resource_mut::<ShortcutInputQueue>().push(
            cosmix_actions::RawInput::pressed(
                cosmix_actions::Key::character('D').unwrap(),
                cosmix_actions::Modifiers {
                    control: true,
                    alt: true,
                    ..Default::default()
                },
            ),
        );
    }

    fn assert_no_action_escaped_under_modal(app: &mut App) {
        let emitted: Vec<_> = app
            .world_mut()
            .resource_mut::<Messages<ActionRequest>>()
            .drain()
            .collect();
        assert!(emitted.is_empty(), "underlying shortcut must be suppressed");
        assert!(
            app.world().resource::<ModalCapture>().is_captured(),
            "interaction request must still open its modal"
        );
    }

    #[derive(Resource, Default)]
    struct LateInteractionProducer(bool);

    fn produce_interaction_after_preflight(
        mut producer: ResMut<LateInteractionProducer>,
        mut pending: MessageWriter<PendingInteractionRequest>,
    ) {
        if producer.0 {
            return;
        }
        producer.0 = true;
        pending.write(PendingInteractionRequest(InteractionRequest::message(
            "Late modal",
            "test",
        )));
    }

    #[test]
    fn pending_interaction_suppresses_underlying_shortcut_before_ingestion() {
        let mut app = modal_timing_app();
        queue_theme_toggle(&mut app);
        app.world_mut()
            .write_message(InteractionRequest::message("Pending modal", "test"));

        app.update();

        assert_no_action_escaped_under_modal(&mut app);
    }

    #[test]
    fn same_frame_theme_failure_producer_precedes_shortcut_preflight() {
        let mut app = modal_timing_app();
        queue_theme_toggle(&mut app);
        app.world_mut().write_message(ThemeWriteCompleted {
            path: PathBuf::from("/tmp/filemgr-theme-write-test"),
            scheme: Scheme::Forest,
            mode: Mode::Dark,
            result: Err("simulated write failure".to_owned()),
        });

        app.update();

        assert_no_action_escaped_under_modal(&mut app);
    }

    #[test]
    fn producer_after_preflight_defers_modal_until_the_next_frame() {
        let mut app = modal_timing_app();
        app.init_resource::<LateInteractionProducer>().add_systems(
            Update,
            produce_interaction_after_preflight.in_set(ActionRoute),
        );
        queue_theme_toggle(&mut app);

        app.update();

        let first_frame_actions: Vec<_> = app
            .world_mut()
            .resource_mut::<Messages<ActionRequest>>()
            .drain()
            .collect();
        assert_eq!(first_frame_actions.len(), 1);
        assert_eq!(first_frame_actions[0].action, theme_ids::MODE_TOGGLE);
        assert!(
            !app.world().resource::<ModalCapture>().is_captured(),
            "a late producer must not open behind an already-resolved key"
        );

        app.update();

        let second_frame_actions: Vec<_> = app
            .world_mut()
            .resource_mut::<Messages<ActionRequest>>()
            .drain()
            .collect();
        assert!(second_frame_actions.is_empty());
        assert!(
            app.world().resource::<ModalCapture>().is_captured(),
            "the deferred interaction must open on the following frame"
        );
    }

    #[test]
    fn menu_definitions_cover_canonical_ids_once_and_have_bindings() {
        let actual: Vec<_> = menu_defs()
            .into_iter()
            .flat_map(|menu| menu.items)
            .map(|item| ActionId::from_static(item.id))
            .collect();
        assert_eq!(actual, ids::MENU_ACTION_IDS);
        let keymap = packaged_keymap();
        assert!(actual
            .iter()
            .all(|action| keymap.binding_for(*action).is_some()));
    }

    #[test]
    fn bus_allowlist_is_audited_and_file_mutations_are_local() {
        let registry = build_action_registry(&FileMgrActionAvailability::default());
        for id in [
            ids::NAV_BACK,
            ids::NAV_FORWARD,
            ids::NAV_PARENT,
            ids::NAV_HOME,
            ids::NAV_SWITCH_PANE,
            ids::VIEW_REFRESH,
            ids::VIEW_TOGGLE_HIDDEN,
            ids::VIEW_SORT_NAME,
            ids::VIEW_SORT_SIZE,
            ids::VIEW_SORT_MODIFIED,
        ]
        .into_iter()
        .chain(theme_ids::ACTION_IDS)
        {
            assert!(registry.metadata(id).unwrap().allowed_sources.bus, "{id}");
        }
        for id in [
            ids::FILE_OPEN,
            ids::FILE_NEW_FOLDER,
            ids::FILE_RENAME,
            ids::FILE_COPY,
            ids::FILE_MOVE,
            ids::FILE_DELETE,
        ] {
            assert!(!registry.metadata(id).unwrap().allowed_sources.bus, "{id}");
        }
    }

    #[test]
    fn theme_selection_preserves_the_other_axis() {
        assert_eq!(
            theme_selection(theme_ids::SCHEME_CRIMSON, (Scheme::Ocean, Mode::Dark)),
            Some((Scheme::Crimson, Mode::Dark))
        );
        assert_eq!(
            theme_selection(theme_ids::MODE_TOGGLE, (Scheme::Ocean, Mode::Dark)),
            Some((Scheme::Ocean, Mode::Light))
        );
    }

    #[test]
    fn same_frame_theme_actions_fold_from_the_pending_selection() {
        let forest =
            theme_selection(theme_ids::SCHEME_FOREST, (Scheme::Ocean, Mode::Dark)).unwrap();
        assert_eq!(
            theme_selection(theme_ids::MODE_TOGGLE, forest),
            Some((Scheme::Forest, Mode::Light))
        );
    }

    #[test]
    fn direct_selection_and_action_fold_to_one_apply_and_persist() {
        let availability = FileMgrActionAvailability::default();
        let registry = build_action_registry(&availability);
        let mut state = ThemeState::default();
        state.scheme = Scheme::Ocean;
        state.mode = Mode::Dark;
        let mut app = App::new();
        app.add_message::<ThemeSelectionRequest>()
            .add_message::<ActionRequest>()
            .add_message::<ApplyTheme>()
            .add_message::<ThemeWriteRequest>()
            .insert_resource(MenuActionRegistry::new(registry))
            .insert_resource(state)
            .add_systems(Update, route_theme_actions);
        app.world_mut().write_message(ThemeSelectionRequest {
            scheme: Scheme::Forest,
            mode: Mode::Dark,
        });
        app.world_mut().write_message(ActionRequest {
            action: theme_ids::MODE_TOGGLE,
            source: Source::Bus,
            args: Default::default(),
            invocation_focus: None,
        });

        app.update();

        let applied: Vec<_> = app
            .world_mut()
            .resource_mut::<Messages<ApplyTheme>>()
            .drain()
            .collect();
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0].0.scheme, Scheme::Forest);
        assert_eq!(applied[0].0.mode, Mode::Light);
        let persisted: Vec<_> = app
            .world_mut()
            .resource_mut::<Messages<ThemeWriteRequest>>()
            .drain()
            .collect();
        assert_eq!(persisted.len(), 1);
        assert_eq!(persisted[0].scheme, Scheme::Forest);
        assert_eq!(persisted[0].mode, Mode::Light);
    }

    #[test]
    fn consumed_modifier_release_still_updates_event_local_state() {
        let mut state = EventKeyState::default();
        assert!(normalise_window_key(
            &keyboard_input(KeyCode::ControlLeft, ButtonState::Pressed),
            true,
            &mut state,
        )
        .is_none());
        assert!(normalise_window_key(
            &keyboard_input(KeyCode::ControlLeft, ButtonState::Released),
            false,
            &mut state,
        )
        .is_none());
        let plain = normalise_window_key(
            &keyboard_input(KeyCode::KeyH, ButtonState::Pressed),
            true,
            &mut state,
        )
        .unwrap();
        assert!(!plain.modifiers.control);
    }

    #[test]
    fn packaged_shortcuts_resolve_and_editable_focus_swallows_them() {
        let keymap = packaged_keymap();
        let mut state = ResolveState::default();
        let resolved = resolve(
            cosmix_actions::RawInput::pressed(
                cosmix_actions::Key::Enter,
                cosmix_actions::Modifiers::NONE,
            ),
            &FocusContext::default(),
            &keymap,
            &mut state,
            Tick(1),
        );
        assert_eq!(resolved.actions, [ids::FILE_OPEN]);

        let resolved = resolve(
            cosmix_actions::RawInput::pressed(
                cosmix_actions::Key::character('H').unwrap(),
                cosmix_actions::Modifiers {
                    control: true,
                    ..Default::default()
                },
            ),
            &FocusContext {
                focused_editable: true,
                ..Default::default()
            },
            &keymap,
            &mut state,
            Tick(2),
        );
        assert!(resolved.actions.is_empty());
        assert!(matches!(
            resolved.outcome,
            cosmix_actions::ResolveOutcome::Suppressed(
                cosmix_actions::SuppressionReason::EditableFocus
            )
        ));
    }
}
