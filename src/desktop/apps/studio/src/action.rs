//! Studio's shared action registry, keymap resolver and transport intent spine.
//!
//! Three types, one direction of flow:
//!
//! ```text
//!  keyboard/menu/Bus ──▶ ActionRequest ──▶ AudioIntent ──▶ apply_audio_intents
//! ```
//!
//! Every producer writes CTK's shared [`ActionRequest`]. Keyboard bindings are
//! strict-data from `cosmix-actions`; menu ids and registry ids are the same
//! canonical [`ActionId`] values. Every transport side effect still funnels
//! through [`apply_audio_intents`].
//!
//! The frame contract is
//! `ActionProduce -> AppPortSystems -> ActionRoute -> ActionApply`. Keyboard
//! resolution and availability mirroring are part of Produce, so Bus ingress
//! sees same-frame capture candidates and current enabled state while every
//! accepted request still reaches its router in the same frame. A
//! capture-establishing shortcut stops its input batch;
//! Route consumers mark capture only after accepting the request. CTK defers
//! focused-control keyboard effects until after Route, allowing Studio to drop
//! only effects later than an accepted capture shortcut. This closes the
//! PreUpdate widget gap without duplicating Bevy's focus-consumption policy.
//!
//! Board-scoped input is registered in [`BoardInputSystems`], ordered after the
//! modal services have ingested this frame's requests. Remote action ingress
//! remains independent; keyboard ownership is enforced inside `resolve()`.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use bevy::ecs::system::SystemParam;
use bevy::input::keyboard::{KeyboardFocusLost, KeyboardInput};
use bevy::input_focus::{FocusedInput, InputFocus};
use bevy::prelude::*;
use bevy::text::EditableText;
use bevy::window::{Window, WindowFocused};
use cosmix_actions::studio as ids;
use cosmix_actions::theme as theme_ids;
use cosmix_actions::{
    load_keymap, parse_keymap, resolve, resolve_timeout, ActionId, ActionMeta, ActionRegistry,
    ActionSources, ArgsSchema, FocusContext, InteractiveAction, Keymap, RegistryError,
    ResolveDiagnostic, ResolveOutcome, ResolveState, Resolved, Tick, STUDIO_DEFAULT_KEYMAP_MIX,
};
use ctk::key_input::EventKeyState;
use ctk::prelude::{
    resolve_app_theme_with_selection, ActionRequest, ApplyTheme, KeyboardControlQueue,
    KeyboardControlSystems, KeyboardInputOrder, MenuActionRegistry, MenuItemMarker, MenuKeymap,
    MenuPresentation, ModalCapture, Mode, Scheme, Source, ThemeState, ThemeWriteCompleted,
    ThemeWriteRequest, TransportSeekRequest, TransportState,
};

use crate::editor::SongEditor;
use crate::views::{ActiveView, RegionEditor, StatusLine, WaveLanes};
use crate::TransportButtons;

const KEYMAP_FILE: &str = "keymap.conf.mix";
const MAX_DEFERRED_INPUTS: usize = 64;
const MAX_DEFERRED_FRAMES: u64 = 8;

#[cfg(test)]
pub(crate) const THEME_MENU_ACTION_IDS: &[ActionId] = &theme_ids::ACTION_IDS;

#[cfg(test)]
pub(crate) fn handles_theme_action(action: ActionId) -> bool {
    THEME_MENU_ACTION_IDS.contains(&action)
}

/// Engine-semantic transport intents — studio's intent layer above ctk. Today
/// these map onto ctk's existing write paths (footer-button `Activate` for
/// state, `TransportSeekRequest` for position); ctk stays the single low-level
/// writer. Nothing outside [`apply_audio_intents`] drives the transport.
///
/// Every variant has a producer: `Play`/`Toggle`/`Reset` from the keyboard,
/// autoplay and song-load; `Stop` and `Seek(0.0)` from the Bus
/// `app.transport.{pause,stop}` verbs via [`route_actions`]. The fold in
/// [`reduce_audio_intents`] resolves them against the desired-state projection.
#[derive(Message, Debug, Clone, Copy, PartialEq)]
#[allow(dead_code)]
pub enum AudioIntent {
    Play,
    Stop,
    /// Play if stopped, stop if playing.
    Toggle,
    Seek(f64),
    /// Full reset for a freshly loaded song: stop the outgoing bank and rewind
    /// to the top.
    Reset,
}

fn board_input_enabled(
    capture: Res<ctk::prelude::ModalCapture>,
    established: Res<CaptureEstablishedThisFrame>,
) -> bool {
    !capture.is_captured() && !established.is_marked()
}

/// Modal-sensitive board keyboard systems. CTK's live modal registry is the
/// authority: this set runs after action routing and after CTK has accepted
/// modal service requests for the frame. Capture-establishing menu ingress also
/// latches the set off until that capture exists, avoiding a one-frame leak.
///
/// CTK board controls already carry `TabIndex`, but Studio deliberately has no
/// non-modal `TabGroup` yet. Adding one must first define how hidden views and
/// disabled controls leave traversal; that accessibility cut is deferred.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct BoardInputSystems;

/// All raw action/intent producers for this frame. Ordered after requester
/// ingestion so file outcomes produced by requester keyboard handling are
/// visible without an extra frame.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ActionProduce;

/// Converts semantic action ingress into engine-facing intents.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ActionRoute;

/// The sole Studio transport-intent application stage.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ActionApply;

/// Absolute theme selection queued by direct ingress such as `app.theme.set`.
///
/// ActionRoute applies these before relative theme actions from the ordinary
/// bus and emits one final live apply/persistence pair for the frame.
#[derive(Message, Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ThemeSelectionRequest {
    pub(crate) scheme: Scheme,
    pub(crate) mode: Mode,
}

/// One normalised input retained independently of Bevy's message lifetime.
#[derive(Debug, Clone, Copy)]
struct QueuedKeyInput {
    raw: cosmix_actions::RawInput,
    physical: KeyCode,
    order: u64,
    enqueued_frame: u64,
}

/// Explicit, bounded shortcut queue independent of Bevy message retention.
///
/// Overflow drops the oldest input at 64 entries. Capture starvation drops
/// inputs older than eight Studio frames. Focus loss clears the queue, and
/// process shutdown drops it without executing or persisting pending input.
#[derive(Resource, Default)]
pub(crate) struct ShortcutInputQueue {
    pending: VecDeque<QueuedKeyInput>,
    frame: u64,
}

impl ShortcutInputQueue {
    pub(crate) fn push(&mut self, raw: cosmix_actions::RawInput, physical: KeyCode, order: u64) {
        if self.pending.len() == MAX_DEFERRED_INPUTS {
            self.pending.pop_front();
            warn!("studio shortcut queue full; dropped oldest input (cap {MAX_DEFERRED_INPUTS})");
        }
        self.pending.push_back(QueuedKeyInput {
            raw,
            physical,
            order,
            enqueued_frame: self.frame,
        });
    }

    fn begin_frame(&mut self) {
        self.frame = self.frame.saturating_add(1);
    }

    fn drop_starved(&mut self) {
        let oldest = self.frame.saturating_sub(MAX_DEFERRED_FRAMES);
        let before = self.pending.len();
        self.pending.retain(|input| input.enqueued_frame >= oldest);
        let dropped = before - self.pending.len();
        if dropped > 0 {
            warn!(
                "studio shortcut queue dropped {dropped} starved input(s) after {MAX_DEFERRED_FRAMES} frames"
            );
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ShortcutInputOutcome {
    pub(crate) raw: cosmix_actions::RawInput,
    pub(crate) physical: KeyCode,
    pub(crate) claimed: bool,
}

/// Ordered physical-key outcomes produced by the resolver this frame.
///
/// Unlike `ButtonInput`, separate presses of the same physical key retain their
/// own modifiers and claim state. Board readers can therefore act on an earlier
/// plain `S` even if a later `Ctrl+Shift+S` was claimed by the keymap.
#[derive(Resource, Default)]
pub(crate) struct ConsumedShortcutInputs {
    resolved: Vec<ShortcutInputOutcome>,
}

impl ConsumedShortcutInputs {
    /// Every processed physical input in original batch order.
    ///
    /// `claimed` is true for resolved, pending, suppressed and ignored-repeat
    /// presses. Releases may remain unclaimed, but [`Self::unclaimed_presses`]
    /// excludes them by construction so they cannot reach board press logic.
    pub(crate) fn outcomes(&self) -> impl Iterator<Item = ShortcutInputOutcome> + '_ {
        self.resolved.iter().copied()
    }

    /// Unclaimed, non-repeat key presses in original batch order.
    pub(crate) fn unclaimed_presses(&self) -> impl Iterator<Item = ShortcutInputOutcome> + '_ {
        self.outcomes().filter(|event| {
            !event.claimed
                && event.raw.state == cosmix_actions::RawInputState::Pressed
                && !event.raw.repeat
        })
    }
}

/// Set only by a consumer after it accepted a request which owns keyboard
/// capture (settings acquired its token, or file I/O emitted a valid request).
#[derive(Resource, Default)]
pub(crate) struct CaptureEstablishedThisFrame {
    candidates: Vec<(ActionId, u64)>,
    accepted_after: Option<u64>,
    external_inflight: bool,
}

impl CaptureEstablishedThisFrame {
    fn candidate(&mut self, action: ActionId, order: u64) {
        self.candidates.push((action, order));
    }

    pub(crate) fn mark_request(&mut self, request: &ActionRequest) {
        let order = (request.source == Source::Key)
            .then(|| {
                self.candidates
                    .iter()
                    .find_map(|(action, order)| (*action == request.action).then_some(*order))
            })
            .flatten()
            .unwrap_or(0);
        self.accepted_after = Some(
            self.accepted_after
                .map_or(order, |current| current.min(order)),
        );
    }

    pub(crate) fn is_marked(&self) -> bool {
        self.accepted_after.is_some()
    }

