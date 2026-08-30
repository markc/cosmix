//! Virtualised, application-bound rows for very large native CTK lists.
//!
//! Applications own the data and every row's content/layout through
//! [`VirtualListModel`]. CTK owns the scroll area, recycled row shells,
//! selection, keyboard/pointer behaviour and accessibility metadata. Every
//! bind receives a newly spawned content child: the previous content subtree
//! is despawned first, so application components cannot leak across recycled
//! identities. Applications must populate only that content entity and its
//! descendants, never the CTK-owned row shell.
//!
//! Bevy's ordinary [`ScrollPosition`] remains authoritative. The scroll area
//! contains one full-height spacer and only the realised rows, positioned
//! absolutely, so wheel/trackpad input and the first-party scrollbar retain
//! their native pixel geometry without allocating an entity per item.
//!
//! Version 1 deliberately has one fixed row height per list. Offset and window
//! calculations are isolated behind the private `RowMetrics` trait; a future
//! variable-height implementation can replace that mapping without changing
//! the public model or selection API.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::ops::Range;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use accesskit::Role;
use bevy::a11y::AccessibilityNode;
use bevy::app::{App, Plugin};
use bevy::ecs::event::EntityEvent;
use bevy::ecs::observer::On;
use bevy::feathers::theme::{ThemeBackgroundColor, UiTheme};
use bevy::input::keyboard::{KeyCode, KeyboardInput};
use bevy::input::ButtonState;
use bevy::input_focus::tab_navigation::TabIndex;
use bevy::input_focus::{FocusCause, FocusedInput, InputFocus};
use bevy::log::warn_once;
use bevy::picking::events::{Click, Pointer};
use bevy::picking::hover::Hovered;
use bevy::picking::Pickable;
use bevy::prelude::*;
use bevy::ui::{Overflow, ScrollPosition};
use bevy::ui_widgets::{ControlOrientation, ScrollArea, Scrollbar, ScrollbarThumb};

use crate::latency::LatencyHistogram;
use crate::theme::tokens;

/// Stable application identity for one model row.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RowId(pub u64);

/// Application-owned data boundary for a virtual list.
///
/// `bind` receives a fresh `content` child after CTK has inserted the current
/// [`VirtualListRow`] on its parent row shell. Implementations can follow the
/// child's [`ChildOf`] relationship to read that metadata, but must add
/// application components only to `content` or its descendants. The model may
/// point at application-owned shared storage; CTK never copies or retains its
/// rows.
///
/// `row_id` values are a model invariant: every live row must have a unique,
/// stable ID.
pub trait VirtualListModel: Send + Sync + 'static {
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    fn row_id(&self, index: usize) -> RowId;
    fn bind(&self, world: &mut World, content: Entity, index: usize);
}

#[derive(Component)]
struct ErasedModel(Arc<dyn VirtualListModel>);

/// How far beyond each viewport edge CTK keeps row shells realised.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Overscan {
    /// A multiple of the current viewport row count.
    Viewports(f32),
    /// An exact number of rows at each edge.
    Rows(usize),
}

impl Default for Overscan {
    fn default() -> Self {
        Self::Viewports(1.0)
    }
}

impl Overscan {
    fn rows(self, viewport_rows: usize) -> usize {
        match self {
            Self::Viewports(viewports) => {
                ((viewport_rows as f32) * viewports.max(0.0)).ceil() as usize
            }
            Self::Rows(rows) => rows,
        }
    }
}

/// Selection gestures accepted by one list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SelectionMode {
    #[default]
    Single,
    /// A normal click starts a range and Shift extends it.
    Contiguous,
    /// Ctrl toggles individual rows; Shift adds a contiguous range.
    Disjoint,
}

/// Placement used by [`scroll_to`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Align {
    #[default]
    Start,
    Center,
    End,
}

/// Model mutation detail used to minimise row rebinding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChangeHint {
    Reset,
    Inserted(Range<usize>),
    Removed(Range<usize>),
    Updated(Range<usize>),
}

/// Construction properties for a virtual list.
#[derive(Clone, Debug)]
pub struct VirtualListProps {
    pub row_height: f32,
    pub viewport_height: f32,
    pub overscan: Overscan,
    pub selection_mode: SelectionMode,
    pub accessible_label: String,
}

impl VirtualListProps {
    pub fn new(row_height: f32, viewport_height: f32, accessible_label: impl Into<String>) -> Self {
        assert!(row_height.is_finite() && row_height > 0.0);
        assert!(viewport_height.is_finite() && viewport_height > 0.0);
        Self {
            row_height,
            viewport_height,
            overscan: Overscan::default(),
            selection_mode: SelectionMode::default(),
            accessible_label: accessible_label.into(),
        }
    }

    pub fn overscan(mut self, overscan: Overscan) -> Self {
        self.overscan = overscan;
        self
    }

    pub fn selection_mode(mut self, selection_mode: SelectionMode) -> Self {
        self.selection_mode = selection_mode;
        self
    }
}

/// Root, scroll viewport, virtual content spacer and scrollbar entities.
#[derive(Clone, Copy, Debug)]
pub struct VirtualListEntities {
    pub root: Entity,
    pub viewport: Entity,
    pub content: Entity,
    pub scrollbar: Entity,
}

#[derive(Default)]
struct ReconcileScratch {
    hints: Vec<ChangeHint>,
    existing: HashMap<RowId, Entity>,
    desired_ids: HashSet<RowId>,
    tracked_ids: HashSet<RowId>,
    // Only selected/cursor/anchor identities are stored here. Duplicate IDs
    // outside the realised window collapse to one position, which is
    // sufficient because stable-ID state cannot distinguish them; the
    // realised-window guard reports either duplicate when it becomes visible.
    model_positions: HashMap<RowId, usize>,
    selection_before: Vec<RowId>,
    assignments: Vec<(usize, RowId, Option<Entity>)>,
    available: Vec<Entity>,
    realised: Vec<Entity>,
    updated: Vec<Range<usize>>,
}

struct VirtualListState {
    viewport: Entity,
    content: Entity,
    row_height: f32,
    fallback_viewport_height: f32,
    overscan: Overscan,
    selection_mode: SelectionMode,
    rows: Vec<Entity>,
    realised: Range<usize>,
    selected: BTreeSet<RowId>,
    selection_anchor: Option<RowId>,
    selection_anchor_index: Option<usize>,
    cursor: Option<RowId>,
    cursor_index: Option<usize>,
    top_anchor: Option<(RowId, f32, usize)>,
    pending_hints: Vec<ChangeHint>,
    pending_scroll: Option<(usize, Align)>,
    pending_reveal: Option<usize>,
    force_rebind_ids: BTreeSet<RowId>,
    last_offset: f32,
    last_viewport_height: f32,
    model_len: usize,
    latency: LatencyHistogram,
    latency_warmup_frames: u8,
    pending_selection_events: VecDeque<Vec<RowId>>,
    #[cfg(debug_assertions)]
    debug_assert_duplicates: bool,
    scratch: ReconcileScratch,
}

/// Behaviour and state attached to the list root.
#[derive(Component)]
pub struct VirtualList {
    state: Arc<Mutex<VirtualListState>>,
}

impl VirtualList {
    pub fn selection_mode(&self) -> SelectionMode {
        self.state
            .lock()
            .expect("virtual-list state lock poisoned")
            .selection_mode
    }

    pub fn set_selection_mode(&mut self, mode: SelectionMode) {
        let mut state = self.state.lock().expect("virtual-list state lock poisoned");
        state.selection_mode = mode;
        if mode == SelectionMode::Single && state.selected.len() > 1 {
            let keep = state
                .cursor
                .filter(|id| state.selected.contains(id))
                .or_else(|| state.selected.first().copied());
            let before = state.selected.clone();
            state.force_rebind_ids.extend(before);
            state.selected.clear();
            if let Some(id) = keep {
                state.selected.insert(id);
                state.selection_anchor = Some(id);
                state.selection_anchor_index = state
                    .cursor
                    .filter(|cursor| *cursor == id)
                    .and(state.cursor_index);
            }
            let selected = state.selected.iter().copied().collect::<Vec<_>>();
            state.force_rebind_ids.extend(selected);
            queue_selection_event(&mut state);
        }
    }

