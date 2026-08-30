//! Native CTK modal interactions and the process-wide modal coordinator.
//!
//! `InteractionRequest::message` and `InteractionRequest::confirm` retain the
//! original action-key outcome (`InteractionOutcome::Action`) for source and
//! behaviour compatibility. Requests built with `from_kind`, plus prompt and
//! secret-prompt constructors, use typed `Resolved` values. Consumers can use
//! [`InteractionOutcome::action_key`] to accept both forms while migrating.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

use accesskit::Role;
use bevy::a11y::AccessibilityNode;
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::message::{MessageReader, MessageWriter};
use bevy::ecs::observer::On;
use bevy::ecs::system::SystemParam;
use bevy::feathers::theme::{ThemeBackgroundColor, ThemeTextColor, ThemeToken, UiTheme};
use bevy::input::ButtonInput;
use bevy::input_focus::tab_navigation::{TabGroup, TabIndex};
use bevy::input_focus::{FocusCause, FocusGained, InputFocus};
use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy::text::{EditableText, Font};
use bevy::ui::{FocusPolicy, GlobalZIndex, InteractionDisabled, Overflow, Pressed, Selected};
use bevy::ui_widgets::{Activate, ActivateOnPress, Button, ScrollArea, ScrollIntoView};

use crate::dialog_shell::{spawn_dialog_shell, DialogShell};
use crate::file_requester::{FileRequest, FileRequestId, FileRequestSpec};
use crate::modal_capture::{
    ensure_modal_capture_plugin, ModalCapture, ModalCaptureLayer, ModalCaptureOwner,
    ModalCaptureSystems, ModalCaptureToken,
};
use crate::style::{lighten, selected_background, InteractionVisualState};
#[cfg(test)]
use crate::style::{selected_background_from_pair, HOVERED_LIFT, PRESSED_LIFT};
use crate::text_field::{
    set_text_field_error, spawn_secret_field, spawn_text_field, CtkSecretField,
    CtkSecretFieldProps, CtkTextField, CtkTextFieldPlugin, CtkTextFieldProps, SecretValue,
    TextValidator,
};
#[cfg(test)]
use crate::theme::{contrast_ratio, AA_CONTRAST};
use crate::theme::{ctk_color, tokens, CtkTypographyOptOut};
use crate::widgets::{
    hfader_sized, ControlRange, ControlValue, CtkWidgetsPlugin, NumericControlProps, ValueMapping,
};

pub(crate) const DIALOG_Z: i32 = 1_100;

/// Opaque process-global correlation identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct InteractionId(u64);

static NEXT_INTERACTION_ID: AtomicU64 = AtomicU64::new(1);

impl InteractionId {
    pub fn next() -> Self {
        Self(NEXT_INTERACTION_ID.fetch_add(1, Ordering::Relaxed))
    }
}

/// Visual and accessibility severity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum InteractionSeverity {
    #[default]
    Info,
    Success,
    Warning,
    Danger,
}

/// Properties shared by every dialog kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DialogCommon {
    pub title: String,
    pub message: Option<String>,
    pub severity: InteractionSeverity,
}

impl DialogCommon {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            message: None,
            severity: InteractionSeverity::Info,
        }
    }

    pub fn message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    pub fn severity(mut self, severity: InteractionSeverity) -> Self {
        self.severity = severity;
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ActionRole {
    Accept,
    Cancel,
    Destructive,
    Help,
    Auxiliary,
}

#[derive(Clone, Debug)]
pub struct InteractionAction {
    pub key: String,
    pub label: String,
    pub role: ActionRole,
    pub default: bool,
}

impl InteractionAction {
    pub fn new(key: impl Into<String>, label: impl Into<String>, role: ActionRole) -> Self {
        Self {
            key: key.into(),
            label: label.into(),
            role,
            default: false,
        }
    }

    pub fn default(mut self) -> Self {
        self.default = true;
        self
    }
}

#[derive(Clone, Debug)]
pub struct MessageSpec {
    actions: Vec<InteractionAction>,
    legacy_outcome: bool,
    invoker: Option<Entity>,
}

impl MessageSpec {
    pub fn new() -> Self {
        Self {
            actions: vec![InteractionAction::new("ok", "OK", ActionRole::Accept).default()],
            legacy_outcome: false,
            invoker: None,
        }
    }
}

impl Default for MessageSpec {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, Default)]
pub struct ConfirmSpec {
    actions: Vec<InteractionAction>,
    legacy_outcome: bool,
    invoker: Option<Entity>,
}

impl ConfirmSpec {
    pub fn new(actions: impl IntoIterator<Item = InteractionAction>) -> Self {
        Self {
            actions: actions.into_iter().collect(),
            legacy_outcome: false,
            invoker: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PromptSpec {
    initial: String,
    max_length: usize,
    select_all: bool,
    validator: Option<TextValidator>,
    invoker: Option<Entity>,
}

impl Default for PromptSpec {
    fn default() -> Self {
        Self {
            initial: String::new(),
            max_length: 4_096,
            select_all: false,
            validator: None,
            invoker: None,
        }
    }
}

impl PromptSpec {
    pub fn new(initial: impl Into<String>) -> Self {
        Self {
            initial: initial.into(),
            ..default()
        }
    }
}

#[derive(Clone, Debug)]
pub struct SecretPromptSpec {
    max_length: usize,
    validator: Option<TextValidator>,
    invoker: Option<Entity>,
}

impl Default for SecretPromptSpec {
    fn default() -> Self {
        Self {
            max_length: 4_096,
            validator: None,
            invoker: None,
        }
    }
}

impl SecretPromptSpec {
    pub fn new() -> Self {
        Self::default()
    }
}

/// One selectable item shared by choice and multi-choice dialogs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChoiceItem {
    pub key: String,
    pub label: String,
    pub description: Option<String>,
    pub enabled: bool,
}

impl ChoiceItem {
    pub fn new(key: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            label: label.into(),
            description: None,
            enabled: true,
        }
    }

    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    pub fn disabled(mut self) -> Self {
        self.enabled = false;
        self
    }
}

/// Collapse items sharing a `key`, keeping the first occurrence. A choice list resolves
/// to a key, so duplicate keys are ambiguous: the renderer keys its selection set by
/// `key`, and clicking either row of a duplicate pair would toggle both. Dedup at
/// construction so a caller's accidental repeat can't produce a self-contradicting list.
fn dedup_choice_items(items: impl IntoIterator<Item = ChoiceItem>) -> Vec<ChoiceItem> {
    let mut seen = std::collections::HashSet::new();
    items
        .into_iter()
        .filter(|item| seen.insert(item.key.clone()))
        .collect()
}

#[derive(Clone, Debug)]
pub struct ChoiceSpec {
    items: Vec<ChoiceItem>,
    initial: Option<String>,
    invoker: Option<Entity>,
}

impl ChoiceSpec {
    pub fn new(items: impl IntoIterator<Item = ChoiceItem>) -> Self {
        Self {
            items: dedup_choice_items(items),
            initial: None,
            invoker: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct MultiChoiceSpec {
    items: Vec<ChoiceItem>,
    initial: Vec<String>,
    invoker: Option<Entity>,
}

impl MultiChoiceSpec {
    pub fn new(items: impl IntoIterator<Item = ChoiceItem>) -> Self {
        Self {
            items: dedup_choice_items(items),
            initial: Vec::new(),
            invoker: None,
        }
    }
}

/// Owner-driven progress value, shaped like `DialogProgressValueV1`.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProgressValue {
    Indeterminate,
    Determinate { current: u64, total: u64 },
}

impl ProgressValue {
    fn fraction(&self) -> Option<f32> {
        match self {
            Self::Indeterminate => None,
            Self::Determinate { current, total } if *total > 0 => {
                Some((*current).min(*total) as f32 / *total as f32)
            }
            Self::Determinate { .. } => Some(0.0),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ProgressSpec {
    progress: ProgressValue,
    cancellable: bool,
}

impl ProgressSpec {
    pub fn new(progress: ProgressValue) -> Self {
        Self {
            progress,
            cancellable: false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SliderSpec {
    min: i32,
    max: i32,
    step: i32,
    initial: i32,
    invoker: Option<Entity>,
}

impl SliderSpec {
    pub fn new(min: i32, max: i32, step: i32, initial: i32) -> Self {
        let (min, max, step) = normalise_slider_range(min, max, step);
        Self {
            min,
            max,
            step,
            initial: canonical_slider_value(min, max, step, initial),
            invoker: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct TextViewSpec {
    text: String,
    monospace: bool,
    invoker: Option<Entity>,
}

impl TextViewSpec {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            monospace: false,
            invoker: None,
        }
    }
}

/// In-process dialog vocabulary aligned with the `dialog.v1` kinds CTK renders.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum InteractionKind {
    Message(MessageSpec),
    Confirm(ConfirmSpec),
    Prompt(PromptSpec),
    SecretPrompt(SecretPromptSpec),
    Choice(ChoiceSpec),
    MultiChoice(MultiChoiceSpec),
    Progress(ProgressSpec),
    Slider(SliderSpec),
    TextView(TextViewSpec),
}

/// An interaction request. Progress is routed to the separate non-modal lane;
/// every other kind uses the modal coordinator.
#[derive(Message, Clone, Debug)]
pub struct InteractionRequest {
    id: InteractionId,
    common: DialogCommon,
    kind: InteractionKind,
}

impl InteractionRequest {
    /// Compatibility constructor: action buttons resolve as the legacy
    /// `InteractionOutcome::Action`.
    pub fn message(title: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            id: InteractionId::next(),
            common: DialogCommon::new(title).message(message),
            kind: InteractionKind::Message(MessageSpec {
                actions: vec![InteractionAction::new("ok", "OK", ActionRole::Accept).default()],
                legacy_outcome: true,
                invoker: None,
            }),
        }
    }

    /// Compatibility constructor: action buttons resolve as the legacy
    /// `InteractionOutcome::Action`.
    pub fn confirm(title: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            id: InteractionId::next(),
            common: DialogCommon::new(title).message(message),
            kind: InteractionKind::Confirm(ConfirmSpec {
                actions: Vec::new(),
                legacy_outcome: true,
                invoker: None,
            }),
        }
    }

    pub fn prompt(title: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            id: InteractionId::next(),
            common: DialogCommon::new(title).message(message),
            kind: InteractionKind::Prompt(PromptSpec::default()),
        }
    }

    pub fn secret_prompt(title: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            id: InteractionId::next(),
            common: DialogCommon::new(title).message(message),
            kind: InteractionKind::SecretPrompt(SecretPromptSpec::default()),
        }
    }

    pub fn choice(
        title: impl Into<String>,
        message: impl Into<String>,
        items: impl IntoIterator<Item = ChoiceItem>,
    ) -> Self {
        Self {
            id: InteractionId::next(),
            common: DialogCommon::new(title).message(message),
            kind: InteractionKind::Choice(ChoiceSpec::new(items)),
        }
    }

    pub fn multi_choice(
        title: impl Into<String>,
        message: impl Into<String>,
        items: impl IntoIterator<Item = ChoiceItem>,
    ) -> Self {
        Self {
            id: InteractionId::next(),
            common: DialogCommon::new(title).message(message),
            kind: InteractionKind::MultiChoice(MultiChoiceSpec::new(items)),
        }
    }

    /// Construct owner-driven, non-modal progress.
    pub fn progress(
        title: impl Into<String>,
        message: impl Into<String>,
        progress: ProgressValue,
    ) -> Self {
        Self {
            id: InteractionId::next(),
            common: DialogCommon::new(title).message(message),
            kind: InteractionKind::Progress(ProgressSpec::new(progress)),
        }
    }

    pub fn slider(
        title: impl Into<String>,
        message: impl Into<String>,
        min: i32,
        max: i32,
        step: i32,
        initial: i32,
    ) -> Self {
        Self {
            id: InteractionId::next(),
            common: DialogCommon::new(title).message(message),
            kind: InteractionKind::Slider(SliderSpec::new(min, max, step, initial)),
        }
    }

    pub fn text_view(
        title: impl Into<String>,
        message: impl Into<String>,
        text: impl Into<String>,
    ) -> Self {
        Self {
            id: InteractionId::next(),
            common: DialogCommon::new(title).message(message),
            kind: InteractionKind::TextView(TextViewSpec::new(text)),
        }
    }

    /// Construct a typed request without exposing correlation-id mutation.
    pub fn from_kind(common: DialogCommon, kind: InteractionKind) -> Self {
        Self {
            id: InteractionId::next(),
            common,
            kind,
        }
    }

    pub fn id(&self) -> InteractionId {
        self.id
    }

    pub fn common(&self) -> &DialogCommon {
        &self.common
    }

    pub fn kind(&self) -> &InteractionKind {
        &self.kind
    }

    pub fn severity(mut self, severity: InteractionSeverity) -> Self {
        self.common.severity = severity;
        self
    }

    pub fn action(mut self, action: InteractionAction) -> Self {
        match &mut self.kind {
            InteractionKind::Message(spec) => spec.actions.push(action),
            InteractionKind::Confirm(spec) => spec.actions.push(action),
            _ => {}
        }
        self
    }

    pub fn initial_text(mut self, initial: impl Into<String>) -> Self {
        if let InteractionKind::Prompt(spec) = &mut self.kind {
            spec.initial = initial.into();
        }
        self
    }

    pub fn max_length(mut self, max_length: usize) -> Self {
        let max_length = max_length.max(1);
        match &mut self.kind {
            InteractionKind::Prompt(spec) => spec.max_length = max_length,
            InteractionKind::SecretPrompt(spec) => spec.max_length = max_length,
            _ => {}
        }
        self
    }

    pub fn select_all(mut self) -> Self {
        if let InteractionKind::Prompt(spec) = &mut self.kind {
            spec.select_all = true;
        }
        self
    }

    pub fn validator(mut self, validator: TextValidator) -> Self {
        match &mut self.kind {
            InteractionKind::Prompt(spec) => spec.validator = Some(validator),
            InteractionKind::SecretPrompt(spec) => spec.validator = Some(validator),
            _ => {}
        }
        self
    }

    pub fn initial_choice(mut self, key: impl Into<String>) -> Self {
        if let InteractionKind::Choice(spec) = &mut self.kind {
            spec.initial = Some(key.into());
        }
        self
    }

    pub fn initial_choices(mut self, keys: impl IntoIterator<Item = impl Into<String>>) -> Self {
        if let InteractionKind::MultiChoice(spec) = &mut self.kind {
            spec.initial = keys.into_iter().map(Into::into).collect();
        }
        self
    }

    pub fn cancellable(mut self) -> Self {
        if let InteractionKind::Progress(spec) = &mut self.kind {
            spec.cancellable = true;
        }
        self
    }

    pub fn monospace(mut self) -> Self {
        if let InteractionKind::TextView(spec) = &mut self.kind {
            spec.monospace = true;
        }
        self
    }

    /// Prefer this live entity when focus is restored after the dialog closes.
    pub fn invoked_by(mut self, invoker: Entity) -> Self {
        match &mut self.kind {
            InteractionKind::Message(spec) => spec.invoker = Some(invoker),
            InteractionKind::Confirm(spec) => spec.invoker = Some(invoker),
            InteractionKind::Prompt(spec) => spec.invoker = Some(invoker),
            InteractionKind::SecretPrompt(spec) => spec.invoker = Some(invoker),
            InteractionKind::Choice(spec) => spec.invoker = Some(invoker),
            InteractionKind::MultiChoice(spec) => spec.invoker = Some(invoker),
            InteractionKind::Progress(_) => {}
            InteractionKind::Slider(spec) => spec.invoker = Some(invoker),
            InteractionKind::TextView(spec) => spec.invoker = Some(invoker),
        }
        self
    }

    fn invoker(&self) -> Option<Entity> {
        match &self.kind {
            InteractionKind::Message(spec) => spec.invoker,
            InteractionKind::Confirm(spec) => spec.invoker,
            InteractionKind::Prompt(spec) => spec.invoker,
            InteractionKind::SecretPrompt(spec) => spec.invoker,
            InteractionKind::Choice(spec) => spec.invoker,
            InteractionKind::MultiChoice(spec) => spec.invoker,
            InteractionKind::Progress(_) => None,
            InteractionKind::Slider(spec) => spec.invoker,
            InteractionKind::TextView(spec) => spec.invoker,
        }
    }
}

fn normalise_slider_range(min: i32, max: i32, step: i32) -> (i32, i32, i32) {
    // Only correct an inverted range by swapping; never widen a caller's bounds.
    // A single-value slider (min == max) is legitimate — it resolves to that value —
    // so it keeps step 1 and a zero span rather than being stretched to min+1.
    let (min, max) = if min <= max { (min, max) } else { (max, min) };
    let span = i64::from(max) - i64::from(min);
    let step = if span == 0 {
        1
    } else {
        i64::from(step.max(1)).min(span) as i32
    };
    (min, max, step)
}

fn canonical_slider_value(min: i32, max: i32, step: i32, value: i32) -> i32 {
    let min = i64::from(min);
    let max = i64::from(max);
    let step = i64::from(step.max(1));
    let value = i64::from(value).clamp(min, max);
    let offset = value - min;
    let rounded_steps = (offset + step / 2) / step;
    let highest_steps = (max - min) / step;
    (min + rounded_steps.min(highest_steps) * step) as i32
}

/// Terminal owner/user completion for non-modal progress.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProgressCompletion {
    Succeeded,
    Cancelled,
    Failed(String),
}

/// Typed successful values.
#[derive(Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum InteractionValue {
    Acknowledged,
    Action(String),
    Text(String),
    Secret(SecretValue),
    Choice(String),
    MultiChoice(Vec<String>),
    Progress(ProgressCompletion),
    Slider(i32),
}

/// Terminal outcome.
#[derive(Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum InteractionOutcome {
    Resolved(InteractionValue),
    Cancelled,
    /// Compatibility result emitted by `message` and `confirm`.
    Action(String),
    Dismissed,
}

impl InteractionOutcome {
    pub fn action_key(&self) -> Option<&str> {
        match self {
            Self::Action(key) | Self::Resolved(InteractionValue::Action(key)) => Some(key),
            _ => None,
        }
    }
}

#[derive(Message, Debug, PartialEq, Eq)]
pub struct InteractionResult {
    pub id: InteractionId,
    pub outcome: InteractionOutcome,
}

/// Programmatically close a modal interaction without producing an
/// [`InteractionResult`].
///
/// This is for an owner or transport adapter that already observed terminal
/// state elsewhere. It withdraws both queued and visible modal interactions;
/// progress remains owner-driven through [`ProgressComplete`].
#[derive(Message, Clone, Copy, Debug, PartialEq, Eq)]
pub struct WithdrawInteraction(pub InteractionId);

/// Owner-driven patch for an open non-modal progress surface.
#[derive(Message, Clone, Debug)]
pub struct ProgressUpdate {
    pub id: InteractionId,
    pub label: Option<String>,
    pub progress: Option<ProgressValue>,
}

impl ProgressUpdate {
    pub fn new(id: InteractionId) -> Self {
        Self {
            id,
            label: None,
            progress: None,
        }
    }

    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    pub fn progress(mut self, progress: ProgressValue) -> Self {
        self.progress = Some(progress);
        self
    }
}

/// Owner-driven terminal event for an open non-modal progress surface.
#[derive(Message, Clone, Debug)]
pub struct ProgressComplete {
    pub id: InteractionId,
    pub completion: ProgressCompletion,
}

impl ProgressComplete {
    pub fn new(id: InteractionId, completion: ProgressCompletion) -> Self {
        Self { id, completion }
    }
}

#[derive(Clone, Copy)]
struct ProgressSurface {
    root: Entity,
    label: Entity,
    indicator: Entity,
    stack_index: usize,
}

/// Active non-modal progress surfaces, deliberately separate from
/// [`ModalCoordinator`].
#[derive(Resource, Default)]
pub struct ProgressState {
    active: HashMap<InteractionId, ProgressSurface>,
}

impl ProgressState {
    pub fn is_active(&self, id: InteractionId) -> bool {
        self.active.contains_key(&id)
    }