    fn accepted_after(&self) -> Option<u64> {
        self.accepted_after
    }
}

#[derive(Resource)]
struct StudioKeymap {
    keymap: Keymap,
    state: ResolveState,
    revision: u64,
    custom_path: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct AvailabilitySnapshot {
    song: bool,
    session: bool,
    waves: bool,
    roll: bool,
    zoom: bool,
}

#[derive(Default)]
struct AvailabilityFlags {
    song: AtomicBool,
    session: AtomicBool,
    waves: AtomicBool,
    roll: AtomicBool,
    zoom: AtomicBool,
}

#[derive(Resource, Clone, Default)]
struct StudioActionAvailability {
    flags: Arc<AvailabilityFlags>,
    snapshot: AvailabilitySnapshot,
    initialised: bool,
    revision: u64,
    theme_revision: u64,
}

#[derive(SystemParam)]
struct ResolutionFocus<'w, 's> {
    capture: Res<'w, ModalCapture>,
    focus: Res<'w, InputFocus>,
    editables: Query<'w, 's, (), With<EditableText>>,
}

#[derive(SystemParam)]
struct KeyboardActionParams<'w, 's> {
    resolution_focus: ResolutionFocus<'w, 's>,
    time: Res<'w, Time>,
    established: ResMut<'w, CaptureEstablishedThisFrame>,
    queue: ResMut<'w, ShortcutInputQueue>,
    keymap: ResMut<'w, StudioKeymap>,
    registry: Res<'w, MenuActionRegistry>,
    consumed: ResMut<'w, ConsumedShortcutInputs>,
    out: MessageWriter<'w, ActionRequest>,
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

pub struct ActionPlugin;

impl Plugin for ActionPlugin {
    fn build(&self, app: &mut App) {
        let custom_path = ctk::app_dirs::AppDirs::resolve(crate::IDENTITY.slug)
            .map(|dirs| dirs.config().join(KEYMAP_FILE));
        let keymap = load_effective_keymap(custom_path.as_deref()).unwrap_or_else(|error| {
            eprintln!("studio: {error}; using packaged keymap");
            packaged_keymap()
        });
        let availability = StudioActionAvailability::default();
        let registry = build_action_registry(&availability);

        app.configure_sets(
            Update,
            BoardInputSystems
                .after(ctk::prelude::ModalCaptureSystems)
                .after(ActionRoute)
                .after(KeyboardControlSystems)
                .before(ActionApply)
                .run_if(board_input_enabled),
        )
        .configure_sets(Update, (ActionProduce, ActionRoute, ActionApply).chain())
        .configure_sets(
            Update,
            ActionProduce.after(ctk::prelude::FileRequesterSystems),
        )
        .configure_sets(
            Update,
            ActionApply.before(ctk::prelude::MixerTransportIngressSystems),
        )
        .configure_sets(
            Update,
            KeyboardControlSystems
                .after(ActionRoute)
                .before(ActionApply),
        )
        .init_resource::<InputFocus>()
        .init_resource::<ModalCapture>()
        .init_resource::<Time>()
        .init_resource::<MenuPresentation>()
        .init_resource::<EventKeyState>()
        .init_resource::<KeyboardInputOrder>()
        .init_resource::<KeyboardControlQueue>()
        .init_resource::<ShortcutInputQueue>()
        .init_resource::<ConsumedShortcutInputs>()
        .init_resource::<CaptureEstablishedThisFrame>()
        .insert_resource(MenuKeymap::new(1, keymap.clone()))
        .insert_resource(StudioKeymap {
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
        .add_message::<AudioIntent>()
        .add_message::<ApplyTheme>()
        .add_message::<ThemeWriteRequest>()
        .add_message::<ThemeWriteCompleted>()
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
        .add_systems(Update, route_actions.in_set(ActionRoute))
        .add_systems(Update, route_theme_actions.in_set(ActionRoute))
        .add_systems(
            Update,
            suppress_controls_after_capture
                .after(ActionRoute)
                .before(KeyboardControlSystems),
        )
        .add_systems(Update, apply_audio_intents.in_set(ActionApply))
        .add_systems(Update, report_theme_write_failures);
    }
}

/// Normalise only after focused controls have had the first opportunity to
/// consume a key. Editable and modal authority are still passed into
/// `cosmix_actions::resolve`; there is no shortcut-specific focus guard here.
fn capture_unconsumed_key_input(
    input: On<FocusedInput<KeyboardInput>>,
    windows: Query<(), With<Window>>,
    mut event_keys: ResMut<EventKeyState>,
    mut order: ResMut<KeyboardInputOrder>,
    mut queue: ResMut<ShortcutInputQueue>,
) {
    let event_order = order.next_order();
    let raw = event_keys.normalise(&input.input);
    if !windows.contains(input.focused_entity) {
        return;
    }
    if let Some(raw) = raw {
        queue.push(raw, input.input.key_code, event_order);
    }
}

fn capture_establishing_action(action: ActionId) -> bool {
    crate::file_io::handles_menu_action(action) || crate::settings::handles_menu_action(action)
}

fn begin_shortcut_frame(
    mut consumed: ResMut<ConsumedShortcutInputs>,
    mut established: ResMut<CaptureEstablishedThisFrame>,
    mut queue: ResMut<ShortcutInputQueue>,
) {
    consumed.resolved.clear();
    established.candidates.clear();
    established.accepted_after = None;
    established.external_inflight = false;
    queue.begin_frame();
}

fn note_external_capture_requests(
    mut requests: MessageReader<ActionRequest>,
    registry: Res<MenuActionRegistry>,
    mut established: ResMut<CaptureEstablishedThisFrame>,
) {
    established.external_inflight = requests.read().any(|request| {
        request.source != Source::Key
            && capture_establishing_action(request.action)
            && registry.registry().is_enabled(request.action) == Some(true)
    });
}

/// Match Bevy's own focus-loss reset: no held modifiers, buffered input or
/// partial chord may survive while the window is unfocused.
fn reset_keyboard_state_on_focus_lost(
    mut lost: MessageReader<KeyboardFocusLost>,
    mut event_keys: ResMut<EventKeyState>,
    mut queue: ResMut<ShortcutInputQueue>,
    mut keymap: ResMut<StudioKeymap>,
    mut consumed: ResMut<ConsumedShortcutInputs>,
    mut controls: ResMut<KeyboardControlQueue>,
) {
    if lost.read().next().is_none() {
        return;
    }
    event_keys.reset();
    queue.pending.clear();
    keymap.state.cancel();
    consumed.resolved.clear();
    controls.clear();
}

/// Resolve normalised keyboard input against the current focus, modal capture
/// and layered keymap. Chord progress and timeout remain in this one system.
fn keyboard_actions(mut params: KeyboardActionParams) {
    params.queue.drop_starved();
    let now = Tick(
        params
            .time
            .elapsed()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX),
    );
    if params.established.is_marked() || params.established.external_inflight {
        if params.keymap.state.is_pending() {
            let context = params.resolution_focus.context();
            let keymap = &mut *params.keymap;
            let expired = resolve_timeout(&context, &keymap.keymap, &mut keymap.state, now);
            for diagnostic in expired.diagnostics {
                report_resolve_diagnostic(&diagnostic);
            }
            if !expired.actions.is_empty() {
                eprintln!(
                    "studio: discarded expired chord action while capture ingress was pending"
                );
            }
        }
        return;
    }
    let mut emitted_in_batch = false;
    while let Some(input) = params.queue.pending.pop_front() {
        let context = params.resolution_focus.context();
        let keymap = &mut *params.keymap;
        let resolved = resolve(input.raw, &context, &keymap.keymap, &mut keymap.state, now);
        let claimed = !matches!(
            resolved.outcome,
            ResolveOutcome::NoMatch | ResolveOutcome::IgnoredRelease
        );
        let emitted = emit_resolved(
            resolved,
            &params.registry,
            &mut params.out,
            params.resolution_focus.focus.get(),
            input.order,
            &mut params.established,
            emitted_in_batch,
        );
        if emitted.disposition == EmitDisposition::RequeueCurrent {
            keymap.state.cancel();
            params.queue.pending.push_front(input);
            return;
        }
        emitted_in_batch |= emitted.emitted;
        params.consumed.resolved.push(ShortcutInputOutcome {
            raw: input.raw,
            physical: input.physical,
            claimed,
        });
        if emitted.disposition == EmitDisposition::Stop {
            return;
        }
    }
    if params.keymap.state.is_pending() {
        let context = params.resolution_focus.context();
        let keymap = &mut *params.keymap;
        let resolved = resolve_timeout(&context, &keymap.keymap, &mut keymap.state, now);
        let _ = emit_resolved(
            resolved,
            &params.registry,
            &mut params.out,
            params.resolution_focus.focus.get(),
            0,
            &mut params.established,
            false,
        );
    }
}

fn suppress_controls_after_capture(
    established: Res<CaptureEstablishedThisFrame>,
    mut controls: ResMut<KeyboardControlQueue>,
) {
    if let Some(order) = established.accepted_after() {
        controls.discard_after(order);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EmitDisposition {
    Continue,
    Stop,
    RequeueCurrent,
}

struct EmitResult {
    disposition: EmitDisposition,
    emitted: bool,
}

fn emit_resolved(
    resolved: Resolved,
    registry: &MenuActionRegistry,
    out: &mut MessageWriter<ActionRequest>,
    invocation_focus: Option<Entity>,
    input_order: u64,
    established: &mut CaptureEstablishedThisFrame,
    defer_disabled: bool,
) -> EmitResult {
    for diagnostic in resolved.diagnostics {
        report_resolve_diagnostic(&diagnostic);
    }
    let action_count = resolved.actions.len();
    let mut emitted = false;
    for (index, action) in resolved.actions.into_iter().enumerate() {
        let args = Default::default();
        if let Err(error) = registry.registry().invoke(action, &args) {
            if defer_disabled && matches!(&error, RegistryError::Disabled(_)) {
                return EmitResult {
                    disposition: EmitDisposition::RequeueCurrent,
                    emitted,
                };
            }
            eprintln!("studio: shortcut action {action} rejected: {error}");
            continue;
        }
        out.write(ActionRequest {
            action,
            source: Source::Key,
            args,
            invocation_focus,
        });
        emitted = true;
        if capture_establishing_action(action) {
            established.candidate(action, input_order);
            return EmitResult {
                disposition: if index + 1 < action_count {
                    EmitDisposition::RequeueCurrent
                } else {
                    EmitDisposition::Stop
                },
                emitted,
            };
        }
    }
    EmitResult {
        disposition: EmitDisposition::Continue,
        emitted,
    }
}

fn report_resolve_diagnostic(diagnostic: &ResolveDiagnostic) {
    eprintln!("studio: keymap resolution diagnostic: {diagnostic:?}");
}

fn packaged_keymap() -> Keymap {
    parse_keymap(STUDIO_DEFAULT_KEYMAP_MIX).expect("checked-in Studio keymap must stay valid")
}

fn load_effective_keymap(custom_path: Option<&Path>) -> Result<Keymap, String> {
    let mut keymap = packaged_keymap();
    let Some(path) = custom_path else {
        return Ok(keymap);
    };
    if !path.exists() {
        return Ok(keymap);
    }
    match load_keymap(path) {
        Ok(custom) => {
            keymap.chord_timeout_ms = custom.chord_timeout_ms;
            keymap.custom = custom.custom;
            keymap.validate().map_err(|error| {
                format!("invalid Studio keymap overlay {}: {error}", path.display())
            })?;
        }
        Err(error) => {
            return Err(format!(
                "loading Studio keymap overlay {}: {error}",
                path.display()
            ));
        }
    }
    Ok(keymap)
}

fn reload_keymap_on_focus(
    mut focused: MessageReader<WindowFocused>,
    mut state: ResMut<StudioKeymap>,
    mut menu: ResMut<MenuKeymap>,
) {
    if !focused.read().any(|event| event.focused) {
        return;
    }
    let reloaded = match load_effective_keymap(state.custom_path.as_deref()) {
        Ok(reloaded) => reloaded,
        Err(error) => {
            eprintln!("studio: {error}; keeping current keymap");
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

fn register(
    registry: &mut ActionRegistry,
    id: ActionId,
    label: &str,
    category: &str,
    icon_name: &str,
    allowed_sources: ActionSources,
    enabled: Arc<dyn Fn() -> bool + Send + Sync>,
) {
    // Bevy ECS systems own application side effects. The registry handler is
    // the nullary validation hook used before keyboard requests enter that bus;
    // enabled predicates remain live authority for both keyboard and menus.
    registry
        .register(
            ActionMeta {
                id,
                label: label.to_owned(),
                args_schema: ArgsSchema::default(),
                category: Some(category.to_owned()),
                icon_name: Some(icon_name.to_owned()),
                description: None,
                interactive: None,
                allowed_sources,
            },
            Arc::new(|_| Ok(())),
            enabled,
        )
        .expect("Studio action ids and schemas are static and unique");
}

fn register_interactive(
    registry: &mut ActionRegistry,
    id: ActionId,
    label: &str,
    category: &str,
    icon_name: &str,
    direct_verb: Option<&str>,
    enabled: Arc<dyn Fn() -> bool + Send + Sync>,
) {
    registry
        .register(
            ActionMeta {
                id,
                label: label.to_owned(),
                args_schema: ArgsSchema::default(),
                category: Some(category.to_owned()),
                icon_name: Some(icon_name.to_owned()),
                description: None,
                interactive: Some(InteractiveAction {
                    direct_verb: direct_verb.map(str::to_owned),
                }),
                allowed_sources: ActionSources::default(),
            },
            Arc::new(|_| Ok(())),
            enabled,
        )
        .expect("Studio action ids and schemas are static and unique");
}

fn flag_enabled(
    flag: &Arc<AvailabilityFlags>,
    select: fn(&AvailabilityFlags) -> &AtomicBool,
) -> Arc<dyn Fn() -> bool + Send + Sync> {
    let flags = Arc::clone(flag);
    Arc::new(move || select(&flags).load(Ordering::Relaxed))
}

fn build_action_registry(availability: &StudioActionAvailability) -> ActionRegistry {
    let mut registry = ActionRegistry::new();
    let always = || Arc::new(|| true) as Arc<dyn Fn() -> bool + Send + Sync>;
    let flags = &availability.flags;

    register(
        &mut registry,
        ids::TRANSPORT_TOGGLE,
        "Play / pause",
        "transport",
        "music",
        ActionSources::BUS,
        always(),
    );
    register(
        &mut registry,
        ids::TRANSPORT_START,
        "Play",
        "transport",
        "music",
        ActionSources::BUS,
        always(),
    );
    register(
        &mut registry,
        ids::TRANSPORT_STOP,
        "Stop",
        "transport",
        "music",
        ActionSources::BUS,
        always(),
    );
    register(
        &mut registry,
        ids::TRANSPORT_PAUSE,
        "Pause",
        "transport",
        "music",
        ActionSources::BUS,
        always(),
    );

    register_interactive(
        &mut registry,
        ids::MENU_SONG_OPEN,
        "Open Song",
        "file",
        "folder-open",
        Some("app.song.load"),
        flag_enabled(flags, |flags| &flags.song),
    );
    for (id, label, icon, direct_verb) in [
        (ids::MENU_SONG_SAVE, "Save Song As", "file-music", None),
        (
            ids::MENU_SF_OPEN,
            "Open SoundFont",
            "folder-open",
            Some("app.soundfont.load"),
        ),
        (ids::MENU_WAV_EXPORT, "Export WAV", "download", None),
    ] {
        register_interactive(
            &mut registry,
            id,
            label,
            "file",
            icon,
            direct_verb,
            flag_enabled(flags, |flags| &flags.song),
        );
    }
    for (id, label, icon, direct_verb) in [
        (
            ids::MENU_SESSION_SAVE,
            "Save Session As",
            "hard-drive",
            None,
        ),
        (
            ids::MENU_SESSION_EXPORT_WAV,
            "Export Session Audio as WAV",
            "download",
            None,
        ),
        (
            ids::MENU_SESSION_EXPORT_FLAC,
            "Export Session Audio as FLAC",
            "download",
            None,
        ),
    ] {
        register_interactive(
            &mut registry,
            id,
            label,
            "file",
            icon,
            direct_verb,
            flag_enabled(flags, |flags| &flags.session),
        );
    }
    register_interactive(
        &mut registry,
        ids::MENU_SETTINGS,
        "Settings",
        "application",
        "menu",
        None,
        always(),
    );
    register(
        &mut registry,
        ids::MENU_VIEW_MIXER,
        "Mixer",
        "view",
        "grid",
        ActionSources::BUS,
        always(),
    );
    register(
        &mut registry,
        ids::MENU_VIEW_WAVES,
        "Waves",
        "view",
        "file-music",
        ActionSources::BUS,
        flag_enabled(flags, |flags| &flags.waves),
    );
    register(
        &mut registry,
        ids::MENU_VIEW_ROLL,
        "Piano Roll",
        "view",
        "music",
        ActionSources::BUS,
        flag_enabled(flags, |flags| &flags.roll),
    );
    for (id, label, icon) in [
        (ids::MENU_ZOOM_IN, "Zoom In", "search"),
        (ids::MENU_ZOOM_OUT, "Zoom Out", "search"),
        (ids::MENU_ZOOM_FIT, "Zoom Fit", "move-horizontal"),
    ] {
        register(
            &mut registry,
            id,
            label,
            "view",
            icon,
            ActionSources::BUS,
            flag_enabled(flags, |flags| &flags.zoom),
        );
    }
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
            ActionSources::BUS,
            always(),
        );
    }
    register_interactive(
        &mut registry,
        ids::SETTINGS_CLOSE,
        "Close Settings",
        "settings",
        "menu",
        None,
        always(),
    );
    register_interactive(
        &mut registry,
        ids::SETTINGS_ACTIVATE,
        "Activate Settings Control",
        "settings",
        "menu",
        None,
        always(),
    );
    registry
}

#[derive(SystemParam)]
struct AvailabilityParams<'w> {
    editor: Option<Res<'w, SongEditor>>,
    region_editor: Option<Res<'w, RegionEditor>>,
    lanes: Option<Res<'w, WaveLanes>>,
    active: Option<Res<'w, ActiveView>>,
    theme: Option<Res<'w, ThemeState>>,
    registry: ResMut<'w, MenuActionRegistry>,
    availability: ResMut<'w, StudioActionAvailability>,
    presentation: ResMut<'w, MenuPresentation>,
}

fn sync_action_availability(mut params: AvailabilityParams) {
    let snapshot = AvailabilitySnapshot {
        song: params.editor.is_some(),
        session: params.region_editor.is_some(),
        waves: params.editor.is_some() || params.lanes.is_some(),
        roll: params.editor.is_some(),
        zoom: params.active.as_deref() == Some(&ActiveView::Waves),
    };
    let theme_revision = params.theme.as_deref().map_or(0, |theme| theme.revision);
    let theme_changed = params.availability.theme_revision != theme_revision;
    if params.availability.initialised && snapshot == params.availability.snapshot && !theme_changed
    {
        return;
    }
    if !params.availability.initialised || snapshot != params.availability.snapshot {
        params.availability.snapshot = snapshot;
        params.availability.initialised = true;
        params
            .availability
            .flags
            .song
            .store(snapshot.song, Ordering::Relaxed);
        params
            .availability
            .flags
            .session
            .store(snapshot.session, Ordering::Relaxed);
        params
            .availability
            .flags
            .waves
            .store(snapshot.waves, Ordering::Relaxed);
        params
            .availability
            .flags
            .roll
            .store(snapshot.roll, Ordering::Relaxed);
        params
            .availability
            .flags
            .zoom
            .store(snapshot.zoom, Ordering::Relaxed);
        params.registry.mark_enabled_changed();
    }
    params.availability.theme_revision = theme_revision;
    params.availability.revision = params.availability.revision.saturating_add(1);
    let mut markers = Vec::with_capacity(2);
    if let Some(theme) = params.theme.as_deref() {
        if theme.mode == Mode::Dark {
            markers.push((theme_ids::MODE_TOGGLE, MenuItemMarker::Checked));
        }
        markers.push((scheme_action(theme.scheme), MenuItemMarker::Radio));
    }

    *params.presentation = MenuPresentation::from_registry(
        params.availability.revision,
        params.registry.registry(),
        markers,
    );
}

/// A leading provenance badge for the status line, so the operator can see WHO
/// drove a transport change — a remote agent over Bus (or MIDI/OSC later) leads
/// with a badge; their own keyboard/mouse is unlabelled (they did it and know).
/// ASCII only: the UI font has no `·`/media glyphs (they render as tofu).
pub(crate) fn source_prefix(source: Source) -> &'static str {
    match source {
        Source::Bus => "[BUS]  ",
        Source::Midi => "[MIDI]  ",
        Source::Osc => "[OSC]  ",
        Source::Key | Source::Mouse | Source::Menu => "",
    }
}

fn route_theme_actions(
    mut selections: MessageReader<ThemeSelectionRequest>,
    mut requests: MessageReader<ActionRequest>,
    state: Option<Res<ThemeState>>,
    mut apply: MessageWriter<ApplyTheme>,
    mut persist: MessageWriter<ThemeWriteRequest>,
) {
    let Some(state) = state else {
        return;
    };
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
        working = next;
        changed = true;
    }
    if changed {
        queue_theme_change(working.0, working.1, &mut apply, &mut persist);
    }
}

fn theme_selection(action: ActionId, current: (Scheme, Mode)) -> Option<(Scheme, Mode)> {
    let (current_scheme, current_mode) = current;
    if action == theme_ids::MODE_TOGGLE {
        return Some((
            current_scheme,
            if current_mode == Mode::Dark {
                Mode::Light
            } else {
                Mode::Dark
            },
        ));
    }
    let scheme = match action {
        theme_ids::SCHEME_OCEAN => Scheme::Ocean,
        theme_ids::SCHEME_CRIMSON => Scheme::Crimson,
        theme_ids::SCHEME_STONE => Scheme::Stone,
        theme_ids::SCHEME_FOREST => Scheme::Forest,
        theme_ids::SCHEME_SUNSET => Scheme::Sunset,
        theme_ids::SCHEME_MONO => Scheme::Mono,
        _ => return None,
    };
    Some((scheme, current_mode))
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

pub(crate) fn queue_theme_change(
    scheme: Scheme,
    mode: Mode,
    apply: &mut MessageWriter<ApplyTheme>,
    persist: &mut MessageWriter<ThemeWriteRequest>,
) {
    let app_config =
        ctk::app_dirs::AppDirs::resolve(crate::IDENTITY.slug).map(|dirs| dirs.config());
    apply.write(ApplyTheme(resolve_app_theme_with_selection(
        app_config.as_deref(),
        scheme,
        mode,
    )));
    persist.write(ThemeWriteRequest::shared(scheme, mode));
}

fn report_theme_write_failures(
    mut completed: MessageReader<ThemeWriteCompleted>,
    mut status: Option<ResMut<StatusLine>>,
) {
    for result in completed.read() {
        let Err(error) = &result.result else {
            continue;
        };
        eprintln!("studio: saving theme selection failed: {error}");
        if let Some(status) = status.as_deref_mut() {
            status.error(format!("Theme preference was not saved: {error}"));
        }
    }
}

/// Map UI-semantic actions to engine-semantic intents, and narrate each on the
/// status line (transient toast + persistent footer echo) tagged with its
/// ingress provenance. This is where policy lives (e.g. a toggle resolves to
/// Play-or-Stop against transport state — but that resolution is the sink's,
/// since it owns the state read), so a toggle is narrated as the neutral
/// "Play / pause".
fn route_actions(
    mut reqs: MessageReader<ActionRequest>,
    mut audio: MessageWriter<AudioIntent>,
    mut status: Option<ResMut<StatusLine>>,
) {
    for req in reqs.read() {
        let verb = match req.action {
            ids::TRANSPORT_TOGGLE => {
                audio.write(AudioIntent::Toggle);
                "Play / pause"
            }
            ids::TRANSPORT_START => {
                audio.write(AudioIntent::Play);
                "Play"
            }
            // Pause halts and keeps the playhead; Stop also rewinds. Both fold
            // through the desired-state reducer in `apply_audio_intents`.
            ids::TRANSPORT_PAUSE => {
                audio.write(AudioIntent::Stop);
                "Pause"
            }
            ids::TRANSPORT_STOP => {
                audio.write(AudioIntent::Stop);
                audio.write(AudioIntent::Seek(0.0));
                "Stop"
            }
            _ => continue,
        };
        if let Some(status) = status.as_deref_mut() {
            status.info(format!("{}{verb}", source_prefix(req.source)));
        }
    }
}

/// The ONE system that drives the transport. Everything else emits
/// [`AudioIntent`]; this maps intents onto ctk's write paths (footer-button
/// `Activate` for play/stop state, `TransportSeekRequest` for position).
///
/// Toggle resolution folds against CTK's desired-state projection, which sees
/// queued, in-flight and retrying writes before acknowledged state. This keeps
/// rapid cross-frame input ordered without duplicating CTK's command lifecycle.
/// Redundant writes are suppressed only against an acknowledged value: a
/// provisional desired value is reaffirmed so a later rejection cannot strand
/// the folded intent.
///
/// Still deferred: bank + stop + seek are not one RT-atomic operation; an
/// active scrub gesture rejects Reset's seek; and Reset is still dropped if
/// the footer is absent or CTK is not ready (ADR finding #5).
fn apply_audio_intents(
    mut intents: MessageReader<AudioIntent>,
    transport_state: TransportState,
    buttons: Option<Res<TransportButtons>>,
    mut seeks: MessageWriter<TransportSeekRequest>,
    mut commands: Commands,
) {
    let batch: Vec<_> = intents.read().copied().collect();
    let desired = transport_state.desired();
    let baseline = desired.map(|desired| desired.playing);
    let provisional = desired.map(|desired| desired.provisional).unwrap_or(false);
    let reduced = reduce_audio_intents(batch, baseline, provisional);

    if reduced.force_stop {
        let Some(buttons) = buttons.as_deref() else {
            // Reset is a stop+rewind barrier: never apply only its seek. A
            // retained Reset for this startup/not-ready window remains ADR #5.
            return;
        };
        trigger(&mut commands, buttons.stop);
    } else if let Some(playing) = reduced.state_target {
        if let Some(buttons) = buttons.as_deref() {
            trigger(
                &mut commands,
                if playing { buttons.play } else { buttons.stop },
            );
        }
    }

    for seconds in reduced.seeks {
        seeks.write(TransportSeekRequest { seconds });
    }
}

#[derive(Debug, PartialEq)]
struct ReducedAudioIntents {
    force_stop: bool,
    state_target: Option<bool>,
    seeks: Vec<f64>,
}

/// Fold one frame's ordered transport intents against CTK's effective desired
/// state. State writes collapse to the final value. Equality is suppressed
/// only for an acknowledged baseline; a provisional baseline is reaffirmed.
/// Seeks retain their input order.
fn reduce_audio_intents(
    batch: Vec<AudioIntent>,
    baseline: Option<bool>,
    provisional: bool,
) -> ReducedAudioIntents {
    if batch
        .iter()
        .any(|intent| matches!(intent, AudioIntent::Reset))
    {
        return ReducedAudioIntents {
            force_stop: true,
            state_target: None,
            seeks: vec![0.0],
        };
    }

    let mut desired = baseline;
    let mut saw_state_intent = false;
    let mut seeks = Vec::new();
    for intent in batch {
        match intent {
            AudioIntent::Play => {
                saw_state_intent = true;
                if desired.is_some() {
                    desired = Some(true);
                }
            }
            AudioIntent::Stop => {
                saw_state_intent = true;
                if desired.is_some() {
                    desired = Some(false);
                }
            }
            AudioIntent::Toggle => {
                saw_state_intent = true;
                if let Some(playing) = desired.as_mut() {
                    *playing = !*playing;
                }
            }
            AudioIntent::Seek(seconds) => seeks.push(seconds),
            AudioIntent::Reset => unreachable!("reset dominance handled before the fold"),
        }
    }

    ReducedAudioIntents {
        force_stop: false,
        state_target: if saw_state_intent {
            desired.filter(|desired| Some(*desired) != baseline || provisional)
        } else {
            None
        },
        seeks,
    }
}

/// Fire a footer transport button through its own `Activate` observer — the
/// same path a pointer click takes, so every route reaches the engine through
/// one revisioned write, no side channel.
fn trigger(commands: &mut Commands, entity: Entity) {
    commands.trigger(bevy::ui_widgets::Activate { entity });
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::{Arc, Mutex};

    use bevy::app::TaskPoolPlugin;
    use bevy::feathers::theme::UiTheme;
    use bevy::input::keyboard::Key;
    use bevy::input::{ButtonInput, ButtonState, InputPlugin};
    use bevy::input_focus::{InputDispatchPlugin, InputFocus, InputFocusPlugin};
    use bevy::text::EditableText;
    use bevy::ui::Checked;
    use bevy::ui_widgets::{ButtonPlugin, CheckboxPlugin};
    use bevy::window::{PrimaryWindow, Window, WindowPlugin};
    use bevy::winit::WinitPlugin;
    use cosmix_mixer_schema::{LeafValue, WriteAck, WriteReject, WriteRequest, WriteResponse};
    use ctk::prelude::{
        action_button, fader, prepare_action_invocation, toggle_button, transport_is_playing,
        ChangedEvent, ControlChange, ControlRange, CtkWidgetsPlugin, FileRequest, FileRequestId,
        FileRequesterPlugin, FileRequesterSystems, InboundRequest, MixerBinding,
        MixerConnectionState, MixerTransport, ModalCapture, MusicdMixerPlugin, MusicdMixerState,
        NumericControlProps, TransportEvent, TransportMessage, TransportReply, ValueMapping,
    };

    #[derive(Resource)]
    struct PendingFileRequest(Option<FileRequest>);

    #[derive(Resource, Default)]
    struct SeenActions(Vec<ActionId>);

    #[derive(Resource, Default)]
    struct SeenIntents(Vec<AudioIntent>);

    #[derive(Resource, Default)]
    struct SeenControlChanges(Vec<Entity>);

    #[derive(Resource, Default)]
    struct BoardRuns(usize);

    #[derive(Resource, Default)]
    struct SeenBoardKeys(Vec<KeyCode>);

    fn produce_file_request(
        mut pending: ResMut<PendingFileRequest>,
        mut requests: MessageWriter<FileRequest>,
    ) {
        if let Some(request) = pending.0.take() {
            requests.write(request);
        }
    }

    fn record_actions(mut requests: MessageReader<ActionRequest>, mut seen: ResMut<SeenActions>) {
        seen.0.extend(requests.read().map(|request| request.action));
    }

    fn record_intents(mut intents: MessageReader<AudioIntent>, mut seen: ResMut<SeenIntents>) {
        seen.0.extend(intents.read().copied());
    }

    fn record_control_changes(change: On<ControlChange>, mut seen: ResMut<SeenControlChanges>) {
        seen.0.push(change.source);
    }

    fn record_board_run(mut runs: ResMut<BoardRuns>) {
        runs.0 += 1;
    }

    fn record_unconsumed_board_s(
        consumed: Res<ConsumedShortcutInputs>,
        mut runs: ResMut<BoardRuns>,
    ) {
        runs.0 += consumed
            .unclaimed_presses()
            .filter(|event| event.physical == KeyCode::KeyS)
            .count();
    }

    fn record_ordered_board_keys(
        consumed: Res<ConsumedShortcutInputs>,
        mut seen: ResMut<SeenBoardKeys>,
    ) {
        seen.0
            .extend(consumed.unclaimed_presses().map(|event| event.physical));
    }

    fn establish_settings_capture(
        mut requests: MessageReader<ActionRequest>,
        mut capture: ResMut<ModalCapture>,
        mut established: ResMut<CaptureEstablishedThisFrame>,
    ) {
        for request in requests
            .read()
            .filter(|request| request.action == ids::MENU_SETTINGS)
        {
            capture.acquire(
                ctk::prelude::ModalCaptureOwner {
                    kind: "studio.settings",
                    entity: None,
                },
                ctk::prelude::ModalCaptureLayer(900),
            );
            established.mark_request(request);
        }
    }

    fn queue_raw(app: &mut App, raw: cosmix_actions::RawInput, physical: KeyCode) {
        let order = app
            .world_mut()
            .resource_mut::<KeyboardInputOrder>()
            .next_order();
        app.world_mut()
            .resource_mut::<ShortcutInputQueue>()
            .push(raw, physical, order);
    }

    fn send_space(app: &mut App, window: Entity) {
        app.world_mut().write_message(KeyboardInput {
            key_code: KeyCode::Space,
            logical_key: Key::Space,
            state: ButtonState::Pressed,
            text: Some(" ".into()),
            repeat: false,
            window,
        });
    }

    fn send_key(
        app: &mut App,
        window: Entity,
        key_code: KeyCode,
        logical_key: Key,
        text: Option<&str>,
    ) {
        send_key_state(
            app,
            window,
            key_code,
            logical_key,
            text,
            ButtonState::Pressed,
        );
    }

    fn send_key_state(
        app: &mut App,
        window: Entity,
        key_code: KeyCode,
        logical_key: Key,
        text: Option<&str>,
        state: ButtonState,
    ) {
        app.world_mut().write_message(KeyboardInput {
            key_code,
            logical_key,
            state,
            text: text.map(Into::into),
            repeat: false,
            window,
        });
    }

    fn focused_action_test_app() -> (App, Entity) {
        let mut app = App::new();
        app.add_plugins((
            TaskPoolPlugin::default(),
            InputPlugin,
            InputFocusPlugin,
            InputDispatchPlugin,
            ButtonPlugin,
            CheckboxPlugin,
            CtkWidgetsPlugin,
        ))
        .init_resource::<ModalCapture>()
        .init_resource::<MusicdMixerState>()
        .init_resource::<SeenActions>()
        .init_resource::<SeenIntents>()
        .init_resource::<SeenControlChanges>()
        .init_resource::<BoardRuns>()
        .init_resource::<SeenBoardKeys>()
        .add_message::<TransportSeekRequest>()
        .add_plugins(ActionPlugin)
        .add_observer(record_control_changes)
        .add_systems(Update, record_actions.after(ActionRoute))
        .add_systems(
            Update,
            record_intents.after(ActionRoute).before(ActionApply),
        );
        let window = app
            .world_mut()
            .spawn((Window::default(), PrimaryWindow))
            .id();
        app.finish();
        app.cleanup();
        app.update();
        (app, window)
    }

    fn custom_keymap_source(chord: &str) -> String {
        format!(
            r#"{{
  version: 1,
  chord_timeout_ms: 750,
  defaults: [],
  custom: [
    {{ action: "song-open", chord: ["{chord}"], scope: "global", repeat: "ignore", allow_in_editable: true }}
  ]
}}"#
        )
    }