    pub fn selected_ids(&self) -> impl Iterator<Item = RowId> + '_ {
        self.state
            .lock()
            .expect("virtual-list state lock poisoned")
            .selected
            .iter()
            .copied()
            .collect::<Vec<_>>()
            .into_iter()
    }

    pub fn is_selected(&self, id: RowId) -> bool {
        self.state
            .lock()
            .expect("virtual-list state lock poisoned")
            .selected
            .contains(&id)
    }

    pub fn realised_range(&self) -> Range<usize> {
        self.state
            .lock()
            .expect("virtual-list state lock poisoned")
            .realised
            .clone()
    }

    pub fn model_len(&self) -> usize {
        self.state
            .lock()
            .expect("virtual-list state lock poisoned")
            .model_len
    }

    /// Steady-state scroll/rebind latency. Initial construction, the first
    /// three layout-settling passes and idle reconciliations are excluded.
    pub fn latency(&self) -> LatencyHistogram {
        self.state
            .lock()
            .expect("virtual-list state lock poisoned")
            .latency
            .clone()
    }
}

#[derive(Resource, Default)]
struct VirtualListFrameTiming {
    started: Option<Instant>,
    lists: Vec<Entity>,
    measured: Vec<Entity>,
    paint_rows: Vec<Entity>,
    bind_jobs: Vec<BindJob>,
}

struct BindJob {
    model: Arc<dyn VirtualListModel>,
    content: Entity,
    index: usize,
}

struct RealiseRequest {
    list: Entity,
    desired: Range<usize>,
    model_len_changed: bool,
}

/// Metadata on each recycled application row shell.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct VirtualListRow {
    pub list: Entity,
    pub index: usize,
    pub row_id: RowId,
    pub selected: bool,
}

/// Signal that the application model has already changed.
#[derive(EntityEvent, Clone, Debug, PartialEq, Eq)]
pub struct VirtualListModelChanged {
    #[event_target]
    pub list: Entity,
    pub hint: ChangeHint,
}

/// Emitted on the list after its stable-ID selection changes.
#[derive(EntityEvent, Clone, Debug, PartialEq, Eq)]
pub struct VirtualListSelectionChanged {
    #[event_target]
    pub list: Entity,
    pub selected: Vec<RowId>,
}

/// Emitted on the list for a double-click or focused Enter activation.
#[derive(EntityEvent, Clone, Copy, Debug, PartialEq, Eq)]
pub struct VirtualListRowActivated {
    #[event_target]
    pub list: Entity,
    pub row_id: RowId,
    pub index: usize,
}

#[derive(EntityEvent, Clone, Copy, Debug, PartialEq, Eq)]
struct ScrollToRequest {
    #[event_target]
    list: Entity,
    index: usize,
    align: Align,
}

#[derive(Component)]
struct VirtualListViewport {
    list: Entity,
}

/// Install virtual-list input, mutation and reconciliation behaviour.
pub struct VirtualListPlugin;

impl Plugin for VirtualListPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<InputFocus>()
            .init_resource::<UiTheme>()
            .init_resource::<VirtualListFrameTiming>()
            .add_observer(on_model_changed)
            .add_observer(on_scroll_to)
            .add_observer(on_row_clicked)
            .add_observer(on_list_key)
            .add_systems(
                Update,
                (reconcile_virtual_lists, paint_virtual_rows).chain(),
            );
    }
}

/// Spawn a virtual list around an application model.
pub fn spawn_virtual_list(
    commands: &mut Commands,
    props: VirtualListProps,
    model: impl VirtualListModel,
) -> VirtualListEntities {
    let model_len = model.len();
    let content = commands
        .spawn(Node {
            position_type: PositionType::Relative,
            width: percent(100),
            height: px((model_len as f32) * props.row_height),
            flex_shrink: 0.0,
            ..default()
        })
        .id();

    let mut list_accessible = accesskit::Node::new(Role::List);
    list_accessible.set_label(props.accessible_label);
    list_accessible.set_size_of_set(model_len);
    if props.selection_mode != SelectionMode::Single {
        list_accessible.set_multiselectable();
    }

    let viewport = commands
        .spawn((
            Node {
                width: percent(100),
                height: percent(100),
                overflow: Overflow::scroll_y(),
                ..default()
            },
            ScrollArea,
            ScrollPosition::default(),
            TabIndex(0),
            AccessibilityNode::from(list_accessible),
        ))
        .add_child(content)
        .id();

    let scrollbar = commands
        .spawn((
            Node {
                width: px(8),
                height: percent(100),
                ..default()
            },
            Scrollbar::new(viewport, ControlOrientation::Vertical, 12.0),
        ))
        .with_child((
            Hovered::default(),
            Pickable::default(),
            ThemeBackgroundColor(tokens::CONTROL),
            ScrollbarThumb {
                border_radius: BorderRadius::all(px(4)),
                border: UiRect::ZERO,
            },
        ))
        .id();

    let root = commands
        .spawn((
            Node {
                display: Display::Grid,
                width: percent(100),
                height: px(props.viewport_height),
                grid_template_columns: vec![
                    RepeatedGridTrack::flex(1, 1.0),
                    RepeatedGridTrack::px(1, 8.0),
                ],
                grid_template_rows: vec![RepeatedGridTrack::flex(1, 1.0)],
                column_gap: px(2),
                ..default()
            },
            ErasedModel(Arc::new(model)),
        ))
        .add_children(&[viewport, scrollbar])
        .id();

    commands
        .entity(viewport)
        .insert(VirtualListViewport { list: root });
    commands.entity(root).insert(VirtualList {
        state: Arc::new(Mutex::new(VirtualListState {
            viewport,
            content,
            row_height: props.row_height,
            fallback_viewport_height: props.viewport_height,
            overscan: props.overscan,
            selection_mode: props.selection_mode,
            rows: Vec::new(),
            realised: 0..0,
            selected: BTreeSet::new(),
            selection_anchor: None,
            selection_anchor_index: None,
            cursor: None,
            cursor_index: None,
            top_anchor: None,
            pending_hints: vec![ChangeHint::Reset],
            pending_scroll: None,
            pending_reveal: None,
            force_rebind_ids: BTreeSet::new(),
            last_offset: f32::NAN,
            last_viewport_height: f32::NAN,
            model_len,
            latency: LatencyHistogram::default(),
            latency_warmup_frames: 3,
            pending_selection_events: VecDeque::new(),
            #[cfg(debug_assertions)]
            debug_assert_duplicates: true,
            scratch: ReconcileScratch::default(),
        })),
    });

    VirtualListEntities {
        root,
        viewport,
        content,
        scrollbar,
    }
}

/// Notify a list after its model has applied `hint`.
pub fn changed(commands: &mut Commands, list: Entity, hint: ChangeHint) {
    commands.trigger(VirtualListModelChanged { list, hint });
}

/// Scroll a model index into the requested viewport alignment.
pub fn scroll_to(commands: &mut Commands, list: Entity, index: usize, align: Align) {
    commands.trigger(ScrollToRequest { list, index, align });
}

fn on_model_changed(event: On<VirtualListModelChanged>, mut lists: Query<&mut VirtualList>) {
    let Ok(list) = lists.get_mut(event.list) else {
        return;
    };
    let mut state = list.state.lock().expect("virtual-list state lock poisoned");
    if event.hint == ChangeHint::Reset {
        state.pending_hints.clear();
    }
    state.pending_hints.push(event.hint.clone());
}

fn on_scroll_to(event: On<ScrollToRequest>, mut lists: Query<&mut VirtualList>) {
    if let Ok(list) = lists.get_mut(event.list) {
        list.state
            .lock()
            .expect("virtual-list state lock poisoned")
            .pending_scroll = Some((event.index, event.align));
    }
}

fn on_row_clicked(
    mut click: On<Pointer<Click>>,
    rows: Query<&VirtualListRow>,
    keys: Res<ButtonInput<KeyCode>>,
    mut commands: Commands,
) {
    let Ok(row) = rows.get(click.entity) else {
        return;
    };
    click.propagate(false);
    let row = *row;
    let shift = keys.any_pressed([KeyCode::ShiftLeft, KeyCode::ShiftRight]);
    let control = keys.any_pressed([KeyCode::ControlLeft, KeyCode::ControlRight]);
    let activate = click.count >= 2;
    commands.queue(move |world: &mut World| {
        if let Some(viewport) = world.get::<VirtualList>(row.list).map(|list| {
            list.state
                .lock()
                .expect("virtual-list state lock poisoned")
                .viewport
        }) {
            world
                .resource_mut::<InputFocus>()
                .set(viewport, FocusCause::Pressed);
        }
        select_index(world, row.list, row.index, shift, control);
        if activate {
            world.trigger(VirtualListRowActivated {
                list: row.list,
                row_id: row.row_id,
                index: row.index,
            });
        }
    });
}

