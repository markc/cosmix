//! Shared wrapped, multi-line plain-text editing.
//!
//! CTK deliberately rides Bevy 0.19's [`EditableText`] rather than owning a
//! second editing core. Source inspection of `bevy_text::{editing,text_edit}`
//! and `bevy_ui_widgets::text_input` found that the upstream Parley
//! `PlainEditor` path soundly supports newlines, soft wrapping, vertical
//! scrolling with the caret kept visible, visual-line Up/Down/Home/End,
//! hard-line and whole-text motion (including Ctrl+Home/End), word motion,
//! bidi, keyboard and pointer selection (shift-click, drag and double-click
//! word), clipboard copy/cut/paste with ordered asynchronous
//! polling, `EditableTextFilter`, `max_characters`, focused-input consumption,
//! and IME preedit plus commit. Parley's
//! `Selection::move_lines` retains its horizontal `h_pos` across consecutive
//! vertical moves, so CTK's Page Up/Down synthesis queues repeated Up/Down
//! edits and inherits the same goal-column behaviour. Paste remains correctly
//! ordered behind Bevy's asynchronous clipboard poller.
//!
//! The source spike also established the boundary. Bevy 0.19 has no undo/redo,
//! placeholder, Page Up/Down edit variant, overwrite mode, form events, or
//! `EditableText` AccessKit integration. Its `ImeCommit` capacity check also
//! fails to subtract a replaced selection. CTK supplies bounded transaction
//! undo/redo, page synthesis, max-length/read-only policy, change/blur/submit
//! events, and an AccessKit [`Role::MultilineTextInput`]. Each hard line is a
//! separate `TextRun`, and the canonical anchor/focus is remapped to the
//! containing run (a collapsed selection is the caret). Hard-line runs are
//! deliberately not joined by `previous_on_line`/`next_on_line`, because those
//! links mean “same rendered line” in AccessKit. Bevy does not expose editable
//! glyph positions through its AccessKit layer or route `SetTextSelection`
//! actions into `EditableText`, so CTK cannot report per-character screen
//! geometry, soft-wrap line links, or accept assistive-technology selection
//! commands yet.
//!
//! Upstream's module docs advertise triple-click line selection, but the 0.19
//! pointer handler actually queues `SelectAll` for the third and later clicks.
//! CTK translates that case to hard-line (paragraph) selection. Double-click
//! word selection, shift-click extension and dragging remain upstream.
//!
//! IME preedit rendering, preedit-internal selection, canonical value
//! exclusion, commit delivery, and Wayland candidate-window positioning were
//! source-verified in Bevy 0.19. CTK keeps the original value and selection as
//! the persistent policy baseline until commit, cancellation, or an explicit
//! programmatic replacement resolves the composition. Its read-only
//! cancellation and selection-aware max-length commit paths were exercised
//! with synthetic [`Ime`] messages. This machine has no fcitx5/IBus service
//! and an automated session has no interactive seat, so live Wayland CJK
//! preedit/commit remains unverified and requires a later seat test.
//!
//! [`CtkTextAreaPlugin`] installs Bevy's `EditableTextInputPlugin` when needed
//! and enforces its `UiPlugin` contract at application finish in all build
//! profiles. OS clipboard integration is opt-in through CTK's
//! `system-clipboard` feature; without it Bevy's process-local clipboard keeps
//! copy/cut/paste functional within the app.
//! Final programmatic-replacement policy and observer event emission share one
//! system. An unordered `PostUpdate` writer therefore runs wholly before it
//! (clamped and reported that frame) or after it (clamped before any event on
//! the next frame); no observer can receive an unchecked intermediate value.
//!
//! V1 owns plain text only: no rich text, syntax highlighting, spellcheck,
//! placeholder, or overwrite mode. Named future extension points are
//! decoration overlays (spellcheck/syntax), a non-value placeholder layer, and
//! an application-provided completion service; none belongs in the editing
//! buffer.

use std::collections::VecDeque;
use std::ops::Range;

use accesskit::{NodeId, Role, TextPosition, TextSelection};
use bevy::a11y::{AccessibilityNode, AccessibilitySystems};
use bevy::camera::RenderTarget;
use bevy::clipboard::Clipboard;
use bevy::feathers::theme::{ThemeBackgroundColor, ThemeTextColor, UiTheme};
use bevy::input::keyboard::{Key, KeyboardInput};
use bevy::input::mouse::{MouseScrollUnit, MouseWheel};
use bevy::input_focus::tab_navigation::TabIndex;
use bevy::input_focus::{FocusGained, FocusLost, FocusedInput, InputFocus};
use bevy::picking::events::{Pointer, Press};
use bevy::picking::hover::Hovered;
use bevy::picking::pointer::PointerButton;
use bevy::prelude::*;
use bevy::text::{
    EditableText, EditableTextFilter, EditableTextSystems, FontCx, LayoutCx, TextCursorStyle,
    TextEdit,
};
use bevy::ui::widget::TextScroll;
use bevy::ui::{
    ComputedNode, ComputedUiRenderTargetInfo, ComputedUiTargetCamera, Overflow, UiGlobalTransform,
    UiPlugin, UiScale, UiSystems,
};
use bevy::ui_widgets::EditableTextInputPlugin;
use bevy::window::{PrimaryWindow, WindowFocused, WindowRef};
use unicode_segmentation::UnicodeSegmentation;

use crate::text_field::{CtkTextFieldPlugin, CtkTextInputFocusBorder};
use crate::theme::tokens;

const DEFAULT_MAX_LEN: usize = 64 * 1024;
const DEFAULT_HISTORY_LIMIT: usize = 128;
const DEFAULT_VISIBLE_LINES: usize = 12;
const DEFAULT_MIN_HEIGHT: f32 = 180.0;
const TYPING_COALESCE_SECS: f64 = 0.75;

/// Behaviour and CTK-owned state attached to the editable node.
#[derive(Component, Clone, Debug)]
pub struct CtkTextArea {
    max_len: usize,
    read_only: bool,
    visible_lines: usize,
    history: EditHistory,
    last_snapshot: EditSnapshot,
    policy_snapshot: EditSnapshot,
    last_read_only: bool,
    history_requests: VecDeque<HistoryAction>,
    page_requests: VecDeque<(PageDirection, bool, usize)>,
    triple_click_point: Option<Vec2>,
    blur_requested: bool,
    blur_latched: bool,
    submit_requested: bool,
    a11y_runs: Vec<Entity>,
    pending_paste_before: Option<EditSnapshot>,
    ime_transaction_before: Option<EditSnapshot>,
    filtered_edits: VecDeque<TextEdit>,
    filtered_inflight: Option<FilteredTransaction>,
    manual_scroll_y: Option<f32>,
}

impl CtkTextArea {
    fn new(
        initial: String,
        max_len: usize,
        read_only: bool,
        visible_lines: usize,
        history_limit: usize,
        a11y_runs: Vec<Entity>,
    ) -> Self {
        Self {
            max_len,
            read_only,
            visible_lines: visible_lines.max(1),
            history: EditHistory::new(history_limit),
            last_snapshot: EditSnapshot {
                anchor: initial.len(),
                focus: initial.len(),
                text: initial.clone(),
            },
            policy_snapshot: EditSnapshot {
                anchor: initial.len(),
                focus: initial.len(),
                text: initial,
            },
            last_read_only: read_only,
            history_requests: VecDeque::new(),
            page_requests: VecDeque::new(),
            triple_click_point: None,
            blur_requested: false,
            blur_latched: false,
            submit_requested: false,
            a11y_runs,
            pending_paste_before: None,
            ime_transaction_before: None,
            filtered_edits: VecDeque::new(),
            filtered_inflight: None,
            manual_scroll_y: None,
        }
    }

    /// Maximum Unicode scalar-value count accepted by user and programmatic edits.
    pub fn max_len(&self) -> usize {
        self.max_len
    }

    /// Whether user-originated mutations and history commands are blocked.
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Change user-editability without replacing the current value or selection.
    pub fn set_read_only(&mut self, read_only: bool) {
        self.read_only = read_only;
    }

    /// Number of value snapshots retained in each history direction.
    pub fn history_limit(&self) -> usize {
        self.history.limit
    }
}

/// Construction properties for a [`CtkTextArea`].
#[derive(Clone, Debug)]
pub struct CtkTextAreaProps {
    pub initial: String,
    pub accessible_label: String,
    pub max_len: usize,
    pub read_only: bool,
    pub visible_lines: usize,
    pub history_limit: usize,
    pub min_height: f32,
}

impl CtkTextAreaProps {
    pub fn new(initial: impl Into<String>, accessible_label: impl Into<String>) -> Self {
        Self {
            initial: initial.into(),
            accessible_label: accessible_label.into(),
            max_len: DEFAULT_MAX_LEN,
            read_only: false,
            visible_lines: DEFAULT_VISIBLE_LINES,
            history_limit: DEFAULT_HISTORY_LIMIT,
            min_height: DEFAULT_MIN_HEIGHT,
        }
    }

    pub fn max_len(mut self, max_len: usize) -> Self {
        self.max_len = max_len;
        self
    }

    pub fn read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    pub fn visible_lines(mut self, visible_lines: usize) -> Self {
        self.visible_lines = visible_lines.max(1);
        self
    }

    pub fn history_limit(mut self, history_limit: usize) -> Self {
        self.history_limit = history_limit;
        self
    }

    pub fn min_height(mut self, min_height: f32) -> Self {
        self.min_height = min_height.max(0.0);
        self
    }
}

/// The layout root and actual editable node of a text area.
#[derive(Clone, Copy, Debug)]
pub struct CtkTextAreaEntities {
    pub root: Entity,
    pub input: Entity,
}