    fn temporary_keymap(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "studio-keymap-{}-{name}.conf.mix",
            std::process::id()
        ))
    }

    #[test]
    fn registry_covers_canonical_menu_transport_and_modal_actions() {
        let availability = StudioActionAvailability::default();
        let registry = build_action_registry(&availability);
        let registered: Vec<_> = registry.iter_metadata().map(|meta| meta.id).collect();
        let expected: Vec<_> = [
            ids::TRANSPORT_TOGGLE,
            ids::TRANSPORT_START,
            ids::TRANSPORT_STOP,
            ids::TRANSPORT_PAUSE,
            ids::SETTINGS_CLOSE,
            ids::SETTINGS_ACTIVATE,
        ]
        .into_iter()
        .chain(ids::MENU_ACTION_IDS)
        .collect();
        for action in &expected {
            let meta = registry
                .metadata(*action)
                .unwrap_or_else(|| panic!("missing registry metadata for {action}"));
            assert!(!meta.label.is_empty());
            assert!(meta.icon_name.is_some());
            assert!(registered.contains(action));
        }
        assert_eq!(registered.len(), expected.len());
        assert_eq!(registry.is_enabled(ids::MENU_SONG_OPEN), Some(false));
        assert_eq!(registry.is_enabled(ids::MENU_SONG_SAVE), Some(false));
        availability.flags.song.store(true, Ordering::Relaxed);
        assert_eq!(registry.is_enabled(ids::MENU_SONG_OPEN), Some(true));
        assert_eq!(
            registry
                .metadata(ids::MENU_SONG_OPEN)
                .and_then(|meta| meta.interactive.as_ref())
                .and_then(|interactive| interactive.direct_verb.as_deref()),
            Some("app.song.load")
        );
        assert!(registry
            .metadata(ids::TRANSPORT_TOGGLE)
            .unwrap()
            .interactive
            .is_none());
        assert_eq!(registry.is_enabled(ids::MENU_SONG_SAVE), Some(true));

        let bus_actions: Vec<_> = [
            ids::TRANSPORT_TOGGLE,
            ids::TRANSPORT_START,
            ids::TRANSPORT_STOP,
            ids::TRANSPORT_PAUSE,
            ids::MENU_VIEW_MIXER,
            ids::MENU_VIEW_WAVES,
            ids::MENU_VIEW_ROLL,
            ids::MENU_ZOOM_IN,
            ids::MENU_ZOOM_OUT,
            ids::MENU_ZOOM_FIT,
        ]
        .into_iter()
        .chain(theme_ids::ACTION_IDS)
        .collect();
        for meta in registry.iter_metadata() {
            assert_eq!(
                meta.allowed_sources.bus,
                bus_actions.contains(&meta.id),
                "unexpected Bus source policy for {}",
                meta.id
            );
            if meta.interactive.is_some() {
                assert!(!meta.allowed_sources.bus);
            }
        }
        for action in [
            ids::MENU_SETTINGS,
            ids::SETTINGS_CLOSE,
            ids::SETTINGS_ACTIVATE,
        ] {
            let interactive = registry
                .metadata(action)
                .unwrap()
                .interactive
                .as_ref()
                .unwrap();
            assert!(interactive.direct_verb.is_none());
        }
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
        let mut state = ThemeState::default();
        state.scheme = Scheme::Ocean;
        state.mode = Mode::Dark;
        let mut app = App::new();
        app.add_message::<ThemeSelectionRequest>()
            .add_message::<ActionRequest>()
            .add_message::<ApplyTheme>()
            .add_message::<ThemeWriteRequest>()
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
    fn user_custom_layer_replaces_packaged_binding_without_replacing_defaults() {
        let path = temporary_keymap("overlay");
        std::fs::write(&path, custom_keymap_source("Ctrl+P")).unwrap();

        let keymap = load_effective_keymap(Some(&path)).unwrap();

        assert_eq!(
            keymap.binding_for(ids::MENU_SONG_OPEN).as_deref(),
            Some("Ctrl+P")
        );
        assert_eq!(
            keymap.binding_for(ids::TRANSPORT_TOGGLE).as_deref(),
            Some("Space")
        );
        assert_eq!(keymap.chord_timeout_ms, 750);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn focus_gain_reloads_keymap_and_menu_revision_together() {
        let path = temporary_keymap("reload");
        std::fs::write(&path, custom_keymap_source("Ctrl+P")).unwrap();
        let initial = packaged_keymap();
        let mut app = App::new();
        app.add_message::<WindowFocused>()
            .insert_resource(MenuKeymap::new(4, initial.clone()))
            .insert_resource(StudioKeymap {
                keymap: initial,
                state: ResolveState::default(),
                revision: 4,
                custom_path: Some(path.clone()),
            })
            .add_systems(Update, reload_keymap_on_focus);

        app.world_mut().write_message(WindowFocused {
            window: Entity::PLACEHOLDER,
            focused: true,
        });
        app.update();

        let state = app.world().resource::<StudioKeymap>();
        let menu = app.world().resource::<MenuKeymap>();
        assert_eq!(state.revision, 5);
        assert_eq!(menu.revision(), 5);
        assert_eq!(
            state.keymap.binding_for(ids::MENU_SONG_OPEN).as_deref(),
            Some("Ctrl+P")
        );
        assert_eq!(
            menu.keymap().binding_for(ids::MENU_SONG_OPEN).as_deref(),
            Some("Ctrl+P")
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn capture_establishing_menu_ingress_defers_same_frame_keyboard() {
        let (mut app, _) = focused_action_test_app();
        app.add_systems(Update, establish_settings_capture.in_set(ActionRoute));
        app.world_mut().write_message(ActionRequest {
            action: ids::MENU_SETTINGS,
            source: Source::Menu,
            args: Default::default(),
            invocation_focus: None,
        });
        queue_raw(
            &mut app,
            cosmix_actions::RawInput::pressed(
                cosmix_actions::Key::Space,
                cosmix_actions::Modifiers::NONE,
            ),
            KeyCode::Space,
        );

        app.update();
        assert!(app.world().resource::<ModalCapture>().is_captured());
        assert!(!app
            .world()
            .resource::<SeenActions>()
            .0
            .contains(&ids::TRANSPORT_TOGGLE));

        // The retained Space is now resolved against the modal context and is
        // suppressed rather than replayed onto transport.
        app.update();
        assert!(!app
            .world()
            .resource::<SeenActions>()
            .0
            .contains(&ids::TRANSPORT_TOGGLE));
    }

    #[test]
    fn later_same_batch_binding_retries_after_route_enables_it() {
        fn make_waves_available(availability: Res<StudioActionAvailability>) {
            availability.flags.waves.store(true, Ordering::Relaxed);
        }
        fn route_view_and_enable_zoom(
            mut requests: MessageReader<ActionRequest>,
            availability: Res<StudioActionAvailability>,
        ) {
            if requests
                .read()
                .any(|request| request.action == ids::MENU_VIEW_WAVES)
            {
                availability.flags.zoom.store(true, Ordering::Relaxed);
            }
        }

        let (mut app, _) = focused_action_test_app();
        app.add_systems(
            Update,
            make_waves_available
                .after(sync_action_availability)
                .before(keyboard_actions)
                .in_set(ActionProduce),
        )
        .add_systems(Update, route_view_and_enable_zoom.in_set(ActionRoute));
        queue_raw(
            &mut app,
            cosmix_actions::RawInput::pressed(
                cosmix_actions::Key::Character('2'),
                cosmix_actions::Modifiers {
                    control: true,
                    ..Default::default()
                },
            ),
            KeyCode::Digit2,
        );
        queue_raw(
            &mut app,
            cosmix_actions::RawInput::pressed(
                cosmix_actions::Key::Equal,
                cosmix_actions::Modifiers {
                    control: true,
                    ..Default::default()
                },
            ),
            KeyCode::Equal,
        );

        app.update();
        assert!(app
            .world()
            .resource::<SeenActions>()
            .0
            .contains(&ids::MENU_VIEW_WAVES));
        assert_eq!(
            app.world().resource::<ShortcutInputQueue>().pending.len(),
            1
        );

        app.update();
        assert!(app
            .world()
            .resource::<SeenActions>()
            .0
            .contains(&ids::MENU_ZOOM_IN));
        assert!(app
            .world()
            .resource::<ShortcutInputQueue>()
            .pending
            .is_empty());
    }

    #[derive(Resource, Default)]
    struct PortZoomAvailability(Vec<bool>);

    fn record_zoom_availability_at_port(
        registry: Res<MenuActionRegistry>,
        mut seen: ResMut<PortZoomAvailability>,
    ) {
        seen.0.push(
            registry
                .registry()
                .is_enabled(ids::MENU_ZOOM_IN)
                .unwrap_or(false),
        );
    }

    fn route_to_mixer(mut requests: MessageReader<ActionRequest>, mut active: ResMut<ActiveView>) {
        if requests
            .read()
            .any(|request| request.action == ids::MENU_VIEW_MIXER)
        {
            *active = ActiveView::Mixer;
        }
    }

    #[test]
    fn availability_refresh_precedes_port_after_previous_frame_view_change() {
        use ctk::prelude::AppPortSystems;

        let availability = StudioActionAvailability::default();
        let registry = build_action_registry(&availability);
        let mut app = App::new();
        app.insert_resource(availability)
            .insert_resource(MenuActionRegistry::new(registry))
            .insert_resource(MenuPresentation::default())
            .insert_resource(ActiveView::Waves)
            .init_resource::<PortZoomAvailability>()
            .add_message::<ActionRequest>()
            .configure_sets(Update, (ActionProduce, AppPortSystems, ActionRoute).chain())
            .add_systems(Update, sync_action_availability.in_set(ActionProduce))
            .add_systems(
                Update,
                record_zoom_availability_at_port.in_set(AppPortSystems),
            )
            .add_systems(Update, route_to_mixer.in_set(ActionRoute));

        app.world_mut().write_message(ActionRequest {
            action: ids::MENU_VIEW_MIXER,
            source: Source::Key,
            args: Default::default(),
            invocation_focus: None,
        });
        app.update();
        app.update();

        assert_eq!(
            app.world().resource::<PortZoomAvailability>().0,
            [true, false]
        );
        assert_eq!(
            app.world()
                .resource::<MenuActionRegistry>()
                .enabled_revision(),
            2
        );
        let request = InboundRequest {
            connection_generation: 1,
            from: "automation".into(),
            command: "action.invoke".into(),
            headers: std::iter::once(("broker_origin".to_string(), "local".to_string())).collect(),
            body: serde_json::json!({ "id": ids::MENU_ZOOM_IN.as_str() }).to_string(),
            reply_id: Some("zoom".into()),
        };
        let error = prepare_action_invocation(
            &request,
            app.world().resource::<MenuActionRegistry>().registry(),
            false,
        )
        .unwrap_err();
        assert_eq!(error.id, ctk::prelude::ACTION_ERROR_DISABLED);
    }

    #[test]
    fn external_capture_deferral_still_expires_pending_chord() {
        let (mut app, _) = focused_action_test_app();
        let keymap = parse_keymap(
            r#"{
  version: 1,
  chord_timeout_ms: 100,
  defaults: [
    { action: "transport.toggle", chord: ["Ctrl+K"] },
    { action: "song-open", chord: ["Ctrl+K", "Ctrl+C"] }
  ],
  custom: []
}"#,
        )
        .unwrap();
        app.world_mut().resource_mut::<StudioKeymap>().keymap = keymap;
        queue_raw(
            &mut app,
            cosmix_actions::RawInput::pressed(
                cosmix_actions::Key::Character('K'),
                cosmix_actions::Modifiers {
                    control: true,
                    ..Default::default()
                },
            ),
            KeyCode::KeyK,
        );
        app.update();
        assert!(app.world().resource::<StudioKeymap>().state.is_pending());

        app.world_mut().write_message(ActionRequest {
            action: ids::MENU_SETTINGS,
            source: Source::Menu,
            args: Default::default(),
            invocation_focus: None,
        });
        app.world_mut()
            .resource_mut::<Time>()
            .advance_by(std::time::Duration::from_millis(150));
        app.update();

        assert!(!app.world().resource::<StudioKeymap>().state.is_pending());
        assert!(!app
            .world()
            .resource::<SeenActions>()
            .0
            .contains(&ids::TRANSPORT_TOGGLE));
    }

    #[test]
    fn keyboard_modal_action_stops_later_keys_in_the_same_batch() {
        let (mut app, window) = focused_action_test_app();
        app.add_systems(Update, establish_settings_capture.in_set(ActionRoute));
        send_key(&mut app, window, KeyCode::ControlLeft, Key::Control, None);
        send_key(
            &mut app,
            window,
            KeyCode::Comma,
            Key::Character(",".into()),
            Some(","),
        );
        send_key_state(
            &mut app,
            window,
            KeyCode::ControlLeft,
            Key::Control,
            None,
            ButtonState::Released,
        );
        send_space(&mut app, window);

        // Ctrl+, emits settings and stops before the queued Space.
        app.update();
        assert_eq!(
            app.world().resource::<ShortcutInputQueue>().pending.len(),
            1
        );
        assert!(!app
            .world()
            .resource::<SeenActions>()
            .0
            .contains(&ids::TRANSPORT_TOGGLE));

        // Routing accepted settings in the first frame; the retained Space is
        // suppressed against that live capture in the next Produce pass.
        app.update();
        assert!(app.world().resource::<ModalCapture>().is_captured());
        assert!(app
            .world()
            .resource::<ShortcutInputQueue>()
            .pending
            .is_empty());
        assert!(!app
            .world()
            .resource::<SeenActions>()
            .0
            .contains(&ids::TRANSPORT_TOGGLE));
    }

    #[test]
    fn accepted_capture_discards_later_focused_widget_activation() {
        let (mut app, window) = focused_action_test_app();
        app.add_systems(Update, establish_settings_capture.in_set(ActionRoute));
        let mute = app.world_mut().spawn(toggle_button("test-mute")).id();
        app.world_mut()
            .insert_resource(InputFocus::from_entity(mute));

        send_key(&mut app, window, KeyCode::ControlLeft, Key::Control, None);
        send_key(
            &mut app,
            window,
            KeyCode::Comma,
            Key::Character(",".into()),
            Some(","),
        );
        send_key_state(
            &mut app,
            window,
            KeyCode::ControlLeft,
            Key::Control,
            None,
            ButtonState::Released,
        );
        send_space(&mut app, window);
        app.update();

        assert!(app.world().resource::<ModalCapture>().is_captured());
        assert!(!app.world().entity(mute).contains::<Checked>());
        assert!(app.world().resource::<SeenControlChanges>().0.is_empty());
    }

    #[test]
    fn accepted_capture_preserves_earlier_focused_widget_activation() {
        let (mut app, window) = focused_action_test_app();
        app.add_systems(Update, establish_settings_capture.in_set(ActionRoute));
        let mute = app.world_mut().spawn(toggle_button("test-mute")).id();
        app.world_mut()
            .insert_resource(InputFocus::from_entity(mute));

        send_space(&mut app, window);
        send_key(&mut app, window, KeyCode::ControlLeft, Key::Control, None);
        send_key(
            &mut app,
            window,
            KeyCode::Comma,
            Key::Character(",".into()),
            Some(","),
        );
        app.update();

        assert!(app.world().resource::<ModalCapture>().is_captured());
        assert!(app.world().entity(mute).contains::<Checked>());
        assert_eq!(app.world().resource::<SeenControlChanges>().0, [mute]);
    }

    #[test]
    fn rejected_capture_candidate_releases_deferred_board_input() {
        let (mut app, window) = focused_action_test_app();
        app.add_systems(Update, record_unconsumed_board_s.in_set(BoardInputSystems));
        send_key(&mut app, window, KeyCode::ControlLeft, Key::Control, None);
        send_key(
            &mut app,
            window,
            KeyCode::Comma,
            Key::Character(",".into()),
            Some(","),
        );
        send_key_state(
            &mut app,
            window,
            KeyCode::ControlLeft,
            Key::Control,
            None,
            ButtonState::Released,
        );
        send_key(
            &mut app,
            window,
            KeyCode::KeyS,
            Key::Character("s".into()),
            Some("s"),
        );

        // The capture candidate stops this batch, but no consumer accepts it.
        app.update();
        assert_eq!(
            app.world().resource::<ShortcutInputQueue>().pending.len(),
            1
        );
        assert_eq!(app.world().resource::<BoardRuns>().0, 0);

        // With no actual capture marker, the retained plain S reaches the board.
        app.update();
        assert!(app
            .world()
            .resource::<ShortcutInputQueue>()
            .pending
            .is_empty());
        assert_eq!(app.world().resource::<BoardRuns>().0, 1);
    }

    #[test]
    fn explicit_queue_survives_multiple_capture_deferral_frames_then_resolves() {
        let (mut app, _) = focused_action_test_app();
        queue_raw(
            &mut app,
            cosmix_actions::RawInput::pressed(
                cosmix_actions::Key::Space,
                cosmix_actions::Modifiers::NONE,
            ),
            KeyCode::Space,
        );

        for _ in 0..4 {
            // Each candidate is deliberately left unhandled. The queue must
            // survive beyond Bevy's two-frame message-retention window.
            app.world_mut().write_message(ActionRequest {
                action: ids::MENU_SETTINGS,
                source: Source::Menu,
                args: Default::default(),
                invocation_focus: None,
            });
            app.update();
            assert_eq!(
                app.world().resource::<ShortcutInputQueue>().pending.len(),
                1
            );
        }
        app.update();
        assert!(app
            .world()
            .resource::<ShortcutInputQueue>()
            .pending
            .is_empty());
        app.update();
        assert!(app
            .world()
            .resource::<SeenActions>()
            .0
            .contains(&ids::TRANSPORT_TOGGLE));
    }

    #[test]
    fn shortcut_queue_drops_oldest_input_at_capacity() {
        let (mut app, _) = focused_action_test_app();
        for _ in 0..=MAX_DEFERRED_INPUTS {
            queue_raw(
                &mut app,
                cosmix_actions::RawInput::pressed(
                    cosmix_actions::Key::Space,
                    cosmix_actions::Modifiers::NONE,
                ),
                KeyCode::Space,
            );
        }

        assert_eq!(
            app.world().resource::<ShortcutInputQueue>().pending.len(),
            MAX_DEFERRED_INPUTS
        );
    }

    #[test]
    fn shortcut_queue_drops_starved_input_after_frame_limit() {
        let (mut app, _) = focused_action_test_app();
        queue_raw(
            &mut app,
            cosmix_actions::RawInput::pressed(
                cosmix_actions::Key::Space,
                cosmix_actions::Modifiers::NONE,
            ),
            KeyCode::Space,
        );

        for _ in 0..=MAX_DEFERRED_FRAMES {
            app.world_mut().write_message(ActionRequest {
                action: ids::MENU_SETTINGS,
                source: Source::Menu,
                args: Default::default(),
                invocation_focus: None,
            });
            app.update();
        }

        assert!(app
            .world()
            .resource::<ShortcutInputQueue>()
            .pending
            .is_empty());
        assert!(!app
            .world()
            .resource::<SeenActions>()
            .0
            .contains(&ids::TRANSPORT_TOGGLE));
    }

    #[test]
    fn keyboard_focus_loss_cancels_pending_chord_and_its_timeout_fallback() {
        let (mut app, _) = focused_action_test_app();
        let keymap = parse_keymap(
            r#"{
  version: 1,
  chord_timeout_ms: 100,
  defaults: [
    { action: "transport.toggle", chord: ["Ctrl+K"] },
    { action: "song-open", chord: ["Ctrl+K", "Ctrl+C"] }
  ],
  custom: []
}"#,
        )
        .unwrap();
        {
            let mut state = app.world_mut().resource_mut::<StudioKeymap>();
            state.keymap = keymap;
            state.state.cancel();
        }
        queue_raw(
            &mut app,
            cosmix_actions::RawInput::pressed(
                cosmix_actions::Key::Character('K'),
                cosmix_actions::Modifiers {
                    control: true,
                    ..Default::default()
                },
            ),
            KeyCode::KeyK,
        );
        app.update();
        assert!(app.world().resource::<StudioKeymap>().state.is_pending());

        app.world_mut().write_message(KeyboardFocusLost);
        app.world_mut()
            .resource_mut::<Time>()
            .advance_by(std::time::Duration::from_millis(500));
        app.update();

        assert!(!app.world().resource::<StudioKeymap>().state.is_pending());
        assert!(app
            .world()
            .resource::<ShortcutInputQueue>()
            .pending
            .is_empty());
        assert!(!app
            .world()
            .resource::<SeenActions>()
            .0
            .contains(&ids::TRANSPORT_TOGGLE));
    }

    #[test]
    fn focus_loss_after_dispatch_clears_inputs_dispatched_that_frame() {
        let (mut app, window) = focused_action_test_app();
        send_space(&mut app, window);
        app.world_mut().write_message(KeyboardFocusLost);

        app.update();

        assert!(app
            .world()
            .resource::<ShortcutInputQueue>()
            .pending
            .is_empty());
        assert!(app.world().resource::<SeenActions>().0.is_empty());
    }

    #[test]
    fn custom_shortcut_claims_key_before_board_just_pressed_reader() {
        let (mut app, window) = focused_action_test_app();
        app.world()
            .resource::<StudioActionAvailability>()
            .flags
            .song
            .store(true, Ordering::Relaxed);
        let custom = parse_keymap(&custom_keymap_source("S")).unwrap();
        {
            let mut keymap = app.world_mut().resource_mut::<StudioKeymap>();
            keymap.keymap.custom = custom.custom;
            keymap.state.cancel();
        }
        app.add_systems(Update, record_unconsumed_board_s.in_set(BoardInputSystems));
        send_key(
            &mut app,
            window,
            KeyCode::KeyS,
            Key::Character("s".into()),
            Some("s"),
        );

        app.update();

        assert!(app
            .world()
            .resource::<SeenActions>()
            .0
            .contains(&ids::MENU_SONG_OPEN));
        assert_eq!(app.world().resource::<BoardRuns>().0, 0);
        assert!(!app
            .world()
            .resource::<ConsumedShortcutInputs>()
            .unclaimed_presses()
            .any(|event| event.physical == KeyCode::KeyS));
    }

    #[test]
    fn unclaimed_releases_never_enter_board_press_iteration() {
        let (mut app, _) = focused_action_test_app();
        queue_raw(
            &mut app,
            cosmix_actions::RawInput {
                key: cosmix_actions::Key::Character('S'),
                modifiers: cosmix_actions::Modifiers::NONE,
                state: cosmix_actions::RawInputState::Released,
                repeat: false,
            },
            KeyCode::KeyS,
        );

        app.update();

        let consumed = app.world().resource::<ConsumedShortcutInputs>();
        assert_eq!(consumed.outcomes().count(), 1);
        assert!(!consumed.outcomes().next().unwrap().claimed);
        assert_eq!(consumed.unclaimed_presses().count(), 0);
    }

    #[test]
    fn duplicate_physical_keys_keep_per_event_claims_and_modifiers() {
        let (mut app, window) = focused_action_test_app();
        app.world()
            .resource::<StudioActionAvailability>()
            .flags
            .song
            .store(true, Ordering::Relaxed);
        let custom = parse_keymap(&custom_keymap_source("Ctrl+Shift+S")).unwrap();
        app.world_mut().resource_mut::<StudioKeymap>().keymap.custom = custom.custom;
        app.add_systems(Update, record_unconsumed_board_s.in_set(BoardInputSystems));

        send_key(
            &mut app,
            window,
            KeyCode::KeyS,
            Key::Character("s".into()),
            Some("s"),
        );
        send_key(&mut app, window, KeyCode::ControlLeft, Key::Control, None);
        send_key(&mut app, window, KeyCode::ShiftLeft, Key::Shift, None);
        send_key(
            &mut app,
            window,
            KeyCode::KeyS,
            Key::Character("s".into()),
            Some("s"),
        );

        app.update();

        assert!(app
            .world()
            .resource::<SeenActions>()
            .0
            .contains(&ids::MENU_SONG_OPEN));
        assert_eq!(app.world().resource::<BoardRuns>().0, 1);
    }

    #[test]
    fn unclaimed_board_presses_are_exposed_in_event_order() {
        let (mut app, _) = focused_action_test_app();
        app.add_systems(Update, record_ordered_board_keys.in_set(BoardInputSystems));
        queue_raw(
            &mut app,
            cosmix_actions::RawInput::pressed(
                cosmix_actions::Key::Delete,
                cosmix_actions::Modifiers::NONE,
            ),
            KeyCode::Delete,
        );
        queue_raw(
            &mut app,
            cosmix_actions::RawInput::pressed(
                cosmix_actions::Key::Character('Z'),
                cosmix_actions::Modifiers {
                    control: true,
                    ..Default::default()
                },
            ),
            KeyCode::KeyZ,
        );

        app.update();

        assert_eq!(
            app.world().resource::<SeenBoardKeys>().0,
            [KeyCode::Delete, KeyCode::KeyZ]
        );
    }

    #[test]
    fn editable_suppression_claims_ctrl_z_before_board_undo() {
        let (mut app, _) = focused_action_test_app();
        let custom = parse_keymap(
            r#"{
  version: 1,
  chord_timeout_ms: 750,
  defaults: [],
  custom: [
    { action: "song-open", chord: ["Ctrl+Z"], scope: "global", repeat: "ignore", allow_in_editable: false }
  ]
}"#,
        )
        .unwrap();
        app.world_mut().resource_mut::<StudioKeymap>().keymap.custom = custom.custom;
        let editable = app.world_mut().spawn(EditableText::new("")).id();
        app.world_mut()
            .insert_resource(InputFocus::from_entity(editable));
        queue_raw(
            &mut app,
            cosmix_actions::RawInput::pressed(
                cosmix_actions::Key::Character('Z'),
                cosmix_actions::Modifiers {
                    control: true,
                    ..Default::default()
                },
            ),
            KeyCode::KeyZ,
        );

        app.update();

        assert!(app.world().resource::<SeenActions>().0.is_empty());
        let outcome = app
            .world()
            .resource::<ConsumedShortcutInputs>()
            .outcomes()
            .find(|event| event.physical == KeyCode::KeyZ)
            .unwrap();
        assert!(outcome.claimed);
        assert!(!app
            .world()
            .resource::<ConsumedShortcutInputs>()
            .unclaimed_presses()
            .any(|event| event.physical == KeyCode::KeyZ));
    }

    #[test]
    fn resolver_routes_escape_and_enter_only_to_active_settings_modal() {
        let (mut app, _) = focused_action_test_app();
        {
            let mut capture = app.world_mut().resource_mut::<ModalCapture>();
            capture.acquire(
                ctk::prelude::ModalCaptureOwner {
                    kind: "studio.settings",
                    entity: None,
                },
                ctk::prelude::ModalCaptureLayer(900),
            );
        }
        for key in [cosmix_actions::Key::Escape, cosmix_actions::Key::Enter] {
            queue_raw(
                &mut app,
                cosmix_actions::RawInput::pressed(key, cosmix_actions::Modifiers::NONE),
                match key {
                    cosmix_actions::Key::Escape => KeyCode::Escape,
                    cosmix_actions::Key::Enter => KeyCode::Enter,
                    _ => unreachable!(),
                },
            );
        }

        app.update();

        assert_eq!(
            app.world().resource::<SeenActions>().0,
            [ids::SETTINGS_CLOSE, ids::SETTINGS_ACTIVATE]
        );
    }

    #[derive(Resource, Default)]
    struct EmitResetAndToggle(bool);

    fn emit_reset_and_toggle(
        mut emit: ResMut<EmitResetAndToggle>,
        mut intents: MessageWriter<AudioIntent>,
        mut actions: MessageWriter<ActionRequest>,
    ) {
        if !emit.0 {
            return;
        }
        emit.0 = false;
        intents.write(AudioIntent::Reset);
        actions.write(ActionRequest {
            action: ids::TRANSPORT_TOGGLE,
            source: Source::Key,
            args: Default::default(),
            invocation_focus: None,
        });
    }

    #[derive(Default)]
    struct RecordingTransportState {
        writes: Vec<(u64, WriteRequest)>,
        events: Vec<TransportEvent>,
        messages: Vec<TransportMessage>,
    }

    struct RecordingTransport {
        state: Arc<Mutex<RecordingTransportState>>,
    }

    impl MixerTransport for RecordingTransport {
        fn service_name(&self) -> &str {
            "studio-action-order-test"
        }

        fn issue_write(&mut self, request_id: u64, request: &WriteRequest) -> Result<(), String> {
            self.state
                .lock()
                .unwrap()
                .writes
                .push((request_id, request.clone()));
            Ok(())
        }

        fn request_snapshot(&mut self, _request_id: u64) -> Result<(), String> {
            Ok(())
        }

        fn request_position(&mut self, _request_id: u64) -> Result<(), String> {
            Ok(())
        }

        fn poll_events(&mut self, out: &mut Vec<TransportEvent>) {
            out.append(&mut self.state.lock().unwrap().events);
        }

        fn poll_messages(&mut self, out: &mut Vec<TransportMessage>) {
            out.append(&mut self.state.lock().unwrap().messages);
        }

        fn discard_backlog(&mut self) {
            self.state.lock().unwrap().messages.clear();
        }
    }

    fn stopped_transport_action_test_app() -> (App, Arc<Mutex<RecordingTransportState>>) {
        let transport_state = Arc::new(Mutex::new(RecordingTransportState {
            messages: vec![TransportMessage::Changed {
                generation: 0,
                event: ChangedEvent {
                    path: "transport.state".to_string(),
                    revision: 1,
                    value: LeafValue::Enum("stopped".to_string()),
                    source_id: None,
                },
            }],
            ..default()
        }));
        let transport = RecordingTransport {
            state: Arc::clone(&transport_state),
        };

        let mut app = App::new();
        app.add_plugins(bevy::MinimalPlugins)
            .init_resource::<ButtonInput<KeyCode>>()
            .init_resource::<ModalCapture>()
            .add_plugins(CtkWidgetsPlugin)
            .add_plugins(ActionPlugin)
            .add_plugins(MusicdMixerPlugin::with_transport(Box::new(transport)));
        app.finish();
        app.cleanup();

        let play = app
            .world_mut()
            .spawn((
                action_button("test-play", 10.0, 10.0),
                MixerBinding::enum_write("transport.state", "playing"),
            ))
            .id();
        let stop = app
            .world_mut()
            .spawn((
                action_button("test-stop", 10.0, 10.0),
                MixerBinding::enum_write("transport.state", "stopped"),
            ))
            .id();
        app.insert_resource(TransportButtons { play, stop });
        {
            let mut state = app.world_mut().resource_mut::<MusicdMixerState>();
            state.connection = MixerConnectionState::Connected;
            state.ready = true;
        }

        app.update();
        assert!(!transport_is_playing(
            app.world().resource::<MusicdMixerState>()
        ));
        transport_state.lock().unwrap().writes.clear();
        (app, transport_state)
    }

    #[test]
    fn space_transport_toggle_is_suppressed_while_modal_captures() {
        let mut request = FileRequest::open_file(FileRequestId(401), "Open");
        request.initial_directory = Some(std::env::temp_dir());

        let mut app = App::new();
        app.add_plugins((
            TaskPoolPlugin::default(),
            InputPlugin,
            InputFocusPlugin,
            InputDispatchPlugin,
        ))
        .init_resource::<UiTheme>()
        .insert_resource(PendingFileRequest(Some(request)))
        .insert_resource(MusicdMixerState::default())
        .init_resource::<SeenActions>()
        .add_plugins(FileRequesterPlugin)
        .add_message::<TransportSeekRequest>()
        .add_plugins(ActionPlugin)
        .add_systems(
            Update,
            (
                produce_file_request.before(FileRequesterSystems),
                record_actions.after(route_actions),
            ),
        );
        let window = app
            .world_mut()
            .spawn((Window::default(), PrimaryWindow))
            .id();
        app.finish();
        app.cleanup();
        send_space(&mut app, window);

        app.update();

        assert!(app.world().resource::<ModalCapture>().is_captured());
        assert!(app.world().resource::<SeenActions>().0.is_empty());
    }

    #[test]
    fn focused_mute_consumes_space_without_transport_toggle() {
        let (mut app, window) = focused_action_test_app();
        let mute = app.world_mut().spawn(toggle_button("test-mute")).id();
        app.world_mut()
            .insert_resource(InputFocus::from_entity(mute));

        send_space(&mut app, window);
        app.update();

        assert!(app.world().entity(mute).contains::<Checked>());
        assert_eq!(app.world().resource::<SeenControlChanges>().0, [mute]);
        assert!(app.world().resource::<SeenActions>().0.is_empty());
    }

    #[test]
    fn focused_rtz_consumes_space_without_transport_toggle() {
        let (mut app, window) = focused_action_test_app();
        let rtz = app
            .world_mut()
            .spawn(action_button("test-rtz", 46.0, 26.0))
            .id();
        app.world_mut()
            .insert_resource(InputFocus::from_entity(rtz));

        send_space(&mut app, window);
        app.update();

        assert_eq!(app.world().resource::<SeenControlChanges>().0, [rtz]);
        assert!(app.world().resource::<SeenActions>().0.is_empty());
    }

    #[test]
    fn space_without_focus_still_emits_transport_toggle() {
        let (mut app, window) = focused_action_test_app();
        app.world_mut().resource_mut::<InputFocus>().clear();

        send_space(&mut app, window);
        app.update();

        assert_eq!(
            app.world().resource::<SeenActions>().0,
            [ids::TRANSPORT_TOGGLE]
        );
        assert_eq!(
            app.world().resource::<SeenIntents>().0,
            [AudioIntent::Toggle],
            "keyboard request must route in its producing frame"
        );
        assert!(app.world().resource::<SeenControlChanges>().0.is_empty());
    }

    #[test]
    fn focused_fader_leaves_space_for_transport_toggle() {
        let (mut app, window) = focused_action_test_app();
        let fader = app
            .world_mut()
            .spawn(fader(NumericControlProps::new(
                "test-fader",
                0.5,
                ControlRange {
                    min: 0.0,
                    max: 1.0,
                    step: 0.1,
                    detent: None,
                },
                ValueMapping::linear(0.0, 1.0).unwrap(),
            )))
            .id();
        app.world_mut()
            .insert_resource(InputFocus::from_entity(fader));

        send_space(&mut app, window);
        app.update();

        assert_eq!(
            app.world().resource::<SeenActions>().0,
            [ids::TRANSPORT_TOGGLE]
        );
        assert!(app.world().resource::<SeenControlChanges>().0.is_empty());
    }

    #[test]
    fn requester_filename_edits_do_not_reach_board_or_transport() {
        let directory =
            std::env::temp_dir().join(format!("studio-focus-requester-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();

        let mut app = App::new();
        app.add_plugins(
            DefaultPlugins
                .set(WindowPlugin {
                    primary_window: Some(Window::default()),
                    ..default()
                })
                .disable::<WinitPlugin>(),
        )
        .init_resource::<UiTheme>()
        .init_resource::<MusicdMixerState>()
        .init_resource::<SeenActions>()
        .init_resource::<BoardRuns>()
        .add_plugins(FileRequesterPlugin)
        .add_message::<TransportSeekRequest>()
        .add_plugins(ActionPlugin)
        .add_systems(Update, record_actions.after(route_actions))
        .add_systems(Update, record_board_run.in_set(BoardInputSystems));
        app.finish();
        app.cleanup();
        let window = app
            .world_mut()
            .query_filtered::<Entity, With<PrimaryWindow>>()
            .single(app.world())
            .unwrap();

        let mut request = FileRequest::save_file(FileRequestId(402), "Save");
        request.initial_directory = Some(directory.clone());
        request.suggested_name = Some(String::new());
        app.world_mut().write_message(request);
        app.update();

        let filename = app.world().resource::<InputFocus>().get().unwrap();
        assert!(app.world().entity(filename).contains::<EditableText>());
        assert_eq!(app.world().resource::<BoardRuns>().0, 0);

        send_key(&mut app, window, KeyCode::Space, Key::Space, Some(" "));
        app.update();
        assert_eq!(
            app.world()
                .get::<EditableText>(filename)
                .unwrap()
                .value()
                .to_string(),
            " "
        );
        send_key(&mut app, window, KeyCode::Backspace, Key::Backspace, None);
        app.update();
        assert_eq!(
            app.world()
                .get::<EditableText>(filename)
                .unwrap()
                .value()
                .to_string(),
            ""
        );
        send_key(
            &mut app,
            window,
            KeyCode::KeyX,
            Key::Character("x".into()),
            Some("x"),
        );
        app.update();
        send_key(&mut app, window, KeyCode::Home, Key::Home, None);
        app.update();
        send_key(&mut app, window, KeyCode::Delete, Key::Delete, None);
        app.update();

        assert_eq!(
            app.world()
                .get::<EditableText>(filename)
                .unwrap()
                .value()
                .to_string(),
            ""
        );
        assert_eq!(app.world().resource::<BoardRuns>().0, 0);
        assert!(app.world().resource::<SeenActions>().0.is_empty());

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn same_frame_reset_dominates_toggle_and_reaches_ctk_ingress() {
        let transport_state = Arc::new(Mutex::new(RecordingTransportState {
            messages: vec![TransportMessage::Changed {
                generation: 0,
                event: ChangedEvent {
                    path: "transport.state".to_string(),
                    revision: 1,
                    value: LeafValue::Enum("playing".to_string()),
                    source_id: None,
                },
            }],
            ..default()
        }));
        let transport = RecordingTransport {
            state: Arc::clone(&transport_state),
        };

        let mut app = App::new();
        app.add_plugins(bevy::MinimalPlugins)
            .init_resource::<ButtonInput<KeyCode>>()
            .init_resource::<ModalCapture>()
            .init_resource::<EmitResetAndToggle>()
            .add_plugins(CtkWidgetsPlugin)
            // Match production registration order: Studio declares its sets
            // before CTK installs the ingress system that occupies its label.
            .add_plugins(ActionPlugin)
            .add_plugins(MusicdMixerPlugin::with_transport(Box::new(transport)))
            .add_systems(Update, emit_reset_and_toggle.in_set(ActionProduce));
        app.finish();
        app.cleanup();

        let play = app
            .world_mut()
            .spawn((
                action_button("test-play", 10.0, 10.0),
                MixerBinding::enum_write("transport.state", "playing"),
            ))
            .id();
        let stop = app
            .world_mut()
            .spawn((
                action_button("test-stop", 10.0, 10.0),
                MixerBinding::enum_write("transport.state", "stopped"),
            ))
            .id();
        app.insert_resource(TransportButtons { play, stop });
        {
            let mut state = app.world_mut().resource_mut::<MusicdMixerState>();
            state.connection = MixerConnectionState::Connected;
            state.ready = true;
        }

        // Warm up only the authoritative playing state. The actual test batch
        // is armed afterwards and must traverse all four stages in one update.
        app.update();
        assert!(transport_is_playing(
            app.world().resource::<MusicdMixerState>()
        ));
        transport_state.lock().unwrap().writes.clear();
        app.world_mut().resource_mut::<EmitResetAndToggle>().0 = true;

        app.update();

        let writes = &transport_state.lock().unwrap().writes;
        assert!(writes.iter().any(|(_, write)| {
            write.path == "transport.state" && write.value == LeafValue::Enum("stopped".to_string())
        }));
        assert!(writes.iter().any(|(_, write)| {
            write.path == "transport.position" && write.value == LeafValue::Number(0.0)
        }));
        assert!(!writes.iter().any(|(_, write)| {
            write.path == "transport.state" && write.value == LeafValue::Enum("playing".to_string())
        }));
    }

    #[test]
    fn cross_frame_toggle_resolves_against_provisional_playing_state() {
        let (mut app, transport_state) = stopped_transport_action_test_app();

        app.world_mut().write_message(AudioIntent::Toggle);
        app.update();
        let (play_request_id, play_request) = {
            let recorded = transport_state.lock().unwrap();
            assert_eq!(recorded.writes.len(), 1);
            assert_eq!(
                recorded.writes[0].1.value,
                LeafValue::Enum("playing".to_string())
            );
            recorded.writes[0].clone()
        };

        // The Play remains unacknowledged. The second Toggle must resolve from
        // that provisional playing value and queue Stop, not read acknowledged
        // stopped and issue/reaffirm Play.
        app.world_mut().write_message(AudioIntent::Toggle);
        app.update();
        assert_eq!(transport_state.lock().unwrap().writes.len(), 1);

        transport_state
            .lock()
            .unwrap()
            .events
            .push(TransportEvent::Reply {
                request_id: play_request_id,
                result: Ok(TransportReply::Write(Ok(WriteResponse::Accepted(
                    WriteAck {
                        revision: 2,
                        path: "transport.state".to_string(),
                        canonical_value: LeafValue::Enum("playing".to_string()),
                        source_id: "studio-action-order-test".to_string(),
                        op_id: play_request.op_id,
                    },
                )))),
                completed_at: None,
            });
        app.update();

        let recorded = transport_state.lock().unwrap();
        assert_eq!(recorded.writes.len(), 2);
        assert_eq!(
            recorded.writes[1].1.value,
            LeafValue::Enum("stopped".to_string())
        );
    }

    #[test]
    fn provisional_reaffirm_reissues_after_first_write_is_rejected() {
        let (mut app, transport_state) = stopped_transport_action_test_app();

        app.world_mut().write_message(AudioIntent::Toggle);
        app.update();
        let (play_request_id, play_request) = {
            let recorded = transport_state.lock().unwrap();
            assert_eq!(recorded.writes.len(), 1);
            recorded.writes[0].clone()
        };

        // Two more Toggles net back to provisional Play. Reaffirm it behind
        // the in-flight command so rejection of that command cannot lose the
        // three-toggle net intent.
        app.world_mut().write_message(AudioIntent::Toggle);
        app.world_mut().write_message(AudioIntent::Toggle);
        app.update();
        assert_eq!(transport_state.lock().unwrap().writes.len(), 1);

        transport_state
            .lock()
            .unwrap()
            .events
            .push(TransportEvent::Reply {
                request_id: play_request_id,
                result: Ok(TransportReply::Write(Ok(WriteResponse::Rejected(
                    WriteReject {
                        path: "transport.state".to_string(),
                        op_id: play_request.op_id,
                        current_revision: 1,
                        current_value: LeafValue::Enum("stopped".to_string()),
                        reason: "test CAS rejection".to_string(),
                    },
                )))),
                completed_at: None,
            });
        app.update();

        let recorded = transport_state.lock().unwrap();
        assert_eq!(recorded.writes.len(), 2);
        assert_eq!(
            recorded.writes[1].1.value,
            LeafValue::Enum("playing".to_string())
        );
    }

    #[test]
    fn reset_reduction_is_independent_of_producer_order() {
        for batch in [
            vec![AudioIntent::Play, AudioIntent::Reset],
            vec![AudioIntent::Reset, AudioIntent::Play],
            vec![AudioIntent::Reset, AudioIntent::Toggle],
        ] {
            assert_eq!(
                reduce_audio_intents(batch, Some(true), false),
                ReducedAudioIntents {
                    force_stop: true,
                    state_target: None,
                    seeks: vec![0.0],
                }
            );
        }
    }

    #[test]
    fn reset_forces_stop_even_when_projected_stopped() {
        assert!(reduce_audio_intents(vec![AudioIntent::Reset], Some(false), false).force_stop);
    }

    #[test]
    fn two_toggles_return_to_the_projected_state_without_a_write() {
        assert_eq!(
            reduce_audio_intents(
                vec![AudioIntent::Toggle, AudioIntent::Toggle],
                Some(false),
                false,
            )
            .state_target,
            None
        );
    }

    #[test]
    fn two_toggles_reaffirm_a_provisional_baseline() {
        assert_eq!(
            reduce_audio_intents(
                vec![AudioIntent::Toggle, AudioIntent::Toggle],
                Some(true),
                true,
            )
            .state_target,
            Some(true)
        );
    }

    #[test]
    fn empty_batch_does_not_reaffirm_a_provisional_baseline() {
        assert_eq!(
            reduce_audio_intents(Vec::new(), Some(true), true).state_target,
            None
        );
    }

    #[test]
    fn three_toggles_emit_one_inverse_target() {
        assert_eq!(
            reduce_audio_intents(
                vec![
                    AudioIntent::Toggle,
                    AudioIntent::Toggle,
                    AudioIntent::Toggle,
                ],
                Some(false),
                false,
            )
            .state_target,
            Some(true)
        );
    }

    #[test]
    fn play_then_stop_emits_only_the_final_stop() {
        assert_eq!(
            reduce_audio_intents(
                vec![AudioIntent::Play, AudioIntent::Stop],
                Some(true),
                false,
            )
            .state_target,
            Some(false)
        );
    }

    #[test]
    fn redundant_stop_while_stopped_is_suppressed() {
        assert_eq!(
            reduce_audio_intents(vec![AudioIntent::Stop], Some(false), false).state_target,
            None
        );
    }

    #[test]
    fn unknown_transport_state_drops_state_intents_but_keeps_seeks() {
        assert_eq!(
            reduce_audio_intents(
                vec![
                    AudioIntent::Play,
                    AudioIntent::Toggle,
                    AudioIntent::Seek(12.5),
                ],
                None,
                false,
            ),
            ReducedAudioIntents {
                force_stop: false,
                state_target: None,
                seeks: vec![12.5],
            }
        );
    }

    #[test]
    fn route_actions_maps_transport_verbs_to_intents() {
        fn route_once(action: ActionId) -> Vec<AudioIntent> {
            let mut app = App::new();
            app.add_message::<ActionRequest>()
                .add_message::<AudioIntent>();
            app.add_systems(Update, route_actions);
            app.world_mut().write_message(ActionRequest {
                action,
                source: Source::Bus,
                args: Default::default(),
                invocation_focus: None,
            });
            app.update();
            app.world_mut()
                .resource_mut::<bevy::ecs::message::Messages<AudioIntent>>()
                .drain()
                .collect()
        }
        assert_eq!(route_once(ids::TRANSPORT_START), vec![AudioIntent::Play]);
        assert_eq!(route_once(ids::TRANSPORT_PAUSE), vec![AudioIntent::Stop]);
        // Stop is the dual intent: halt AND rewind to zero.
        assert_eq!(
            route_once(ids::TRANSPORT_STOP),
            vec![AudioIntent::Stop, AudioIntent::Seek(0.0)]
        );
        assert_eq!(route_once(ids::TRANSPORT_TOGGLE), vec![AudioIntent::Toggle]);
    }

    #[test]
    fn bus_action_invoke_transport_toggle_reaches_the_space_audio_intent() {
        let registry = build_action_registry(&StudioActionAvailability::default());
        let request = InboundRequest {
            connection_generation: 1,
            from: "automation".into(),
            command: "action.invoke".into(),
            headers: std::iter::once(("broker_origin".to_string(), "local".to_string())).collect(),
            body: serde_json::json!({ "id": "transport.toggle" }).to_string(),
            reply_id: Some("42".into()),
        };
        let action = prepare_action_invocation(&request, &registry, false).unwrap();

        let mut app = App::new();
        app.add_message::<ActionRequest>()
            .add_message::<AudioIntent>()
            .add_systems(Update, route_actions);
        app.world_mut().write_message(action);
        app.update();

        let intents: Vec<_> = app
            .world_mut()
            .resource_mut::<bevy::ecs::message::Messages<AudioIntent>>()
            .drain()
            .collect();
        assert_eq!(intents, [AudioIntent::Toggle]);
    }
}