fn on_list_key(
    mut input: On<FocusedInput<KeyboardInput>>,
    viewports: Query<&VirtualListViewport>,
    keys: Res<ButtonInput<KeyCode>>,
    mut commands: Commands,
) {
    if input.input.state != ButtonState::Pressed {
        return;
    }
    let Ok(viewport) = viewports.get(input.focused_entity) else {
        return;
    };
    let key = input.input.key_code;
    let handled = matches!(
        key,
        KeyCode::ArrowUp
            | KeyCode::ArrowDown
            | KeyCode::PageUp
            | KeyCode::PageDown
            | KeyCode::Home
            | KeyCode::End
            | KeyCode::Enter
            | KeyCode::NumpadEnter
    );
    if !handled {
        return;
    }
    input.propagate(false);
    let list = viewport.list;
    let shift = keys.any_pressed([KeyCode::ShiftLeft, KeyCode::ShiftRight]);
    let control = keys.any_pressed([KeyCode::ControlLeft, KeyCode::ControlRight]);
    commands.queue(move |world: &mut World| handle_key(world, list, key, shift, control));
}

trait RowMetrics {
    fn extent(&self, len: usize) -> f32;
    fn index_at_offset(&self, offset: f32, len: usize) -> usize;
    fn offset_of(&self, index: usize) -> f32;
    fn window(
        &self,
        offset: f32,
        viewport_height: f32,
        len: usize,
        overscan: Overscan,
    ) -> Range<usize>;
}

#[derive(Clone, Copy)]
struct FixedRows(f32);

impl RowMetrics for FixedRows {
    fn extent(&self, len: usize) -> f32 {
        (len as f32) * self.0
    }

    fn index_at_offset(&self, offset: f32, len: usize) -> usize {
        if len == 0 {
            0
        } else {
            ((offset.max(0.0) / self.0).floor() as usize).min(len - 1)
        }
    }

    fn offset_of(&self, index: usize) -> f32 {
        (index as f32) * self.0
    }

    fn window(
        &self,
        offset: f32,
        viewport_height: f32,
        len: usize,
        overscan: Overscan,
    ) -> Range<usize> {
        if len == 0 {
            return 0..0;
        }
        let visible = ((viewport_height.max(self.0) / self.0).ceil() as usize).max(1);
        let extra = overscan.rows(visible);
        let first = self.index_at_offset(offset, len);
        let end_visible =
            (((offset.max(0.0) + viewport_height.max(self.0)) / self.0).ceil() as usize).min(len);
        first.saturating_sub(extra)..end_visible.saturating_add(extra).min(len)
    }
}

fn reconcile_virtual_lists(world: &mut World) {
    let mut timing = world
        .remove_resource::<VirtualListFrameTiming>()
        .unwrap_or_default();
    timing.started = Some(Instant::now());
    timing.lists.clear();
    timing.measured.clear();
    timing.bind_jobs.clear();
    timing.lists.extend(
        world
            .query_filtered::<Entity, With<VirtualList>>()
            .iter(world),
    );
    for index in 0..timing.lists.len() {
        let entity = timing.lists[index];
        if reconcile_one(world, entity, &mut timing.bind_jobs) {
            timing.measured.push(entity);
        }
    }
    for index in 0..timing.bind_jobs.len() {
        let job = &timing.bind_jobs[index];
        let model = Arc::clone(&job.model);
        model.bind(world, job.content, job.index);
    }
    timing.bind_jobs.clear();
    world.insert_resource(timing);
}