    pub fn active_len(&self) -> usize {
        self.active.len()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ModalPresenter {
    Interaction,
    FileRequester,
}

pub(crate) struct QueuedFileRequest {
    pub correlation: InteractionId,
    pub request: FileRequestSpec,
}

pub(crate) enum QueuedModal {
    Interaction(InteractionRequest),
    FileRequester(QueuedFileRequest),
}

struct ActiveModal {
    presenter: ModalPresenter,
    correlation: InteractionId,
    root: Entity,
    focus_root: Entity,
    default_focus: Entity,
    previous_focus: Option<Entity>,
}

/// One FIFO modal lane and one capture-token owner for every CTK presenter.
#[derive(Resource, Default)]
pub struct ModalCoordinator {
    queue: VecDeque<QueuedModal>,
    active: Option<ActiveModal>,
    capture: Option<ModalCaptureToken>,
    pending_despawns: Vec<Entity>,
}

/// Boundary map for the caller-id based file-request API.
#[derive(Resource, Default)]
pub(crate) struct FileRequestCompatAdapter {
    legacy_ids: HashMap<InteractionId, FileRequestId>,
}

impl FileRequestCompatAdapter {
    pub(crate) fn register(&mut self, correlation: InteractionId, legacy_id: FileRequestId) {
        self.legacy_ids.insert(correlation, legacy_id);
    }

    pub fn resolve(&mut self, correlation: InteractionId) -> Option<FileRequestId> {
        self.legacy_ids.remove(&correlation)
    }

    pub(crate) fn correlations_for(&self, legacy_id: FileRequestId) -> Vec<InteractionId> {
        self.legacy_ids
            .iter()
            .filter_map(|(correlation, candidate)| {
                (*candidate == legacy_id).then_some(*correlation)
            })
            .collect()
    }

    #[cfg(test)]
    pub fn legacy_id(&self, correlation: InteractionId) -> Option<FileRequestId> {
        self.legacy_ids.get(&correlation).copied()
    }
}

impl ModalCoordinator {
    pub fn is_active(&self) -> bool {
        self.active.is_some()
    }

    /// Root entity of the active modal subtree, used by routed pointer input.
    pub fn active_root(&self) -> Option<Entity> {
        self.active.as_ref().map(|active| active.root)
    }

    pub fn queued_len(&self) -> usize {
        self.queue.len()
    }
}

const COORDINATOR_OWNER: ModalCaptureOwner = ModalCaptureOwner {
    kind: "ctk.modal-coordinator",
    entity: None,
};
const COORDINATOR_LAYER: ModalCaptureLayer = ModalCaptureLayer(DIALOG_Z);

fn acquire_capture(coordinator: &mut ModalCoordinator, capture: &mut ModalCapture) {
    if coordinator.capture.is_none() {
        coordinator.capture = Some(capture.acquire(COORDINATOR_OWNER, COORDINATOR_LAYER));
    }
}

#[cfg(test)]
pub(crate) fn acquire_coordinator_capture(
    coordinator: &mut ModalCoordinator,
    capture: &mut ModalCapture,
) {
    acquire_capture(coordinator, capture);
}

pub(crate) fn queue_file_request(
    coordinator: &mut ModalCoordinator,
    capture: &mut ModalCapture,
    compat: &mut FileRequestCompatAdapter,
    request: FileRequest,
) {
    let correlation = InteractionId::next();
    compat.register(correlation, request.id);
    coordinator
        .queue
        .push_back(QueuedModal::FileRequester(QueuedFileRequest {
            correlation,
            request: request.into(),
        }));
    acquire_capture(coordinator, capture);
}

pub(crate) fn take_next_file_request(
    coordinator: &mut ModalCoordinator,
) -> Option<QueuedFileRequest> {
    if coordinator.active.is_some()
        || !matches!(
            coordinator.queue.front(),
            Some(QueuedModal::FileRequester(_))
        )
    {
        return None;
    }
    match coordinator.queue.pop_front() {
        Some(QueuedModal::FileRequester(request)) => Some(request),
        _ => unreachable!(),
    }
}

pub(crate) fn activate_modal(
    coordinator: &mut ModalCoordinator,
    presenter: ModalPresenter,
    correlation: InteractionId,
    root: Entity,
    default_focus: Entity,
    previous_focus: Option<Entity>,
) {
    debug_assert!(coordinator.active.is_none());
    coordinator.active = Some(ActiveModal {
        presenter,
        correlation,
        root,
        focus_root: root,
        default_focus,
        previous_focus,
    });
}

pub(crate) fn set_modal_focus_scope(
    coordinator: &mut ModalCoordinator,
    presenter: ModalPresenter,
    focus_root: Entity,
    default_focus: Entity,
) {
    if let Some(active) = coordinator
        .active
        .as_mut()
        .filter(|active| active.presenter == presenter)
    {
        active.focus_root = focus_root;
        active.default_focus = default_focus;
    }
}

pub(crate) fn defer_modal_despawn(coordinator: &mut ModalCoordinator, entity: Entity) {
    coordinator.pending_despawns.push(entity);
}

pub(crate) fn coordinator_is_top(
    coordinator: &ModalCoordinator,
    capture: &ModalCapture,
    presenter: ModalPresenter,
) -> bool {
    coordinator
        .active
        .as_ref()
        .is_some_and(|active| active.presenter == presenter)
        && coordinator
            .capture
            .is_some_and(|token| capture.is_top(token))
}

pub(crate) fn close_active_modal(
    coordinator: &mut ModalCoordinator,
    capture: &mut ModalCapture,
    presenter: ModalPresenter,
    commands: &mut Commands,
    focus: &mut InputFocus,
) -> Option<InteractionId> {
    if coordinator
        .active
        .as_ref()
        .is_none_or(|active| active.presenter != presenter)
    {
        return None;
    }
    let active = coordinator.active.take()?;
    coordinator.pending_despawns.push(active.root);
    if let Some(previous) = active
        .previous_focus
        .filter(|entity| commands.get_entity(*entity).is_ok())
    {
        focus.set(previous, FocusCause::Navigated);
    } else {
        focus.clear();
    }
    release_coordinator_capture_if_idle(coordinator, capture);
    Some(active.correlation)
}

pub(crate) fn remove_queued_file_request(
    coordinator: &mut ModalCoordinator,
    correlation: InteractionId,
) -> bool {
    let before = coordinator.queue.len();
    coordinator.queue.retain(|queued| {
        !matches!(
            queued,
            QueuedModal::FileRequester(request) if request.correlation == correlation
        )
    });
    coordinator.queue.len() != before
}

pub(crate) fn release_coordinator_capture_if_idle(
    coordinator: &mut ModalCoordinator,
    capture: &mut ModalCapture,
) {
    if coordinator.active.is_none() && coordinator.queue.is_empty() {
        if let Some(token) = coordinator.capture.take() {
            let released = capture.release_latched(token);
            debug_assert!(released, "modal coordinator token was not live");
        }
    }
}

#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct InteractionSystems;

#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct InteractionPresentationSystems;

#[derive(Resource, Default)]
pub struct InteractionState {
    active: Option<ActiveInteraction>,
}

struct ActiveInteraction {
    default_key: Option<String>,
    cancel_key: Option<String>,
    legacy_actions: bool,
    input: ActiveInput,
}

enum ActiveInput {
    None,
    Text {
        input: Entity,
        error: Entity,
    },
    Secret {
        input: Entity,
        error: Entity,
    },
    Choice(ActiveChoiceList),
    MultiChoice(ActiveChoiceList),
    Slider {
        control: Entity,
        min: i32,
        max: i32,
        step: i32,
    },
}

struct ActiveChoiceList {
    items: Vec<ActiveChoiceItem>,
    selected: HashSet<String>,
}

struct ActiveChoiceItem {
    key: String,
    enabled: bool,
    entity: Entity,
}

#[derive(Component, Clone)]
struct InteractionButton {
    key: String,
    role: ActionRole,
}

#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
enum ChoiceSelectionMode {
    Single,
    Multiple,
}

#[derive(Component, Clone)]
struct ChoiceOption {
    key: String,
    enabled: bool,
    mode: ChoiceSelectionMode,
}

#[derive(Component)]
struct ChoiceMarker;

#[derive(Component)]
struct ChoiceText {
    dim: bool,
}

#[derive(Component)]
struct SliderValueLabel {
    control: Entity,
    // The true i32 grid, so the live readout matches exactly what OK will resolve.
    // The f32 fader mapping is deliberately widened (see spawn_dialog_slider), so the raw
    // ControlValue must be re-canonicalised before display or the label can drift one unit
    // from the resolved value at collapsed-precision endpoints.
    min: i32,
    max: i32,
    step: i32,
}

#[derive(Component)]
struct ProgressIndicator(ProgressValue);

#[derive(Component)]
struct ProgressFill;

#[derive(Component)]
struct ProgressCancelButton {
    id: InteractionId,
}

pub struct InteractionPlugin;

impl Plugin for InteractionPlugin {
    fn build(&self, app: &mut App) {
        ensure_modal_coordinator(app);
        app.init_resource::<InteractionState>()
            .init_resource::<ProgressState>()
            .add_message::<InteractionRequest>()
            .add_message::<InteractionResult>()
            .add_message::<WithdrawInteraction>()
            .add_message::<ProgressUpdate>()
            .add_message::<ProgressComplete>()
            .add_observer(on_interaction_button)
            .add_observer(on_choice_option)
            .add_observer(on_progress_cancel)
            .add_observer(scroll_focused_choice_into_view)
            // The keyboard and ingress phases intentionally remain disjoint.
            .add_systems(Update, interaction_keyboard.in_set(ModalCaptureSystems))
            .add_systems(
                Update,
                (
                    receive_interactions,
                    withdraw_interactions,
                    receive_progress,
                    update_progress,
                    complete_progress,
                )
                    .chain()
                    .in_set(InteractionSystems)
                    .after(ModalCaptureSystems),
            )
            .add_systems(
                Update,
                present_interaction.in_set(InteractionPresentationSystems),
            )
            .add_systems(
                Update,
                (
                    update_interaction_styles,
                    update_slider_value_labels,
                    update_progress_visuals,
                ),
            );
    }
}

pub(crate) fn ensure_modal_coordinator(app: &mut App) {
    ensure_modal_capture_plugin(app);
    if !app.is_plugin_added::<CtkTextFieldPlugin>() {
        app.add_plugins(CtkTextFieldPlugin);
    }
    if !app.is_plugin_added::<CtkWidgetsPlugin>() {
        app.add_plugins(CtkWidgetsPlugin);
    }
    if !app.world().contains_resource::<ModalCoordinator>() {
        app.init_resource::<ModalCoordinator>()
            .init_resource::<InputFocus>()
            .init_resource::<UiTheme>()
            .configure_sets(
                Update,
                InteractionSystems.before(crate::file_requester::FileRequesterSystems),
            )
            .configure_sets(
                Update,
                InteractionPresentationSystems
                    .after(InteractionSystems)
                    .after(crate::file_requester::FileRequesterSystems),
            )
            .add_systems(PostUpdate, sanitize_modal_focus)
            .add_systems(Last, despawn_closed_modals);
    }
}

fn despawn_closed_modals(mut coordinator: ResMut<ModalCoordinator>, mut commands: Commands) {
    for entity in coordinator.pending_despawns.drain(..) {
        commands.entity(entity).try_despawn();
    }
}

fn receive_interactions(
    mut requests: MessageReader<InteractionRequest>,
    mut coordinator: ResMut<ModalCoordinator>,
    mut capture: ResMut<ModalCapture>,
) {
    let requests: Vec<_> = requests
        .read()
        .filter(|request| !matches!(request.kind, InteractionKind::Progress(_)))
        .cloned()
        .collect();
    if requests.is_empty() {
        return;
    }
    coordinator
        .queue
        .extend(requests.into_iter().map(QueuedModal::Interaction));
    acquire_capture(&mut coordinator, &mut capture);
}

fn withdraw_interactions(
    mut withdrawals: MessageReader<WithdrawInteraction>,
    mut state: ResMut<InteractionState>,
    mut coordinator: ResMut<ModalCoordinator>,
    mut capture: ResMut<ModalCapture>,
    mut commands: Commands,
    mut focus: ResMut<InputFocus>,
) {
    for withdrawal in withdrawals.read() {
        let id = withdrawal.0;
        coordinator.queue.retain(|queued| {
            !matches!(
                queued,
                QueuedModal::Interaction(request) if request.id == id
            )
        });

        let is_active = coordinator.active.as_ref().is_some_and(|active| {
            active.presenter == ModalPresenter::Interaction && active.correlation == id
        });
        if is_active {
            state.active = None;
            let closed = close_active_modal(
                &mut coordinator,
                &mut capture,
                ModalPresenter::Interaction,
                &mut commands,
                &mut focus,
            );
            debug_assert_eq!(closed, Some(id));
        }
        release_coordinator_capture_if_idle(&mut coordinator, &mut capture);
    }
}

fn receive_progress(
    mut requests: MessageReader<InteractionRequest>,
    mut state: ResMut<ProgressState>,
    mut commands: Commands,
) {
    for request in requests.read() {
        let InteractionKind::Progress(spec) = &request.kind else {
            continue;
        };
        if state.active.contains_key(&request.id) {
            continue;
        }
        let stack_index = next_progress_stack_index(&state);
        let surface = spawn_progress_surface(
            &mut commands,
            request.id,
            &request.common,
            spec,
            stack_index,
        );
        state.active.insert(request.id, surface);
    }
}

fn next_progress_stack_index(state: &ProgressState) -> usize {
    let occupied: HashSet<_> = state
        .active
        .values()
        .map(|surface| surface.stack_index)
        .collect();
    (0..).find(|index| !occupied.contains(index)).unwrap()
}

fn update_progress(
    mut updates: MessageReader<ProgressUpdate>,
    state: Res<ProgressState>,
    mut commands: Commands,
    mut labels: Query<&mut Text>,
) {
    for update in updates.read() {
        let Some(surface) = state.active.get(&update.id) else {
            continue;
        };
        if let Some(label) = update.label.as_deref() {
            if let Ok(mut text) = labels.get_mut(surface.label) {
                **text = label.into();
            }
        }
        if let Some(progress) = &update.progress {
            commands
                .entity(surface.indicator)
                .insert(ProgressIndicator(progress.clone()));
        }
    }
}

fn complete_progress(
    mut completions: MessageReader<ProgressComplete>,
    mut state: ResMut<ProgressState>,
    mut results: MessageWriter<InteractionResult>,
    mut commands: Commands,
) {
    for complete in completions.read() {
        finish_progress(
            complete.id,
            complete.completion.clone(),
            &mut state,
            &mut results,
            &mut commands,
        );
    }
}

fn finish_progress(
    id: InteractionId,
    completion: ProgressCompletion,
    state: &mut ProgressState,
    results: &mut MessageWriter<InteractionResult>,
    commands: &mut Commands,
) {
    let Some(surface) = state.active.remove(&id) else {
        return;
    };
    commands.entity(surface.root).try_despawn();
    results.write(InteractionResult {
        id,
        outcome: InteractionOutcome::Resolved(InteractionValue::Progress(completion)),
    });
}

fn spawn_progress_surface(
    commands: &mut Commands,
    id: InteractionId,
    common: &DialogCommon,
    spec: &ProgressSpec,
    stack_index: usize,
) -> ProgressSurface {
    let title = label(commands, &common.title, 15.0, false, false);
    let label = label(
        commands,
        common.message.as_deref().unwrap_or("Working…"),
        12.0,
        true,
        false,
    );
    let fill = commands
        .spawn((
            ProgressFill,
            Node {
                position_type: PositionType::Absolute,
                left: px(0),
                width: percent(spec.progress.fraction().unwrap_or(32.0 / 100.0) * 100.0),
                height: percent(100),
                border_radius: BorderRadius::all(px(3)),
                ..default()
            },
            ThemeBackgroundColor(tokens::CONTROL_ACTIVE),
        ))
        .id();
    let mut accessible = accesskit::Node::new(Role::ProgressIndicator);
    accessible.set_label(common.title.clone());
    set_progress_accessibility(&mut accessible, &spec.progress);
    let indicator = commands
        .spawn((
            ProgressIndicator(spec.progress.clone()),
            Node {
                width: percent(100),
                height: px(8),
                border_radius: BorderRadius::all(px(3)),
                overflow: Overflow::clip(),
                ..default()
            },
            ThemeBackgroundColor(tokens::TRACK),
            AccessibilityNode::from(accessible),
        ))
        .add_child(fill)
        .id();
    let mut children = vec![title, label, indicator];
    if spec.cancellable {
        let action = InteractionAction::new("cancel", "Cancel", ActionRole::Cancel);
        let button = spawn_progress_cancel_button(commands, id, &action);
        children.push(button);
    }
    let root = commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                top: px(16.0 + stack_index as f32 * 112.0),
                right: px(16),
                width: px(360),
                min_height: px(88),
                flex_direction: FlexDirection::Column,
                row_gap: px(9),
                padding: UiRect::all(px(13)),
                border: UiRect::all(px(1)),
                border_radius: BorderRadius::all(px(7)),
                ..default()
            },
            ThemeBackgroundColor(tokens::PANEL),
            BorderColor::all(Color::NONE),
            bevy::feathers::theme::ThemeBorderColor(accent(common.severity)),
            GlobalZIndex(DIALOG_Z - 1),
            FocusPolicy::Pass,
            // Non-modal group so a cancellable progress card's Cancel button is
            // Tab-reachable. Must NOT be a modal group — progress never captures focus,
            // so a modal group here would fight the real dialog lane's TabGroup::modal().
            TabGroup::new(0),
        ))
        .add_children(&children)
        .id();
    ProgressSurface {
        root,
        label,
        indicator,
        stack_index,
    }
}

fn spawn_progress_cancel_button(
    commands: &mut Commands,
    id: InteractionId,
    action: &InteractionAction,
) -> Entity {
    let text = label(commands, &action.label, 12.0, false, false);
    let mut accessible = accesskit::Node::new(Role::Button);
    accessible.set_label(action.label.clone());
    commands
        .spawn((
            Node {
                align_self: AlignSelf::FlexEnd,
                min_height: px(28),
                padding: UiRect::axes(px(10), px(4)),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                border: UiRect::all(px(1)),
                border_radius: BorderRadius::all(px(4)),
                ..default()
            },
            ThemeBackgroundColor(tokens::CONTROL),
            BorderColor::all(Color::NONE),
            bevy::feathers::theme::ThemeBorderColor(tokens::BORDER),
            Button,
            ActivateOnPress,
            Hovered::default(),
            TabIndex(0),
            AccessibilityNode::from(accessible),
            ProgressCancelButton { id },
        ))
        .add_child(text)
        .id()
}

fn set_progress_accessibility(node: &mut accesskit::Node, progress: &ProgressValue) {
    match progress {
        ProgressValue::Indeterminate => {
            node.set_busy();
            node.clear_min_numeric_value();
            node.clear_max_numeric_value();
            node.clear_numeric_value();
        }
        ProgressValue::Determinate { current, total } => {
            node.clear_busy();
            node.set_min_numeric_value(0.0);
            node.set_max_numeric_value(*total as f64);
            node.set_numeric_value((*current).min(*total) as f64);
        }
    }
}

#[derive(SystemParam)]
struct InteractionPresentationResources<'w> {
    theme: Res<'w, UiTheme>,
    asset_server: Option<Res<'w, AssetServer>>,
}