/// Spawn a wrapped multi-line text area.
pub fn spawn_text_area(commands: &mut Commands, props: CtkTextAreaProps) -> CtkTextAreaEntities {
    let initial = truncate_chars(&props.initial, props.max_len);
    let initial_lines = hard_lines(&initial);
    let a11y_runs: Vec<Entity> = initial_lines
        .iter()
        .map(|_| commands.spawn_empty().id())
        .collect();
    for (&line, &entity) in initial_lines.iter().zip(&a11y_runs) {
        commands.entity(entity).insert(text_run_accessibility(line));
    }

    let mut editable = EditableText::new(&initial);
    editable.max_characters = Some(props.max_len);
    editable.visible_lines = Some(props.visible_lines.max(1) as f32);
    editable.allow_newlines = true;

    let input = commands
        .spawn((
            Node {
                width: percent(100),
                min_width: px(100),
                min_height: px(props.min_height),
                height: percent(100),
                flex_grow: 1.0,
                padding: UiRect::axes(px(7), px(4)),
                border: UiRect::all(px(1)),
                overflow: Overflow::clip(),
                ..default()
            },
            editable,
            TextLayout::default(),
            TextFont::from_font_size(13.0),
            ThemeTextColor(tokens::TEXT),
            TextCursorStyle::default(),
            TextScroll::default(),
            Hovered::default(),
            ThemeBackgroundColor(tokens::SURFACE),
            BorderColor::all(Color::NONE),
            TabIndex(0),
            text_area_accessibility(&props.accessible_label, props.read_only),
            CtkTextInputFocusBorder::new(tokens::CONTROL),
            CtkTextArea::new(
                initial,
                props.max_len,
                props.read_only,
                props.visible_lines,
                props.history_limit,
                a11y_runs.clone(),
            ),
        ))
        .add_children(&a11y_runs)
        .id();

    let root = commands
        .spawn(Node {
            width: percent(100),
            min_width: px(100),
            min_height: px(props.min_height),
            flex_grow: 1.0,
            flex_direction: FlexDirection::Column,
            ..default()
        })
        .add_child(input)
        .id();

    CtkTextAreaEntities { root, input }
}

/// Emitted after the persistent value changes, including undo and redo.
#[derive(EntityEvent, Clone, Debug, PartialEq, Eq)]
pub struct CtkTextAreaChanged {
    #[event_target]
    pub area: Entity,
    pub value: String,
}

/// Emitted after the area loses input focus.
#[derive(EntityEvent, Clone, Debug, PartialEq, Eq)]
pub struct CtkTextAreaBlurred {
    #[event_target]
    pub area: Entity,
    pub value: String,
}

/// Emitted for Ctrl+Enter (Command+Enter on macOS); ordinary Enter inserts a newline.
#[derive(EntityEvent, Clone, Debug, PartialEq, Eq)]
pub struct CtkTextAreaSubmitted {
    #[event_target]
    pub area: Entity,
    pub value: String,
}

/// Programmatic undo request.
#[derive(EntityEvent, Clone, Copy, Debug, PartialEq, Eq)]
pub struct CtkTextAreaUndo {
    #[event_target]
    pub area: Entity,
}

/// Programmatic redo request.
#[derive(EntityEvent, Clone, Copy, Debug, PartialEq, Eq)]
pub struct CtkTextAreaRedo {
    #[event_target]
    pub area: Entity,
}

/// Queue an undo through the same observer path as the keyboard shortcut.
pub fn undo_text_area(commands: &mut Commands, area: Entity) {
    commands.trigger(CtkTextAreaUndo { area });
}

/// Queue a redo through the same observer path as the keyboard shortcut.
pub fn redo_text_area(commands: &mut Commands, area: Entity) {
    commands.trigger(CtkTextAreaRedo { area });
}

/// Installs text-area input policy, history, events and accessibility updates.
pub struct CtkTextAreaPlugin;

impl Plugin for CtkTextAreaPlugin {
    fn build(&self, app: &mut App) {
        if !app.is_plugin_added::<EditableTextInputPlugin>() {
            app.add_plugins(EditableTextInputPlugin);
        }
        if !app.is_plugin_added::<CtkTextFieldPlugin>() {
            app.add_plugins(CtkTextFieldPlugin);
        }
        app.init_resource::<InputFocus>()
            .init_resource::<UiTheme>()
            .add_observer(on_text_area_keyboard)
            .add_observer(on_text_area_focus_lost)
            .add_observer(on_text_area_focus_gained)
            .add_observer(on_text_area_undo)
            .add_observer(on_text_area_redo)
            .add_observer(on_text_area_pointer_press)
            .add_systems(PreUpdate, (blur_on_window_unfocus, scroll_text_areas))
            .add_systems(
                PostUpdate,
                process_text_area_edits.before(EditableTextSystems),
            )
            .add_systems(
                PostUpdate,
                finish_filtered_text_area_edits
                    .after(EditableTextSystems)
                    .before(UiSystems::PostLayout),
            )
            .add_systems(
                PostUpdate,
                (
                    apply_text_area_wheel_scroll,
                    sync_text_areas.before(AccessibilitySystems::Update),
                )
                    .chain()
                    .after(finish_filtered_text_area_edits)
                    .after(UiSystems::PostLayout),
            );
    }