fn reconcile_one(world: &mut World, entity: Entity, bind_jobs: &mut Vec<BindJob>) -> bool {
    let Some(state_handle) = world
        .get::<VirtualList>(entity)
        .map(|list| Arc::clone(&list.state))
    else {
        return false;
    };
    let Some(model) = world
        .get::<ErasedModel>(entity)
        .map(|model| Arc::clone(&model.0))
    else {
        return false;
    };
    let mut state = state_handle
        .lock()
        .expect("virtual-list state lock poisoned");
    let mut scratch = std::mem::take(&mut state.scratch);
    let metrics = FixedRows(state.row_height);
    let len = model.len();
    let model_len_changed = state.model_len != len;

    let viewport_height = world
        .get::<ComputedNode>(state.viewport)
        .map(|node| node.size().y * node.inverse_scale_factor)
        .filter(|height| height.is_finite() && *height > 0.0)
        .unwrap_or(state.fallback_viewport_height);
    let mut offset = world
        .get::<ScrollPosition>(state.viewport)
        .map(|position| position.y)
        .unwrap_or(0.0);

    scratch.hints.clear();
    std::mem::swap(&mut scratch.hints, &mut state.pending_hints);
    scratch.updated.clear();
    let had_mutations = !scratch.hints.is_empty();
    let had_structural_mutations = scratch.hints.iter().any(|hint| {
        matches!(
            hint,
            ChangeHint::Reset | ChangeHint::Inserted(_) | ChangeHint::Removed(_)
        )
    });
    let mut working_len = state.model_len;
    let mut working_top_index = state.top_anchor.map(|(_, _, index)| index);
    let mut working_cursor_index = state.cursor_index;
    let mut working_selection_anchor_index = state.selection_anchor_index;
    for (hint_index, hint) in scratch.hints.iter().enumerate() {
        match hint {
            ChangeHint::Reset => {
                let (reset_len, final_anchor_index) = (
                    rewind_len(len, &scratch.hints[hint_index + 1..]),
                    state
                        .top_anchor
                        .and_then(|(anchor, _, _)| find_row(&*model, anchor)),
                );
                if let Some((_anchor, intra, _)) = state.top_anchor {
                    let index = final_anchor_index
                        .map(|index| rewind_index(index, &scratch.hints[hint_index + 1..]))
                        .or_else(|| {
                            working_top_index.map(|index| index.min(reset_len.saturating_sub(1)))
                        });
                    if let Some(index) = index.filter(|_| reset_len > 0) {
                        offset = metrics.offset_of(index) + intra;
                        working_top_index = Some(index);
                    } else {
                        offset = 0.0;
                        working_top_index = None;
                    }
                }
                working_len = reset_len;
                scratch.updated.push(0..len);
            }
            ChangeHint::Inserted(range) => {
                let inserted = range.len();
                if let Some(top_index) = working_top_index {
                    if range.start <= top_index {
                        offset += (range.len() as f32) * state.row_height;
                    }
                }
                working_top_index =
                    working_top_index.map(|index| insert_index(index, range.start, inserted));
                working_cursor_index =
                    working_cursor_index.map(|index| insert_index(index, range.start, inserted));
                working_selection_anchor_index = working_selection_anchor_index
                    .map(|index| insert_index(index, range.start, inserted));
                working_len = working_len.saturating_add(inserted);
                scratch.updated.push(range.start..len);
            }
            ChangeHint::Removed(range) => {
                let start = range.start.min(working_len);
                let end = range.end.min(working_len).max(start);
                let removed = end - start;
                let next_len = working_len.saturating_sub(removed);
                if let Some(top_index) = working_top_index {
                    if range.start < top_index {
                        let removed_before = removed.min(top_index - range.start);
                        offset -= (removed_before as f32) * state.row_height;
                    }
                }
                working_top_index =
                    working_top_index.and_then(|index| remove_index(index, start..end, next_len));
                working_cursor_index = working_cursor_index
                    .and_then(|index| remove_index(index, start..end, next_len));
                working_selection_anchor_index = working_selection_anchor_index
                    .and_then(|index| remove_index(index, start..end, next_len));
                working_len = next_len;
                scratch.updated.push(range.start.min(len)..len);
            }
            ChangeHint::Updated(range) => scratch.updated.push(range.clone()),
        }
    }
    debug_assert!(
        scratch
            .hints
            .iter()
            .any(|hint| matches!(hint, ChangeHint::Reset))
            || working_len == len,
        "virtual-list mutation hints describe {working_len} rows but model has {len}"
    );
    state.model_len = len;

    let mut selection_changed = false;
    if had_structural_mutations {
        scratch.tracked_ids.clear();
        scratch.tracked_ids.extend(state.selected.iter().copied());
        scratch.tracked_ids.extend(state.cursor);
        scratch.tracked_ids.extend(state.selection_anchor);
        scratch.model_positions.clear();
        if !scratch.tracked_ids.is_empty() {
            for index in 0..len {
                let row_id = model.row_id(index);
                if scratch.tracked_ids.contains(&row_id) {
                    scratch.model_positions.insert(row_id, index);
                }
            }
        }
        scratch.selection_before.clear();
        scratch
            .selection_before
            .extend(state.selected.iter().copied());
        state
            .selected
            .retain(|id| scratch.model_positions.contains_key(id));
        selection_changed |= scratch.selection_before.len() != state.selected.len();
        reconcile_stable_focus(
            &*model,
            &scratch.model_positions,
            &mut state.cursor,
            &mut working_cursor_index,
        );
        reconcile_stable_focus(
            &*model,
            &scratch.model_positions,
            &mut state.selection_anchor,
            &mut working_selection_anchor_index,
        );
        state.cursor_index = working_cursor_index;
        state.selection_anchor_index = working_selection_anchor_index;
        if selection_changed {
            state
                .force_rebind_ids
                .extend(scratch.selection_before.iter().copied());
            let selected = state.selected.iter().copied().collect::<Vec<_>>();
            state.force_rebind_ids.extend(selected);
        }
    }

    let had_scroll_request = state.pending_scroll.is_some();
    if let Some((index, align)) = state.pending_scroll.take() {
        if len > 0 {
            let index = index.min(len - 1);
            let row_top = metrics.offset_of(index);
            offset = match align {
                Align::Start => row_top,
                Align::Center => row_top - (viewport_height - state.row_height) * 0.5,
                Align::End => row_top - (viewport_height - state.row_height),
            };
            state.cursor = Some(model.row_id(index));
            state.cursor_index = Some(index);
        }
    }
    let had_reveal_request = state.pending_reveal.is_some();
    if let Some(index) = state.pending_reveal.take() {
        if len > 0 {
            let index = index.min(len - 1);
            let row_top = metrics.offset_of(index);
            let row_bottom = row_top + state.row_height;
            if row_top < offset {
                offset = row_top;
            } else if row_bottom > offset + viewport_height {
                offset = row_bottom - viewport_height;
            }
        }
    }

    let max_offset = (metrics.extent(len) - viewport_height).max(0.0);
    offset = offset.clamp(0.0, max_offset);
    if let Some(mut position) = world.get_mut::<ScrollPosition>(state.viewport) {
        if position.y != offset {
            position.y = offset;
        }
        if position.x != 0.0 {
            position.x = 0.0;
        }
    }
    let content_height = px(metrics.extent(len));
    if let Some(mut content) = world.get_mut::<Node>(state.content) {
        if content.height != content_height {
            content.height = content_height;
        }
    }
    if let Some(mut accessible) = world.get_mut::<AccessibilityNode>(state.viewport) {
        if accessible.size_of_set() != Some(len) {
            accessible.set_size_of_set(len);
        }
        let multiselectable = state.selection_mode != SelectionMode::Single;
        if multiselectable && !accessible.is_multiselectable() {
            accessible.set_multiselectable();
        } else if !multiselectable && accessible.is_multiselectable() {
            accessible.clear_multiselectable();
        }
    }

    let desired = metrics.window(offset, viewport_height, len, state.overscan);
    scratch
        .updated
        .retain(|range| ranges_intersect(range, &desired));
    let geometry_changed = offset != state.last_offset
        || viewport_height != state.last_viewport_height
        || desired != state.realised;
    let has_rebind = !scratch.updated.is_empty() || !state.force_rebind_ids.is_empty();
    if geometry_changed || has_rebind {
        realise_window(
            world,
            Arc::clone(&model),
            &mut state,
            &mut scratch,
            RealiseRequest {
                list: entity,
                desired: desired.clone(),
                model_len_changed,
            },
            bind_jobs,
        );
    }

    state.realised = desired;
    state.last_offset = offset;
    state.last_viewport_height = viewport_height;
    state.top_anchor = if len == 0 {
        None
    } else {
        let index = metrics.index_at_offset(offset, len);
        Some((
            model.row_id(index),
            offset - metrics.offset_of(index),
            index,
        ))
    };
    state.force_rebind_ids.clear();
    let record_latency = if state.latency_warmup_frames > 0 {
        state.latency_warmup_frames -= 1;
        false
    } else {
        had_mutations
            || had_scroll_request
            || had_reveal_request
            || geometry_changed
            || has_rebind
            || selection_changed
    };
    scratch.hints.clear();
    state.scratch = scratch;
    if selection_changed {
        queue_selection_event(&mut state);
    }
    drop(state);
    flush_selection_events(world, entity, &state_handle);
    record_latency
}

fn insert_index(index: usize, start: usize, inserted: usize) -> usize {
    if start <= index {
        index.saturating_add(inserted)
    } else {
        index
    }
}

fn rewind_len(mut len: usize, later_hints: &[ChangeHint]) -> usize {
    for hint in later_hints.iter().rev() {
        match hint {
            ChangeHint::Inserted(range) => len = len.saturating_sub(range.len()),
            ChangeHint::Removed(range) => len = len.saturating_add(range.len()),
            ChangeHint::Reset => break,
            ChangeHint::Updated(_) => {}
        }
    }
    len
}

fn rewind_index(mut index: usize, later_hints: &[ChangeHint]) -> usize {
    for hint in later_hints.iter().rev() {
        match hint {
            ChangeHint::Inserted(range) => {
                if index >= range.end {
                    index -= range.len();
                } else if index >= range.start {
                    index = range.start;
                }
            }
            ChangeHint::Removed(range) => {
                if index >= range.start {
                    index = index.saturating_add(range.len());
                }
            }
            ChangeHint::Reset => break,
            ChangeHint::Updated(_) => {}
        }
    }
    index
}

fn remove_index(index: usize, removed: Range<usize>, next_len: usize) -> Option<usize> {
    if next_len == 0 {
        return None;
    }
    if index < removed.start {
        Some(index)
    } else if index >= removed.end {
        Some(index - removed.len())
    } else {
        Some(removed.start.min(next_len - 1))
    }
}

fn reconcile_stable_focus(
    model: &dyn VirtualListModel,
    positions: &HashMap<RowId, usize>,
    id: &mut Option<RowId>,
    index: &mut Option<usize>,
) {
    let Some(current) = *id else {
        *index = None;
        return;
    };
    if let Some(surviving_index) = positions.get(&current).copied() {
        *index = Some(surviving_index);
        return;
    }
    if model.is_empty() {
        *id = None;
        *index = None;
        return;
    }
    let neighbour = index.unwrap_or(0).min(model.len() - 1);
    *id = Some(model.row_id(neighbour));
    *index = Some(neighbour);
}

fn ranges_intersect(left: &Range<usize>, right: &Range<usize>) -> bool {
    left.start < right.end && right.start < left.end
}