fn present_interaction(
    mut coordinator: ResMut<ModalCoordinator>,
    mut capture: ResMut<ModalCapture>,
    mut state: ResMut<InteractionState>,
    mut commands: Commands,
    mut focus: ResMut<InputFocus>,
    mut results: MessageWriter<InteractionResult>,
    resources: InteractionPresentationResources,
) {
    if coordinator.active.is_some()
        || !matches!(coordinator.queue.front(), Some(QueuedModal::Interaction(_)))
    {
        return;
    }
    let request = match coordinator.queue.pop_front() {
        Some(QueuedModal::Interaction(request)) => request,
        _ => unreachable!(),
    };
    let has_actions = match &request.kind {
        InteractionKind::Message(spec) => !spec.actions.is_empty(),
        InteractionKind::Confirm(spec) => !spec.actions.is_empty(),
        InteractionKind::Prompt(_)
        | InteractionKind::SecretPrompt(_)
        | InteractionKind::Choice(_)
        | InteractionKind::MultiChoice(_)
        | InteractionKind::Slider(_)
        | InteractionKind::TextView(_) => true,
        InteractionKind::Progress(_) => false,
    };
    if !has_actions {
        results.write(InteractionResult {
            id: request.id,
            outcome: InteractionOutcome::Dismissed,
        });
        if coordinator.queue.is_empty() {
            if let Some(token) = coordinator.capture.take() {
                let released = capture.release_latched(token);
                debug_assert!(released, "modal coordinator token was not live");
            }
        }
        return;
    }
    let previous_focus = request.invoker().or_else(|| focus.get());
    let ((correlation, active), root, default_focus) = spawn_interaction(
        &mut commands,
        request,
        &resources.theme,
        resources.asset_server.as_deref(),
    );
    state.active = Some(active);
    activate_modal(
        &mut coordinator,
        ModalPresenter::Interaction,
        correlation,
        root,
        default_focus,
        previous_focus,
    );
    focus.set(default_focus, FocusCause::Navigated);
}