    fn finish(&self, app: &mut App) {
        assert!(
            app.is_plugin_added::<UiPlugin>(),
            "CtkTextAreaPlugin requires Bevy's UiPlugin"
        );
        assert!(
            app.is_plugin_added::<EditableTextInputPlugin>(),
            "CtkTextAreaPlugin requires Bevy's EditableTextInputPlugin"
        );
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HistoryAction {
    Undo,
    Redo,
}

#[derive(Clone, Debug)]
struct EditHistory {
    undo: VecDeque<EditSnapshot>,
    redo: VecDeque<EditSnapshot>,
    limit: usize,
    typing: Option<TypingGroup>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct EditSnapshot {
    text: String,
    anchor: usize,
    focus: usize,
}

#[derive(Clone, Debug)]
struct FilteredTransaction {
    before: EditSnapshot,
    kind: TransactionKind,
    commit_expected: Option<Option<EditSnapshot>>,
}

struct ProgrammaticReplacement {
    previous: EditSnapshot,
    replacement: EditSnapshot,
    max_len: usize,
    now: f64,
}

#[derive(Clone, Copy, Debug)]
struct TypingGroup {
    last_at: f64,
    next_caret: usize,
}

impl EditHistory {
    fn new(limit: usize) -> Self {
        Self {
            undo: VecDeque::new(),
            redo: VecDeque::new(),
            limit,
            typing: None,
        }
    }

    fn record(&mut self, previous: EditSnapshot) {
        if self.limit == 0 {
            self.undo.clear();
            self.redo.clear();
            return;
        }
        if self.undo.back() != Some(&previous) {
            self.undo.push_back(previous);
            trim_history(&mut self.undo, self.limit);
        }
        self.redo.clear();
    }

    fn record_transaction(
        &mut self,
        previous: EditSnapshot,
        current: &EditSnapshot,
        kind: TransactionKind,
        now: f64,
    ) {
        let coalesce = matches!(kind, TransactionKind::Typing { .. })
            && self.typing.is_some_and(|typing| {
                now - typing.last_at <= TYPING_COALESCE_SECS
                    && typing.next_caret == previous.focus
                    && previous.anchor == previous.focus
            });
        if !coalesce {
            self.record(previous);
        } else {
            self.redo.clear();
        }
        self.typing = match kind {
            TransactionKind::Typing { boundary: false } => Some(TypingGroup {
                last_at: now,
                next_caret: current.focus,
            }),
            _ => None,
        };
    }

    fn close_typing(&mut self) {
        self.typing = None;
    }

    fn undo(&mut self, current: EditSnapshot) -> Option<EditSnapshot> {
        self.close_typing();
        let previous = self.undo.pop_back()?;
        if self.limit > 0 {
            self.redo.push_back(current);
            trim_history(&mut self.redo, self.limit);
        }
        Some(previous)
    }

    fn redo(&mut self, current: EditSnapshot) -> Option<EditSnapshot> {
        self.close_typing();
        let next = self.redo.pop_back()?;
        if self.limit > 0 {
            self.undo.push_back(current);
            trim_history(&mut self.undo, self.limit);
        }
        Some(next)
    }
}

#[derive(Clone, Copy, Debug)]
enum TransactionKind {
    Typing { boundary: bool },
    Other,
}

fn trim_history(history: &mut VecDeque<EditSnapshot>, limit: usize) {
    while history.len() > limit {
        history.pop_front();
    }
}

fn on_text_area_undo(event: On<CtkTextAreaUndo>, mut areas: Query<&mut CtkTextArea>) {
    if let Ok(mut area) = areas.get_mut(event.area) {
        area.history_requests.push_back(HistoryAction::Undo);
    }
}

fn on_text_area_redo(event: On<CtkTextAreaRedo>, mut areas: Query<&mut CtkTextArea>) {
    if let Ok(mut area) = areas.get_mut(event.area) {
        area.history_requests.push_back(HistoryAction::Redo);
    }
}

fn on_text_area_focus_lost(event: On<FocusLost>, mut areas: Query<&mut CtkTextArea>) {
    if let Ok(mut area) = areas.get_mut(event.entity) {
        request_blur(&mut area);
    }
}

fn on_text_area_focus_gained(event: On<FocusGained>, mut areas: Query<&mut CtkTextArea>) {
    if let Ok(mut area) = areas.get_mut(event.entity) {
        area.blur_latched = false;
    }
}

fn request_blur(area: &mut CtkTextArea) {
    if !area.blur_latched {
        area.blur_latched = true;
        area.blur_requested = true;
    }
}

fn blur_on_window_unfocus(
    mut events: MessageReader<WindowFocused>,
    focus: Res<InputFocus>,
    mut areas: Query<(&ComputedUiTargetCamera, &mut CtkTextArea)>,
    cameras: Query<&RenderTarget>,
    primary_window: Query<Entity, With<PrimaryWindow>>,
) {
    for event in events.read() {
        let Some(focused) = focus.get() else {
            continue;
        };
        let Ok((target_camera, mut area)) = areas.get_mut(focused) else {
            continue;
        };
        if area_window(target_camera, &cameras, &primary_window) != Some(event.window) {
            continue;
        }
        if event.focused {
            area.blur_latched = false;
        } else {
            request_blur(&mut area);
        }
    }
}

fn on_text_area_keyboard(
    mut event: On<FocusedInput<KeyboardInput>>,
    keys: Res<ButtonInput<Key>>,
    mut areas: Query<(&EditableText, &ComputedNode, &mut CtkTextArea)>,
) {
    let Ok((editable, node, mut area)) = areas.get_mut(event.focused_entity) else {
        return;
    };
    if !event.input.state.is_pressed() || editable.is_composing() {
        return;
    }

    let shift = keys.pressed(Key::Shift);
    let control = keys.pressed(Key::Control);
    let super_key = keys.pressed(Key::Super);
    let alt = keys.pressed(Key::Alt);
    #[cfg(target_os = "macos")]
    let command = super_key;
    #[cfg(not(target_os = "macos"))]
    let command = control;

    let handled = match &event.input.logical_key {
        Key::PageUp if !control && !super_key && !alt => {
            queue_page_motion(
                &mut area,
                PageDirection::Up,
                shift,
                page_line_count(editable, node.content_box().height(), PageDirection::Up),
            );
            true
        }
        Key::PageDown if !control && !super_key && !alt => {
            queue_page_motion(
                &mut area,
                PageDirection::Down,
                shift,
                page_line_count(editable, node.content_box().height(), PageDirection::Down),
            );
            true
        }
        Key::Character(value) if command && !alt && value.eq_ignore_ascii_case("z") && shift => {
            area.history_requests.push_back(HistoryAction::Redo);
            true
        }
        Key::Character(value) if command && !alt && value.eq_ignore_ascii_case("z") && !shift => {
            area.history_requests.push_back(HistoryAction::Undo);
            true
        }
        Key::Character(value) if command && !alt && value.eq_ignore_ascii_case("y") && !shift => {
            area.history_requests.push_back(HistoryAction::Redo);
            true
        }
        Key::Enter if command && !alt && !shift && !event.input.repeat => {
            area.submit_requested = true;
            true
        }
        Key::Enter if command && !alt && !shift => true,
        _ => false,
    };
    if handled {
        event.propagate(false);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PageDirection {
    Up,
    Down,
}

fn queue_page_motion(area: &mut CtkTextArea, direction: PageDirection, extend: bool, lines: usize) {
    area.page_requests.push_back((direction, extend, lines));
}

fn page_edits(direction: PageDirection, extend: bool, viewport_lines: usize) -> Vec<TextEdit> {
    let count = viewport_lines.saturating_sub(1).max(1);
    let edit = match direction {
        PageDirection::Up => TextEdit::Up(extend),
        PageDirection::Down => TextEdit::Down(extend),
    };
    vec![edit; count]
}

fn page_line_count(
    editable: &EditableText,
    viewport_height: f32,
    direction: PageDirection,
) -> usize {
    let Some(layout) = editable.editor().try_layout() else {
        return 1;
    };
    let focus = editable.editor().raw_selection().focus().index();
    let lines: Vec<_> = layout.lines().collect();
    let current = lines
        .iter()
        .position(|line| {
            let range = line.text_range();
            focus >= range.start && focus < range.end
        })
        .unwrap_or_else(|| lines.len().saturating_sub(1));
    let mut used = 0.0;
    let count = match direction {
        PageDirection::Down => {
            let mut count = 0;
            for line in &lines[current..] {
                used += line.metrics().line_height;
                if used > viewport_height + f32::EPSILON {
                    break;
                }
                count += 1;
            }
            count
        }
        PageDirection::Up => {
            let mut count = 0;
            for line in lines[..=current].iter().rev() {
                used += line.metrics().line_height;
                if used > viewport_height + f32::EPSILON {
                    break;
                }
                count += 1;
            }
            count
        }
    };
    count.max(1)
}

fn on_text_area_pointer_press(
    press: On<Pointer<Press>>,
    mut areas: Query<(
        &EditableText,
        &ComputedNode,
        &ComputedUiRenderTargetInfo,
        &UiGlobalTransform,
        &TextScroll,
        &mut CtkTextArea,
    )>,
    ui_scale: Res<UiScale>,
) {
    if press.button != PointerButton::Primary || press.count < 3 {
        return;
    }
    let Ok((editable, node, target, transform, text_scroll, mut area)) =
        areas.get_mut(press.entity)
    else {
        return;
    };
    if editable.is_composing() {
        return;
    }
    area.triple_click_point = transform.try_inverse().map(|inverse| {
        inverse
            .transform_point2(press.pointer_location.position * target.scale_factor() / ui_scale.0)
            - node.content_box().min
            + text_scroll.0
    });
}

type ScrollTextAreas<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static Hovered,
        &'static EditableText,
        &'static ComputedNode,
        &'static ComputedUiTargetCamera,
        &'static TextScroll,
        &'static mut CtkTextArea,
    ),
    With<CtkTextArea>,
>;

fn scroll_text_areas(
    mut wheel: MessageReader<MouseWheel>,
    focus: Res<InputFocus>,
    mut areas: ScrollTextAreas,
    cameras: Query<&RenderTarget>,
    primary_window: Query<Entity, With<PrimaryWindow>>,
) {
    for event in wheel.read() {
        let target = areas
            .iter()
            .find_map(|(entity, hovered, _, _, target_camera, _, _)| {
                (hovered.get()
                    && area_window(target_camera, &cameras, &primary_window) == Some(event.window))
                .then_some(entity)
            })
            .or(focus.get().filter(|focused| {
                areas
                    .get(*focused)
                    .is_ok_and(|(_, _, _, _, target_camera, _, _)| {
                        area_window(target_camera, &cameras, &primary_window) == Some(event.window)
                    })
            }));
        let Some(target) = target else {
            continue;
        };
        let Ok((_, _, editable, node, _, scroll, mut area)) = areas.get_mut(target) else {
            continue;
        };
        let Some(layout) = editable.editor().try_layout() else {
            continue;
        };
        let line_height = layout
            .lines()
            .next()
            .map_or(1.0, |line| line.metrics().line_height.max(1.0));
        let delta = match event.unit {
            MouseScrollUnit::Line => event.y * line_height,
            MouseScrollUnit::Pixel => event.y,
        };
        let max_scroll = (layout.height() - node.content_box().height()).max(0.0);
        let current = area.manual_scroll_y.unwrap_or(scroll.0.y);
        area.manual_scroll_y = Some((current - delta).clamp(0.0, max_scroll));
    }
}

fn apply_text_area_wheel_scroll(
    mut areas: Query<(
        &EditableText,
        &ComputedNode,
        &mut TextScroll,
        &mut CtkTextArea,
    )>,
) {
    for (editable, node, mut scroll, mut area) in &mut areas {
        let Some(requested) = area.manual_scroll_y else {
            continue;
        };
        if snapshot(editable) != area.last_snapshot {
            area.manual_scroll_y = None;
            continue;
        }
        let Some(layout) = editable.editor().try_layout() else {
            continue;
        };
        let max_scroll = (layout.height() - node.content_box().height()).max(0.0);
        scroll.0.x = 0.0;
        scroll.0.y = requested.clamp(0.0, max_scroll);
    }
}

fn area_window(
    target_camera: &ComputedUiTargetCamera,
    cameras: &Query<&RenderTarget>,
    primary_window: &Query<Entity, With<PrimaryWindow>>,
) -> Option<Entity> {
    match cameras.get(target_camera.get()?).ok()? {
        RenderTarget::Window(WindowRef::Primary) => primary_window.single().ok(),
        RenderTarget::Window(WindowRef::Entity(window)) => Some(*window),
        _ => None,
    }
}

fn process_text_area_edits(
    mut areas: Query<(
        &mut EditableText,
        &mut CtkTextArea,
        Option<&EditableTextFilter>,
    )>,
    mut fonts: ResMut<FontCx>,
    mut layouts: ResMut<LayoutCx>,
    mut clipboard: ResMut<Clipboard>,
    time: Option<Res<Time<Real>>>,
) {
    let now = time.as_ref().map_or(0.0, |time| time.elapsed_secs_f64());
    for (mut editable, mut area, filter) in &mut areas {
        if editable.max_characters != Some(area.max_len) {
            editable.max_characters = Some(area.max_len);
        }
        if editable.visible_lines != Some(area.visible_lines as f32) {
            editable.visible_lines = Some(area.visible_lines as f32);
        }
        if !editable.allow_newlines {
            editable.allow_newlines = true;
        }

        if area.ime_transaction_before.is_some() {
            if !editable.is_composing() {
                resolve_programmatic_replacement_during_composition(
                    &mut editable,
                    &mut area,
                    now,
                    &mut fonts,
                    &mut layouts,
                );
            }
        } else if !editable.is_composing() {
            let current = snapshot(&editable);
            if current.text != area.policy_snapshot.text {
                let previous = area.policy_snapshot.clone();
                let max_len = area.max_len;
                area.policy_snapshot = reconcile_programmatic_replacement(
                    &mut editable,
                    &mut area.history,
                    ProgrammaticReplacement {
                        previous,
                        replacement: current,
                        max_len,
                        now,
                    },
                    &mut fonts,
                    &mut layouts,
                );
            }
        }

        if area.read_only && !area.last_read_only {
            if let Some(before) = area.ime_transaction_before.take() {
                restore_snapshot(&mut editable, &before, &mut fonts, &mut layouts);
            } else if editable.is_composing() {
                TextEdit::clear_ime_compose().apply(
                    &mut editable
                        .editor_mut()
                        .driver(&mut fonts.context, &mut layouts.0),
                    &mut clipboard,
                    Some(area.max_len),
                    |_| true,
                );
            }
        }
        area.last_read_only = area.read_only;

        if area.read_only {
            if editable.pending_paste.is_some() {
                editable.pending_paste = None;
            }
            area.pending_paste_before = None;
            area.filtered_inflight = None;
            area.filtered_edits.retain(read_only_edit_allowed);
            area.history_requests.clear();
        }

        while let Some((direction, extend, lines)) = area.page_requests.pop_front() {
            editable
                .pending_edits
                .extend(page_edits(direction, extend, lines));
        }

        if let Some(point) = area.triple_click_point.take() {
            if let Some(index) = editable
                .pending_edits
                .iter()
                .rposition(|edit| matches!(edit, TextEdit::SelectAll))
            {
                editable.pending_edits.remove(index);
            }
            editable.queue_edit(TextEdit::SelectedHardLineAtPoint(point));
        }

        if !area.history_requests.is_empty() {
            if let Some(before) = area.ime_transaction_before.take() {
                restore_snapshot(&mut editable, &before, &mut fonts, &mut layouts);
            }
        }
        while let Some(action) = area.history_requests.pop_front() {
            if area.read_only {
                continue;
            }
            let current = snapshot(&editable);
            let target = match action {
                HistoryAction::Undo => area.history.undo(current),
                HistoryAction::Redo => area.history.redo(current),
            };
            if let Some(target) = target {
                editable.pending_edits.clear();
                editable.pending_paste = None;
                area.pending_paste_before = None;
                area.filtered_edits.clear();
                area.filtered_inflight = None;
                restore_snapshot(&mut editable, &target, &mut fonts, &mut layouts);
            }
        }

        if filter.is_some() {
            if !editable.pending_edits.is_empty() {
                area.filtered_edits
                    .extend(std::mem::take(&mut editable.pending_edits));
            }
            stage_filtered_edit(&mut editable, &mut area, &mut fonts, &mut layouts);
            refresh_policy_snapshot(&editable, &mut area);
            continue;
        }

        if editable.pending_paste.is_some() {
            let queued = std::mem::take(&mut editable.pending_edits);
            editable.apply_pending_edits(
                &mut fonts.context,
                &mut layouts.0,
                &mut clipboard,
                |_| true,
            );
            editable.pending_edits = queued;
            if editable.pending_paste.is_some() {
                refresh_policy_snapshot(&editable, &mut area);
                continue;
            }
            if let Some(before) = area.pending_paste_before.take() {
                let after = snapshot(&editable);
                if before.text != after.text {
                    area.history
                        .record_transaction(before, &after, TransactionKind::Other, now);
                }
            }
        }

        if editable.pending_edits.is_empty() {
            refresh_policy_snapshot(&editable, &mut area);
            continue;
        }
        let edits = std::mem::take(&mut editable.pending_edits);
        let mut edits = edits.into_iter();
        while let Some(edit) = edits.next() {
            if area.read_only && !read_only_edit_allowed(&edit) {
                continue;
            }

            let mut before = snapshot(&editable);
            if is_mutating_edit(&edit) && !is_ime_edit(&edit) {
                if let Some(composition_start) = area.ime_transaction_before.take() {
                    restore_snapshot(&mut editable, &composition_start, &mut fonts, &mut layouts);
                    before = composition_start;
                }
            }
            let transaction = transaction_kind(&edit, &before);
            if !is_mutating_edit(&edit) {
                area.history.close_typing();
            }

            match edit {
                TextEdit::Paste => {
                    editable.queue_edit(TextEdit::Paste);
                    editable.apply_pending_edits(
                        &mut fonts.context,
                        &mut layouts.0,
                        &mut clipboard,
                        |_| true,
                    );
                    if editable.pending_paste.is_some() {
                        area.pending_paste_before = Some(before.clone());
                        editable.pending_edits.extend(edits);
                        break;
                    }
                }
                TextEdit::ImeCommit { value } => {
                    if let Some(composition_start) = area.ime_transaction_before.take() {
                        restore_snapshot(
                            &mut editable,
                            &composition_start,
                            &mut fonts,
                            &mut layouts,
                        );
                        before = composition_start;
                    }
                    TextEdit::Insert(value).apply(
                        &mut editable
                            .editor_mut()
                            .driver(&mut fonts.context, &mut layouts.0),
                        &mut clipboard,
                        Some(area.max_len),
                        |_| true,
                    );
                }
                TextEdit::ImeSetCompose { ref value, .. } if !value.is_empty() => {
                    area.ime_transaction_before.get_or_insert(before.clone());
                    edit.apply(
                        &mut editable
                            .editor_mut()
                            .driver(&mut fonts.context, &mut layouts.0),
                        &mut clipboard,
                        Some(area.max_len),
                        |_| true,
                    );
                    continue;
                }
                TextEdit::ImeSetCompose { .. } => {
                    if let Some(composition_start) = area.ime_transaction_before.take() {
                        restore_snapshot(
                            &mut editable,
                            &composition_start,
                            &mut fonts,
                            &mut layouts,
                        );
                    } else {
                        edit.apply(
                            &mut editable
                                .editor_mut()
                                .driver(&mut fonts.context, &mut layouts.0),
                            &mut clipboard,
                            Some(area.max_len),
                            |_| true,
                        );
                    }
                    continue;
                }
                other => other.apply(
                    &mut editable
                        .editor_mut()
                        .driver(&mut fonts.context, &mut layouts.0),
                    &mut clipboard,
                    Some(area.max_len),
                    |_| true,
                ),
            }

            let after = snapshot(&editable);
            if before.text != after.text {
                area.history
                    .record_transaction(before, &after, transaction, now);
            }
        }
        refresh_policy_snapshot(&editable, &mut area);
    }
}

fn refresh_policy_snapshot(editable: &EditableText, area: &mut CtkTextArea) {
    area.policy_snapshot = area
        .ime_transaction_before
        .clone()
        .unwrap_or_else(|| snapshot(editable));
}

fn stage_filtered_edit(
    editable: &mut Mut<EditableText>,
    area: &mut CtkTextArea,
    fonts: &mut FontCx,
    layouts: &mut LayoutCx,
) {
    if area.filtered_inflight.is_some() || editable.pending_paste.is_some() {
        return;
    }

    while let Some(edit) = area.filtered_edits.pop_front() {
        if area.read_only && !read_only_edit_allowed(&edit) {
            continue;
        }
        let mut before = snapshot(editable);
        if is_mutating_edit(&edit) && !is_ime_edit(&edit) {
            if let Some(composition_start) = area.ime_transaction_before.take() {
                restore_snapshot(editable, &composition_start, fonts, layouts);
                before = composition_start;
            }
        }
        match edit {
            TextEdit::ImeSetCompose { ref value, .. } if !value.is_empty() => {
                area.ime_transaction_before.get_or_insert(before);
                editable.queue_edit(edit);
                return;
            }
            TextEdit::ImeSetCompose { .. } => {
                if let Some(composition_start) = area.ime_transaction_before.take() {
                    restore_snapshot(editable, &composition_start, fonts, layouts);
                } else {
                    editable.queue_edit(edit);
                    return;
                }
            }
            TextEdit::ImeCommit { value } => {
                if let Some(composition_start) = area.ime_transaction_before.take() {
                    restore_snapshot(editable, &composition_start, fonts, layouts);
                    before = composition_start;
                }
                let expected = replacement_snapshot(&before, value.as_str(), area.max_len);
                editable.queue_edit(TextEdit::Insert(value));
                area.filtered_inflight = Some(FilteredTransaction {
                    before,
                    kind: TransactionKind::Other,
                    commit_expected: Some(expected),
                });
                return;
            }
            other => {
                let kind = transaction_kind(&other, &before);
                if !is_mutating_edit(&other) {
                    area.history.close_typing();
                    editable.queue_edit(other);
                    return;
                }
                editable.queue_edit(other);
                area.filtered_inflight = Some(FilteredTransaction {
                    before,
                    kind,
                    commit_expected: None,
                });
                return;
            }
        }
    }
}

fn replacement_snapshot(
    before: &EditSnapshot,
    insertion: &str,
    max_len: usize,
) -> Option<EditSnapshot> {
    let start = before.anchor.min(before.focus);
    let end = before.anchor.max(before.focus);
    let selected_chars = before.text[start..end].chars().count();
    let final_chars = before.text.chars().count() - selected_chars + insertion.chars().count();
    if final_chars > max_len {
        return None;
    }
    let mut text = before.text.clone();
    text.replace_range(start..end, insertion);
    let caret = start + insertion.len();
    Some(EditSnapshot {
        text,
        anchor: caret,
        focus: caret,
    })
}

fn finish_filtered_text_area_edits(
    mut areas: Query<(&mut EditableText, &mut CtkTextArea)>,
    mut fonts: ResMut<FontCx>,
    mut layouts: ResMut<LayoutCx>,
    time: Option<Res<Time<Real>>>,
) {
    let now = time.as_ref().map_or(0.0, |time| time.elapsed_secs_f64());
    for (mut editable, mut area) in &mut areas {
        if editable.pending_paste.is_some() {
            continue;
        }
        let Some(transaction) = area.filtered_inflight.take() else {
            continue;
        };
        let mut after = snapshot(&editable);
        if let Some(expected) = &transaction.commit_expected {
            if expected.as_ref() != Some(&after) {
                restore_snapshot(&mut editable, &transaction.before, &mut fonts, &mut layouts);
                after = transaction.before.clone();
            }
        }
        if transaction.before.text != after.text {
            area.history
                .record_transaction(transaction.before, &after, transaction.kind, now);
        }
        area.policy_snapshot = after;
    }
}

fn reconcile_programmatic_replacement(
    editable: &mut EditableText,
    history: &mut EditHistory,
    replacement: ProgrammaticReplacement,
    fonts: &mut FontCx,
    layouts: &mut LayoutCx,
) -> EditSnapshot {
    let current = snapshot(editable);
    let selection = editable.editor().raw_selection();
    let raw_anchor = selection.anchor().index();
    let raw_focus = selection.focus().index();
    let bounded = truncate_chars(&replacement.replacement.text, replacement.max_len);
    let target = EditSnapshot {
        anchor: snap_char_boundary(&bounded, replacement.replacement.anchor),
        focus: snap_char_boundary(&bounded, replacement.replacement.focus),
        text: bounded,
    };
    if target.text != current.text || raw_anchor != target.anchor || raw_focus != target.focus {
        restore_snapshot(editable, &target, fonts, layouts);
    }
    if target.text != replacement.previous.text {
        history.record_transaction(
            replacement.previous,
            &target,
            TransactionKind::Other,
            replacement.now,
        );
    }
    target
}

fn resolve_programmatic_replacement_during_composition(
    editable: &mut EditableText,
    area: &mut CtkTextArea,
    now: f64,
    fonts: &mut FontCx,
    layouts: &mut LayoutCx,
) {
    let replacement = snapshot(editable);
    let previous = area
        .ime_transaction_before
        .take()
        .expect("composition resolution requires a parked snapshot");
    restore_snapshot(editable, &previous, fonts, layouts);
    area.policy_snapshot = reconcile_programmatic_replacement(
        editable,
        &mut area.history,
        ProgrammaticReplacement {
            previous,
            replacement,
            max_len: area.max_len,
            now,
        },
        fonts,
        layouts,
    );
}

fn snapshot(editable: &EditableText) -> EditSnapshot {
    let text = editable.value().to_string();
    let selection = editable.editor().raw_selection();
    let compose = editable.editor().raw_compose().as_ref();
    EditSnapshot {
        anchor: snap_char_boundary(
            &text,
            canonical_byte(selection.anchor().index(), compose, text.len()),
        ),
        focus: snap_char_boundary(
            &text,
            canonical_byte(selection.focus().index(), compose, text.len()),
        ),
        text,
    }
}

fn snap_char_boundary(value: &str, index: usize) -> usize {
    let mut index = index.min(value.len());
    while !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn restore_snapshot(
    editable: &mut EditableText,
    snapshot: &EditSnapshot,
    fonts: &mut FontCx,
    layouts: &mut LayoutCx,
) {
    editable.editor_mut().set_text(&snapshot.text);
    editable
        .editor_mut()
        .driver(&mut fonts.context, &mut layouts.0)
        .select_byte_range(
            snap_char_boundary(&snapshot.text, snapshot.anchor),
            snap_char_boundary(&snapshot.text, snapshot.focus),
        );
}

fn transaction_kind(edit: &TextEdit, before: &EditSnapshot) -> TransactionKind {
    match edit {
        TextEdit::Insert(value)
            if before.anchor == before.focus
                && !value.is_empty()
                && !value.chars().any(char::is_control) =>
        {
            TransactionKind::Typing {
                boundary: value
                    .chars()
                    .last()
                    .is_some_and(|character| !character.is_alphanumeric()),
            }
        }
        _ => TransactionKind::Other,
    }
}

fn read_only_edit_allowed(edit: &TextEdit) -> bool {
    !is_mutating_edit(edit)
        || matches!(edit, TextEdit::ImeSetCompose { value, .. } if value.is_empty())
}

fn sync_text_areas(
    mut commands: Commands,
    mut areas: Query<(
        Entity,
        &mut EditableText,
        &mut CtkTextArea,
        &mut AccessibilityNode,
    )>,
    mut text_runs: Query<&mut AccessibilityNode, Without<CtkTextArea>>,
    mut fonts: ResMut<FontCx>,
    mut layouts: ResMut<LayoutCx>,
    time: Option<Res<Time<Real>>>,
) {
    let now = time.as_ref().map_or(0.0, |time| time.elapsed_secs_f64());
    for (entity, mut editable, mut area, mut accessibility) in &mut areas {
        if area.ime_transaction_before.is_some() {
            if editable.is_composing() {
                area.policy_snapshot = area.ime_transaction_before.clone().unwrap();
            } else {
                resolve_programmatic_replacement_during_composition(
                    &mut editable,
                    &mut area,
                    now,
                    &mut fonts,
                    &mut layouts,
                );
            }
        } else {
            let current = snapshot(&editable);
            if current.text != area.policy_snapshot.text {
                let previous = area.policy_snapshot.clone();
                let max_len = area.max_len;
                area.policy_snapshot = reconcile_programmatic_replacement(
                    &mut editable,
                    &mut area.history,
                    ProgrammaticReplacement {
                        previous,
                        replacement: current,
                        max_len,
                        now,
                    },
                    &mut fonts,
                    &mut layouts,
                );
            } else {
                area.policy_snapshot = current;
            }
        }

        let value = editable.value().to_string();
        let lines = hard_lines(&value);
        while area.a11y_runs.len() < lines.len() {
            let run = commands.spawn_empty().id();
            commands.entity(entity).add_child(run);
            area.a11y_runs.push(run);
        }
        while area.a11y_runs.len() > lines.len() {
            if let Some(run) = area.a11y_runs.pop() {
                commands.entity(run).despawn();
            }
        }
        for (&line, &run) in lines.iter().zip(&area.a11y_runs) {
            let node = text_run_accessibility(line);
            if let Ok(mut current) = text_runs.get_mut(run) {
                *current = node;
            } else {
                commands.entity(run).insert(node);
            }
        }

        sync_text_accessibility(
            &editable,
            &value,
            area.read_only,
            &area.a11y_runs,
            &mut accessibility,
        );

        let composing = area.ime_transaction_before.is_some();
        if !composing {
            if value != area.last_snapshot.text {
                commands.trigger(CtkTextAreaChanged {
                    area: entity,
                    value: value.clone(),
                });
            }
            area.last_snapshot = snapshot(&editable);
        }

        let reported_value = area
            .ime_transaction_before
            .as_ref()
            .map_or_else(|| value.clone(), |before| before.text.clone());
        if std::mem::take(&mut area.blur_requested) {
            commands.trigger(CtkTextAreaBlurred {
                area: entity,
                value: reported_value,
            });
        }
        if std::mem::take(&mut area.submit_requested) {
            commands.trigger(CtkTextAreaSubmitted {
                area: entity,
                value,
            });
        }
    }
}

fn is_mutating_edit(edit: &TextEdit) -> bool {
    matches!(
        edit,
        TextEdit::Cut
            | TextEdit::Paste
            | TextEdit::Insert(_)
            | TextEdit::Backspace
            | TextEdit::BackspaceWord
            | TextEdit::Delete
            | TextEdit::DeleteWord
            | TextEdit::ImeSetCompose { .. }
            | TextEdit::ImeCommit { .. }
    )
}

fn is_ime_edit(edit: &TextEdit) -> bool {
    matches!(
        edit,
        TextEdit::ImeSetCompose { .. } | TextEdit::ImeCommit { .. }
    )
}

fn truncate_chars(value: &str, max_len: usize) -> String {
    if value.chars().count() <= max_len {
        return value.to_owned();
    }
    value.chars().take(max_len).collect()
}

fn text_area_accessibility(label: &str, read_only: bool) -> AccessibilityNode {
    let mut node = accesskit::Node::new(Role::MultilineTextInput);
    node.set_label(label);
    if read_only {
        node.set_read_only();
    }
    AccessibilityNode::from(node)
}

fn hard_lines(value: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = value.split_inclusive('\n').collect();
    if value.is_empty() || value.ends_with('\n') {
        lines.push("");
    }
    lines
}

fn text_run_accessibility(value: &str) -> AccessibilityNode {
    let mut node = accesskit::Node::new(Role::TextRun);
    node.set_value(value);
    node.set_character_lengths(a11y_character_lengths(value));
    AccessibilityNode::from(node)
}

fn sync_text_accessibility(
    editable: &EditableText,
    value: &str,
    read_only: bool,
    text_runs: &[Entity],
    parent: &mut AccessibilityNode,
) {
    parent.set_value(value);
    if read_only {
        parent.set_read_only();
    } else {
        parent.clear_read_only();
    }

    let selection = editable.editor().raw_selection();
    let compose = editable.editor().raw_compose().clone();
    let anchor_byte = canonical_byte(selection.anchor().index(), compose.as_ref(), value.len());
    let focus_byte = canonical_byte(selection.focus().index(), compose.as_ref(), value.len());
    parent.set_text_selection(TextSelection {
        anchor: a11y_position(value, anchor_byte, text_runs),
        focus: a11y_position(value, focus_byte, text_runs),
    });
}

fn a11y_position(value: &str, byte: usize, runs: &[Entity]) -> TextPosition {
    let lines = hard_lines(value);
    let mut start = 0;
    let byte = byte.min(value.len());
    for (index, line) in lines.iter().enumerate() {
        let end = start + line.len();
        if byte < end || index + 1 == lines.len() {
            return TextPosition {
                node: NodeId(runs[index].to_bits()),
                character_index: byte_to_a11y_index(line, byte.saturating_sub(start)),
            };
        }
        start = end;
    }
    unreachable!("hard_lines always returns at least one run")
}

fn canonical_byte(raw: usize, compose: Option<&Range<usize>>, value_len: usize) -> usize {
    let canonical = match compose {
        Some(compose) if raw > compose.start && raw < compose.end => compose.start,
        Some(compose) if raw >= compose.end => raw.saturating_sub(compose.len()),
        _ => raw,
    };
    canonical.min(value_len)
}

fn a11y_character_lengths(value: &str) -> Vec<u8> {
    let grapheme_lengths: Vec<usize> = value.graphemes(true).map(str::len).collect();
    if grapheme_lengths
        .iter()
        .all(|length| u8::try_from(*length).is_ok())
    {
        grapheme_lengths
            .into_iter()
            .map(|length| u8::try_from(length).expect("length checked above"))
            .collect()
    } else {
        value
            .chars()
            .map(|character| character.len_utf8() as u8)
            .collect()
    }
}

fn byte_to_a11y_index(value: &str, byte: usize) -> usize {
    let graphemes_fit = value
        .graphemes(true)
        .all(|part| part.len() <= u8::MAX as usize);
    if graphemes_fit {
        value
            .grapheme_indices(true)
            .take_while(|(start, _)| *start < byte)
            .count()
    } else {
        value
            .char_indices()
            .take_while(|(start, _)| *start < byte)
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use bevy::asset::AssetPlugin;
    use bevy::camera::{Camera2d, CameraPlugin, ComputedCameraValues, RenderTargetInfo, Viewport};
    use bevy::clipboard::ClipboardRead;
    use bevy::ecs::change_detection::Tick;
    use bevy::ecs::system::RunSystemOnce;
    use bevy::image::{ImagePlugin, TextureAtlasPlugin};
    use bevy::input::{ButtonState, InputPlugin};
    use bevy::input_focus::{InputDispatchPlugin, InputFocusPlugin};
    use bevy::mesh::MeshPlugin;
    use bevy::picking::DefaultPickingPlugins;
    use bevy::platform::sync::Mutex;
    use bevy::text::TextPlugin;
    use bevy::transform::TransformPlugin;
    use bevy::ui::{UiPlugin, UiTargetCamera};

    #[test]
    fn parley_vertical_motion_retains_goal_column_across_a_short_line() {
        let mut editable = EditableText::new("abcdefghij\nx\nabcdefghij");
        editable.editor_mut().set_width(Some(1_000.0));
        let mut fonts = FontCx::default();
        let mut layouts = LayoutCx::default();
        {
            let mut driver = editable
                .editor_mut()
                .driver(&mut fonts.context, &mut layouts.0);
            driver.move_to_byte(8);
            driver.move_down();
            driver.move_down();
        }
        assert_eq!(editable.editor().raw_selection().focus().index(), 21);
    }

    #[test]
    fn history_is_bounded_and_redo_is_invalidated_by_a_new_edit() {
        fn state(value: &str, caret: usize) -> EditSnapshot {
            EditSnapshot {
                text: value.into(),
                anchor: caret,
                focus: caret,
            }
        }

        let mut history = EditHistory::new(2);
        history.record(state("zero", 0));
        history.record(state("one", 1));
        history.record(state("two", 2));
        assert_eq!(
            history
                .undo
                .iter()
                .map(|snapshot| snapshot.text.as_str())
                .collect::<Vec<_>>(),
            ["one", "two"]
        );

        let mut history = EditHistory::new(3);
        history.record(state("a", 1));
        assert_eq!(history.undo(state("b", 1)).unwrap().text, "a");
        assert_eq!(history.redo(state("a", 1)).unwrap().text, "b");
        assert_eq!(history.undo(state("b", 1)).unwrap().text, "a");
        history.record(state("branch", 2));
        assert!(history.redo(state("new", 3)).is_none());
    }

    #[test]
    #[should_panic(expected = "CtkTextAreaPlugin requires Bevy's UiPlugin")]
    fn plugin_contract_survives_release_builds() {
        let mut app = App::new();
        app.add_plugins(CtkTextAreaPlugin);
        app.finish();
    }

    #[derive(Resource, Default)]
    struct EventLog {
        changes: Vec<String>,
        blurs: Vec<String>,
        submits: Vec<String>,
    }

    #[derive(Resource)]
    struct LateReplacement {
        area: Entity,
        value: Option<String>,
    }

    #[derive(Resource, Default)]
    struct EditableChangeAudit {
        before: Option<Tick>,
        changed: Vec<bool>,
    }

    fn record_change(event: On<CtkTextAreaChanged>, mut log: ResMut<EventLog>) {
        log.changes.push(event.value.clone());
    }

    fn record_blur(event: On<CtkTextAreaBlurred>, mut log: ResMut<EventLog>) {
        log.blurs.push(event.value.clone());
    }

    fn record_submit(event: On<CtkTextAreaSubmitted>, mut log: ResMut<EventLog>) {
        log.submits.push(event.value.clone());
    }

    fn apply_late_replacement(
        mut request: ResMut<LateReplacement>,
        mut editables: Query<&mut EditableText>,
    ) {
        let Some(value) = request.value.take() else {
            return;
        };
        editables
            .get_mut(request.area)
            .unwrap()
            .editor_mut()
            .set_text(&value);
    }

    fn record_editable_change_tick(
        areas: Query<Ref<EditableText>, With<CtkTextArea>>,
        mut audit: ResMut<EditableChangeAudit>,
    ) {
        audit.before = areas.single().ok().map(|editable| editable.last_changed());
    }

    fn audit_editable_change_tick(
        areas: Query<Ref<EditableText>, With<CtkTextArea>>,
        mut audit: ResMut<EditableChangeAudit>,
    ) {
        let after = areas.single().unwrap().last_changed();
        let changed = audit.before != Some(after);
        audit.changed.push(changed);
    }

    fn test_app() -> (App, Entity, Entity) {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_plugins(bevy::a11y::AccessibilityPlugin)
            .add_plugins(AssetPlugin::default())
            .add_plugins(TransformPlugin)
            .add_plugins(CameraPlugin)
            .add_plugins(ImagePlugin::default())
            .add_plugins(TextureAtlasPlugin)
            .add_plugins(MeshPlugin)
            .add_plugins(WindowPlugin {
                primary_window: None,
                ..default()
            })
            .add_plugins(InputPlugin)
            .add_plugins(DefaultPickingPlugins)
            .add_plugins(TextPlugin)
            .add_plugins(InputFocusPlugin)
            .add_plugins(InputDispatchPlugin)
            .add_plugins(UiPlugin)
            .add_plugins(CtkTextAreaPlugin)
            .init_resource::<EventLog>()
            .add_observer(record_change)
            .add_observer(record_blur)
            .add_observer(record_submit);
        let window = app
            .world_mut()
            .spawn((Window::default(), PrimaryWindow))
            .id();
        let camera = spawn_camera(app.world_mut(), window);
        app.finish();
        app.cleanup();
        (app, window, camera)
    }

    fn spawn_camera(world: &mut World, window: Entity) -> Entity {
        let physical_size = UVec2::new(320, 240);
        world
            .spawn((
                Camera2d,
                Camera {
                    computed: ComputedCameraValues {
                        target_info: Some(RenderTargetInfo {
                            physical_size,
                            scale_factor: 1.0,
                        }),
                        ..default()
                    },
                    viewport: Some(Viewport {
                        physical_size,
                        ..default()
                    }),
                    ..default()
                },
                RenderTarget::Window(WindowRef::Entity(window)),
            ))
            .id()
    }

    fn spawn_area(app: &mut App, camera: Entity, value: &str, max_len: usize) -> Entity {
        let initial = value.to_owned();
        let entities = app
            .world_mut()
            .run_system_once(move |mut commands: Commands| {
                spawn_text_area(
                    &mut commands,
                    CtkTextAreaProps::new(initial.clone(), "Body")
                        .max_len(max_len)
                        .visible_lines(4)
                        .history_limit(8)
                        .min_height(0.0),
                )
            })
            .unwrap();
        app.world_mut()
            .entity_mut(entities.root)
            .insert(UiTargetCamera(camera));
        {
            let mut input = app.world_mut().entity_mut(entities.input);
            let mut node = input.get_mut::<Node>().unwrap();
            node.width = px(240);
            node.height = px(48);
            node.min_height = px(0);
        }
        app.world_mut()
            .insert_resource(InputFocus::from_entity(entities.input));
        entities.input
    }

    fn select_range(app: &mut App, area: Entity, anchor: usize, focus: usize) {
        app.world_mut()
            .resource_scope(|world, mut fonts: Mut<FontCx>| {
                world.resource_scope(|world, mut layouts: Mut<LayoutCx>| {
                    world
                        .entity_mut(area)
                        .get_mut::<EditableText>()
                        .unwrap()
                        .editor_mut()
                        .driver(&mut fonts.context, &mut layouts.0)
                        .select_byte_range(anchor, focus);
                });
            });
    }

    #[test]
    fn accessibility_maps_lines_composition_and_grapheme_selection() {
        let value = "a👨‍👩‍👧‍👦z";
        assert_eq!(a11y_character_lengths(value).len(), 3);
        let family_end = value.find('z').expect("fixture contains z");
        assert_eq!(byte_to_a11y_index(value, family_end), 2);
        assert_eq!(canonical_byte(7, Some(&(1..9)), 4), 1);
        assert_eq!(canonical_byte(10, Some(&(1..9)), 4), 2);
        assert_eq!(hard_lines("first\nsecond\n"), ["first\n", "second\n", ""]);
        let run = text_run_accessibility("first\n");
        assert_eq!(run.previous_on_line(), None);
        assert_eq!(run.next_on_line(), None);
    }

    #[test]
    fn synthetic_ime_is_cleared_when_read_only_flips() {
        let (mut app, window, camera) = test_app();
        let area = spawn_area(&mut app, camera, "fixed", 8);
        app.world_mut().write_message(Ime::Preedit {
            window,
            value: "候補".into(),
            cursor: Some((0, 2)),
        });
        app.update();
        assert!(app
            .world()
            .entity(area)
            .get::<EditableText>()
            .unwrap()
            .is_composing());

        app.world_mut()
            .entity_mut(area)
            .get_mut::<CtkTextArea>()
            .unwrap()
            .set_read_only(true);
        app.update();
        let editable = app.world().entity(area).get::<EditableText>().unwrap();
        assert!(!editable.is_composing());
        assert_eq!(editable.value().to_string(), "fixed");
        assert!(app.world().resource::<EventLog>().changes.is_empty());
    }

    #[test]
    fn selected_synthetic_ime_commit_fits_at_max_len() {
        let (mut app, window, camera) = test_app();
        let area = spawn_area(&mut app, camera, "abcd", 4);
        app.update();
        app.world_mut()
            .resource_scope(|world, mut fonts: Mut<FontCx>| {
                world.resource_scope(|world, mut layouts: Mut<LayoutCx>| {
                    world
                        .entity_mut(area)
                        .get_mut::<EditableText>()
                        .unwrap()
                        .editor_mut()
                        .driver(&mut fonts.context, &mut layouts.0)
                        .select_byte_range(1, 3);
                });
            });
        app.world_mut().write_message(Ime::Commit {
            window,
            value: "XY".into(),
        });
        app.update();
        assert_eq!(
            app.world()
                .entity(area)
                .get::<EditableText>()
                .unwrap()
                .value()
                .to_string(),
            "aXYd"
        );
        assert_eq!(app.world().resource::<EventLog>().changes, ["aXYd"]);
    }

    #[test]
    fn selected_preedit_cancellation_restores_text_selection_and_emits_nothing() {
        let (mut app, window, camera) = test_app();
        let area = spawn_area(&mut app, camera, "abcd", 4);
        app.update();
        select_range(&mut app, area, 1, 3);
        app.world_mut().write_message(Ime::Preedit {
            window,
            value: "候".into(),
            cursor: Some((0, 3)),
        });
        app.update();
        assert_eq!(
            app.world()
                .entity(area)
                .get::<EditableText>()
                .unwrap()
                .value()
                .to_string(),
            "ad"
        );
        assert!(app.world().resource::<EventLog>().changes.is_empty());

        app.world_mut().write_message(Ime::Disabled { window });
        app.update();
        let editable = app.world().entity(area).get::<EditableText>().unwrap();
        assert_eq!(editable.value().to_string(), "abcd");
        let selection = editable.editor().raw_selection();
        assert_eq!(
            (selection.anchor().index(), selection.focus().index()),
            (1, 3)
        );
        let entity = app.world().entity(area);
        assert!(entity.get::<CtkTextArea>().unwrap().history.undo.is_empty());
        assert!(app.world().resource::<EventLog>().changes.is_empty());
    }

    #[test]
    fn oversized_selected_ime_commit_restores_parked_snapshot() {
        let (mut app, window, camera) = test_app();
        let area = spawn_area(&mut app, camera, "abcd", 4);
        app.update();
        select_range(&mut app, area, 1, 3);
        app.world_mut().write_message(Ime::Preedit {
            window,
            value: "候".into(),
            cursor: Some((0, 3)),
        });
        app.update();
        app.world_mut().write_message(Ime::Commit {
            window,
            value: "WXYZ".into(),
        });
        app.update();

        let entity = app.world().entity(area);
        let editable = entity.get::<EditableText>().unwrap();
        assert_eq!(editable.value().to_string(), "abcd");
        let selection = editable.editor().raw_selection();
        assert_eq!(
            (selection.anchor().index(), selection.focus().index()),
            (1, 3)
        );
        assert!(entity.get::<CtkTextArea>().unwrap().history.undo.is_empty());
        assert!(app.world().resource::<EventLog>().changes.is_empty());
    }

    fn keyboard(
        window: Entity,
        key_code: KeyCode,
        logical_key: Key,
        repeat: bool,
    ) -> KeyboardInput {
        KeyboardInput {
            key_code,
            logical_key,
            state: ButtonState::Pressed,
            text: None,
            repeat,
            window,
        }
    }

    fn typing(window: Entity, value: &str) -> KeyboardInput {
        KeyboardInput {
            key_code: KeyCode::KeyA,
            logical_key: Key::Character(value.into()),
            state: ButtonState::Pressed,
            text: Some(value.into()),
            repeat: false,
            window,
        }
    }

    #[test]
    fn repeated_submit_is_rejected_and_events_are_exact() {
        let (mut app, window, camera) = test_app();
        let area = spawn_area(&mut app, camera, "body", 20);
        app.world_mut()
            .resource_mut::<ButtonInput<Key>>()
            .press(Key::Control);
        app.world_mut()
            .write_message(keyboard(window, KeyCode::Enter, Key::Enter, false));
        app.world_mut()
            .write_message(keyboard(window, KeyCode::Enter, Key::Enter, true));
        app.update();
        let log = app.world().resource::<EventLog>();
        assert_eq!(log.submits, ["body"]);
        assert!(log.changes.is_empty());
        assert!(log.blurs.is_empty());
        assert_eq!(app.world().resource::<InputFocus>().get(), Some(area));
    }

    #[test]
    fn window_unfocus_blurs_once_per_focus_period() {
        let (mut app, window, camera) = test_app();
        let _area = spawn_area(&mut app, camera, "body", 20);
        app.update();
        app.world_mut().write_message(WindowFocused {
            window,
            focused: false,
        });
        app.world_mut().write_message(WindowFocused {
            window,
            focused: false,
        });
        app.update();
        app.update();
        assert_eq!(app.world().resource::<EventLog>().blurs, ["body"]);

        app.world_mut().write_message(WindowFocused {
            window,
            focused: true,
        });
        app.world_mut().write_message(WindowFocused {
            window,
            focused: false,
        });
        app.update();
        assert_eq!(app.world().resource::<EventLog>().blurs, ["body", "body"]);
    }

    #[test]
    fn blur_and_wheel_ignore_another_window() {
        let (mut app, window, camera) = test_app();
        let other_window = app.world_mut().spawn(Window::default()).id();
        let _other_camera = spawn_camera(app.world_mut(), other_window);
        let area = spawn_area(&mut app, camera, "0\n1\n2\n3\n4\n5\n6\n7\n8\n9", 40);
        app.update();
        app.update();
        app.world_mut().entity_mut(area).insert(Hovered(true));
        let baseline = app.world().entity(area).get::<TextScroll>().unwrap().0.y;
        app.world_mut().write_message(WindowFocused {
            window: other_window,
            focused: false,
        });
        app.world_mut().write_message(MouseWheel {
            unit: MouseScrollUnit::Pixel,
            x: 0.0,
            y: -40.0,
            window: other_window,
            phase: bevy::input::touch::TouchPhase::Moved,
        });
        app.update();
        assert!(app.world().resource::<EventLog>().blurs.is_empty());
        assert_eq!(
            app.world().entity(area).get::<TextScroll>().unwrap().0.y,
            baseline
        );

        app.world_mut().write_message(WindowFocused {
            window,
            focused: false,
        });
        app.world_mut().write_message(MouseWheel {
            unit: MouseScrollUnit::Pixel,
            x: 0.0,
            y: -40.0,
            window,
            phase: bevy::input::touch::TouchPhase::Moved,
        });
        app.update();
        assert_eq!(
            app.world().resource::<EventLog>().blurs,
            ["0\n1\n2\n3\n4\n5\n6\n7\n8\n9"]
        );
        assert!(app.world().entity(area).get::<TextScroll>().unwrap().0.y > 0.0);
    }

    #[test]
    fn read_only_drops_a_late_paste_before_it_can_mutate() {
        let (mut app, _window, camera) = test_app();
        let area = spawn_area(&mut app, camera, "body", 20);
        let pending = Arc::new(Mutex::new(Some(Ok(" late".to_string()))));
        {
            let mut entity = app.world_mut().entity_mut(area);
            entity.get_mut::<EditableText>().unwrap().pending_paste =
                Some(ClipboardRead::Pending(pending));
            let snapshot = snapshot(entity.get::<EditableText>().unwrap());
            let mut area = entity.get_mut::<CtkTextArea>().unwrap();
            area.pending_paste_before = Some(snapshot);
            area.set_read_only(true);
        }
        app.update();
        let entity = app.world().entity(area);
        let editable = entity.get::<EditableText>().unwrap();
        assert_eq!(editable.value().to_string(), "body");
        assert!(editable.pending_paste.is_none());
        assert!(entity
            .get::<CtkTextArea>()
            .unwrap()
            .pending_paste_before
            .is_none());
        assert!(app.world().resource::<EventLog>().changes.is_empty());
    }

    #[test]
    fn programmatic_set_text_clamps_then_records_once_and_clears_redo() {
        let (mut app, _window, camera) = test_app();
        let area = spawn_area(&mut app, camera, "old", 4);
        app.update();
        app.world_mut()
            .entity_mut(area)
            .get_mut::<EditableText>()
            .unwrap()
            .queue_edit(TextEdit::Insert("x".into()));
        app.update();
        app.world_mut().trigger(CtkTextAreaUndo { area });
        app.update();
        assert!(!app
            .world()
            .entity(area)
            .get::<CtkTextArea>()
            .unwrap()
            .history
            .redo
            .is_empty());
        app.world_mut().resource_mut::<EventLog>().changes.clear();

        app.world_mut()
            .entity_mut(area)
            .get_mut::<EditableText>()
            .unwrap()
            .editor_mut()
            .set_text("abcdef");
        app.update();
        let entity = app.world().entity(area);
        assert_eq!(
            entity.get::<EditableText>().unwrap().value().to_string(),
            "abcd"
        );
        let area_state = entity.get::<CtkTextArea>().unwrap();
        assert!(area_state.history.redo.is_empty());
        assert!(area_state
            .history
            .undo
            .iter()
            .all(|snapshot| snapshot.text.chars().count() <= 4));
        assert_eq!(app.world().resource::<EventLog>().changes, ["abcd"]);

        app.world_mut().trigger(CtkTextAreaUndo { area });
        app.update();
        assert_eq!(
            app.world()
                .entity(area)
                .get::<EditableText>()
                .unwrap()
                .value()
                .to_string(),
            "old"
        );
    }

    #[test]
    fn writer_immediately_before_sync_is_clamped_and_recorded() {
        let (mut app, _window, camera) = test_app();
        let area = spawn_area(&mut app, camera, "old", 4);
        app.update();
        app.insert_resource(LateReplacement {
            area,
            value: Some("abcdef".into()),
        })
        .add_systems(
            PostUpdate,
            apply_late_replacement
                .after(apply_text_area_wheel_scroll)
                .before(sync_text_areas),
        );

        app.update();
        assert_eq!(
            app.world()
                .entity(area)
                .get::<EditableText>()
                .unwrap()
                .value(),
            "abcd"
        );
        assert_eq!(app.world().resource::<EventLog>().changes, ["abcd"]);

        app.world_mut().trigger(CtkTextAreaUndo { area });
        app.update();
        assert_eq!(
            app.world()
                .entity(area)
                .get::<EditableText>()
                .unwrap()
                .value(),
            "old"
        );
    }

    #[test]
    fn set_text_during_preedit_resolves_the_park() {
        let (mut app, window, camera) = test_app();
        let area = spawn_area(&mut app, camera, "abcd", 8);
        app.update();
        select_range(&mut app, area, 1, 3);
        app.world_mut().write_message(Ime::Preedit {
            window,
            value: "候".into(),
            cursor: Some((0, 3)),
        });
        app.update();
        assert!(app
            .world()
            .entity(area)
            .get::<CtkTextArea>()
            .unwrap()
            .ime_transaction_before
            .is_some());

        app.insert_resource(LateReplacement {
            area,
            value: Some("replacement".into()),
        })
        .add_systems(
            PostUpdate,
            apply_late_replacement
                .after(apply_text_area_wheel_scroll)
                .before(sync_text_areas),
        );
        app.update();
        let entity = app.world().entity(area);
        assert_eq!(entity.get::<EditableText>().unwrap().value(), "replacem");
        assert!(entity
            .get::<CtkTextArea>()
            .unwrap()
            .ime_transaction_before
            .is_none());
        assert_eq!(app.world().resource::<EventLog>().changes, ["replacem"]);

        app.world_mut().trigger(CtkTextAreaUndo { area });
        app.update();
        assert_eq!(
            app.world()
                .entity(area)
                .get::<EditableText>()
                .unwrap()
                .value(),
            "abcd"
        );
    }

    #[test]
    fn filtered_selected_preedit_survives_to_commit() {
        let (mut app, window, camera) = test_app();
        let area = spawn_area(&mut app, camera, "abcd", 4);
        app.world_mut()
            .entity_mut(area)
            .insert(EditableTextFilter::new(|character| {
                character.is_ascii_alphabetic()
            }));
        app.update();
        select_range(&mut app, area, 1, 3);
        app.world_mut().write_message(Ime::Preedit {
            window,
            value: "X".into(),
            cursor: Some((0, 1)),
        });
        app.update();
        let entity = app.world().entity(area);
        assert!(entity.get::<EditableText>().unwrap().is_composing());
        let area_state = entity.get::<CtkTextArea>().unwrap();
        assert_eq!(
            area_state.ime_transaction_before.as_ref().unwrap().text,
            "abcd"
        );
        assert_eq!(area_state.policy_snapshot.text, "abcd");
        assert!(app.world().resource::<EventLog>().changes.is_empty());

        app.world_mut().write_message(Ime::Commit {
            window,
            value: "YZ".into(),
        });
        app.update();
        assert_eq!(
            app.world()
                .entity(area)
                .get::<EditableText>()
                .unwrap()
                .value(),
            "aYZd"
        );
        assert_eq!(app.world().resource::<EventLog>().changes, ["aYZd"]);

        app.world_mut().trigger(CtkTextAreaUndo { area });
        app.update();
        assert_eq!(
            app.world()
                .entity(area)
                .get::<EditableText>()
                .unwrap()
                .value(),
            "abcd"
        );
    }

    #[test]
    fn clamp_to_current_value_preserves_redo() {
        let (mut app, _window, camera) = test_app();
        let area = spawn_area(&mut app, camera, "old", 3);
        app.update();
        app.world_mut()
            .entity_mut(area)
            .get_mut::<EditableText>()
            .unwrap()
            .editor_mut()
            .set_text("new");
        app.update();
        app.world_mut().trigger(CtkTextAreaUndo { area });
        app.update();
        assert_eq!(
            app.world()
                .entity(area)
                .get::<EditableText>()
                .unwrap()
                .value(),
            "old"
        );

        app.world_mut()
            .entity_mut(area)
            .get_mut::<EditableText>()
            .unwrap()
            .editor_mut()
            .set_text("old overflow");
        app.update();
        assert_eq!(
            app.world()
                .entity(area)
                .get::<EditableText>()
                .unwrap()
                .value(),
            "old"
        );
        app.world_mut().trigger(CtkTextAreaRedo { area });
        app.update();
        assert_eq!(
            app.world()
                .entity(area)
                .get::<EditableText>()
                .unwrap()
                .value(),
            "new"
        );
    }

    #[test]
    fn replacement_selection_snaps_to_utf8_boundaries() {
        let (mut app, _window, camera) = test_app();
        let area = spawn_area(&mut app, camera, "ab", 4);
        app.update();
        select_range(&mut app, area, 1, 1);
        app.world_mut()
            .entity_mut(area)
            .get_mut::<EditableText>()
            .unwrap()
            .editor_mut()
            .set_text("é");
        app.update();

        let entity = app.world().entity(area);
        let editable = entity.get::<EditableText>().unwrap();
        let selection = editable.editor().raw_selection();
        assert_eq!(
            (selection.anchor().index(), selection.focus().index()),
            (0, 0)
        );
        let area = entity.get::<CtkTextArea>().unwrap();
        assert!(area
            .history
            .undo
            .iter()
            .chain(std::iter::once(&area.last_snapshot))
            .all(|snapshot| snapshot.text.is_char_boundary(snapshot.anchor)
                && snapshot.text.is_char_boundary(snapshot.focus)));
    }

    #[test]
    fn idle_filtered_area_does_not_mark_editable_changed() {
        let (mut app, _window, camera) = test_app();
        let area = spawn_area(&mut app, camera, "", 8);
        app.world_mut()
            .entity_mut(area)
            .insert(EditableTextFilter::new(|character| {
                character.is_ascii_digit()
            }));
        app.init_resource::<EditableChangeAudit>().add_systems(
            PostUpdate,
            (
                record_editable_change_tick.before(process_text_area_edits),
                audit_editable_change_tick
                    .after(process_text_area_edits)
                    .before(EditableTextSystems),
            ),
        );
        app.update();
        app.update();
        app.world_mut()
            .resource_mut::<EditableChangeAudit>()
            .changed
            .clear();

        app.update();
        app.update();
        assert_eq!(
            app.world().resource::<EditableChangeAudit>().changed,
            [false, false]
        );
    }

    #[test]
    fn editable_filter_applies_to_typing_paste_and_ime() {
        let (mut app, window, camera) = test_app();
        let area = spawn_area(&mut app, camera, "", 8);
        app.world_mut()
            .entity_mut(area)
            .insert(EditableTextFilter::new(|character| {
                character.is_ascii_digit()
            }));
        app.update();

        for value in ["x", "1"] {
            app.world_mut()
                .entity_mut(area)
                .get_mut::<EditableText>()
                .unwrap()
                .queue_edit(TextEdit::Insert(value.into()));
            app.update();
        }
        assert_eq!(
            app.world()
                .entity(area)
                .get::<EditableText>()
                .unwrap()
                .value()
                .to_string(),
            "1"
        );

        app.world_mut()
            .resource_mut::<Clipboard>()
            .set_text("2x")
            .unwrap();
        app.world_mut()
            .entity_mut(area)
            .get_mut::<EditableText>()
            .unwrap()
            .queue_edit(TextEdit::Paste);
        app.update();
        app.world_mut()
            .resource_mut::<Clipboard>()
            .set_text("2")
            .unwrap();
        app.world_mut()
            .entity_mut(area)
            .get_mut::<EditableText>()
            .unwrap()
            .queue_edit(TextEdit::Paste);
        app.update();

        app.world_mut().write_message(Ime::Commit {
            window,
            value: "x".into(),
        });
        app.update();
        app.world_mut().write_message(Ime::Commit {
            window,
            value: "3".into(),
        });
        app.update();
        assert_eq!(
            app.world()
                .entity(area)
                .get::<EditableText>()
                .unwrap()
                .value()
                .to_string(),
            "123"
        );
        assert_eq!(
            app.world().resource::<EventLog>().changes,
            ["1", "12", "123"]
        );
    }

    #[test]
    fn page_motion_uses_computed_viewport_and_real_line_metrics() {
        let (mut app, window, camera) = test_app();
        let area = spawn_area(&mut app, camera, "0\n1\n2\n3\n4\n5\n6\n7", 40);
        app.update();
        app.world_mut()
            .resource_scope(|world, mut fonts: Mut<FontCx>| {
                world.resource_scope(|world, mut layouts: Mut<LayoutCx>| {
                    world
                        .entity_mut(area)
                        .get_mut::<EditableText>()
                        .unwrap()
                        .editor_mut()
                        .driver(&mut fonts.context, &mut layouts.0)
                        .move_to_byte(0);
                });
            });
        let line_height = app
            .world()
            .entity(area)
            .get::<EditableText>()
            .unwrap()
            .editor()
            .try_layout()
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .metrics()
            .line_height;
        let inset = {
            let computed = app.world().entity(area).get::<ComputedNode>().unwrap();
            computed.size().y - computed.content_box().height()
        };
        {
            let mut entity = app.world_mut().entity_mut(area);
            entity.get_mut::<Node>().unwrap().height = px(line_height * 3.1 + inset);
        }
        app.update();
        {
            let entity = app.world().entity(area);
            assert_eq!(
                page_line_count(
                    entity.get::<EditableText>().unwrap(),
                    entity.get::<ComputedNode>().unwrap().content_box().height(),
                    PageDirection::Down,
                ),
                3
            );
        }
        app.world_mut()
            .write_message(keyboard(window, KeyCode::PageDown, Key::PageDown, false));
        app.update();
        let focus = app
            .world()
            .entity(area)
            .get::<EditableText>()
            .unwrap()
            .editor()
            .raw_selection()
            .focus()
            .index();
        assert_eq!(focus, 4, "three visible lines synthesize two Down edits");
    }

    #[test]
    fn wheel_scroll_uses_line_metrics_and_clamps_on_each_input() {
        let (mut app, window, camera) = test_app();
        let area = spawn_area(&mut app, camera, "0\n1\n2\n3\n4\n5\n6\n7", 40);
        app.update();
        app.update();
        app.world_mut()
            .resource_scope(|world, mut fonts: Mut<FontCx>| {
                world.resource_scope(|world, mut layouts: Mut<LayoutCx>| {
                    world
                        .entity_mut(area)
                        .get_mut::<EditableText>()
                        .unwrap()
                        .editor_mut()
                        .driver(&mut fonts.context, &mut layouts.0)
                        .move_to_byte(0);
                });
            });
        app.update();
        app.world_mut().entity_mut(area).insert(Hovered(true));
        assert_eq!(
            app.world()
                .entity(area)
                .get::<EditableText>()
                .unwrap()
                .editor()
                .raw_selection()
                .focus()
                .index(),
            0
        );
        assert_eq!(
            app.world().entity(area).get::<TextScroll>().unwrap().0.y,
            0.0
        );
        app.world_mut().write_message(MouseWheel {
            unit: MouseScrollUnit::Pixel,
            x: 0.0,
            y: -40.0,
            window,
            phase: bevy::input::touch::TouchPhase::Moved,
        });
        app.update();
        let entity = app.world().entity(area);
        let layout_height = entity
            .get::<EditableText>()
            .unwrap()
            .editor()
            .try_layout()
            .unwrap()
            .height();
        let viewport = entity.get::<ComputedNode>().unwrap().content_box().height();
        assert_eq!(
            entity.get::<TextScroll>().unwrap().0.y,
            40.0_f32.min((layout_height - viewport).max(0.0))
        );
        let settled = entity.get::<TextScroll>().unwrap().0.y;
        app.update();
        app.update();
        assert_eq!(
            app.world().entity(area).get::<TextScroll>().unwrap().0.y,
            settled,
            "an unmoved caret must not snap wheel scrolling back"
        );

        app.world_mut().write_message(MouseWheel {
            unit: MouseScrollUnit::Pixel,
            x: 0.0,
            y: -10_000.0,
            window,
            phase: bevy::input::touch::TouchPhase::Moved,
        });
        app.update();
        let entity = app.world().entity(area);
        assert_eq!(
            entity.get::<TextScroll>().unwrap().0.y,
            (layout_height - viewport).max(0.0),
            "each wheel input clamps against the current layout"
        );
    }

    #[test]
    fn history_coalesces_typing_across_frames_and_restores_selection() {
        let (mut app, window, camera) = test_app();
        let area = spawn_area(&mut app, camera, "middle", 40);
        app.update();
        for _ in 0..3 {
            app.world_mut().write_message(keyboard(
                window,
                KeyCode::ArrowLeft,
                Key::ArrowLeft,
                false,
            ));
        }
        app.update();
        for value in ["A", "B"] {
            app.world_mut().write_message(typing(window, value));
        }
        app.update();
        app.world_mut().write_message(typing(window, "C"));
        app.update();
        app.world_mut().trigger(CtkTextAreaUndo { area });
        app.update();
        let editable = app.world().entity(area).get::<EditableText>().unwrap();
        assert_eq!(editable.value().to_string(), "middle");
        let selection = editable.editor().raw_selection();
        assert_eq!(selection.anchor().index(), 3);
        assert_eq!(selection.focus().index(), 3);
    }

    #[test]
    fn typing_word_boundary_closes_the_coalesced_transaction() {
        let (mut app, window, camera) = test_app();
        let area = spawn_area(&mut app, camera, "", 40);
        app.update();
        for value in ["a", "b", " ", "c"] {
            app.world_mut().write_message(typing(window, value));
        }
        app.update();
        app.world_mut().trigger(CtkTextAreaUndo { area });
        app.update();
        assert_eq!(
            app.world()
                .entity(area)
                .get::<EditableText>()
                .unwrap()
                .value()
                .to_string(),
            "ab "
        );
        app.world_mut().trigger(CtkTextAreaUndo { area });
        app.update();
        assert_eq!(
            app.world()
                .entity(area)
                .get::<EditableText>()
                .unwrap()
                .value()
                .to_string(),
            ""
        );
    }
}