fn realise_window(
    world: &mut World,
    model: Arc<dyn VirtualListModel>,
    state: &mut VirtualListState,
    scratch: &mut ReconcileScratch,
    request: RealiseRequest,
    bind_jobs: &mut Vec<BindJob>,
) {
    scratch.existing.clear();
    scratch.available.clear();
    for entity in state.rows.drain(..) {
        if let Some(row) = world.get::<VirtualListRow>(entity) {
            if let Some(duplicate) = scratch.existing.insert(row.row_id, entity) {
                scratch.available.push(duplicate);
            }
        } else {
            scratch.available.push(entity);
        }
    }
    scratch.assignments.clear();
    scratch.desired_ids.clear();
    for index in request.desired {
        let row_id = model.row_id(index);
        if !scratch.desired_ids.insert(row_id) {
            warn_once!(
                ?row_id,
                index,
                "virtual-list model returned a duplicate RowId in the realised window; skipping it"
            );
            #[cfg(debug_assertions)]
            if state.debug_assert_duplicates {
                debug_assert!(
                    false,
                    "virtual-list model returned duplicate {row_id:?} at index {index}"
                );
            }
            continue;
        }
        scratch
            .assignments
            .push((index, row_id, scratch.existing.remove(&row_id)));
    }
    scratch
        .available
        .extend(scratch.existing.drain().map(|(_, entity)| entity));
    scratch.realised.clear();
    for (index, row_id, existing) in scratch.assignments.drain(..) {
        let row = existing
            .or_else(|| scratch.available.pop())
            .unwrap_or_else(|| {
                world
                    .spawn((Node::default(), Pickable::default(), Hovered::default()))
                    .id()
            });
        let old = world.get::<VirtualListRow>(row).copied();
        let selected = state.selected.contains(&row_id);
        let changed_binding = old.is_none_or(|old| {
            old.index != index || old.row_id != row_id || old.selected != selected
        });
        let explicitly_updated = scratch.updated.iter().any(|range| range.contains(&index))
            || state.force_rebind_ids.contains(&row_id);

        let desired_row = VirtualListRow {
            list: request.list,
            index,
            row_id,
            selected,
        };
        if old != Some(desired_row) {
            world.entity_mut(row).insert(desired_row);
        }
        let desired_node = Node {
            position_type: PositionType::Absolute,
            top: px((index as f32) * state.row_height),
            left: px(0),
            width: percent(100),
            height: px(state.row_height),
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Center,
            overflow: Overflow::clip(),
            ..default()
        };
        if world.get::<Node>(row) != Some(&desired_node) {
            world.entity_mut(row).insert(desired_node);
        }
        if world.get::<AccessibilityNode>(row).is_none() {
            let mut item_accessible = accesskit::Node::new(Role::ListItem);
            item_accessible.set_position_in_set(index);
            item_accessible.set_size_of_set(state.model_len);
            item_accessible.set_selected(selected);
            world
                .entity_mut(row)
                .insert(AccessibilityNode::from(item_accessible));
        } else if changed_binding || request.model_len_changed {
            let mut accessible = world
                .get_mut::<AccessibilityNode>(row)
                .expect("checked virtual-list row accessibility");
            if accessible.position_in_set() != Some(index) {
                accessible.set_position_in_set(index);
            }
            if accessible.size_of_set() != Some(state.model_len) {
                accessible.set_size_of_set(state.model_len);
            }
            if accessible.is_selected() != Some(selected) {
                accessible.set_selected(selected);
            }
        }
        let background = ThemeBackgroundColor(if selected {
            tokens::ROW_SELECTED
        } else {
            tokens::SURFACE
        });
        if world
            .get::<ThemeBackgroundColor>(row)
            .is_none_or(|current| current.0 != background.0)
        {
            world.entity_mut(row).insert(background);
        }
        if world.get::<ChildOf>(row).is_none() {
            world.entity_mut(state.content).add_child(row);
        }
        if changed_binding || explicitly_updated {
            world.entity_mut(row).despawn_children();
            let content_accessible = accesskit::Node::new(Role::GenericContainer);
            let content = world
                .spawn((
                    Node {
                        width: percent(100),
                        height: percent(100),
                        flex_direction: FlexDirection::Row,
                        align_items: AlignItems::Center,
                        ..default()
                    },
                    Pickable::IGNORE,
                    AccessibilityNode::from(content_accessible),
                ))
                .id();
            world.entity_mut(row).add_child(content);
            bind_jobs.push(BindJob {
                model: Arc::clone(&model),
                content,
                index,
            });
        }
        scratch.realised.push(row);
    }
    for stale in scratch.available.drain(..) {
        world.despawn(stale);
    }
    std::mem::swap(&mut state.rows, &mut scratch.realised);
}

fn paint_virtual_rows(world: &mut World) {
    let mut timing = world
        .remove_resource::<VirtualListFrameTiming>()
        .unwrap_or_default();
    timing.paint_rows.clear();
    timing.paint_rows.extend(
        world
            .query_filtered::<Entity, With<VirtualListRow>>()
            .iter(world),
    );
    for index in 0..timing.paint_rows.len() {
        let entity = timing.paint_rows[index];
        let Some((row, hovered, background)) = world
            .get::<VirtualListRow>(entity)
            .zip(world.get::<Hovered>(entity))
            .zip(world.get::<ThemeBackgroundColor>(entity))
            .map(|((row, hovered), background)| (*row, hovered.get(), background.0.clone()))
        else {
            continue;
        };
        let token = if row.selected {
            tokens::ROW_SELECTED
        } else if hovered {
            tokens::ROW_HOVER
        } else {
            tokens::SURFACE
        };
        if background != token {
            world.entity_mut(entity).insert(ThemeBackgroundColor(token));
        }
    }
    let Some(started) = timing.started.take() else {
        world.insert_resource(timing);
        return;
    };
    let elapsed = started.elapsed();
    for index in 0..timing.measured.len() {
        let list_entity = timing.measured[index];
        if let Some(list) = world.get::<VirtualList>(list_entity) {
            list.state
                .lock()
                .expect("virtual-list state lock poisoned")
                .latency
                .record(elapsed);
        }
    }
    timing.measured.clear();
    world.insert_resource(timing);
}

fn find_row(model: &dyn VirtualListModel, id: RowId) -> Option<usize> {
    (0..model.len()).find(|index| model.row_id(*index) == id)
}

fn queue_selection_event(state: &mut VirtualListState) {
    let selected = state.selected.iter().copied().collect::<Vec<_>>();
    if state.pending_selection_events.back() != Some(&selected) {
        state.pending_selection_events.push_back(selected);
    }
}

fn flush_selection_events(
    world: &mut World,
    list_entity: Entity,
    state_handle: &Arc<Mutex<VirtualListState>>,
) {
    loop {
        let selected = state_handle
            .lock()
            .expect("virtual-list state lock poisoned")
            .pending_selection_events
            .pop_front();
        let Some(selected) = selected else {
            break;
        };
        world.trigger(VirtualListSelectionChanged {
            list: list_entity,
            selected,
        });
    }
}

fn select_index(world: &mut World, list_entity: Entity, index: usize, shift: bool, ctrl: bool) {
    let Some(state_handle) = world
        .get::<VirtualList>(list_entity)
        .map(|list| Arc::clone(&list.state))
    else {
        return;
    };
    let Some(model) = world
        .get::<ErasedModel>(list_entity)
        .map(|model| Arc::clone(&model.0))
    else {
        return;
    };
    if index >= model.len() {
        return;
    }
    let mut state = state_handle
        .lock()
        .expect("virtual-list state lock poisoned");
    let id = model.row_id(index);
    let before = state.selected.clone();
    let anchor_index = state
        .selection_anchor
        .and_then(|anchor| find_row(&*model, anchor));

    if shift && state.selection_mode != SelectionMode::Single {
        let anchor = anchor_index.unwrap_or(index);
        if state.selection_mode == SelectionMode::Contiguous {
            state.selected.clear();
        }
        for range_index in anchor.min(index)..=anchor.max(index) {
            state.selected.insert(model.row_id(range_index));
        }
    } else if ctrl && state.selection_mode == SelectionMode::Disjoint {
        if !state.selected.remove(&id) {
            state.selected.insert(id);
        }
        state.selection_anchor = Some(id);
        state.selection_anchor_index = Some(index);
    } else {
        state.selected.clear();
        state.selected.insert(id);
        state.selection_anchor = Some(id);
        state.selection_anchor_index = Some(index);
    }
    state.cursor = Some(id);
    state.cursor_index = Some(index);

    if before != state.selected {
        state.force_rebind_ids.extend(before.iter().copied());
        let selected_now = state.selected.iter().copied().collect::<Vec<_>>();
        state.force_rebind_ids.extend(selected_now);
        queue_selection_event(&mut state);
        drop(state);
        flush_selection_events(world, list_entity, &state_handle);
    }
}