fn accent(severity: InteractionSeverity) -> ThemeToken {
    match severity {
        InteractionSeverity::Info => tokens::CONTROL_ACTIVE,
        InteractionSeverity::Success => tokens::METER_GREEN,
        InteractionSeverity::Warning => tokens::METER_AMBER,
        InteractionSeverity::Danger => tokens::METER_RED,
    }
}

fn role(severity: InteractionSeverity) -> Role {
    match severity {
        InteractionSeverity::Info | InteractionSeverity::Success => Role::Dialog,
        InteractionSeverity::Warning | InteractionSeverity::Danger => Role::AlertDialog,
    }
}

fn spawn_interaction(
    commands: &mut Commands,
    request: InteractionRequest,
    theme: &UiTheme,
    asset_server: Option<&AssetServer>,
) -> ((InteractionId, ActiveInteraction), Entity, Entity) {
    let shell = spawn_dialog_shell(
        commands,
        DialogShell::new(
            &request.common.title,
            role(request.common.severity),
            accent(request.common.severity),
            DIALOG_Z,
        ),
    );
    if let Some(message) = request.common.message.as_deref() {
        let message = label(commands, message, 13.0, true, false);
        commands.entity(shell.body).add_child(message);
    }

    let mut default_key = None;
    let mut cancel_key = None;
    let mut first_button = None;
    let mut default_button = None;
    let (actions, legacy_actions, input) = match request.kind {
        InteractionKind::Message(spec) => (spec.actions, spec.legacy_outcome, ActiveInput::None),
        InteractionKind::Confirm(spec) => (spec.actions, spec.legacy_outcome, ActiveInput::None),
        InteractionKind::Prompt(spec) => {
            let props = CtkTextFieldProps::new(spec.initial, "Response")
                .max_length(spec.max_length)
                .select_all(spec.select_all);
            let props = if let Some(validator) = spec.validator {
                props.validator(validator)
            } else {
                props
            };
            let field = spawn_text_field(commands, props);
            commands.entity(shell.body).add_child(field.root);
            (
                prompt_actions(),
                false,
                ActiveInput::Text {
                    input: field.input,
                    error: field.error,
                },
            )
        }
        InteractionKind::SecretPrompt(spec) => {
            let props = CtkSecretFieldProps::new("", "Secret response").max_length(spec.max_length);
            let props = if let Some(validator) = spec.validator {
                props.validator(validator)
            } else {
                props
            };
            let field = spawn_secret_field(commands, props);
            commands.entity(shell.body).add_child(field.root);
            (
                prompt_actions(),
                false,
                ActiveInput::Secret {
                    input: field.input,
                    error: field.error,
                },
            )
        }
        InteractionKind::Choice(spec) => {
            let (list, active) = spawn_choice_list(
                commands,
                &spec.items,
                spec.initial.iter().map(String::as_str),
                ChoiceSelectionMode::Single,
                theme,
            );
            commands.entity(shell.body).add_child(list);
            (prompt_actions(), false, ActiveInput::Choice(active))
        }
        InteractionKind::MultiChoice(spec) => {
            let (list, active) = spawn_choice_list(
                commands,
                &spec.items,
                spec.initial.iter().map(String::as_str),
                ChoiceSelectionMode::Multiple,
                theme,
            );
            commands.entity(shell.body).add_child(list);
            (prompt_actions(), false, ActiveInput::MultiChoice(active))
        }
        InteractionKind::Slider(spec) => {
            let (control, container) = spawn_dialog_slider(commands, request.id, &spec);
            commands.entity(shell.body).add_child(container);
            (
                prompt_actions(),
                false,
                ActiveInput::Slider {
                    control,
                    min: spec.min,
                    max: spec.max,
                    step: spec.step,
                },
            )
        }
        InteractionKind::TextView(spec) => {
            let view = spawn_text_view(commands, &spec, asset_server);
            commands.entity(shell.body).add_child(view);
            (
                vec![InteractionAction::new("ok", "OK", ActionRole::Accept).default()],
                false,
                ActiveInput::None,
            )
        }
        InteractionKind::Progress(_) => unreachable!("progress uses the non-modal presenter"),
    };
    for action in &actions {
        if action.default && action.role != ActionRole::Help && default_key.is_none() {
            default_key = Some(action.key.clone());
        }
        if action.role == ActionRole::Cancel && cancel_key.is_none() {
            cancel_key = Some(action.key.clone());
        }
        let button = spawn_button(commands, action);
        first_button.get_or_insert(button);
        if action.default && default_button.is_none() {
            default_button = Some(button);
        }
        commands.entity(shell.actions).add_child(button);
    }
    let default_focus = match &input {
        ActiveInput::Text { input, .. } | ActiveInput::Secret { input, .. } => *input,
        ActiveInput::Choice(list) | ActiveInput::MultiChoice(list) => list
            .items
            .iter()
            .find(|item| item.enabled && list.selected.contains(&item.key))
            .or_else(|| list.items.iter().find(|item| item.enabled))
            .map(|item| item.entity)
            .or(default_button)
            .or(first_button)
            .expect("choice dialogs always have actions"),
        ActiveInput::Slider { control, .. } => *control,
        ActiveInput::None => default_button
            .or(first_button)
            .expect("presenter rejects actionless dialogs"),
    };
    (
        (
            request.id,
            ActiveInteraction {
                default_key,
                cancel_key,
                legacy_actions,
                input,
            },
        ),
        shell.root,
        default_focus,
    )
}

fn prompt_actions() -> Vec<InteractionAction> {
    vec![
        InteractionAction::new("cancel", "Cancel", ActionRole::Cancel),
        InteractionAction::new("ok", "OK", ActionRole::Accept).default(),
    ]
}

fn spawn_choice_list<'a>(
    commands: &mut Commands,
    items: &[ChoiceItem],
    initial: impl IntoIterator<Item = &'a str>,
    mode: ChoiceSelectionMode,
    theme: &UiTheme,
) -> (Entity, ActiveChoiceList) {
    let valid_keys: HashSet<&str> = items
        .iter()
        .filter(|item| item.enabled)
        .map(|item| item.key.as_str())
        .collect();
    let mut selected: HashSet<String> = initial
        .into_iter()
        .filter(|key| valid_keys.contains(*key))
        .map(str::to_owned)
        .collect();
    if mode == ChoiceSelectionMode::Single {
        if let Some(first) = items
            .iter()
            .find(|item| selected.contains(&item.key))
            .map(|item| item.key.clone())
        {
            selected.clear();
            selected.insert(first);
        }
    }

    let mut accessible = accesskit::Node::new(match mode {
        ChoiceSelectionMode::Single => Role::ListBox,
        ChoiceSelectionMode::Multiple => Role::List,
    });
    accessible.set_label(match mode {
        ChoiceSelectionMode::Single => "Choices",
        ChoiceSelectionMode::Multiple => "Choices; multiple selection allowed",
    });
    if mode == ChoiceSelectionMode::Multiple {
        accessible.set_multiselectable();
    }
    let list = commands
        .spawn((
            Node {
                width: percent(100),
                max_height: px(300),
                flex_direction: FlexDirection::Column,
                row_gap: px(5),
                overflow: Overflow::scroll_y(),
                ..default()
            },
            AccessibilityNode::from(accessible),
            ScrollArea,
        ))
        .id();
    let mut active_items = Vec::with_capacity(items.len());
    for item in items {
        let is_selected = selected.contains(&item.key);
        let marker_text = match (mode, is_selected) {
            (ChoiceSelectionMode::Single, false) => "○",
            (ChoiceSelectionMode::Single, true) => "●",
            (ChoiceSelectionMode::Multiple, false) => "☐",
            (ChoiceSelectionMode::Multiple, true) => "☑",
        };
        let marker = commands
            .spawn((
                ChoiceMarker,
                ChoiceText { dim: false },
                Text::new(marker_text),
                TextFont::from_font_size(15.0),
                TextColor(ctk_color(theme, &label_token(false, is_selected))),
            ))
            .id();
        let title = label(commands, &item.label, 13.0, !item.enabled, is_selected);
        commands.entity(title).remove::<ThemeTextColor>().insert((
            TextColor(ctk_color(theme, &label_token(!item.enabled, is_selected))),
            ChoiceText { dim: !item.enabled },
        ));
        let mut text_children = vec![title];
        if let Some(description) = item.description.as_deref() {
            let description = label(commands, description, 11.0, true, is_selected);
            commands
                .entity(description)
                .remove::<ThemeTextColor>()
                .insert((
                    TextColor(ctk_color(theme, &label_token(true, is_selected))),
                    ChoiceText { dim: true },
                ));
            text_children.push(description);
        }
        let text = commands
            .spawn(Node {
                flex_grow: 1.0,
                flex_direction: FlexDirection::Column,
                row_gap: px(2),
                ..default()
            })
            .add_children(&text_children)
            .id();
        let mut option_accessible = accesskit::Node::new(match mode {
            ChoiceSelectionMode::Single => Role::ListBoxOption,
            ChoiceSelectionMode::Multiple => Role::CheckBox,
        });
        option_accessible.set_label(item.label.clone());
        if let Some(description) = item.description.as_deref() {
            option_accessible.set_description(description);
        }
        if mode == ChoiceSelectionMode::Single {
            option_accessible.set_selected(is_selected);
        } else {
            option_accessible.set_toggled(if is_selected {
                accesskit::Toggled::True
            } else {
                accesskit::Toggled::False
            });
        }
        if !item.enabled {
            option_accessible.set_disabled();
        }
        let entity = commands
            .spawn((
                Node {
                    width: percent(100),
                    min_height: px(42),
                    flex_direction: FlexDirection::Row,
                    align_items: AlignItems::Center,
                    column_gap: px(9),
                    padding: UiRect::axes(px(10), px(7)),
                    border: UiRect::all(px(1)),
                    border_radius: BorderRadius::all(px(4)),
                    ..default()
                },
                ThemeBackgroundColor(if is_selected {
                    tokens::ROW_SELECTED
                } else {
                    tokens::CONTROL
                }),
                BorderColor::all(Color::NONE),
                bevy::feathers::theme::ThemeBorderColor(tokens::BORDER),
                Button,
                ActivateOnPress,
                Hovered::default(),
                TabIndex(0),
                AccessibilityNode::from(option_accessible),
                ChoiceOption {
                    key: item.key.clone(),
                    enabled: item.enabled,
                    mode,
                },
            ))
            .add_children(&[marker, text])
            .id();
        if is_selected {
            commands.entity(entity).insert(Selected);
        }
        if !item.enabled {
            commands.entity(entity).insert(InteractionDisabled);
        }
        commands.entity(list).add_child(entity);
        active_items.push(ActiveChoiceItem {
            key: item.key.clone(),
            enabled: item.enabled,
            entity,
        });
    }
    (
        list,
        ActiveChoiceList {
            items: active_items,
            selected,
        },
    )
}