fn handle_key(world: &mut World, list_entity: Entity, key: KeyCode, shift: bool, control: bool) {
    let Some(state_handle) = world
        .get::<VirtualList>(list_entity)
        .map(|list| Arc::clone(&list.state))
    else {
        return;
    };
    let Some(model) = world
        .get::<ErasedModel>(list_entity)
        .map(|model| Arc::clone(&model.0))
    else {
        return;
    };
    let len = model.len();
    if len == 0 {
        return;
    }
    let mut state = state_handle
        .lock()
        .expect("virtual-list state lock poisoned");
    let current = state.cursor_index.unwrap_or(0).min(len - 1);
    if matches!(key, KeyCode::Enter | KeyCode::NumpadEnter) {
        let row_id = model.row_id(current);
        drop(state);
        world.trigger(VirtualListRowActivated {
            list: list_entity,
            row_id,
            index: current,
        });
        return;
    }

    let page = ((state.last_viewport_height / state.row_height).floor() as usize).max(1);
    let next = match key {
        KeyCode::ArrowUp => current.saturating_sub(1),
        KeyCode::ArrowDown => current.saturating_add(1).min(len - 1),
        KeyCode::PageUp => current.saturating_sub(page),
        KeyCode::PageDown => current.saturating_add(page).min(len - 1),
        KeyCode::Home => 0,
        KeyCode::End => len - 1,
        _ => current,
    };
    state.pending_reveal = Some(next);
    drop(state);
    select_index(world, list_entity, next, shift, control);
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, RwLock};

    use super::*;

    #[derive(Resource, Default)]
    struct SelectionLog(Vec<Vec<RowId>>);

    fn log_selection(event: On<VirtualListSelectionChanged>, mut log: ResMut<SelectionLog>) {
        log.0.push(event.selected.clone());
    }

    #[derive(Clone)]
    struct TestModel(Arc<RwLock<Vec<RowId>>>, Arc<Mutex<Vec<usize>>>);

    impl TestModel {
        fn ids(ids: impl IntoIterator<Item = u64>) -> Self {
            Self(
                Arc::new(RwLock::new(ids.into_iter().map(RowId).collect())),
                Arc::new(Mutex::new(Vec::new())),
            )
        }
    }

    impl VirtualListModel for TestModel {
        fn len(&self) -> usize {
            self.0.read().unwrap().len()
        }

        fn row_id(&self, index: usize) -> RowId {
            self.0.read().unwrap()[index]
        }

        fn bind(&self, world: &mut World, content: Entity, index: usize) {
            self.1.lock().unwrap().push(index);
            world
                .entity_mut(content)
                .insert(Name::new(format!("row {index}")));
        }
    }

    #[derive(Clone)]
    struct CountingModel {
        ids: Arc<Vec<RowId>>,
        row_id_calls: Arc<AtomicUsize>,
    }

    impl CountingModel {
        fn new(len: usize) -> Self {
            Self {
                ids: Arc::new((0..len as u64).map(RowId).collect()),
                row_id_calls: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl VirtualListModel for CountingModel {
        fn len(&self) -> usize {
            self.ids.len()
        }

        fn row_id(&self, index: usize) -> RowId {
            self.row_id_calls.fetch_add(1, Ordering::Relaxed);
            self.ids[index]
        }

        fn bind(&self, _world: &mut World, _content: Entity, _index: usize) {}
    }

    #[derive(Clone)]
    struct ReentrantModel {
        list: Arc<Mutex<Option<Entity>>>,
        binds: Arc<AtomicUsize>,
    }

    impl VirtualListModel for ReentrantModel {
        fn len(&self) -> usize {
            10
        }

        fn row_id(&self, index: usize) -> RowId {
            RowId(index as u64)
        }

        fn bind(&self, world: &mut World, _content: Entity, _index: usize) {
            let list = self.list.lock().unwrap().expect("test list assigned");
            let list = world.get::<VirtualList>(list).unwrap();
            assert_eq!(list.model_len(), 10);
            let _ = list.selected_ids().collect::<Vec<_>>();
            self.binds.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn fixed_window_math_clamps_and_overscans() {
        let rows = FixedRows(10.0);
        assert_eq!(rows.window(0.0, 200.0, 100, Overscan::Rows(3)), 0..23);
        assert_eq!(rows.window(400.0, 200.0, 100, Overscan::Rows(3)), 37..63);
        assert_eq!(rows.window(950.0, 200.0, 100, Overscan::Rows(3)), 92..100);
        assert_eq!(rows.window(0.0, 200.0, 0, Overscan::Rows(3)), 0..0);
    }

    #[test]
    fn headless_rows_stay_bounded() {
        let mut app = App::new();
        app.add_plugins(VirtualListPlugin);
        let mut commands = app.world_mut().commands();
        let entities = spawn_virtual_list(
            &mut commands,
            VirtualListProps::new(10.0, 200.0, "Rows").overscan(Overscan::Rows(3)),
            TestModel::ids(0..100_000),
        );
        app.world_mut().flush();
        app.update();
        let live_rows = {
            let world = app.world_mut();
            let mut rows = world.query::<&VirtualListRow>();
            rows.iter(world).count()
        };
        assert!(live_rows <= 20 + 2 * 3, "live rows: {live_rows}");

        app.world_mut()
            .entity_mut(entities.root)
            .get_mut::<VirtualList>()
            .unwrap()
            .state
            .lock()
            .unwrap()
            .pending_scroll = Some((50_000, Align::Start));
        app.update();
        let live_rows = {
            let world = app.world_mut();
            let mut rows = world.query::<&VirtualListRow>();
            rows.iter(world).count()
        };
        assert!(live_rows <= 20 + 2 * 3, "live rows: {live_rows}");
    }

    #[test]
    fn binder_can_reenter_list_read_apis_without_deadlocking() {
        let list_slot = Arc::new(Mutex::new(None));
        let binds = Arc::new(AtomicUsize::new(0));
        let model = ReentrantModel {
            list: list_slot.clone(),
            binds: binds.clone(),
        };
        let mut app = App::new();
        app.add_plugins(VirtualListPlugin);
        let root = {
            let mut commands = app.world_mut().commands();
            spawn_virtual_list(
                &mut commands,
                VirtualListProps::new(10.0, 50.0, "Rows"),
                model,
            )
            .root
        };
        *list_slot.lock().unwrap() = Some(root);
        app.world_mut().flush();
        app.update();
        assert_eq!(binds.load(Ordering::Relaxed), 10);
    }

    #[test]
    fn updated_hint_rebinds_only_intersection() {
        let model = TestModel::ids(0..100);
        let binds = model.1.clone();
        let mut app = App::new();
        app.add_plugins(VirtualListPlugin);
        let root = {
            let mut commands = app.world_mut().commands();
            spawn_virtual_list(
                &mut commands,
                VirtualListProps::new(10.0, 200.0, "Rows").overscan(Overscan::Rows(0)),
                model,
            )
            .root
        };
        app.world_mut().flush();
        app.update();
        binds.lock().unwrap().clear();

        app.world_mut().trigger(VirtualListModelChanged {
            list: root,
            hint: ChangeHint::Updated(5..7),
        });
        app.update();
        let realised = app.world().get::<VirtualList>(root).unwrap();
        assert_eq!(realised.realised_range(), 0..20);
        assert_eq!(*binds.lock().unwrap(), vec![5, 6]);
    }

    #[test]
    fn off_window_updated_hint_does_not_reconcile() {
        let model = TestModel::ids(0..100);
        let binds = model.1.clone();
        let mut app = App::new();
        app.add_plugins(VirtualListPlugin);
        let root = {
            let mut commands = app.world_mut().commands();
            spawn_virtual_list(
                &mut commands,
                VirtualListProps::new(10.0, 200.0, "Rows").overscan(Overscan::Rows(0)),
                model,
            )
            .root
        };
        app.world_mut().flush();
        app.update();
        binds.lock().unwrap().clear();

        app.world_mut().trigger(VirtualListModelChanged {
            list: root,
            hint: ChangeHint::Updated(80..90),
        });
        app.update();
        assert!(binds.lock().unwrap().is_empty());
    }

    #[test]
    fn off_window_updated_hint_does_not_scan_the_model() {
        let model = CountingModel::new(100_000);
        let calls = model.row_id_calls.clone();
        let mut app = App::new();
        app.add_plugins(VirtualListPlugin);
        let root = {
            let mut commands = app.world_mut().commands();
            spawn_virtual_list(
                &mut commands,
                VirtualListProps::new(10.0, 200.0, "Rows").overscan(Overscan::Rows(0)),
                model,
            )
            .root
        };
        app.world_mut().flush();
        app.update();
        app.update();
        app.update();
        calls.store(0, Ordering::Relaxed);

        app.world_mut().trigger(VirtualListModelChanged {
            list: root,
            hint: ChangeHint::Updated(90_000..90_010),
        });
        app.update();

        assert!(
            calls.load(Ordering::Relaxed) <= 1,
            "off-window update scanned model rows"
        );
        assert_eq!(
            app.world()
                .get::<VirtualList>(root)
                .unwrap()
                .latency()
                .count(),
            1,
            "mutation frame was omitted from latency"
        );
    }

    #[test]
    fn insert_and_remove_hints_preserve_the_visible_anchor() {
        let model = TestModel::ids(0..100);
        let shared = model.0.clone();
        let mut app = App::new();
        app.add_plugins(VirtualListPlugin);
        let (root, viewport) = {
            let mut commands = app.world_mut().commands();
            let entities = spawn_virtual_list(
                &mut commands,
                VirtualListProps::new(10.0, 50.0, "Rows").overscan(Overscan::Rows(0)),
                model,
            );
            (entities.root, entities.viewport)
        };
        app.world_mut().flush();
        app.update();
        app.world_mut()
            .get_mut::<ScrollPosition>(viewport)
            .unwrap()
            .y = 200.0;
        app.update();

        shared.write().unwrap().insert(0, RowId(999));
        app.world_mut().trigger(VirtualListModelChanged {
            list: root,
            hint: ChangeHint::Inserted(0..1),
        });
        app.update();
        assert_eq!(
            app.world().get::<ScrollPosition>(viewport).unwrap().y,
            210.0
        );
        assert_eq!(
            app.world()
                .get::<VirtualList>(root)
                .unwrap()
                .state
                .lock()
                .unwrap()
                .top_anchor,
            Some((RowId(20), 0.0, 21))
        );

        shared.write().unwrap().remove(0);
        app.world_mut().trigger(VirtualListModelChanged {
            list: root,
            hint: ChangeHint::Removed(0..1),
        });
        app.update();
        assert_eq!(
            app.world().get::<ScrollPosition>(viewport).unwrap().y,
            200.0
        );
        assert_eq!(
            app.world()
                .get::<VirtualList>(root)
                .unwrap()
                .state
                .lock()
                .unwrap()
                .top_anchor,
            Some((RowId(20), 0.0, 20))
        );
    }

    #[test]
    fn multiple_hints_advance_the_working_anchor_sequentially() {
        let model = TestModel::ids(0..100);
        let shared = model.0.clone();
        let mut app = App::new();
        app.add_plugins(VirtualListPlugin);
        let (root, viewport) = {
            let mut commands = app.world_mut().commands();
            let entities = spawn_virtual_list(
                &mut commands,
                VirtualListProps::new(10.0, 50.0, "Rows").overscan(Overscan::Rows(0)),
                model,
            );
            (entities.root, entities.viewport)
        };
        app.world_mut().flush();
        app.update();
        app.world_mut()
            .get_mut::<ScrollPosition>(viewport)
            .unwrap()
            .y = 200.0;
        app.update();

        {
            let mut ids = shared.write().unwrap();
            ids.insert(0, RowId(1_000));
            ids.insert(21, RowId(1_001));
        }
        app.world_mut().trigger(VirtualListModelChanged {
            list: root,
            hint: ChangeHint::Inserted(0..1),
        });
        app.world_mut().trigger(VirtualListModelChanged {
            list: root,
            hint: ChangeHint::Inserted(21..22),
        });
        app.update();

        assert_eq!(
            app.world().get::<ScrollPosition>(viewport).unwrap().y,
            220.0
        );
        assert_eq!(
            app.world()
                .get::<VirtualList>(root)
                .unwrap()
                .state
                .lock()
                .unwrap()
                .top_anchor,
            Some((RowId(20), 0.0, 22))
        );
    }

    #[test]
    fn removal_consuming_the_anchor_chooses_the_surviving_successor() {
        let model = TestModel::ids(0..100);
        let shared = model.0.clone();
        let mut app = App::new();
        app.add_plugins(VirtualListPlugin);
        let (root, viewport) = {
            let mut commands = app.world_mut().commands();
            let entities = spawn_virtual_list(
                &mut commands,
                VirtualListProps::new(10.0, 50.0, "Rows").overscan(Overscan::Rows(0)),
                model,
            );
            (entities.root, entities.viewport)
        };
        app.world_mut().flush();
        app.update();
        app.world_mut()
            .get_mut::<ScrollPosition>(viewport)
            .unwrap()
            .y = 200.0;
        app.update();

        shared.write().unwrap().drain(18..23);
        app.world_mut().trigger(VirtualListModelChanged {
            list: root,
            hint: ChangeHint::Removed(18..23),
        });
        app.update();

        assert_eq!(
            app.world().get::<ScrollPosition>(viewport).unwrap().y,
            180.0
        );
        assert_eq!(
            app.world()
                .get::<VirtualList>(root)
                .unwrap()
                .state
                .lock()
                .unwrap()
                .top_anchor,
            Some((RowId(23), 0.0, 18))
        );
    }

    #[test]
    fn selection_is_stable_across_insert() {
        let model = TestModel::ids(10..20);
        let shared = model.0.clone();
        let mut app = App::new();
        app.add_plugins(VirtualListPlugin);
        let root = {
            let mut commands = app.world_mut().commands();
            spawn_virtual_list(
                &mut commands,
                VirtualListProps::new(10.0, 50.0, "Rows"),
                model,
            )
            .root
        };
        app.world_mut().flush();
        app.update();
        select_index(app.world_mut(), root, 3, false, false);
        shared.write().unwrap().insert(0, RowId(99));
        app.world_mut().trigger(VirtualListModelChanged {
            list: root,
            hint: ChangeHint::Inserted(0..1),
        });
        app.update();
        assert_eq!(
            app.world()
                .get::<VirtualList>(root)
                .unwrap()
                .selected_ids()
                .collect::<Vec<_>>(),
            vec![RowId(13)]
        );
    }

    #[test]
    fn mutation_prunes_selection_and_repairs_cursor_and_shift_anchor_once() {
        let model = TestModel::ids(10..20);
        let shared = model.0.clone();
        let mut app = App::new();
        app.init_resource::<SelectionLog>()
            .add_observer(log_selection)
            .add_plugins(VirtualListPlugin);
        let root = {
            let mut commands = app.world_mut().commands();
            spawn_virtual_list(
                &mut commands,
                VirtualListProps::new(10.0, 50.0, "Rows").selection_mode(SelectionMode::Disjoint),
                model,
            )
            .root
        };
        app.world_mut().flush();
        app.update();
        select_index(app.world_mut(), root, 2, false, false);
        select_index(app.world_mut(), root, 3, false, true);
        app.world_mut().resource_mut::<SelectionLog>().0.clear();

        shared.write().unwrap().drain(2..4);
        app.world_mut().trigger(VirtualListModelChanged {
            list: root,
            hint: ChangeHint::Removed(2..4),
        });
        app.update();

        let list = app.world().get::<VirtualList>(root).unwrap();
        let state = list.state.lock().unwrap();
        assert!(state.selected.is_empty());
        assert_eq!(state.cursor, Some(RowId(14)));
        assert_eq!(state.cursor_index, Some(2));
        assert_eq!(state.selection_anchor, Some(RowId(14)));
        assert_eq!(state.selection_anchor_index, Some(2));
        drop(state);
        assert_eq!(
            app.world().resource::<SelectionLog>().0,
            vec![Vec::<RowId>::new()]
        );

        app.world_mut()
            .get_mut::<VirtualList>(root)
            .unwrap()
            .set_selection_mode(SelectionMode::Contiguous);
        select_index(app.world_mut(), root, 0, true, false);
        assert_eq!(
            app.world()
                .get::<VirtualList>(root)
                .unwrap()
                .selected_ids()
                .collect::<Vec<_>>(),
            vec![RowId(10), RowId(11), RowId(14)]
        );
    }

    #[test]
    fn single_mode_keeps_a_selected_id_and_emits_once() {
        let model = TestModel::ids(10..20);
        let mut app = App::new();
        app.init_resource::<SelectionLog>()
            .add_observer(log_selection)
            .add_plugins(VirtualListPlugin);
        let root = {
            let mut commands = app.world_mut().commands();
            spawn_virtual_list(
                &mut commands,
                VirtualListProps::new(10.0, 50.0, "Rows").selection_mode(SelectionMode::Disjoint),
                model,
            )
            .root
        };
        app.world_mut().flush();
        app.update();
        select_index(app.world_mut(), root, 1, false, false);
        select_index(app.world_mut(), root, 2, false, true);
        select_index(app.world_mut(), root, 3, false, true);
        select_index(app.world_mut(), root, 3, false, true);
        app.world_mut().resource_mut::<SelectionLog>().0.clear();

        app.world_mut()
            .get_mut::<VirtualList>(root)
            .unwrap()
            .set_selection_mode(SelectionMode::Single);
        app.update();

        assert_eq!(
            app.world()
                .get::<VirtualList>(root)
                .unwrap()
                .selected_ids()
                .collect::<Vec<_>>(),
            vec![RowId(11)]
        );
        assert_eq!(
            app.world().resource::<SelectionLog>().0,
            vec![vec![RowId(11)]]
        );
    }

    #[test]
    fn deferred_mode_selection_precedes_direct_selection_without_duplicates() {
        let model = TestModel::ids(10..20);
        let mut app = App::new();
        app.init_resource::<SelectionLog>()
            .add_observer(log_selection)
            .add_plugins(VirtualListPlugin);
        let root = {
            let mut commands = app.world_mut().commands();
            spawn_virtual_list(
                &mut commands,
                VirtualListProps::new(10.0, 50.0, "Rows").selection_mode(SelectionMode::Disjoint),
                model,
            )
            .root
        };
        app.world_mut().flush();
        app.update();
        select_index(app.world_mut(), root, 1, false, false);
        select_index(app.world_mut(), root, 2, false, true);
        app.world_mut().resource_mut::<SelectionLog>().0.clear();

        app.world_mut()
            .get_mut::<VirtualList>(root)
            .unwrap()
            .set_selection_mode(SelectionMode::Single);
        select_index(app.world_mut(), root, 3, false, false);
        assert_eq!(
            app.world().resource::<SelectionLog>().0,
            vec![vec![RowId(12)], vec![RowId(13)]]
        );

        app.update();
        assert_eq!(
            app.world().resource::<SelectionLog>().0,
            vec![vec![RowId(12)], vec![RowId(13)]]
        );
    }

    #[test]
    fn duplicate_realised_ids_skip_without_leaking_shells() {
        let model = TestModel::ids([0, 1, 1, 3, 4, 5, 6, 7, 8, 9]);
        let mut app = App::new();
        app.add_plugins(VirtualListPlugin);
        let (root, viewport) = {
            let mut commands = app.world_mut().commands();
            let entities = spawn_virtual_list(
                &mut commands,
                VirtualListProps::new(10.0, 50.0, "Rows").overscan(Overscan::Rows(0)),
                model,
            );
            (entities.root, entities.viewport)
        };
        app.world_mut().flush();
        #[cfg(debug_assertions)]
        {
            app.world()
                .get::<VirtualList>(root)
                .unwrap()
                .state
                .lock()
                .unwrap()
                .debug_assert_duplicates = false;
        }
        app.update();
        let initial_entities = {
            let world = app.world_mut();
            let mut entities = world.query::<Entity>();
            entities.iter(world).count()
        };
        let realised_rows = {
            let world = app.world_mut();
            let mut rows = world.query::<&VirtualListRow>();
            rows.iter(world).count()
        };
        assert_eq!(realised_rows, 4);

        app.world_mut()
            .get_mut::<ScrollPosition>(viewport)
            .unwrap()
            .y = 50.0;
        app.update();
        let live_entities = {
            let world = app.world_mut();
            let mut entities = world.query::<Entity>();
            entities.iter(world).count()
        };
        assert_eq!(live_entities, initial_entities + 2);

        app.world_mut()
            .get_mut::<ScrollPosition>(viewport)
            .unwrap()
            .y = 0.0;
        app.update();
        let live_entities = {
            let world = app.world_mut();
            let mut entities = world.query::<Entity>();
            entities.iter(world).count()
        };
        assert_eq!(live_entities, initial_entities);
        let realised_rows = {
            let world = app.world_mut();
            let mut rows = world.query::<&VirtualListRow>();
            rows.iter(world).count()
        };
        assert_eq!(realised_rows, 4);
    }

    #[test]
    fn rebind_replaces_the_application_content_entity() {
        let model = TestModel::ids(0..10);
        let mut app = App::new();
        app.add_plugins(VirtualListPlugin);
        let root = {
            let mut commands = app.world_mut().commands();
            spawn_virtual_list(
                &mut commands,
                VirtualListProps::new(10.0, 50.0, "Rows"),
                model,
            )
            .root
        };
        app.world_mut().flush();
        app.update();
        let row = {
            let world = app.world_mut();
            let mut query = world.query::<(Entity, &VirtualListRow)>();
            query
                .iter(world)
                .find_map(|(entity, row)| (row.index == 0).then_some(entity))
                .unwrap()
        };
        let old_content = app.world().get::<Children>(row).unwrap()[0];

        app.world_mut().trigger(VirtualListModelChanged {
            list: root,
            hint: ChangeHint::Updated(0..1),
        });
        app.update();

        let new_content = app.world().get::<Children>(row).unwrap()[0];
        assert_ne!(old_content, new_content);
        assert!(app.world().get_entity(old_content).is_err());
    }

    #[test]
    fn accessibility_positions_are_zero_based_for_the_adapter() {
        let mut app = App::new();
        app.add_plugins(VirtualListPlugin);
        {
            let mut commands = app.world_mut().commands();
            spawn_virtual_list(
                &mut commands,
                VirtualListProps::new(10.0, 50.0, "Rows"),
                TestModel::ids(0..10),
            );
        }
        app.world_mut().flush();
        app.update();
        let first_position = {
            let world = app.world_mut();
            let mut rows = world.query::<(&VirtualListRow, &AccessibilityNode)>();
            rows.iter(world)
                .find_map(|(row, accessible)| {
                    (row.index == 0).then(|| accessible.position_in_set())
                })
                .flatten()
        };
        assert_eq!(first_position, Some(0));
    }

    #[test]
    fn reset_preserves_top_row_id_anchor() {
        let model = TestModel::ids(0..100);
        let shared = model.0.clone();
        let mut app = App::new();
        app.add_plugins(VirtualListPlugin);
        let (root, viewport) = {
            let mut commands = app.world_mut().commands();
            let entities = spawn_virtual_list(
                &mut commands,
                VirtualListProps::new(10.0, 50.0, "Rows"),
                model,
            );
            (entities.root, entities.viewport)
        };
        app.world_mut().flush();
        app.update();
        app.world_mut()
            .get_mut::<ScrollPosition>(viewport)
            .unwrap()
            .y = 253.0;
        app.update();
        shared.write().unwrap().rotate_left(10);
        app.world_mut().trigger(VirtualListModelChanged {
            list: root,
            hint: ChangeHint::Reset,
        });
        app.update();
        let scroll = app.world().get::<ScrollPosition>(viewport).unwrap().y;
        assert_eq!(scroll, 153.0);
        assert_eq!(
            app.world()
                .get::<VirtualList>(root)
                .unwrap()
                .state
                .lock()
                .unwrap()
                .top_anchor,
            Some((RowId(25), 3.0, 15))
        );
    }

    #[test]
    fn reset_then_insert_resolves_anchor_at_the_reset_stage() {
        let model = TestModel::ids(0..100);
        let shared = model.0.clone();
        let mut app = App::new();
        app.add_plugins(VirtualListPlugin);
        let (root, viewport) = {
            let mut commands = app.world_mut().commands();
            let entities = spawn_virtual_list(
                &mut commands,
                VirtualListProps::new(10.0, 50.0, "Rows").overscan(Overscan::Rows(0)),
                model,
            );
            (entities.root, entities.viewport)
        };
        app.world_mut().flush();
        app.update();
        app.world_mut()
            .get_mut::<ScrollPosition>(viewport)
            .unwrap()
            .y = 200.0;
        app.update();

        {
            let mut ids = shared.write().unwrap();
            ids.rotate_left(10);
            ids.insert(10, RowId(1_000));
        }
        app.world_mut().trigger(VirtualListModelChanged {
            list: root,
            hint: ChangeHint::Reset,
        });
        app.world_mut().trigger(VirtualListModelChanged {
            list: root,
            hint: ChangeHint::Inserted(10..11),
        });
        app.update();

        assert_eq!(
            app.world().get::<ScrollPosition>(viewport).unwrap().y,
            110.0
        );
        assert_eq!(
            app.world()
                .get::<VirtualList>(root)
                .unwrap()
                .state
                .lock()
                .unwrap()
                .top_anchor,
            Some((RowId(20), 0.0, 11))
        );
    }
}