fn spawn_dialog_slider(
    commands: &mut Commands,
    id: InteractionId,
    spec: &SliderSpec,
) -> (Entity, Entity) {
    let reachable_max = canonical_slider_value(spec.min, spec.max, spec.step, spec.max);
    let min_f = spec.min as f32;
    // The f32 fader is only the input mechanism — resolution re-clamps through
    // canonical_slider_value against the true i32 [min, max] grid, so a widened f32 top
    // never yields an out-of-range result. But ValueMapping::linear rejects an empty
    // span, and two distinct i32 endpoints can still collapse to one f32 when the range
    // is a single value (min == max) or the magnitudes exceed f32 integer precision
    // (≥ 2^24). Nudge the top up one ULP in those cases so the mapping stays valid.
    let max_f = (reachable_max as f32).max(min_f.next_up());
    let range = ControlRange {
        min: min_f,
        max: max_f,
        step: (spec.step as f32).max(f32::MIN_POSITIVE),
        detent: None,
    };
    let mapping = ValueMapping::linear(range.min, range.max)
        .expect("slider f32 range is non-empty by construction");
    let mut accessible = accesskit::Node::new(Role::Slider);
    accessible.set_label("Dialog value");
    accessible.set_min_numeric_value(spec.min as f64);
    accessible.set_max_numeric_value(reachable_max as f64);
    accessible.set_numeric_value(spec.initial as f64);
    accessible.set_numeric_value_step(spec.step as f64);
    let control = commands
        .spawn((
            hfader_sized(
                NumericControlProps::new(
                    format!("dialog.{id}.slider"),
                    spec.initial as f32,
                    range,
                    mapping,
                ),
                420.0,
                34.0,
            ),
            AccessibilityNode::from(accessible),
        ))
        .id();
    let value = commands
        .spawn((
            SliderValueLabel {
                control,
                min: spec.min,
                max: spec.max,
                step: spec.step,
            },
            Text::new(spec.initial.to_string()),
            TextFont::from_font_size(14.0),
            ThemeTextColor(tokens::TEXT),
        ))
        .id();
    let endpoints = label(
        commands,
        &format!("{}  —  {}", spec.min, reachable_max),
        11.0,
        true,
        false,
    );
    let container = commands
        .spawn(Node {
            width: percent(100),
            flex_direction: FlexDirection::Column,
            align_items: AlignItems::Center,
            row_gap: px(6),
            ..default()
        })
        .add_children(&[value, control, endpoints])
        .id();
    (control, container)
}

fn spawn_text_view(
    commands: &mut Commands,
    spec: &TextViewSpec,
    asset_server: Option<&AssetServer>,
) -> Entity {
    let mut font = TextFont::from_font_size(12.0);
    if spec.monospace {
        if let Some(asset_server) = asset_server {
            font.font = FontSource::Handle(
                asset_server.load::<Font>(bevy::feathers::constants::fonts::MONO),
            );
        }
    }
    let mut accessible = accesskit::Node::new(Role::Document);
    accessible.set_label("Read-only text");
    accessible.set_value(spec.text.clone());
    accessible.set_read_only();
    let text = commands
        .spawn((
            Text::new(&spec.text),
            font,
            ThemeTextColor(tokens::TEXT),
            AccessibilityNode::from(accessible),
        ))
        .id();
    if spec.monospace {
        commands.entity(text).insert(CtkTypographyOptOut);
    }
    let mut scroll_accessible = accesskit::Node::new(Role::ScrollView);
    scroll_accessible.set_label("Text view");
    commands
        .spawn((
            Node {
                width: percent(100),
                height: px(300),
                padding: UiRect::all(px(10)),
                border: UiRect::all(px(1)),
                border_radius: BorderRadius::all(px(4)),
                overflow: Overflow::scroll_y(),
                ..default()
            },
            ThemeBackgroundColor(tokens::SURFACE),
            BorderColor::all(Color::NONE),
            bevy::feathers::theme::ThemeBorderColor(tokens::BORDER),
            AccessibilityNode::from(scroll_accessible),
            ScrollArea,
            TabIndex(0),
        ))
        .add_child(text)
        .id()
}

fn spawn_button(commands: &mut Commands, action: &InteractionAction) -> Entity {
    let label = label(
        commands,
        &action.label,
        13.0,
        false,
        action.role == ActionRole::Accept,
    );
    let mut accessible = accesskit::Node::new(Role::Button);
    accessible.set_label(action.label.clone());
    let background = match action.role {
        ActionRole::Destructive => tokens::DANGER_SURFACE,
        ActionRole::Accept => tokens::ROW_SELECTED,
        _ => tokens::CONTROL,
    };
    let border = if action.role == ActionRole::Destructive {
        tokens::METER_RED
    } else {
        tokens::BORDER
    };
    commands
        .spawn((
            Node {
                min_width: px(72),
                min_height: px(30),
                padding: UiRect::axes(px(11), px(5)),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                border: UiRect::all(px(1)),
                border_radius: BorderRadius::all(px(4)),
                ..default()
            },
            ThemeBackgroundColor(background),
            BorderColor::all(Color::NONE),
            bevy::feathers::theme::ThemeBorderColor(border),
            Button,
            ActivateOnPress,
            Hovered::default(),
            TabIndex(0),
            AccessibilityNode::from(accessible),
            InteractionButton {
                key: action.key.clone(),
                role: action.role,
            },
        ))
        .add_child(label)
        .id()
}

fn on_choice_option(
    activated: On<Activate>,
    options: Query<&ChoiceOption>,
    mut state: ResMut<InteractionState>,
    mut commands: Commands,
    mut accessibility: Query<&mut AccessibilityNode>,
) {
    let Ok(option) = options.get(activated.entity) else {
        return;
    };
    if !option.enabled {
        return;
    }
    let Some(active) = state.active.as_mut() else {
        return;
    };
    let list = match (&mut active.input, option.mode) {
        (ActiveInput::Choice(list), ChoiceSelectionMode::Single)
        | (ActiveInput::MultiChoice(list), ChoiceSelectionMode::Multiple) => list,
        _ => return,
    };
    if !list
        .items
        .iter()
        .any(|item| item.entity == activated.entity && item.enabled)
    {
        return;
    }

    match option.mode {
        ChoiceSelectionMode::Single => {
            list.selected.clear();
            list.selected.insert(option.key.clone());
            for item in &list.items {
                let selected = item.key == option.key;
                if selected {
                    commands.entity(item.entity).insert(Selected);
                } else {
                    commands.entity(item.entity).remove::<Selected>();
                }
                if let Ok(mut node) = accessibility.get_mut(item.entity) {
                    node.set_selected(selected);
                }
            }
        }
        ChoiceSelectionMode::Multiple => {
            let selected = if list.selected.remove(&option.key) {
                false
            } else {
                list.selected.insert(option.key.clone());
                true
            };
            if selected {
                commands.entity(activated.entity).insert(Selected);
            } else {
                commands.entity(activated.entity).remove::<Selected>();
            }
            if let Ok(mut node) = accessibility.get_mut(activated.entity) {
                node.set_toggled(if selected {
                    accesskit::Toggled::True
                } else {
                    accesskit::Toggled::False
                });
            }
        }
    }
}

/// Keep the keyboard-focused choice row visible. When a row inside the scrollable list
/// gains focus — the initial selection sitting past the 300px fold, or Tab navigation
/// moving through the list — ask its `ScrollArea` ancestor to bring it into view.
/// `ScrollIntoView` propagates up to the enclosing scroll area and is a harmless no-op if
/// there is none, so this is safe for short (un-scrolled) lists and non-choice focus.
///
/// Rows are Tab-navigable `Button` controls; arrow-key navigation within the list is not
/// wired (that would mean adopting `bevy_ui_widgets::ListBox` in place of the hand-rolled
/// rows — a Phase-3-era enhancement, not a Phase 2b requirement).
fn scroll_focused_choice_into_view(
    focus: On<FocusGained>,
    choices: Query<(), With<ChoiceOption>>,
    mut commands: Commands,
) {
    let entity = focus.entity;
    if choices.contains(entity) {
        commands.trigger(ScrollIntoView { entity });
    }
}

fn on_progress_cancel(
    activated: On<Activate>,
    buttons: Query<&ProgressCancelButton>,
    mut state: ResMut<ProgressState>,
    mut results: MessageWriter<InteractionResult>,
    mut commands: Commands,
) {
    let Ok(button) = buttons.get(activated.entity) else {
        return;
    };
    finish_progress(
        button.id,
        ProgressCompletion::Cancelled,
        &mut state,
        &mut results,
        &mut commands,
    );
}

#[allow(clippy::too_many_arguments)]
fn on_interaction_button(
    activated: On<Activate>,
    buttons: Query<&InteractionButton>,
    mut state: ResMut<InteractionState>,
    mut coordinator: ResMut<ModalCoordinator>,
    mut capture: ResMut<ModalCapture>,
    mut results: MessageWriter<InteractionResult>,
    mut commands: Commands,
    mut focus: ResMut<InputFocus>,
    plain: Query<(&EditableText, &CtkTextField), Without<CtkSecretField>>,
    mut secrets: Query<(&mut EditableText, &mut CtkSecretField), Without<CtkTextField>>,
    controls: Query<&ControlValue>,
    mut texts: Query<&mut Text>,
    mut visibility: Query<&mut Visibility>,
) {
    let Ok(button) = buttons.get(activated.entity) else {
        return;
    };
    if button.role == ActionRole::Help {
        return;
    }
    resolve_interaction(
        &button.key,
        &mut state,
        &mut coordinator,
        &mut capture,
        &mut results,
        &mut commands,
        &mut focus,
        &plain,
        &mut secrets,
        &controls,
        &mut texts,
        &mut visibility,
    );
}

#[allow(clippy::too_many_arguments)]
fn interaction_keyboard(
    keys: Res<ButtonInput<KeyCode>>,
    mut state: ResMut<InteractionState>,
    mut coordinator: ResMut<ModalCoordinator>,
    mut capture: ResMut<ModalCapture>,
    mut results: MessageWriter<InteractionResult>,
    mut commands: Commands,
    mut focus: ResMut<InputFocus>,
    plain: Query<(&EditableText, &CtkTextField), Without<CtkSecretField>>,
    mut secrets: Query<(&mut EditableText, &mut CtkSecretField), Without<CtkTextField>>,
    controls: Query<&ControlValue>,
    mut texts: Query<&mut Text>,
    mut visibility: Query<&mut Visibility>,
) {
    if !coordinator_is_top(&coordinator, &capture, ModalPresenter::Interaction) {
        return;
    }
    if keys.just_pressed(KeyCode::Escape) {
        let key = state
            .active
            .as_ref()
            .and_then(|active| active.cancel_key.clone());
        if let Some(key) = key {
            resolve_interaction(
                &key,
                &mut state,
                &mut coordinator,
                &mut capture,
                &mut results,
                &mut commands,
                &mut focus,
                &plain,
                &mut secrets,
                &controls,
                &mut texts,
                &mut visibility,
            );
        } else {
            finish_interaction(
                &mut state,
                &mut coordinator,
                &mut capture,
                InteractionOutcome::Dismissed,
                &mut results,
                &mut commands,
                &mut focus,
            );
        }
        return;
    }
    if !keys.just_pressed(KeyCode::Enter) {
        return;
    }
    let Some(active) = state.active.as_ref() else {
        return;
    };
    let composing = match &active.input {
        ActiveInput::Text { input, .. } => plain
            .get(*input)
            .is_ok_and(|(editable, _)| editable.is_composing()),
        ActiveInput::Secret { input, .. } => secrets
            .get(*input)
            .is_ok_and(|(editable, _)| editable.is_composing()),
        ActiveInput::None
        | ActiveInput::Choice(_)
        | ActiveInput::MultiChoice(_)
        | ActiveInput::Slider { .. } => false,
    };
    if composing {
        return;
    }
    if let Some(key) = active.default_key.clone() {
        resolve_interaction(
            &key,
            &mut state,
            &mut coordinator,
            &mut capture,
            &mut results,
            &mut commands,
            &mut focus,
            &plain,
            &mut secrets,
            &controls,
            &mut texts,
            &mut visibility,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn resolve_interaction(
    key: &str,
    state: &mut InteractionState,
    coordinator: &mut ModalCoordinator,
    capture: &mut ModalCapture,
    results: &mut MessageWriter<InteractionResult>,
    commands: &mut Commands,
    focus: &mut InputFocus,
    plain: &Query<(&EditableText, &CtkTextField), Without<CtkSecretField>>,
    secrets: &mut Query<(&mut EditableText, &mut CtkSecretField), Without<CtkTextField>>,
    controls: &Query<&ControlValue>,
    texts: &mut Query<&mut Text>,
    visibility: &mut Query<&mut Visibility>,
) {
    let Some(active) = state.active.as_ref() else {
        return;
    };
    let outcome = match &active.input {
        ActiveInput::None if active.legacy_actions => InteractionOutcome::Action(key.into()),
        ActiveInput::None if key == "ok" => {
            InteractionOutcome::Resolved(InteractionValue::Acknowledged)
        }
        ActiveInput::None => InteractionOutcome::Resolved(InteractionValue::Action(key.into())),
        ActiveInput::Text { .. }
        | ActiveInput::Secret { .. }
        | ActiveInput::Choice(_)
        | ActiveInput::MultiChoice(_)
        | ActiveInput::Slider { .. }
            if key == "cancel" =>
        {
            InteractionOutcome::Cancelled
        }
        ActiveInput::Text { input, error } => {
            let Ok((editable, field)) = plain.get(*input) else {
                return;
            };
            match field.validate(&editable.value().to_string()) {
                Ok(value) => InteractionOutcome::Resolved(InteractionValue::Text(value)),
                Err(message) => {
                    set_text_field_error(*error, Some(&message), texts, visibility);
                    focus.set(*input, FocusCause::Navigated);
                    return;
                }
            }
        }
        ActiveInput::Secret { input, error } => {
            let Ok((mut editable, mut field)) = secrets.get_mut(*input) else {
                return;
            };
            if let Err(message) = field.validate() {
                set_text_field_error(*error, Some(&message), texts, visibility);
                focus.set(*input, FocusCause::Navigated);
                return;
            }
            let value = field.take_value();
            editable.clear();
            InteractionOutcome::Resolved(InteractionValue::Secret(value))
        }
        ActiveInput::Choice(list) if key == "ok" => {
            let Some(key) = ordered_selected_keys(list).into_iter().next() else {
                return;
            };
            InteractionOutcome::Resolved(InteractionValue::Choice(key))
        }
        ActiveInput::MultiChoice(list) if key == "ok" => {
            InteractionOutcome::Resolved(InteractionValue::MultiChoice(ordered_selected_keys(list)))
        }
        ActiveInput::Slider {
            control,
            min,
            max,
            step,
        } if key == "ok" => {
            let Ok(value) = controls.get(*control) else {
                return;
            };
            let value = canonical_slider_value(*min, *max, *step, value.0.round() as i32);
            InteractionOutcome::Resolved(InteractionValue::Slider(value))
        }
        ActiveInput::Choice(_) | ActiveInput::MultiChoice(_) | ActiveInput::Slider { .. } => return,
    };
    finish_interaction(
        state,
        coordinator,
        capture,
        outcome,
        results,
        commands,
        focus,
    );
}

fn ordered_selected_keys(list: &ActiveChoiceList) -> Vec<String> {
    list.items
        .iter()
        .filter(|item| item.enabled && list.selected.contains(&item.key))
        .map(|item| item.key.clone())
        .collect()
}

fn finish_interaction(
    state: &mut InteractionState,
    coordinator: &mut ModalCoordinator,
    capture: &mut ModalCapture,
    outcome: InteractionOutcome,
    results: &mut MessageWriter<InteractionResult>,
    commands: &mut Commands,
    focus: &mut InputFocus,
) {
    state.active = None;
    if let Some(id) = close_active_modal(
        coordinator,
        capture,
        ModalPresenter::Interaction,
        commands,
        focus,
    ) {
        results.write(InteractionResult { id, outcome });
    }
}

/// `pub(crate)` so `PostUpdate` readers of `InputFocus` — the text input focus
/// border painter — can order themselves after the sanitiser and see the
/// settled focus rather than the pre-sanitation one.
pub(crate) fn sanitize_modal_focus(
    coordinator: Res<ModalCoordinator>,
    mut focus: ResMut<InputFocus>,
    parents: Query<&ChildOf>,
    live: Query<()>,
) {
    let Some(active) = coordinator.active.as_ref() else {
        return;
    };
    let focus_is_inside = focus
        .get()
        .filter(|entity| live.contains(*entity))
        .is_some_and(|entity| is_descendant_or_self(entity, active.focus_root, &parents));
    if !focus_is_inside && live.contains(active.default_focus) {
        focus.set(active.default_focus, FocusCause::Navigated);
    }
}

fn is_descendant_or_self(mut entity: Entity, root: Entity, parents: &Query<&ChildOf>) -> bool {
    loop {
        if entity == root {
            return true;
        }
        let Ok(parent) = parents.get(entity) else {
            return false;
        };
        entity = parent.parent();
    }
}

#[allow(clippy::type_complexity)]
fn update_interaction_styles(
    theme: Res<UiTheme>,
    mut buttons: Query<
        (
            &Hovered,
            Has<Pressed>,
            &InteractionButton,
            &mut BackgroundColor,
        ),
        Without<ChoiceOption>,
    >,
    mut options: Query<
        (
            Entity,
            &Hovered,
            Has<Pressed>,
            Has<Selected>,
            Has<InteractionDisabled>,
            &mut BackgroundColor,
        ),
        (With<ChoiceOption>, Without<InteractionButton>),
    >,
    mut markers: Query<(&ChildOf, &mut Text), With<ChoiceMarker>>,
    mut choice_text: Query<(&ChoiceText, &mut TextColor)>,
    descendants: Query<&Children>,
    option_state: Query<(&ChoiceOption, Has<Selected>)>,
) {
    for (hovered, pressed, button, mut background) in &mut buttons {
        let base = match button.role {
            ActionRole::Destructive => ctk_color(&theme, &tokens::DANGER_SURFACE),
            ActionRole::Accept => ctk_color(&theme, &tokens::ROW_SELECTED),
            _ => ctk_color(&theme, &tokens::CONTROL),
        };
        let state = if pressed {
            InteractionVisualState::Pressed
        } else if hovered.get() {
            InteractionVisualState::Hovered
        } else {
            InteractionVisualState::Resting
        };
        *background = BackgroundColor(if button.role == ActionRole::Accept {
            selected_background(&theme, state)
        } else {
            lighten(base, state.legacy_lift())
        });
    }
    for (entity, hovered, pressed, selected, disabled, mut background) in &mut options {
        let base = if selected {
            ctk_color(&theme, &tokens::ROW_SELECTED)
        } else {
            ctk_color(&theme, &tokens::CONTROL)
        };
        let state = if disabled {
            InteractionVisualState::Disabled
        } else if pressed {
            InteractionVisualState::Pressed
        } else if hovered.get() {
            InteractionVisualState::Hovered
        } else {
            InteractionVisualState::Resting
        };
        *background = BackgroundColor(if selected {
            selected_background(&theme, state)
        } else {
            lighten(base, state.legacy_lift())
        });
        for descendant in descendants.iter_descendants(entity) {
            let Ok((managed, mut colour)) = choice_text.get_mut(descendant) else {
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
    for (parent, mut marker) in &mut markers {
        let Ok((option, selected)) = option_state.get(parent.parent()) else {
            continue;
        };
        **marker = match (option.mode, selected) {
            (ChoiceSelectionMode::Single, false) => "○".into(),
            (ChoiceSelectionMode::Single, true) => "●".into(),
            (ChoiceSelectionMode::Multiple, false) => "☐".into(),
            (ChoiceSelectionMode::Multiple, true) => "☑".into(),
        };
    }
}

fn update_slider_value_labels(
    controls: Query<&ControlValue>,
    mut labels: Query<(&SliderValueLabel, &mut Text)>,
) {
    for (label, mut text) in &mut labels {
        let Ok(value) = controls.get(label.control) else {
            continue;
        };
        // Re-canonicalise through the true i32 grid so the readout can never disagree with
        // the value OK resolves (the f32 fader mapping is intentionally widened).
        let value =
            canonical_slider_value(label.min, label.max, label.step, value.0.round() as i32)
                .to_string();
        if text.as_str() != value {
            **text = value;
        }
    }
}

fn update_progress_visuals(
    time: Option<Res<Time>>,
    indicators: Query<(Entity, &ProgressIndicator)>,
    descendants: Query<&Children>,
    mut fills: Query<&mut Node, With<ProgressFill>>,
    mut accessibility: Query<&mut AccessibilityNode>,
) {
    let phase = time
        .as_deref()
        .map_or(0.0, |time| (time.elapsed_secs() * 0.65).fract());
    for (entity, indicator) in &indicators {
        if let Ok(mut node) = accessibility.get_mut(entity) {
            set_progress_accessibility(&mut node, &indicator.0);
        }
        for child in descendants.iter_descendants(entity) {
            let Ok(mut fill) = fills.get_mut(child) else {
                continue;
            };
            match indicator.0.fraction() {
                Some(fraction) => {
                    fill.left = px(0);
                    fill.width = percent(fraction * 100.0);
                }
                None => {
                    fill.left = percent(phase * 68.0);
                    fill.width = percent(32);
                }
            }
        }
    }
}

fn label(commands: &mut Commands, value: &str, size: f32, dim: bool, selected: bool) -> Entity {
    commands
        .spawn((
            Text::new(value),
            TextFont::from_font_size(size),
            ThemeTextColor(label_token(dim, selected)),
        ))
        .id()
}

fn label_token(dim: bool, selected: bool) -> ThemeToken {
    match (selected, dim) {
        (true, true) => tokens::ROW_SELECTED_TEXT_DIM,
        (true, false) => tokens::ROW_SELECTED_TEXT,
        (false, true) => tokens::TEXT_DIM,
        (false, false) => tokens::TEXT,
    }
}

impl fmt::Display for InteractionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_requester::FileRequest;
    use crate::theme::{Mode, Scheme, ThemeSpec};
    use bevy::app::TaskPoolPlugin;

    fn interaction_test_app() -> App {
        let mut app = App::new();
        app.add_plugins(TaskPoolPlugin::default())
            .init_resource::<ButtonInput<KeyCode>>()
            .add_plugins(InteractionPlugin);
        app.finish();
        app.cleanup();
        app
    }

    fn trigger_action(app: &mut App, key: &str) {
        let entity = {
            let world = app.world_mut();
            let mut query = world.query::<(Entity, &InteractionButton)>();
            query
                .iter(world)
                .find(|(_, button)| button.key == key)
                .map(|(entity, _)| entity)
                .expect("interaction action exists")
        };
        app.world_mut().trigger(Activate { entity });
        app.world_mut().flush();
    }

    #[test]
    fn selected_foregrounds_stay_legible_and_emphasis_stays_visible() {
        for (name, state, authored_emphasis) in [
            (
                "pressed",
                InteractionVisualState::Pressed,
                // Independent contract value: referring to `PRESSED_LIFT`
                // here would let implementation and expectation weaken
                // together without turning this test red.
                Some(0.06),
            ),
            ("hovered", InteractionVisualState::Hovered, Some(0.04)),
            ("resting", InteractionVisualState::Resting, None),
            ("disabled", InteractionVisualState::Disabled, None),
        ] {
            let mut worst_contrast = (
                f32::INFINITY,
                Scheme::Ocean,
                Mode::Dark,
                "row.selected.text",
            );
            let mut worst_delta = (f32::INFINITY, Scheme::Ocean, Mode::Dark);
            for scheme in Scheme::ALL {
                for mode in [Mode::Dark, Mode::Light] {
                    let colors = ThemeSpec::from_scheme(scheme, mode).colors;
                    let rendered = selected_background_from_pair(
                        colors.row_selected,
                        colors.row_selected_text,
                        colors.row_selected_text_dim,
                        state,
                    );
                    let base_l = bevy::color::Oklcha::from(colors.row_selected).lightness;
                    let rendered_l = bevy::color::Oklcha::from(rendered).lightness;
                    let delta = (rendered_l - base_l).abs();
                    if delta < worst_delta.0 {
                        worst_delta = (delta, scheme, mode);
                    }
                    if let Some(authored) = authored_emphasis {
                        // Built-in foregrounds sit on the same side of the bar,
                        // so emphasis moves away from both and the clamp must
                        // not bind. Pin the authored response itself, not a
                        // lower visibility floor that a weakened constant could
                        // still satisfy.
                        assert!(
                            (delta - authored).abs() <= 1e-6,
                            "{scheme:?}/{mode:?} {name} realised {delta:.6} OKLCH lightness; \
                             authored lift is {authored:.6}"
                        );
                    }

                    for (foreground_name, foreground) in [
                        ("row.selected.text", colors.row_selected_text),
                        ("row.selected.text.dim", colors.row_selected_text_dim),
                    ] {
                        let measured = contrast_ratio(foreground, rendered);
                        if measured < worst_contrast.0 {
                            worst_contrast = (measured, scheme, mode, foreground_name);
                        }
                        assert!(
                            measured >= AA_CONTRAST,
                            "{scheme:?}/{mode:?} {name} {foreground_name} measures \
                             {measured:.3}:1"
                        );
                        if matches!(
                            state,
                            InteractionVisualState::Pressed | InteractionVisualState::Hovered
                        ) {
                            let resting = contrast_ratio(foreground, colors.row_selected);
                            assert!(
                                measured >= resting,
                                "{scheme:?}/{mode:?} {name} moved toward \
                                 {foreground_name}: {resting:.3}:1 to {measured:.3}:1"
                            );
                        }
                    }
                }
            }
            println!(
                "{name}: worst contrast {:.3}:1 ({}, {:?}/{:?}); worst realised ΔL {:.4} \
                 ({:?}/{:?})",
                worst_contrast.0,
                worst_contrast.3,
                worst_contrast.1,
                worst_contrast.2,
                worst_delta.0,
                worst_delta.1,
                worst_delta.2,
            );
        }
    }

    #[test]
    fn a_straddling_selected_pair_clamps_hover_and_press_before_either_side_fails() {
        let colour = |hex| {
            Color::from(bevy::color::Srgba::hex(hex).expect("regression colour is valid hex"))
        };
        let base = colour("#767676");
        let knockout = Color::BLACK;
        let dim = Color::WHITE;
        assert!(contrast_ratio(knockout, base) >= AA_CONTRAST);
        assert!(contrast_ratio(dim, base) >= AA_CONTRAST);

        for (name, state, full_lift) in [
            ("hovered", InteractionVisualState::Hovered, HOVERED_LIFT),
            ("pressed", InteractionVisualState::Pressed, PRESSED_LIFT),
        ] {
            let unclamped = lighten(base, full_lift);
            assert!(
                contrast_ratio(dim, unclamped) < AA_CONTRAST,
                "the fixture must expose the {name} bypass regression"
            );

            let rendered = selected_background_from_pair(base, knockout, dim, state);
            let base_l = bevy::color::Oklcha::from(base).lightness;
            let rendered_l = bevy::color::Oklcha::from(rendered).lightness;
            let realised = (rendered_l - base_l).abs();
            // This adversarial pair straddles the bar, so the clamp legitimately
            // binds. Unlike the built-ins above, a floor is appropriate here:
            // the fixture has enough headroom for at least one of the clamp's
            // hundred rungs, proving it preserves some safe response instead of
            // returning the resting bar unconditionally.
            let one_rung = full_lift / 100.0;
            assert!(
                realised + 1e-6 >= one_rung,
                "{name} clamp returned only {realised:.6}; one safe rung is {one_rung:.6}"
            );
            assert!(
                realised < full_lift,
                "{name} fixture no longer exercises a binding clamp"
            );
            for (foreground_name, foreground) in [("main", knockout), ("dim", dim)] {
                let measured = contrast_ratio(foreground, rendered);
                assert!(
                    measured >= AA_CONTRAST,
                    "{name} straddling {foreground_name} measures {measured:.3}:1"
                );
            }
        }
    }

    #[test]
    fn compatibility_constructors_keep_action_outcomes() {
        let message = InteractionRequest::message("Title", "Body");
        let confirm = InteractionRequest::confirm("Title", "Body");
        assert!(matches!(
            message.kind(),
            InteractionKind::Message(MessageSpec {
                legacy_outcome: true,
                ..
            })
        ));
        assert!(matches!(
            confirm.kind(),
            InteractionKind::Confirm(ConfirmSpec {
                legacy_outcome: true,
                ..
            })
        ));
        assert_eq!(
            InteractionOutcome::Action("ok".into()).action_key(),
            Some("ok")
        );
        assert_eq!(
            InteractionOutcome::Resolved(InteractionValue::Action("go".into())).action_key(),
            Some("go")
        );
    }

    #[test]
    fn legacy_file_ids_are_remapped_to_unique_interaction_ids() {
        let mut coordinator = ModalCoordinator::default();
        let mut capture = ModalCapture::default();
        let mut compat = FileRequestCompatAdapter::default();
        queue_file_request(
            &mut coordinator,
            &mut capture,
            &mut compat,
            FileRequest::open_file(FileRequestId(3), "First"),
        );
        queue_file_request(
            &mut coordinator,
            &mut capture,
            &mut compat,
            FileRequest::open_file(FileRequestId(3), "Second"),
        );
        let ids: Vec<_> = coordinator
            .queue
            .iter()
            .filter_map(|queued| match queued {
                QueuedModal::FileRequester(request) => Some(request.correlation),
                QueuedModal::Interaction(_) => None,
            })
            .collect();
        assert_eq!(ids.len(), 2);
        assert_ne!(ids[0], ids[1]);
        assert_eq!(compat.resolve(ids[0]), Some(FileRequestId(3)));
        assert_eq!(compat.resolve(ids[1]), Some(FileRequestId(3)));
        assert_eq!(coordinator.capture.map(|_| ()), Some(()));
    }

    #[test]
    fn interaction_and_file_requests_share_fifo_order() {
        let mut coordinator = ModalCoordinator::default();
        let mut capture = ModalCapture::default();
        let mut compat = FileRequestCompatAdapter::default();
        coordinator
            .queue
            .push_back(QueuedModal::Interaction(InteractionRequest::message(
                "First", "Message",
            )));
        queue_file_request(
            &mut coordinator,
            &mut capture,
            &mut compat,
            FileRequest::open_file(FileRequestId(9), "Second"),
        );

        assert!(matches!(
            coordinator.queue.pop_front(),
            Some(QueuedModal::Interaction(_))
        ));
        assert!(matches!(
            coordinator.queue.pop_front(),
            Some(QueuedModal::FileRequester(_))
        ));
    }

    #[test]
    fn withdrawal_closes_active_interaction_without_emitting_a_result() {
        let mut app = interaction_test_app();
        let request = InteractionRequest::message("Withdraw", "Close without a result.");
        let id = request.id();
        app.world_mut().write_message(request);
        app.update();
        assert!(app.world().resource::<ModalCoordinator>().is_active());

        app.world_mut().write_message(WithdrawInteraction(id));
        app.update();

        assert!(app.world().resource::<InteractionState>().active.is_none());
        assert!(!app.world().resource::<ModalCoordinator>().is_active());
        assert!(!app.world().resource::<ModalCapture>().is_captured());
        let messages = app.world().resource::<Messages<InteractionResult>>();
        let mut cursor = messages.get_cursor();
        assert_eq!(cursor.read(messages).count(), 0);
    }

    #[test]
    fn withdrawal_removes_queued_interaction_without_disturbing_active_modal() {
        let mut app = interaction_test_app();
        let active = InteractionRequest::message("Active", "Keep this open.");
        let queued = InteractionRequest::message("Queued", "Withdraw this one.");
        let queued_id = queued.id();
        app.world_mut().write_message(active);
        app.world_mut().write_message(queued);
        app.update();
        assert_eq!(app.world().resource::<ModalCoordinator>().queued_len(), 1);

        app.world_mut()
            .write_message(WithdrawInteraction(queued_id));
        app.update();

        assert!(app.world().resource::<ModalCoordinator>().is_active());
        assert_eq!(app.world().resource::<ModalCoordinator>().queued_len(), 0);
        assert!(app.world().resource::<ModalCapture>().is_captured());
        let messages = app.world().resource::<Messages<InteractionResult>>();
        let mut cursor = messages.get_cursor();
        assert_eq!(cursor.read(messages).count(), 0);
    }

    #[test]
    fn explicit_invoker_is_kept_inside_the_kind_spec() {
        let mut world = World::new();
        let invoker = world.spawn_empty().id();
        let request = InteractionRequest::prompt("Rename", "Name").invoked_by(invoker);
        assert_eq!(request.invoker(), Some(invoker));
    }

    /// Lives here rather than in `text_field` because it needs `ActiveModal`.
    /// A field focused underneath a modal that opens this frame must not be lit:
    /// `sanitize_modal_focus` moves focus onto the modal's default in
    /// `PostUpdate`, and unordered the border painter could read the
    /// pre-sanitation focus and extract a stale border for the frame.
    #[test]
    fn a_field_under_an_opening_modal_is_not_lit_by_the_pre_sanitation_focus() {
        use crate::text_field::{CtkTextFieldPlugin, CtkTextInputFocusBorder};
        use crate::theme::tokens;
        use bevy::feathers::theme::UiTheme;

        let resting_border = Color::srgb(0.2, 0.3, 0.4);
        let active = Color::srgb(0.8, 0.7, 0.2);
        use bevy::ecs::schedule::{LogLevel, ScheduleBuildSettings};

        let mut app = App::new();
        app.add_plugins(CtkTextFieldPlugin)
            .init_resource::<ModalCoordinator>()
            .add_systems(PostUpdate, sanitize_modal_focus);
        // The behavioural assertions below cannot fail reliably on their own: with
        // no ordering the executor is free to pick either order and often picks the
        // safe one, so they would pass while the bug was live. This is the
        // load-bearing half — `sanitize_modal_focus` takes `ResMut<InputFocus>` and
        // the painter takes `Res<InputFocus>`, so without the `.after()` the pair is
        // a genuine ambiguity and building the schedule panics here.
        app.edit_schedule(PostUpdate, |schedule| {
            schedule.set_build_settings(ScheduleBuildSettings {
                ambiguity_detection: LogLevel::Error,
                ..default()
            });
        });
        {
            let mut theme = app.world_mut().resource_mut::<UiTheme>();
            theme.set_color("ctk.border", resting_border);
            theme.set_color("ctk.control.active", active);
        }

        let underlying = app
            .world_mut()
            .spawn(CtkTextInputFocusBorder::new(tokens::BORDER))
            .id();
        let root = app.world_mut().spawn_empty().id();
        let default_focus = app.world_mut().spawn_empty().id();
        app.world_mut().entity_mut(root).add_child(default_focus);
        app.world_mut().resource_mut::<ModalCoordinator>().active = Some(ActiveModal {
            presenter: ModalPresenter::Interaction,
            correlation: InteractionId::next(),
            root,
            focus_root: root,
            default_focus,
            previous_focus: None,
        });
        app.world_mut()
            .resource_mut::<InputFocus>()
            .set(underlying, FocusCause::Pressed);

        app.update();

        assert_eq!(
            app.world().resource::<InputFocus>().get(),
            Some(default_focus),
            "the sanitiser should have moved focus onto the modal"
        );
        assert_eq!(
            app.world().get::<BorderColor>(underlying).unwrap().top,
            resting_border,
            "the field under the modal must not be lit by the pre-sanitation focus"
        );
    }

    #[test]
    fn focus_sanitation_restores_the_modal_default() {
        let mut app = App::new();
        app.init_resource::<InputFocus>()
            .init_resource::<ModalCoordinator>()
            .add_systems(PostUpdate, sanitize_modal_focus);
        let root = app.world_mut().spawn_empty().id();
        let default_focus = app.world_mut().spawn_empty().id();
        let underlying = app.world_mut().spawn_empty().id();
        app.world_mut().entity_mut(root).add_child(default_focus);
        app.world_mut().resource_mut::<ModalCoordinator>().active = Some(ActiveModal {
            presenter: ModalPresenter::Interaction,
            correlation: InteractionId::next(),
            root,
            focus_root: root,
            default_focus,
            previous_focus: None,
        });
        app.world_mut()
            .resource_mut::<InputFocus>()
            .set(underlying, FocusCause::Pressed);

        app.update();

        assert_eq!(
            app.world().resource::<InputFocus>().get(),
            Some(default_focus)
        );
    }

    #[test]
    fn choice_single_select_resolves_the_selected_key() {
        let mut app = interaction_test_app();
        let request = InteractionRequest::choice(
            "Pick one",
            "Only one item may be selected.",
            [
                ChoiceItem::new("first", "First"),
                ChoiceItem::new("second", "Second"),
            ],
        )
        .initial_choice("first");
        let id = request.id();
        app.world_mut().write_message(request);
        app.update();

        let second = {
            let world = app.world_mut();
            let mut query = world.query::<(Entity, &ChoiceOption)>();
            query
                .iter(world)
                .find(|(_, option)| option.key == "second")
                .map(|(entity, _)| entity)
                .unwrap()
        };
        app.world_mut().trigger(Activate { entity: second });
        app.world_mut().flush();
        trigger_action(&mut app, "ok");

        let messages = app.world().resource::<Messages<InteractionResult>>();
        let mut cursor = messages.get_cursor();
        let results: Vec<_> = cursor.read(messages).collect();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, id);
        assert_eq!(
            results[0].outcome,
            InteractionOutcome::Resolved(InteractionValue::Choice("second".into()))
        );
    }

    #[test]
    fn multichoice_resolves_in_item_order_not_click_order() {
        let mut app = interaction_test_app();
        let request = InteractionRequest::multi_choice(
            "Pick several",
            "Result order follows item order.",
            [
                ChoiceItem::new("alpha", "Alpha"),
                ChoiceItem::new("beta", "Beta"),
                ChoiceItem::new("gamma", "Gamma"),
            ],
        )
        .initial_choices(["beta"]);
        app.world_mut().write_message(request);
        app.update();

        for key in ["gamma", "alpha"] {
            let entity = {
                let world = app.world_mut();
                let mut query = world.query::<(Entity, &ChoiceOption)>();
                query
                    .iter(world)
                    .find(|(_, option)| option.key == key)
                    .map(|(entity, _)| entity)
                    .unwrap()
            };
            app.world_mut().trigger(Activate { entity });
            app.world_mut().flush();
        }
        trigger_action(&mut app, "ok");

        let messages = app.world().resource::<Messages<InteractionResult>>();
        let mut cursor = messages.get_cursor();
        let results: Vec<_> = cursor.read(messages).collect();
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].outcome,
            InteractionOutcome::Resolved(InteractionValue::MultiChoice(vec![
                "alpha".into(),
                "beta".into(),
                "gamma".into(),
            ]))
        );
    }

    #[test]
    fn slider_clamps_steps_and_resolves_an_integer() {
        let spec = SliderSpec::new(0, 10, 3, 8);
        assert_eq!(spec.initial, 9);
        assert_eq!(canonical_slider_value(0, 10, 3, -50), 0);
        assert_eq!(canonical_slider_value(0, 10, 3, 50), 9);

        let mut app = interaction_test_app();
        app.world_mut().write_message(InteractionRequest::slider(
            "Value",
            "Choose a value.",
            0,
            10,
            3,
            8,
        ));
        app.update();
        let control = match &app
            .world()
            .resource::<InteractionState>()
            .active
            .as_ref()
            .unwrap()
            .input
        {
            ActiveInput::Slider { control, .. } => *control,
            _ => panic!("slider input is active"),
        };
        app.world_mut()
            .entity_mut(control)
            .insert(ControlValue(50.0));
        trigger_action(&mut app, "ok");

        let messages = app.world().resource::<Messages<InteractionResult>>();
        let mut cursor = messages.get_cursor();
        let results: Vec<_> = cursor.read(messages).collect();
        assert_eq!(
            results[0].outcome,
            InteractionOutcome::Resolved(InteractionValue::Slider(9))
        );
    }

    #[test]
    fn normalise_slider_range_swaps_but_never_widens() {
        // Inverted range is corrected by swap, keeping the caller's bounds.
        assert_eq!(normalise_slider_range(10, 0, 2), (0, 10, 2));
        // Single-value range stays a single value (span 0, step 1) — not widened to min+1.
        assert_eq!(normalise_slider_range(5, 5, 3), (5, 5, 1));
        assert_eq!(
            normalise_slider_range(i32::MAX, i32::MAX, 1),
            (i32::MAX, i32::MAX, 1)
        );
    }

    #[test]
    fn slider_single_value_range_resolves_that_value_without_panicking() {
        // A min == max slider once widened its range and could panic building the f32
        // fader mapping; it must now spawn cleanly and resolve exactly the single value.
        assert_eq!(SliderSpec::new(5, 5, 1, 5).initial, 5);

        let mut app = interaction_test_app();
        app.world_mut().write_message(InteractionRequest::slider(
            "Value",
            "Only one value is possible.",
            5,
            5,
            1,
            5,
        ));
        app.update();
        trigger_action(&mut app, "ok");

        let messages = app.world().resource::<Messages<InteractionResult>>();
        let mut cursor = messages.get_cursor();
        let results: Vec<_> = cursor.read(messages).collect();
        assert_eq!(
            results[0].outcome,
            InteractionOutcome::Resolved(InteractionValue::Slider(5))
        );
    }

    #[test]
    fn slider_live_label_matches_the_resolved_grid_value() {
        // The f32 fader mapping is deliberately widened, so a raw ControlValue can round
        // to one unit off the true grid at collapsed-precision endpoints. The live readout
        // must re-canonicalise so it never disagrees with what OK resolves.
        const MIN: i32 = 16_777_217;
        let mut app = interaction_test_app();
        app.world_mut().write_message(InteractionRequest::slider(
            "Value",
            "Single value at collapsed precision.",
            MIN,
            MIN,
            1,
            MIN,
        ));
        app.update();
        let control = match &app
            .world()
            .resource::<InteractionState>()
            .active
            .as_ref()
            .unwrap()
            .input
        {
            ActiveInput::Slider { control, .. } => *control,
            _ => panic!("slider input is active"),
        };
        // Whatever raw f32 the fader holds, the label re-canonicalises to the grid value.
        app.world_mut()
            .entity_mut(control)
            .insert(ControlValue(16_777_216.0));
        app.update();
        let world = app.world_mut();
        let mut labels = world.query::<(&SliderValueLabel, &Text)>();
        let (_, text) = labels.iter(world).next().expect("a slider label exists");
        assert_eq!(text.as_str(), MIN.to_string());
    }

    #[test]
    fn slider_extreme_range_spawns_and_resolves_in_range() {
        // Two distinct i32 endpoints at ≥ 2^24 collapse to the SAME f32 (both round to
        // 16_777_216.0); without the next_up widening, ValueMapping::linear would receive
        // an empty span and panic. Spawning must not panic and resolution must stay in
        // [min, max] on the true i32 grid. (i32::MIN..i32::MAX does NOT collapse, so it
        // would not have exercised this failure.)
        const MIN: i32 = 16_777_216;
        const MAX: i32 = 16_777_217;
        let mut app = interaction_test_app();
        app.world_mut().write_message(InteractionRequest::slider(
            "Value",
            "Collapsed-precision endpoints.",
            MIN,
            MAX,
            1,
            MIN,
        ));
        app.update();
        let control = match &app
            .world()
            .resource::<InteractionState>()
            .active
            .as_ref()
            .unwrap()
            .input
        {
            ActiveInput::Slider { control, .. } => *control,
            _ => panic!("slider input is active"),
        };
        // Drive the fader past its top; resolution re-clamps against the true i32 grid.
        app.world_mut()
            .entity_mut(control)
            .insert(ControlValue(f32::MAX));
        trigger_action(&mut app, "ok");

        let messages = app.world().resource::<Messages<InteractionResult>>();
        let mut cursor = messages.get_cursor();
        let results: Vec<_> = cursor.read(messages).collect();
        match results[0].outcome {
            InteractionOutcome::Resolved(InteractionValue::Slider(v)) => {
                assert!((MIN..=MAX).contains(&v));
                assert_eq!(v, MAX, "driving past the top resolves the reachable max");
            }
            ref other => panic!("expected a resolved slider value, got {other:?}"),
        }
    }

    #[test]
    fn choice_dedup_keeps_first_occurrence_of_a_key() {
        let mut app = interaction_test_app();
        app.world_mut().write_message(InteractionRequest::choice(
            "Pick one",
            "Duplicate keys collapse to the first.",
            [
                ChoiceItem::new("dup", "First label"),
                ChoiceItem::new("dup", "Second label"),
                ChoiceItem::new("other", "Other"),
            ],
        ));
        app.update();

        let world = app.world_mut();
        let mut query = world.query::<&ChoiceOption>();
        let options: Vec<_> = query.iter(world).collect();
        assert_eq!(options.len(), 2, "duplicate key must be collapsed");
        let dup: Vec<_> = options.iter().filter(|o| o.key == "dup").collect();
        assert_eq!(dup.len(), 1);
    }

    #[test]
    fn textview_acknowledges_with_the_message_value_shape() {
        let mut app = interaction_test_app();
        let request =
            InteractionRequest::text_view("Log", "Read-only output.", "line one\nline two")
                .monospace();
        assert!(matches!(
            request.kind(),
            InteractionKind::TextView(TextViewSpec {
                monospace: true,
                ..
            })
        ));
        app.world_mut().write_message(request);
        app.update();
        trigger_action(&mut app, "ok");

        let messages = app.world().resource::<Messages<InteractionResult>>();
        let mut cursor = messages.get_cursor();
        let results: Vec<_> = cursor.read(messages).collect();
        assert_eq!(
            results[0].outcome,
            InteractionOutcome::Resolved(InteractionValue::Acknowledged)
        );
    }

    #[test]
    fn progress_never_captures_or_blocks_the_modal_lane() {
        let mut app = interaction_test_app();
        let progress = InteractionRequest::progress(
            "Exporting",
            "Preparing files…",
            ProgressValue::Determinate {
                current: 0,
                total: 10,
            },
        )
        .cancellable();
        let progress_id = progress.id();
        app.world_mut().write_message(progress);
        app.update();

        assert!(app
            .world()
            .resource::<ProgressState>()
            .is_active(progress_id));
        assert!(!app.world().resource::<ModalCapture>().is_captured());
        assert!(!app.world().resource::<ModalCoordinator>().is_active());
        assert_eq!(app.world().resource::<ModalCoordinator>().queued_len(), 0);

        app.world_mut().write_message(InteractionRequest::message(
            "Modal",
            "This must open while progress remains active.",
        ));
        app.update();

        assert!(app.world().resource::<ModalCapture>().is_captured());
        assert!(app.world().resource::<ModalCoordinator>().is_active());
        assert_eq!(app.world().resource::<ModalCoordinator>().queued_len(), 0);
        assert!(app
            .world()
            .resource::<ProgressState>()
            .is_active(progress_id));
    }

    #[test]
    fn progress_accepts_same_frame_owner_updates_and_completion() {
        let mut app = interaction_test_app();
        let request =
            InteractionRequest::progress("Indexing", "Starting…", ProgressValue::Indeterminate);
        let id = request.id();
        app.world_mut().write_message(request);
        app.world_mut()
            .write_message(ProgressUpdate::new(id).label("Indexed 4 of 4").progress(
                ProgressValue::Determinate {
                    current: 4,
                    total: 4,
                },
            ));
        app.world_mut()
            .write_message(ProgressComplete::new(id, ProgressCompletion::Succeeded));

        app.update();

        assert!(!app.world().resource::<ProgressState>().is_active(id));
        assert!(!app.world().resource::<ModalCapture>().is_captured());
        let messages = app.world().resource::<Messages<InteractionResult>>();
        let mut cursor = messages.get_cursor();
        let results: Vec<_> = cursor.read(messages).collect();
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].outcome,
            InteractionOutcome::Resolved(InteractionValue::Progress(ProgressCompletion::Succeeded))
        );
    }
}
