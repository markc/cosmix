//! Generic CTK controls built on Bevy's headless widget contracts.

use core::f32::consts::PI;
use std::collections::{HashMap, VecDeque};

use accesskit::Role;
use bevy::a11y::AccessibilityNode;
use bevy::ecs::event::EntityEvent;
use bevy::ecs::hierarchy::{ChildOf, Children};
use bevy::ecs::lifecycle::Insert;
use bevy::ecs::observer::On;
use bevy::ecs::query::{Changed, Has, Or, With};
use bevy::ecs::schedule::IntoScheduleConfigs;
use bevy::ecs::system::{Commands, Query, Res, ResMut};
use bevy::feathers::theme::{ThemeBackgroundColor, UiTheme};
use bevy::input::keyboard::{KeyCode, KeyboardInput};
use bevy::input::{ButtonState, InputSystems};
use bevy::input_focus::tab_navigation::TabIndex;
use bevy::input_focus::{
    FocusCause, FocusedInput, InputFocus, InputFocusSystems, InputFocusVisible,
};
use bevy::math::Rot2;
use bevy::picking::events::{Cancel, Click, DragEnd, Pointer, Press, Release};
use bevy::picking::hover::Hovered;
use bevy::picking::Pickable;
use bevy::prelude::{
    default, App, BackgroundColor, Bundle, Component, DetectChanges, Entity, First, Node, Plugin,
    PreUpdate, Ref, Resource, SpawnRelated, SystemSet, UiRect, UiTransform, Update,
};
use bevy::ui::{
    percent, px, AlignItems, BorderRadius, Checkable, Checked, ComputedNode,
    ComputedUiRenderTargetInfo, Display, FlexDirection, InteractionDisabled, JustifyContent,
    PositionType, Pressed, UiGlobalTransform, UiScale,
};
use bevy::ui_widgets::{
    Activate, Slider, SliderDragState, SliderOrientation, SliderRange, SliderStep, SliderThumb,
    SliderValue, TrackClick, ValueChange,
};

/// Semantic metadata shared by local-only and Bus-bound widgets.
#[derive(Component, Clone, Debug, PartialEq, Eq)]
pub struct BusWidget {
    pub id: String,
    pub queryable: bool,
    pub writable: bool,
}

/// Optional per-control metadata the generic app-control surface cannot
/// derive from the widget itself. Attach alongside [`BusWidget`] where it
/// helps an agent (units, enum choices); absent fields are simply omitted
/// from `list`. Lives here (not `app_control`) because feature-independent
/// spawners attach it — the mixer board builds without the `bus` feature.
#[derive(Component, Clone, Debug, Default, PartialEq, Eq)]
pub struct ControlMeta {
    pub unit: Option<String>,
    pub choices: Vec<String>,
    /// For action buttons: the fixed effect a press dispatches, in
    /// `path=value` form (e.g. `transport.state=playing`), so an agent can
    /// pick the right button without pressing them all.
    pub action: Option<String>,
    /// Overrides the marker-derived contract `kind` when the widget class
    /// undersells the semantics (the transport scrubber is an `hfader` to
    /// the widget layer but a `scrubber` to an agent).
    pub kind: Option<String>,
}

impl ControlMeta {
    pub fn unit(unit: impl Into<String>) -> Self {
        Self {
            unit: Some(unit.into()),
            ..Self::default()
        }
    }

    pub fn action(effect: impl Into<String>) -> Self {
        Self {
            action: Some(effect.into()),
            ..Self::default()
        }
    }
}

impl BusWidget {
    pub fn writable(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            queryable: true,
            writable: true,
        }
    }

    pub fn read_only(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            queryable: true,
            writable: false,
        }
    }
}

/// A point on a monotonic domain-value mapping.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MappingPoint {
    pub position: f32,
    pub value: f32,
}

/// Maps a domain value to the headless slider's normalised travel.
#[derive(Component, Clone, Debug, PartialEq)]
pub enum ValueMapping {
    Linear { min: f32, max: f32 },
    Piecewise(Vec<MappingPoint>),
}

impl ValueMapping {
    pub fn linear(min: f32, max: f32) -> Result<Self, MappingError> {
        if !min.is_finite() || !max.is_finite() || min >= max {
            return Err(MappingError::InvalidRange);
        }
        Ok(Self::Linear { min, max })
    }

    pub fn piecewise(points: impl IntoIterator<Item = (f32, f32)>) -> Result<Self, MappingError> {
        let points: Vec<_> = points
            .into_iter()
            .map(|(position, value)| MappingPoint { position, value })
            .collect();
        if points.len() < 2 {
            return Err(MappingError::TooFewPoints);
        }
        if points.first().map(|p| p.position) != Some(0.0)
            || points.last().map(|p| p.position) != Some(1.0)
        {
            return Err(MappingError::MissingEndpoints);
        }
        if points
            .iter()
            .any(|p| !p.position.is_finite() || !p.value.is_finite())
        {
            return Err(MappingError::NonFinite);
        }
        if points
            .windows(2)
            .any(|pair| pair[0].position >= pair[1].position || pair[0].value >= pair[1].value)
        {
            return Err(MappingError::NotStrictlyIncreasing);
        }
        Ok(Self::Piecewise(points))
    }

    pub fn min(&self) -> f32 {
        match self {
            Self::Linear { min, .. } => *min,
            Self::Piecewise(points) => points[0].value,
        }
    }

    pub fn max(&self) -> f32 {
        match self {
            Self::Linear { max, .. } => *max,
            Self::Piecewise(points) => points[points.len() - 1].value,
        }
    }

    pub fn to_position(&self, value: f32) -> f32 {
        match self {
            Self::Linear { min, max } => {
                ((value.clamp(*min, *max) - min) / (max - min)).clamp(0.0, 1.0)
            }
            Self::Piecewise(points) => interpolate_by_value(points, value),
        }
    }

    pub fn to_value(&self, position: f32) -> f32 {
        match self {
            Self::Linear { min, max } => min + position.clamp(0.0, 1.0) * (max - min),
            Self::Piecewise(points) => interpolate_by_position(points, position),
        }
    }
}

fn interpolate_by_position(points: &[MappingPoint], position: f32) -> f32 {
    let position = position.clamp(0.0, 1.0);
    let pair = points
        .windows(2)
        .find(|pair| position <= pair[1].position)
        .unwrap_or_else(|| &points[points.len() - 2..]);
    let t = (position - pair[0].position) / (pair[1].position - pair[0].position);
    pair[0].value + t * (pair[1].value - pair[0].value)
}

fn interpolate_by_value(points: &[MappingPoint], value: f32) -> f32 {
    let value = value.clamp(points[0].value, points[points.len() - 1].value);
    let pair = points
        .windows(2)
        .find(|pair| value <= pair[1].value)
        .unwrap_or_else(|| &points[points.len() - 2..]);
    let t = (value - pair[0].value) / (pair[1].value - pair[0].value);
    pair[0].position + t * (pair[1].position - pair[0].position)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MappingError {
    InvalidRange,
    TooFewPoints,
    MissingEndpoints,
    NonFinite,
    NotStrictlyIncreasing,
}

impl core::fmt::Display for MappingError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "invalid value mapping: {self:?}")
    }
}

impl std::error::Error for MappingError {}

/// Domain range and quantisation independent of visual travel.
#[derive(Component, Clone, Copy, Debug, PartialEq)]
pub struct ControlRange {
    pub min: f32,
    pub max: f32,
    pub step: f32,
    pub detent: Option<f32>,
}

impl ControlRange {
    pub fn canonicalise(self, value: f32) -> f32 {
        let mut value = value.clamp(self.min, self.max);
        if self.step > 0.0 {
            value = self.min + ((value - self.min) / self.step).round() * self.step;
        }
        if let Some(detent) = self.detent {
            if (value - detent).abs() <= self.step.max((self.max - self.min) / 200.0) {
                value = detent;
            }
        }
        if value == 0.0 {
            0.0
        } else {
            value.clamp(self.min, self.max)
        }
    }
}

/// Canonical domain value. The view reads this; local and remote sources write it.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct ControlValue(pub f32);

/// Domain-level user edit emitted by CTK controls.
#[derive(EntityEvent, Clone, Copy, Debug, PartialEq)]
pub struct ControlChange {
    #[event_target]
    pub source: bevy::ecs::entity::Entity,
    pub value: f32,
    pub is_final: bool,
}

/// A local manipulation ended without a semantic commit.
///
/// Pointer cancellation is distinct from release: consumers should discard
/// gesture ownership and restore their last authoritative value, not write the
/// partially manipulated value.
#[derive(EntityEvent, Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControlGestureCancel {
    #[event_target]
    pub source: bevy::ecs::entity::Entity,
}

/// Present while a local pointer gesture is manipulating the control
/// (inserted on non-final slider changes, removed on the final one). The
/// app-control surface refuses remote writes to an actively-dragged control.
#[derive(Component)]
pub(crate) struct ActiveControlGesture;

/// Programmatic value update. This changes state without emitting [`ControlChange`].
#[derive(EntityEvent, Clone, Copy, Debug, PartialEq)]
pub struct SetControlValue {
    #[event_target]
    pub source: bevy::ecs::entity::Entity,
    pub value: f32,
}

/// Programmatic toggle update. This changes state without emitting [`ControlChange`].
#[derive(EntityEvent, Clone, Copy, Debug, PartialEq)]
pub struct SetToggleValue {
    #[event_target]
    pub source: Entity,
    pub value: bool,
}

#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Fader;

#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Knob;

#[derive(Component, Clone, Copy, Debug, Default)]
pub struct ToggleButton;

#[derive(Component, Clone, Copy, Debug, Default)]
pub struct LevelMeter;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MeterLane {
    pub level: f32,
    pub peak: f32,
    pub hold: f32,
    pub clipped: bool,
}

/// Generic one/two-lane read-only meter state, normalised to `0..=1`.
#[derive(Component, Clone, Copy, Debug, PartialEq)]
pub struct MeterValue {
    pub lanes: [MeterLane; 2],
    pub lane_count: u8,
}

impl Default for MeterValue {
    fn default() -> Self {
        Self {
            lanes: [MeterLane::default(); 2],
            lane_count: 2,
        }
    }
}

#[derive(Component)]
struct FaderFill;

#[derive(Component)]
struct FaderThumb;

/// Marks a [`Fader`] laid out horizontally (the transport scrubber). The
/// vertical `update_fader_visuals` and track-click path branch on this so one
/// value pipeline drives both orientations.
#[derive(Component)]
pub struct FaderHorizontal;

#[derive(Component)]
struct FaderFillH;

#[derive(Component)]
struct FaderThumbH;

/// Marks a momentary [`action_button`] so its [`Activate`] observer knows to
/// emit a one-shot final [`ControlChange`].
#[derive(Component)]
pub struct ActionButton;

/// Monotonic order assigned to focused keyboard inputs handled by CTK or
/// allowed to bubble into an application action resolver.
///
/// The counter resets immediately before focused-input dispatch. Applications
/// use the order to retain effects before a modal-opening shortcut while
/// rejecting focused-control effects later in the same input batch.
#[derive(Resource, Default)]
pub struct KeyboardInputOrder(u64);

impl KeyboardInputOrder {
    /// Allocate the next input position in the current dispatched batch.
    pub fn next_order(&mut self) -> u64 {
        self.0 = self.0.saturating_add(1);
        self.0
    }

    fn reset(&mut self) {
        self.0 = 0;
    }
}

#[derive(Clone, Copy)]
enum DeferredKeyboardControlKind {
    Activate,
    Toggle,
    SliderKey(KeyCode),
}

#[derive(Clone, Copy)]
struct DeferredKeyboardControl {
    order: u64,
    entity: Entity,
    kind: DeferredKeyboardControlKind,
}

const MAX_DEFERRED_KEYBOARD_CONTROLS: usize = 64;

/// Focused CTK control keys waiting for Update-stage arbitration.
///
/// Pointer effects remain immediate. Keyboard effects are held until
/// [`KeyboardControlSystems`], allowing an application action router to discard
/// only controls which occurred after an accepted modal-opening shortcut.
/// Consequently, a focused key's visible value change is delayed until this
/// Update-stage set, at most one frame after input dispatch. The queue is
/// bounded to 64 effects; overflow drops the oldest effect and logs a warning.
#[derive(Resource, Default)]
pub struct KeyboardControlQueue {
    pending: VecDeque<DeferredKeyboardControl>,
    pointer_sliders: std::collections::HashSet<Entity>,
    expected_slider_key_changes: HashMap<Entity, usize>,
}

impl KeyboardControlQueue {
    /// Discard focused-control inputs strictly later than `order`.
    pub fn discard_after(&mut self, order: u64) {
        self.pending.retain(|event| event.order <= order);
    }

    /// Discard every deferred focused-control input.
    pub fn clear(&mut self) {
        self.pending.clear();
        self.pointer_sliders.clear();
        self.expected_slider_key_changes.clear();
    }

    fn begin_frame(&mut self) {
        self.pointer_sliders.clear();
        self.expected_slider_key_changes.clear();
    }

    fn claim_pointer_slider(&mut self, entity: Entity) {
        self.pointer_sliders.insert(entity);
        self.pending.retain(|effect| {
            effect.entity != entity
                || !matches!(effect.kind, DeferredKeyboardControlKind::SliderKey(_))
        });
    }

    fn expect_slider_key_change(&mut self, entity: Entity) {
        *self.expected_slider_key_changes.entry(entity).or_default() += 1;
    }

    fn consume_slider_key_change(&mut self, entity: Entity) -> bool {
        let Some(remaining) = self.expected_slider_key_changes.get_mut(&entity) else {
            return false;
        };
        *remaining -= 1;
        if *remaining == 0 {
            self.expected_slider_key_changes.remove(&entity);
        }
        true
    }

    fn push(&mut self, order: u64, entity: Entity, kind: DeferredKeyboardControlKind) {
        if matches!(kind, DeferredKeyboardControlKind::SliderKey(_))
            && self.pointer_sliders.contains(&entity)
        {
            return;
        }
        if self.pending.len() == MAX_DEFERRED_KEYBOARD_CONTROLS {
            self.pending.pop_front();
            bevy::log::warn!(
                "CTK keyboard-control queue full; dropped oldest effect (cap {MAX_DEFERRED_KEYBOARD_CONTROLS})"
            );
        }
        self.pending.push_back(DeferredKeyboardControl {
            order,
            entity,
            kind,
        });
    }
}

/// Applies focused CTK control keys after application action routing.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct KeyboardControlSystems;

/// Horizontal inset inside the fader root. MUST stay 0: Bevy's upstream
/// slider DRAG observer maps over the unpadded `width − thumb` extent, and
/// the click mapping, spawn markup, and visual updates must all share that
/// exact extent — reintroducing padding here desynchronises dragging from
/// the rendered thumb.
const HFADER_PAD: f32 = 0.0;

/// Horizontal fader geometry captured at spawn. The width is a FALLBACK for
/// the first frames before layout runs; [`update_hfader_visuals`] prefers the
/// live [`ComputedNode`] width so flex shrink cannot desynchronise the visuals
/// from the pointer mapping.
#[derive(Component, Clone, Copy)]
struct HFaderGeometry {
    width: f32,
    thumb: f32,
}

/// Marks the horizontal fader's background track so it can follow the live
/// node width alongside the fill and thumb.
#[derive(Component)]
struct FaderTrackH;

#[derive(Component)]
struct KnobIndicator;

#[derive(Component, Clone, Copy)]
struct MeterLaneRoot(usize);

#[derive(Component, Clone, Copy)]
struct MeterFill(usize);

#[derive(Component, Clone, Copy)]
struct MeterPeak(usize);

#[derive(Component, Clone, Copy)]
struct MeterHold(usize);

/// Construction properties shared by faders and knobs.
#[derive(Clone, Debug)]
pub struct NumericControlProps {
    pub id: String,
    pub value: f32,
    pub range: ControlRange,
    pub mapping: ValueMapping,
}

impl NumericControlProps {
    pub fn new(
        id: impl Into<String>,
        value: f32,
        range: ControlRange,
        mapping: ValueMapping,
    ) -> Self {
        Self {
            id: id.into(),
            value: range.canonicalise(value),
            range,
            mapping,
        }
    }
}

/// A complete generic vertical fader bundle at the default 42×250 size.
pub fn fader(props: NumericControlProps) -> impl Bundle {
    fader_sized(props, 42.0, 250.0)
}

/// A vertical fader sized to `width_px` × `height_px`. The decoration scales
/// proportionally from the 42×250 baseline: the thumb spans `width-8` (min 10)
/// and its cap height tracks the fader height, while the coloured travel bar is
/// centred at `~24%` of the width. Every component and observer contract is
/// identical to [`fader`] — only the pixel geometry moves.
pub fn fader_sized(props: NumericControlProps, width_px: f32, height_px: f32) -> impl Bundle {
    let position = props.mapping.to_position(props.value);
    let keyboard_step = position_step(&props.mapping, props.range, props.value);
    let thumb_w = (width_px - 8.0).max(10.0);
    let thumb_h = (13.0 * height_px / 250.0).max(8.0);
    let track_w = (width_px * 10.0 / 42.0).max(3.0);
    let track_left = (width_px - track_w) / 2.0;
    let thumb_left = (width_px - thumb_w) / 2.0;
    (
        Fader,
        BusWidget::writable(props.id),
        ControlValue(props.value),
        props.range,
        props.mapping,
        Slider {
            orientation: SliderOrientation::Vertical,
            track_click: TrackClick::Snap,
        },
        SliderValue(position),
        SliderRange::new(0.0, 1.0),
        SliderStep(keyboard_step),
        TabIndex(0),
        Hovered::default(),
        Node {
            width: px(width_px),
            height: px(height_px),
            position_type: PositionType::Relative,
            justify_content: JustifyContent::Center,
            align_items: AlignItems::Center,
            padding: UiRect::vertical(px(8)),
            border_radius: BorderRadius::all(px(5)),
            ..default()
        },
        ThemeBackgroundColor(crate::theme::tokens::PANEL),
        Children::spawn((
            bevy::ecs::spawn::Spawn((
                Node {
                    position_type: PositionType::Absolute,
                    left: px(track_left),
                    bottom: px(8),
                    width: px(track_w),
                    height: percent(94),
                    border_radius: BorderRadius::all(px(4)),
                    ..default()
                },
                Pickable::IGNORE,
                ThemeBackgroundColor(crate::theme::tokens::TRACK),
            )),
            bevy::ecs::spawn::Spawn((
                FaderFill,
                Node {
                    position_type: PositionType::Absolute,
                    left: px(track_left),
                    bottom: px(8),
                    width: px(track_w),
                    height: percent(position * 94.0),
                    border_radius: BorderRadius::all(px(4)),
                    ..default()
                },
                Pickable::IGNORE,
                ThemeBackgroundColor(crate::theme::tokens::CONTROL_ACTIVE),
            )),
            bevy::ecs::spawn::Spawn((
                FaderThumb,
                SliderThumb,
                Node {
                    position_type: PositionType::Absolute,
                    left: px(thumb_left),
                    bottom: percent(position * 94.0),
                    width: px(thumb_w),
                    height: px(thumb_h),
                    border_radius: BorderRadius::all(px(3)),
                    ..default()
                },
                Pickable::default(),
                ThemeBackgroundColor(crate::theme::tokens::THUMB),
            )),
        )),
    )
}

/// A horizontal fader (transport scrubber) sized to `width_px` × `height_px`.
///
/// Shares the [`Fader`] value pipeline — drag emits [`ControlChange`] exactly
/// like the vertical fader — but carries [`FaderHorizontal`] so the visual
/// systems ([`update_hfader_visuals`], the horizontal track-click branch of
/// [`on_fader_click`]) drive the fill *width* and thumb *left* instead of the
/// vertical height/bottom. The fill and thumb use dedicated [`FaderFillH`] /
/// [`FaderThumbH`] markers so the vertical `update_fader_visuals` skips them.
pub fn hfader_sized(props: NumericControlProps, width_px: f32, height_px: f32) -> impl Bundle {
    let position = props.mapping.to_position(props.value);
    let keyboard_step = position_step(&props.mapping, props.range, props.value);
    let track_h = (height_px * 0.4).max(4.0);
    let track_bottom = (height_px - track_h) / 2.0;
    let thumb_w = (13.0 * width_px / 900.0).clamp(8.0, 18.0);
    let thumb_h = (height_px - 8.0).max(8.0);
    (
        (
            Fader,
            FaderHorizontal,
            HFaderGeometry {
                width: width_px,
                thumb: thumb_w,
            },
        ),
        BusWidget::writable(props.id),
        ControlValue(props.value),
        props.range,
        props.mapping,
        Slider {
            orientation: SliderOrientation::Horizontal,
            track_click: TrackClick::Snap,
        },
        SliderValue(position),
        SliderRange::new(0.0, 1.0),
        SliderStep(keyboard_step),
        TabIndex(0),
        Hovered::default(),
        Node {
            width: px(width_px),
            height: px(height_px),
            position_type: PositionType::Relative,
            justify_content: JustifyContent::Center,
            align_items: AlignItems::Center,
            border_radius: BorderRadius::all(px(5)),
            ..default()
        },
        ThemeBackgroundColor(crate::theme::tokens::PANEL),
        Children::spawn((
            bevy::ecs::spawn::Spawn((
                FaderTrackH,
                Node {
                    position_type: PositionType::Absolute,
                    left: px(HFADER_PAD),
                    bottom: px(track_bottom),
                    width: px(width_px - 2.0 * HFADER_PAD),
                    height: px(track_h),
                    border_radius: BorderRadius::all(px(4)),
                    ..default()
                },
                Pickable::IGNORE,
                ThemeBackgroundColor(crate::theme::tokens::TRACK),
            )),
            bevy::ecs::spawn::Spawn((
                FaderFillH,
                Node {
                    position_type: PositionType::Absolute,
                    left: px(HFADER_PAD),
                    bottom: px(track_bottom),
                    width: px(position * (width_px - 2.0 * HFADER_PAD - thumb_w) + thumb_w / 2.0),
                    height: px(track_h),
                    border_radius: BorderRadius::all(px(4)),
                    ..default()
                },
                Pickable::IGNORE,
                ThemeBackgroundColor(crate::theme::tokens::CONTROL_ACTIVE),
            )),
            bevy::ecs::spawn::Spawn((
                FaderThumbH,
                SliderThumb,
                Node {
                    position_type: PositionType::Absolute,
                    left: px(HFADER_PAD + position * (width_px - 2.0 * HFADER_PAD - thumb_w)),
                    bottom: px(4),
                    width: px(thumb_w),
                    height: px(thumb_h),
                    border_radius: BorderRadius::all(px(3)),
                    ..default()
                },
                Pickable::default(),
                ThemeBackgroundColor(crate::theme::tokens::THUMB),
            )),
        )),
    )
}

/// A complete generic rotary control at the default 58px size.
pub fn knob(props: NumericControlProps) -> impl Bundle {
    knob_sized(props, 58.0)
}

/// A rotary control with a `size_px` diameter. The pointer indicator scales
/// proportionally from the 58px baseline; every component and observer contract
/// matches [`knob`].
pub fn knob_sized(props: NumericControlProps, size_px: f32) -> impl Bundle {
    let position = props.mapping.to_position(props.value);
    let keyboard_step = position_step(&props.mapping, props.range, props.value);
    let scale = size_px / 58.0;
    (
        Knob,
        BusWidget::writable(props.id),
        ControlValue(props.value),
        props.range,
        props.mapping,
        Slider {
            orientation: SliderOrientation::Vertical,
            track_click: TrackClick::Drag,
        },
        SliderValue(position),
        SliderRange::new(0.0, 1.0),
        SliderStep(keyboard_step),
        TabIndex(0),
        Hovered::default(),
        Node {
            width: px(size_px),
            height: px(size_px),
            position_type: PositionType::Relative,
            justify_content: JustifyContent::Center,
            align_items: AlignItems::Center,
            border_radius: BorderRadius::MAX,
            ..default()
        },
        ThemeBackgroundColor(crate::theme::tokens::CONTROL),
        // The rotating part is a full-size transparent ARM whose center
        // coincides with the knob center — UiTransform rotates a node about
        // its own center, so rotating the visible notch directly would pivot
        // it around the notch's own middle, not the knob's. The notch rides
        // near the arm's top edge and sweeps a true circle.
        Children::spawn(bevy::ecs::spawn::Spawn((
            KnobIndicator,
            Node {
                position_type: PositionType::Absolute,
                top: px(0),
                left: px(0),
                width: percent(100),
                height: percent(100),
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::Center,
                ..default()
            },
            Pickable::IGNORE,
            UiTransform::from_rotation(Rot2::radians(-0.75 * PI + position * 1.5 * PI)),
            Children::spawn(bevy::ecs::spawn::Spawn((
                Node {
                    margin: UiRect::top(px(5.0 * scale)),
                    width: px(3.0 * scale),
                    height: px(20.0 * scale),
                    border_radius: BorderRadius::all(px(2)),
                    ..default()
                },
                Pickable::IGNORE,
                ThemeBackgroundColor(crate::theme::tokens::CONTROL_ACTIVE),
            ))),
        ))),
    )
}

/// Read-only one/two-lane meter bundle at the default 28×250 size.
pub fn level_meter(id: impl Into<String>, value: MeterValue) -> impl Bundle {
    level_meter_sized(id, value, 28.0, 250.0)
}

/// A read-only meter sized to `width_px` × `height_px`. Lane width, gap and
/// padding scale proportionally from the 28px baseline; the peak/hold marker
/// thicknesses stay crisp at 1/2px. The [`MeterValue`] contract is unchanged.
pub fn level_meter_sized(
    id: impl Into<String>,
    value: MeterValue,
    width_px: f32,
    height_px: f32,
) -> impl Bundle {
    let pad = (3.0 * width_px / 28.0).max(1.0);
    let gap = (3.0 * width_px / 28.0).max(1.0);
    let lane_w = (9.0 * width_px / 28.0).max(3.0);
    (
        LevelMeter,
        BusWidget::read_only(id),
        value,
        Node {
            display: bevy::ui::Display::Flex,
            flex_direction: FlexDirection::Row,
            column_gap: px(gap),
            width: px(width_px),
            height: px(height_px),
            padding: UiRect::all(px(pad)),
            align_items: AlignItems::End,
            ..default()
        },
        ThemeBackgroundColor(crate::theme::tokens::TRACK),
        Children::spawn((
            bevy::ecs::spawn::Spawn((
                MeterLaneRoot(0),
                Node {
                    display: meter_lane_display(value.lane_count, 0),
                    position_type: PositionType::Relative,
                    width: px(lane_w),
                    height: percent(100),
                    ..default()
                },
                ThemeBackgroundColor(crate::theme::tokens::TRACK),
                Children::spawn((
                    bevy::ecs::spawn::Spawn((
                        MeterFill(0),
                        Node {
                            position_type: PositionType::Absolute,
                            bottom: px(0),
                            width: percent(100),
                            height: percent(value.lanes[0].level * 100.0),
                            ..default()
                        },
                        BackgroundColor::DEFAULT, // dynamic: painted by update_meter_visuals
                    )),
                    bevy::ecs::spawn::Spawn((
                        MeterPeak(0),
                        Node {
                            position_type: PositionType::Absolute,
                            bottom: percent(value.lanes[0].peak * 100.0),
                            width: percent(100),
                            height: px(1),
                            ..default()
                        },
                        BackgroundColor::DEFAULT,
                    )),
                    bevy::ecs::spawn::Spawn((
                        MeterHold(0),
                        Node {
                            position_type: PositionType::Absolute,
                            bottom: percent(value.lanes[0].hold * 100.0),
                            width: percent(100),
                            height: px(2),
                            ..default()
                        },
                        BackgroundColor::DEFAULT,
                    )),
                )),
            )),
            bevy::ecs::spawn::Spawn((
                MeterLaneRoot(1),
                Node {
                    display: meter_lane_display(value.lane_count, 1),
                    position_type: PositionType::Relative,
                    width: px(lane_w),
                    height: percent(100),
                    ..default()
                },
                ThemeBackgroundColor(crate::theme::tokens::TRACK),
                Children::spawn((
                    bevy::ecs::spawn::Spawn((
                        MeterFill(1),
                        Node {
                            position_type: PositionType::Absolute,
                            bottom: px(0),
                            width: percent(100),
                            height: percent(value.lanes[1].level * 100.0),
                            ..default()
                        },
                        BackgroundColor::DEFAULT,
                    )),
                    bevy::ecs::spawn::Spawn((
                        MeterPeak(1),
                        Node {
                            position_type: PositionType::Absolute,
                            bottom: percent(value.lanes[1].peak * 100.0),
                            width: percent(100),
                            height: px(1),
                            ..default()
                        },
                        BackgroundColor::DEFAULT,
                    )),
                    bevy::ecs::spawn::Spawn((
                        MeterHold(1),
                        Node {
                            position_type: PositionType::Absolute,
                            bottom: percent(value.lanes[1].hold * 100.0),
                            width: percent(100),
                            height: px(2),
                            ..default()
                        },
                        BackgroundColor::DEFAULT,
                    )),
                )),
            )),
        )),
    )
}

/// Toggle control at the default 48×26 size.
pub fn toggle_button(id: impl Into<String>) -> impl Bundle {
    toggle_button_sized(id, 48.0, 26.0)
}

/// A toggle sized to `min_width_px` × `height_px`. Horizontal padding scales
/// from the 48px baseline so single-letter compact toggles stay legible; the
/// checked-state contract and [`update_toggle_style`] colours are unchanged.
/// CTK owns both pointer and focused-key activation; callers should not attach
/// Bevy's `Checkbox` marker, which would add a second input path.
/// Pointer activation commits on press, matching `ActivateOnPress`; dragging
/// away after pressing does not roll the toggle back.
pub fn toggle_button_sized(
    id: impl Into<String>,
    min_width_px: f32,
    height_px: f32,
) -> impl Bundle {
    let pad_h = (min_width_px * 8.0 / 48.0).max(2.0);
    (
        ToggleButton,
        BusWidget::writable(id),
        Checkable,
        AccessibilityNode::from(accesskit::Node::new(Role::Switch)),
        TabIndex(0),
        Hovered::default(),
        Node {
            min_width: px(min_width_px),
            height: px(height_px),
            padding: UiRect::horizontal(px(pad_h)),
            justify_content: JustifyContent::Center,
            align_items: AlignItems::Center,
            border_radius: BorderRadius::all(px(4)),
            ..default()
        },
        BackgroundColor::DEFAULT, // dynamic: painted by update_toggle_style
    )
}

/// A momentary action button (transport Play/Stop/RTZ). Unlike [`toggle_button`]
/// it holds no state: a click emits one final [`ControlChange`] with value `0.0`
/// via [`on_action_button_activate`], and the caller's binding decides what that
/// commits (e.g. an enum write for `transport.state`, a `0.0` seek for RTZ).
/// CTK owns both pointer and focused-key activation; callers should not attach
/// Bevy's `Button` marker, which would add a second input path.
pub fn action_button(id: impl Into<String>, min_width_px: f32, height_px: f32) -> impl Bundle {
    let pad_h = (min_width_px * 8.0 / 48.0).max(2.0);
    (
        ActionButton,
        BusWidget::writable(id),
        AccessibilityNode::from(accesskit::Node::new(Role::Button)),
        TabIndex(0),
        Hovered::default(),
        Node {
            min_width: px(min_width_px),
            height: px(height_px),
            padding: UiRect::horizontal(px(pad_h)),
            justify_content: JustifyContent::Center,
            align_items: AlignItems::Center,
            border_radius: BorderRadius::all(px(4)),
            ..default()
        },
        ThemeBackgroundColor(crate::theme::tokens::CONTROL),
    )
}

pub struct CtkWidgetsPlugin;

impl Plugin for CtkWidgetsPlugin {
    fn build(&self, app: &mut App) {
        // UiTheme normally arrives via FeathersPlugins; init it here too so a
        // headless/test app can run the dynamic repaint systems (an empty
        // theme resolves missing tokens to feathers' loud error colour). Apps
        // install CtkThemePlugin and call apply_theme for the real palette.
        app.init_resource::<UiTheme>()
            .init_resource::<crate::theme::CtkThemeMetrics>()
            .init_resource::<crate::theme::CtkTypography>()
            .init_resource::<InputFocus>()
            .init_resource::<InputFocusVisible>()
            .init_resource::<crate::button::ButtonDiagnostics>()
            .init_resource::<KeyboardInputOrder>()
            .init_resource::<KeyboardControlQueue>()
            .add_observer(on_slider_value_change)
            .add_observer(on_final_control_change)
            .add_observer(on_control_key_input)
            .add_observer(on_ctk_control_pointer_press)
            .add_observer(on_ctk_control_pointer_release)
            .add_observer(on_ctk_control_pointer_cancel)
            .add_observer(on_ctk_control_pointer_drag_end)
            .add_observer(on_ctk_control_click)
            .add_observer(on_control_thumb_press)
            .add_observer(on_pointer_cancel)
            .add_observer(on_control_disabled)
            .add_observer(on_fader_click)
            .add_observer(on_toggle_value_change)
            .add_observer(on_action_button_activate)
            .add_observer(on_set_control_value)
            .add_observer(on_set_toggle_value)
            .add_observer(crate::button::paint_added_button)
            .add_observer(crate::button::paint_added_button_label)
            .add_observer(crate::button::paint_disabled_button)
            .add_observer(crate::button::paint_enabled_button)
            .add_systems(First, begin_keyboard_control_frame)
            .add_systems(
                PreUpdate,
                begin_keyboard_dispatch
                    .after(InputSystems)
                    .before(InputFocusSystems::Dispatch),
            )
            .add_systems(
                Update,
                apply_deferred_keyboard_controls.in_set(KeyboardControlSystems),
            )
            .add_systems(
                Update,
                (
                    update_fader_visuals,
                    update_hfader_visuals,
                    update_knob_visuals,
                    update_meter_visuals,
                    update_toggle_style,
                    crate::button::update_button_style,
                    crate::button::update_button_metrics,
                    update_numeric_accessibility,
                ),
            );
        app.add_systems(Update, crate::button::warn_button_marker_collisions);
        #[cfg(feature = "icons")]
        app.add_observer(crate::button::materialise_added_button_label)
            .add_systems(
                Update,
                (
                    crate::button::spawn_pending_button_labels,
                    crate::button::update_button_icon_style,
                )
                    .chain(),
            );
    }
}

fn on_final_control_change(
    change: On<ControlChange>,
    active: Query<(), With<ActiveControlGesture>>,
    mut commands: Commands,
) {
    if change.is_final && active.contains(change.source) {
        commands
            .entity(change.source)
            .remove::<ActiveControlGesture>();
    }
}

fn begin_keyboard_control_frame(mut queue: ResMut<KeyboardControlQueue>) {
    queue.begin_frame();
}

fn begin_keyboard_dispatch(mut order: ResMut<KeyboardInputOrder>) {
    order.reset();
}

fn position_step(mapping: &ValueMapping, range: ControlRange, value: f32) -> f32 {
    if range.step <= 0.0 {
        return 0.01;
    }
    if matches!(mapping, ValueMapping::Piecewise(_)) {
        // A single normalised step cannot represent both directions of a
        // non-linear mapping. CTK performs domain-space keyboard stepping;
        // the component remains present so Bevy's pointer observer can run.
        return 0.0;
    }
    let value = range.canonicalise(value);
    let lower = range.canonicalise(value - range.step);
    let upper = range.canonicalise(value + range.step);
    let position = mapping.to_position(value);
    let lower_delta = (position - mapping.to_position(lower)).abs();
    let upper_delta = (mapping.to_position(upper) - position).abs();
    lower_delta.max(upper_delta).max(0.0001)
}

fn vertical_track_position(node_height: f32, thumb_size: f32, normalised_y: f32) -> Option<f32> {
    let track_size = node_height - thumb_size;
    if track_size <= 0.0 {
        return None;
    }
    let y_from_bottom = (0.5 - normalised_y) * node_height;
    Some(((y_from_bottom - thumb_size / 2.0) / track_size).clamp(0.0, 1.0))
}

/// The horizontal mirror of [`vertical_track_position`]: left→0, right→1. The
/// scrubber travels along X, so its track-click seek reads the pointer's X.
/// Shares the padded thumb-aware extent with the spawn markup and
/// [`update_hfader_visuals`]: `travel = width − 2·HFADER_PAD − thumb`.
fn horizontal_track_position(node_width: f32, thumb_size: f32, normalised_x: f32) -> Option<f32> {
    let travel = node_width - 2.0 * HFADER_PAD - thumb_size;
    if travel <= 0.0 {
        return None;
    }
    let x_from_left = (normalised_x + 0.5) * node_width;
    Some(((x_from_left - HFADER_PAD - thumb_size / 2.0) / travel).clamp(0.0, 1.0))
}

type CtkClickables<'w, 's> = Query<
    'w,
    's,
    (
        Has<InteractionDisabled>,
        Has<ActionButton>,
        Has<ToggleButton>,
        Has<crate::button::CtkButton>,
        Has<Checked>,
        Has<Pressed>,
    ),
    Or<(
        With<ActionButton>,
        With<ToggleButton>,
        With<crate::button::CtkButton>,
    )>,
>;

type CtkActions<'w, 's> = Query<
    'w,
    's,
    Has<InteractionDisabled>,
    Or<(With<ActionButton>, With<crate::button::CtkButton>)>,
>;

type ActivatableButtons<'w, 's> = Query<
    'w,
    's,
    (
        Has<ActionButton>,
        Has<crate::button::CtkButton>,
        Has<InteractionDisabled>,
    ),
    Or<(With<ActionButton>, With<crate::button::CtkButton>)>,
>;

fn on_ctk_control_pointer_press(
    mut press: On<Pointer<Press>>,
    controls: CtkClickables,
    focus: Option<ResMut<InputFocus>>,
    focus_visible: Option<ResMut<InputFocusVisible>>,
    mut commands: Commands,
) {
    let Ok((disabled, _, toggle, ctk_button, checked, pressed)) = controls.get(press.entity) else {
        return;
    };
    press.propagate(false);
    if let Some(mut focus) = focus {
        focus.set(press.entity, FocusCause::Pressed);
    }
    if let Some(mut focus_visible) = focus_visible {
        focus_visible.0 = false;
    }
    if disabled || pressed {
        return;
    }
    commands.entity(press.entity).insert(Pressed);
    if ctk_button {
        commands.trigger(Activate {
            entity: press.entity,
        });
    } else if toggle {
        // ToggleButton deliberately preserves the old ActivateOnPress
        // contract: pointer-down commits even if the pointer later drags off.
        commands.trigger(ValueChange {
            source: press.entity,
            value: !checked,
            is_final: true,
        });
    }
}

fn on_ctk_control_pointer_release(
    mut release: On<Pointer<Release>>,
    controls: CtkClickables,
    mut commands: Commands,
) {
    if controls.contains(release.entity) {
        release.propagate(false);
        commands.entity(release.entity).remove::<Pressed>();
    }
}

fn on_ctk_control_pointer_cancel(
    mut cancel: On<Pointer<Cancel>>,
    controls: CtkClickables,
    mut commands: Commands,
) {
    if controls.contains(cancel.entity) {
        cancel.propagate(false);
        commands.entity(cancel.entity).remove::<Pressed>();
    }
}

fn on_ctk_control_pointer_drag_end(
    mut drag_end: On<Pointer<DragEnd>>,
    controls: CtkClickables,
    mut commands: Commands,
) {
    if controls.contains(drag_end.entity) {
        drag_end.propagate(false);
        commands.entity(drag_end.entity).remove::<Pressed>();
    }
}

fn on_ctk_control_click(
    mut click: On<Pointer<Click>>,
    controls: CtkClickables,
    mut commands: Commands,
) {
    let Ok((disabled, action, _, ctk_button, _, pressed)) = controls.get(click.entity) else {
        return;
    };
    // Click is owned even when disabled so it cannot activate a clickable
    // ancestor. ToggleButton already committed on Press.
    click.propagate(false);
    if ctk_button {
        return;
    }
    if action && !disabled && pressed {
        commands.trigger(Activate {
            entity: click.entity,
        });
    }
}

#[allow(clippy::type_complexity)] // Hit testing needs Bevy's complete computed UI tuple.
fn on_fader_click(
    click: On<Pointer<Click>>,
    faders: Query<
        (
            &ControlValue,
            &ControlRange,
            &ValueMapping,
            &SliderDragState,
            Has<InteractionDisabled>,
            Has<FaderHorizontal>,
            &ComputedNode,
            &ComputedUiRenderTargetInfo,
            &UiGlobalTransform,
        ),
        With<Fader>,
    >,
    thumbs: Query<&ComputedNode, With<SliderThumb>>,
    children: Query<&Children>,
    ui_scale: Res<UiScale>,
    mut keyboard: ResMut<KeyboardControlQueue>,
    mut commands: Commands,
) {
    let thumb_click = thumbs.contains(click.original_event_target());
    let Ok((current, range, mapping, drag, disabled, horizontal, node, target, transform)) =
        faders.get(click.entity)
    else {
        return;
    };
    // Bevy emits Click before DragEnd. DragEnd owns the semantic commit for a
    // drag; Click only supplies the missing final event for a stationary track
    // click. A thumb click bubbles to the root after Bevy uses the pickable
    // thumb to suppress track snapping.
    if fader_click_is_blocked(thumb_click, disabled, drag.dragging) {
        return;
    }
    keyboard.claim_pointer_slider(click.entity);
    let thumb_size = children
        .iter_descendants(click.entity)
        .find_map(|child| {
            thumbs.get(child).ok().map(|thumb| {
                if horizontal {
                    thumb.size().x
                } else {
                    thumb.size().y
                }
            })
        })
        .unwrap_or(0.0);
    let Some(normalised) = node.normalize_point(
        *transform,
        click.pointer_location.position * target.scale_factor() / ui_scale.0,
    ) else {
        return;
    };
    let track = if horizontal {
        horizontal_track_position(node.size().x, thumb_size, normalised.x)
    } else {
        vertical_track_position(node.size().y, thumb_size, normalised.y)
    };
    let value = if let Some(position) = track {
        range.canonicalise(mapping.to_value(position))
    } else {
        current.0
    };
    commands
        .entity(click.entity)
        .insert((ControlValue(value), SliderValue(mapping.to_position(value))));
    commands.trigger(ControlChange {
        source: click.entity,
        value,
        is_final: true,
    });
}

fn fader_click_is_blocked(thumb_click: bool, disabled: bool, dragging: bool) -> bool {
    thumb_click || disabled || dragging
}

fn on_toggle_value_change(
    change: On<ValueChange<bool>>,
    toggles: Query<(), With<ToggleButton>>,
    mut commands: Commands,
) {
    if !toggles.contains(change.source) {
        return;
    }
    // CTK owns this update so a nested authoritative rollback from a
    // ControlChange observer cannot be overwritten afterwards by Bevy's
    // checkbox self-update observer.
    if change.value {
        commands.entity(change.source).insert(Checked);
    } else {
        commands.entity(change.source).remove::<Checked>();
    }
    commands.trigger(ControlChange {
        source: change.source,
        value: if change.value { 1.0 } else { 0.0 },
        is_final: change.is_final,
    });
}

fn on_action_button_activate(
    activate: On<Activate>,
    buttons: Query<Has<InteractionDisabled>, With<ActionButton>>,
    mut commands: Commands,
) {
    // A momentary button carries no persistent value; the single final
    // ControlChange is the whole interaction. The bound writer (an enum write,
    // a 0.0 seek) decides what value 0.0 commits.
    if let Ok(false) = buttons.get(activate.entity) {
        commands.trigger(ControlChange {
            source: activate.entity,
            value: 0.0,
            is_final: true,
        });
    }
}

#[allow(clippy::type_complexity)] // Bevy query filters are expressed in the type system.
fn on_slider_value_change(
    change: On<ValueChange<f32>>,
    controls: Query<(&ValueMapping, &ControlRange), Or<(With<Fader>, With<Knob>)>>,
    mut keyboard: ResMut<KeyboardControlQueue>,
    mut commands: Commands,
) {
    let Ok((mapping, range)) = controls.get(change.source) else {
        return;
    };
    if keyboard.consume_slider_key_change(change.source) {
        // Bevy's Slider observer also translates Left/Right/Home/End into a
        // ValueChange. CTK has already queued the domain-space semantic key
        // effect, so suppress only this nested keyboard-generated duplicate.
        return;
    }
    keyboard.claim_pointer_slider(change.source);
    let value = range.canonicalise(mapping.to_value(change.value));
    let position = mapping.to_position(value);
    commands
        .entity(change.source)
        .insert((ControlValue(value), SliderValue(position)));
    if change.is_final {
        commands
            .entity(change.source)
            .remove::<ActiveControlGesture>();
    } else {
        commands.entity(change.source).insert(ActiveControlGesture);
    }
    commands.trigger(ControlChange {
        source: change.source,
        value,
        is_final: change.is_final,
    });
}

#[allow(clippy::type_complexity)] // Only CTK numeric controls own this press state.
fn on_control_thumb_press(
    press: On<Pointer<Press>>,
    thumbs: Query<&ChildOf, With<SliderThumb>>,
    controls: Query<Has<InteractionDisabled>, Or<(With<Fader>, With<Knob>)>>,
    mut commands: Commands,
) {
    let Ok(parent) = thumbs.get(press.original_event_target()) else {
        return;
    };
    let source = parent.parent();
    if matches!(controls.get(source), Ok(false)) {
        // Upstream deliberately stops a thumb press before it bubbles to the
        // slider, so mirror its track-press ownership on the CTK control. This
        // gives disable/cancel handling state to clear before DragStart.
        commands.entity(source).insert(Pressed);
    }
}

#[allow(clippy::type_complexity)] // CTK control ownership is expressed in the query filter.
fn on_pointer_cancel(
    _cancel: On<Pointer<Cancel>>,
    mut controls: Query<
        (
            Entity,
            &mut SliderDragState,
            Has<ActiveControlGesture>,
            Has<Pressed>,
        ),
        Or<(With<Fader>, With<Knob>)>,
    >,
    mut commands: Commands,
) {
    // Bevy sends Cancel only to the entity currently under the pointer, which
    // may no longer be the control where a drag began. Track active CTK
    // gestures explicitly and cancel them regardless of the event target.
    for (source, mut drag, active, pressed) in &mut controls {
        if !active && !pressed && !drag.dragging {
            continue;
        }
        drag.dragging = false;
        commands
            .entity(source)
            .remove::<ActiveControlGesture>()
            .remove::<Pressed>();
        commands.trigger(ControlGestureCancel { source });
    }
}

#[allow(clippy::type_complexity)] // CTK control ownership is expressed in the query filter.
fn on_control_disabled(
    insert: On<Insert, InteractionDisabled>,
    mut controls: Query<
        (
            &mut SliderDragState,
            Has<ActiveControlGesture>,
            Has<Pressed>,
        ),
        Or<(With<Fader>, With<Knob>)>,
    >,
    mut commands: Commands,
) {
    let Ok((mut drag, active, pressed)) = controls.get_mut(insert.entity) else {
        return;
    };
    if !active && !pressed && !drag.dragging {
        return;
    }
    drag.dragging = false;
    commands
        .entity(insert.entity)
        .remove::<ActiveControlGesture>()
        .remove::<Pressed>();
    commands.trigger(ControlGestureCancel {
        source: insert.entity,
    });
}

fn on_control_key_input(
    mut input: On<FocusedInput<KeyboardInput>>,
    sliders: Query<Has<InteractionDisabled>, With<ValueMapping>>,
    actions: CtkActions,
    toggles: Query<Has<InteractionDisabled>, With<ToggleButton>>,
    mut order: ResMut<KeyboardInputOrder>,
    mut queue: ResMut<KeyboardControlQueue>,
) {
    if input.input.state != ButtonState::Pressed {
        return;
    }
    let entity = input.focused_entity;
    let activation = matches!(input.input.key_code, KeyCode::Enter | KeyCode::Space);
    if let Ok(disabled) = actions.get(entity) {
        if !disabled && activation && !input.input.repeat {
            input.propagate(false);
            let event_order = order.next_order();
            queue.push(event_order, entity, DeferredKeyboardControlKind::Activate);
        }
        return;
    }
    if let Ok(disabled) = toggles.get(entity) {
        if !disabled && activation && !input.input.repeat {
            input.propagate(false);
            let event_order = order.next_order();
            queue.push(event_order, entity, DeferredKeyboardControlKind::Toggle);
        }
        return;
    }
    let Ok(disabled) = sliders.get(entity) else {
        return;
    };
    if disabled {
        return;
    }
    if !matches!(
        input.input.key_code,
        KeyCode::ArrowLeft
            | KeyCode::ArrowRight
            | KeyCode::ArrowDown
            | KeyCode::ArrowUp
            | KeyCode::Home
            | KeyCode::End
    ) {
        return;
    }
    input.propagate(false);
    if matches!(
        input.input.key_code,
        KeyCode::ArrowLeft | KeyCode::ArrowRight | KeyCode::Home | KeyCode::End
    ) {
        queue.expect_slider_key_change(entity);
    }
    let event_order = order.next_order();
    queue.push(
        event_order,
        entity,
        DeferredKeyboardControlKind::SliderKey(input.input.key_code),
    );
}

fn apply_deferred_keyboard_controls(
    mut queue: ResMut<KeyboardControlQueue>,
    mut sliders: Query<(
        &mut ControlValue,
        &SliderValue,
        &ValueMapping,
        &ControlRange,
    )>,
    buttons: ActivatableButtons,
    toggles: Query<Has<Checked>, With<ToggleButton>>,
    mut commands: Commands,
) {
    let mut toggle_states = HashMap::new();
    for event in std::mem::take(&mut queue.pending) {
        match event.kind {
            DeferredKeyboardControlKind::Activate => {
                let Ok((_action, ctk_button, disabled)) = buttons.get(event.entity) else {
                    continue;
                };
                // Canonical buttons re-check disabled state at drain time so
                // bus-less callers never observe a stale Activate. Legacy
                // ActionButton keeps its existence-only drain contract; its
                // downstream observer rejects ControlChange when disabled.
                if ctk_button && disabled {
                    continue;
                }
                commands.trigger(Activate {
                    entity: event.entity,
                });
            }
            DeferredKeyboardControlKind::Toggle => {
                let current = if let Some(current) = toggle_states.get(&event.entity) {
                    *current
                } else {
                    let Ok(current) = toggles.get(event.entity) else {
                        continue;
                    };
                    current
                };
                let value = !current;
                toggle_states.insert(event.entity, value);
                commands.trigger(ValueChange {
                    source: event.entity,
                    value,
                    is_final: true,
                });
            }
            DeferredKeyboardControlKind::SliderKey(key) => {
                let Ok((mut current, _, mapping, range)) = sliders.get_mut(event.entity) else {
                    continue;
                };
                let Some(value) = keyboard_domain_value(current.0, *range, key) else {
                    continue;
                };
                current.0 = value;
                commands
                    .entity(event.entity)
                    .insert(SliderValue(mapping.to_position(value)));
                commands.trigger(ControlChange {
                    source: event.entity,
                    value,
                    is_final: true,
                });
            }
        }
    }
}

fn keyboard_domain_value(current: f32, range: ControlRange, key: KeyCode) -> Option<f32> {
    let step = if range.step > 0.0 {
        range.step
    } else {
        (range.max - range.min) / 100.0
    };
    // Detent capture is useful for continuous pointer motion but would trap a
    // discrete key step at the detent. Quantise keyboard edits without the
    // capture band; an exact step onto the detent still lands there naturally.
    let keyboard_range = ControlRange {
        detent: None,
        ..range
    };
    match key {
        KeyCode::ArrowLeft | KeyCode::ArrowDown => {
            Some(keyboard_range.canonicalise(current - step))
        }
        KeyCode::ArrowRight | KeyCode::ArrowUp => Some(keyboard_range.canonicalise(current + step)),
        KeyCode::Home => Some(range.min),
        KeyCode::End => Some(range.max),
        _ => None,
    }
}

fn on_set_toggle_value(set: On<SetToggleValue>, mut commands: Commands) {
    if set.value {
        commands.entity(set.source).insert(Checked);
    } else {
        commands.entity(set.source).remove::<Checked>();
    }
}

fn on_set_control_value(
    set: On<SetControlValue>,
    controls: Query<(&ValueMapping, &ControlRange)>,
    mut commands: Commands,
) {
    let Ok((mapping, range)) = controls.get(set.source) else {
        return;
    };
    // Inbound Bus/automation values are already authoritative. Clamp for view
    // safety, but do not quantise or apply the local gesture detent.
    let value = set.value.clamp(range.min, range.max);
    commands
        .entity(set.source)
        .insert((ControlValue(value), SliderValue(mapping.to_position(value))));
    if !matches!(mapping, ValueMapping::Piecewise(_)) {
        commands
            .entity(set.source)
            .insert(SliderStep(position_step(mapping, *range, value)));
    }
}

#[allow(clippy::type_complexity)] // Bevy query filters are expressed in the type system.
fn update_numeric_accessibility(
    mut controls: Query<
        (
            &BusWidget,
            &ControlValue,
            &ControlRange,
            &mut AccessibilityNode,
        ),
        Or<(With<Fader>, With<Knob>)>,
    >,
) {
    for (widget, value, range, mut node) in &mut controls {
        node.set_label(widget.id.clone());
        node.set_min_numeric_value(range.min.into());
        node.set_max_numeric_value(range.max.into());
        node.set_numeric_value(value.0.into());
        node.set_numeric_value_step(range.step.into());
    }
}

#[allow(clippy::type_complexity)] // Bevy query filters are expressed in the type system.
fn update_fader_visuals(
    roots: Query<
        (Entity, &SliderValue),
        (
            With<Fader>,
            bevy::ecs::query::Without<FaderHorizontal>,
            Changed<SliderValue>,
        ),
    >,
    descendants: Query<&Children>,
    mut fills: Query<&mut Node, (With<FaderFill>, bevy::ecs::query::Without<FaderThumb>)>,
    mut thumbs: Query<&mut Node, With<FaderThumb>>,
) {
    for (entity, value) in &roots {
        for child in descendants.iter_descendants(entity) {
            if let Ok(mut node) = fills.get_mut(child) {
                node.height = percent(value.0 * 94.0);
            }
            if let Ok(mut node) = thumbs.get_mut(child) {
                node.bottom = percent(value.0 * 94.0);
            }
        }
    }
}

#[allow(clippy::type_complexity)] // Bevy query filters are expressed in the type system.
fn update_hfader_visuals(
    roots: Query<
        (Entity, &SliderValue, &HFaderGeometry, &ComputedNode),
        (
            With<FaderHorizontal>,
            bevy::ecs::query::Or<(Changed<SliderValue>, Changed<ComputedNode>)>,
        ),
    >,
    descendants: Query<&Children>,
    mut tracks: Query<
        &mut Node,
        (
            With<FaderTrackH>,
            bevy::ecs::query::Without<FaderFillH>,
            bevy::ecs::query::Without<FaderThumbH>,
        ),
    >,
    mut fills: Query<&mut Node, (With<FaderFillH>, bevy::ecs::query::Without<FaderThumbH>)>,
    mut thumbs: Query<&mut Node, With<FaderThumbH>>,
) {
    for (entity, value, geometry, computed) in &roots {
        // Live layout width (flex may have shrunk the node); the spawn-time
        // width is the pre-layout fallback. Same padded extent as the pointer
        // mapping: travel = width − 2·HFADER_PAD − thumb.
        let live_width = computed.size().x * computed.inverse_scale_factor();
        let width = if live_width > 0.0 {
            live_width
        } else {
            geometry.width
        };
        let travel = (width - 2.0 * HFADER_PAD - geometry.thumb).max(0.0);
        let offset = value.0 * travel;
        for child in descendants.iter_descendants(entity) {
            if let Ok(mut node) = tracks.get_mut(child) {
                node.width = px((width - 2.0 * HFADER_PAD).max(0.0));
            }
            if let Ok(mut node) = fills.get_mut(child) {
                node.width = px(offset + geometry.thumb / 2.0);
            }
            if let Ok(mut node) = thumbs.get_mut(child) {
                node.left = px(HFADER_PAD + offset);
            }
        }
    }
}

#[allow(clippy::type_complexity)] // Bevy query filters are expressed in the type system.
fn update_knob_visuals(
    roots: Query<(Entity, &SliderValue), (With<Knob>, Changed<SliderValue>)>,
    descendants: Query<&Children>,
    mut indicators: Query<&mut UiTransform, With<KnobIndicator>>,
) {
    for (entity, value) in &roots {
        for child in descendants.iter_descendants(entity) {
            if let Ok(mut transform) = indicators.get_mut(child) {
                transform.rotation = Rot2::radians(-0.75 * PI + value.0 * 1.5 * PI);
            }
        }
    }
}

#[allow(clippy::type_complexity)] // Bevy query filters are expressed in the type system.
fn update_meter_visuals(
    theme: Res<UiTheme>,
    roots: Query<(Entity, Ref<MeterValue>), With<LevelMeter>>,
    descendants: Query<&Children>,
    mut visuals: Query<(
        &mut Node,
        &mut BackgroundColor,
        Option<&MeterFill>,
        Option<&MeterPeak>,
        Option<&MeterHold>,
        Option<&MeterLaneRoot>,
    )>,
) {
    // Level→colour transitions are dynamic (a token component can't switch
    // per-sample); resolve the palette once per run, values from the theme.
    // A theme change repaints EVERY meter (these nodes carry no static token,
    // so nothing else would recolour an idle meter); otherwise only meters
    // whose value changed are touched.
    let theme_changed = theme.is_changed();
    let green = theme.color(&crate::theme::tokens::METER_GREEN);
    let amber = theme.color(&crate::theme::tokens::METER_AMBER);
    let red = theme.color(&crate::theme::tokens::METER_RED);
    let hold_tick = theme.color(&crate::theme::tokens::TEXT);
    for (entity, meter) in &roots {
        if !theme_changed && !meter.is_changed() {
            continue;
        }
        for child in descendants.iter_descendants(entity) {
            if let Ok((mut node, mut color, fill, peak, hold, lane_root)) = visuals.get_mut(child) {
                if let Some(lane) = lane_root {
                    node.display = meter_lane_display(meter.lane_count, lane.0);
                } else if let Some(lane) = fill {
                    let sample = meter.lanes[lane.0];
                    node.height = percent(sample.level.clamp(0.0, 1.0) * 100.0);
                    color.0 = if sample.clipped || sample.level >= 0.95 {
                        red
                    } else if sample.level >= 0.8 {
                        amber
                    } else {
                        green
                    };
                } else if let Some(lane) = peak {
                    let sample = meter.lanes[lane.0];
                    node.bottom = percent(sample.peak.clamp(0.0, 1.0) * 100.0);
                    color.0 = if sample.clipped { red } else { amber };
                } else if let Some(lane) = hold {
                    let sample = meter.lanes[lane.0];
                    node.bottom = percent(sample.hold.clamp(0.0, 1.0) * 100.0);
                    color.0 = if sample.clipped { red } else { hold_tick };
                }
            }
        }
    }
}

fn meter_lane_display(lane_count: u8, lane: usize) -> Display {
    if lane < usize::from(lane_count.clamp(1, 2)) {
        Display::Flex
    } else {
        Display::None
    }
}

fn update_toggle_style(
    theme: Res<UiTheme>,
    mut toggles: Query<(&mut BackgroundColor, Has<Checked>), With<ToggleButton>>,
) {
    // Dynamic repaint resolves tokens per write (a token component can't
    // express a two-state swap); token values come through apply_theme.
    let lit = theme.color(&crate::theme::tokens::CONTROL_ACTIVE);
    let base = theme.color(&crate::theme::tokens::CONTROL);
    for (mut background, checked) in &mut toggles {
        let target = if checked { lit } else { base };
        // Compare through the immutable deref: an unconditional write marks
        // every toggle changed every frame (repaint across 60+ buttons).
        if background.0 != target {
            background.0 = target;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::input_focus::InputFocus;

    #[test]
    fn piecewise_mapping_round_trips_control_points() {
        let mapping = ValueMapping::piecewise([
            (0.0, -120.0),
            (0.1, -60.0),
            (0.25, -30.0),
            (0.5, -12.0),
            (0.75, 0.0),
            (1.0, 6.0),
        ])
        .unwrap();
        for (position, value) in [(0.0, -120.0), (0.1, -60.0), (0.75, 0.0), (1.0, 6.0)] {
            assert!((mapping.to_value(position) - value).abs() < 1e-5);
            assert!((mapping.to_position(value) - position).abs() < 1e-5);
        }
    }

    #[test]
    fn invalid_piecewise_mappings_are_rejected() {
        assert_eq!(
            ValueMapping::piecewise([(0.0, 0.0), (0.5, 1.0)]),
            Err(MappingError::MissingEndpoints)
        );
        assert_eq!(
            ValueMapping::piecewise([(0.0, 0.0), (0.5, 1.0), (1.0, 0.5)]),
            Err(MappingError::NotStrictlyIncreasing)
        );
    }

    #[test]
    fn mono_meter_hides_the_second_lane() {
        assert_eq!(meter_lane_display(1, 0), Display::Flex);
        assert_eq!(meter_lane_display(1, 1), Display::None);
        assert_eq!(meter_lane_display(2, 1), Display::Flex);
    }

    #[test]
    fn range_quantises_and_honours_detent() {
        let range = ControlRange {
            min: -1.0,
            max: 1.0,
            step: 1.0 / 512.0,
            detent: Some(0.0),
        };
        assert_eq!(range.canonicalise(0.0001), 0.0);
        assert_eq!(range.canonicalise(9.0), 1.0);
    }

    #[test]
    fn range_quantisation_is_relative_to_its_minimum() {
        let range = ControlRange {
            min: 0.05,
            max: 0.35,
            step: 0.1,
            detent: None,
        };

        assert!((range.canonicalise(0.05) - 0.05).abs() < f32::EPSILON);
        assert!((range.canonicalise(0.14) - 0.15).abs() < f32::EPSILON);
        assert!((range.canonicalise(0.35) - 0.35).abs() < f32::EPSILON);
    }

    #[test]
    fn non_linear_keyboard_step_uses_the_value_domain() {
        let range = ControlRange {
            min: -120.0,
            max: 6.0,
            step: 0.1,
            detent: Some(0.0),
        };
        assert!(
            (keyboard_domain_value(-60.0, range, KeyCode::ArrowRight).unwrap() + 59.9).abs() < 1e-5
        );
        assert!(
            (keyboard_domain_value(-60.0, range, KeyCode::ArrowLeft).unwrap() + 60.1).abs() < 1e-5
        );
    }

    #[test]
    fn keyboard_step_can_leave_a_detent() {
        let range = ControlRange {
            min: -1.0,
            max: 1.0,
            step: 0.1,
            detent: Some(0.0),
        };

        assert!(
            (keyboard_domain_value(0.0, range, KeyCode::ArrowRight).unwrap() - 0.1).abs() < 1e-6
        );
        assert!(
            (keyboard_domain_value(0.0, range, KeyCode::ArrowLeft).unwrap() + 0.1).abs() < 1e-6
        );
    }

    #[test]
    fn continuous_keyboard_step_uses_one_percent_of_the_domain() {
        let range = ControlRange {
            min: 0.0,
            max: 10.0,
            step: 0.0,
            detent: None,
        };
        assert!((keyboard_domain_value(5.0, range, KeyCode::ArrowUp).unwrap() - 5.1).abs() < 1e-6);
    }

    #[test]
    fn piecewise_drag_preserves_its_final_semantic_event() {
        #[derive(bevy::prelude::Resource, Default)]
        struct Changes(Vec<bool>);

        fn record(change: On<ControlChange>, mut changes: bevy::prelude::ResMut<Changes>) {
            changes.0.push(change.is_final);
        }

        let mut app = App::new();
        app.add_plugins(CtkWidgetsPlugin)
            .init_resource::<Changes>()
            .add_observer(record);
        let control = app
            .world_mut()
            .spawn(fader(NumericControlProps::new(
                "test.drag-final",
                -12.0,
                ControlRange {
                    min: -120.0,
                    max: 6.0,
                    step: 0.1,
                    detent: Some(0.0),
                },
                ValueMapping::piecewise([(0.0, -120.0), (0.75, 0.0), (1.0, 6.0)]).unwrap(),
            )))
            .id();
        app.update();

        app.world_mut().trigger(ValueChange::<f32> {
            source: control,
            value: 0.6,
            is_final: false,
        });
        app.update();
        app.world_mut().trigger(ValueChange::<f32> {
            source: control,
            value: 0.6,
            is_final: true,
        });
        app.update();

        assert_eq!(app.world().resource::<Changes>().0, [false, true]);
    }

    #[test]
    fn pointer_cancel_emits_a_semantic_gesture_cancel() {
        use bevy::camera::NormalizedRenderTarget;
        use bevy::math::Vec2;
        use bevy::picking::backend::HitData;
        use bevy::picking::pointer::{Location, PointerId};
        use bevy::window::WindowRef;

        #[derive(bevy::prelude::Resource, Default)]
        struct Cancelled(Vec<Entity>);

        fn record(
            cancel: On<ControlGestureCancel>,
            mut cancelled: bevy::prelude::ResMut<Cancelled>,
        ) {
            cancelled.0.push(cancel.source);
        }

        let mut app = App::new();
        app.add_plugins(CtkWidgetsPlugin)
            .init_resource::<Cancelled>()
            .add_observer(record);
        let control = app
            .world_mut()
            .spawn(fader(NumericControlProps::new(
                "test.cancel",
                0.0,
                ControlRange {
                    min: -1.0,
                    max: 1.0,
                    step: 0.1,
                    detent: Some(0.0),
                },
                ValueMapping::linear(-1.0, 1.0).unwrap(),
            )))
            .id();
        app.world_mut().trigger(ValueChange::<f32> {
            source: control,
            value: 0.75,
            is_final: false,
        });
        app.update();
        app.world_mut()
            .entity_mut(control)
            .get_mut::<SliderDragState>()
            .unwrap()
            .dragging = true;
        app.world_mut().entity_mut(control).insert(Pressed);

        // Cancel may target whatever is currently hovered, not the control
        // where the gesture began.
        let target = app.world_mut().spawn_empty().id();
        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            Location {
                target: NormalizedRenderTarget::Window(
                    WindowRef::Entity(target).normalize(None).unwrap(),
                ),
                position: Vec2::ZERO,
            },
            Cancel {
                hit: HitData::new(Entity::PLACEHOLDER, 0.0, None, None),
            },
            target,
        ));
        app.update();

        assert_eq!(app.world().resource::<Cancelled>().0, [control]);
        assert!(
            !app.world()
                .get::<SliderDragState>(control)
                .unwrap()
                .dragging
        );
        assert!(!app.world().entity(control).contains::<Pressed>());
    }

    #[test]
    fn disabling_an_active_control_cancels_its_gesture() {
        #[derive(bevy::prelude::Resource, Default)]
        struct Cancelled(Vec<Entity>);
        fn record(
            cancel: On<ControlGestureCancel>,
            mut cancelled: bevy::prelude::ResMut<Cancelled>,
        ) {
            cancelled.0.push(cancel.source);
        }

        let mut app = App::new();
        app.add_plugins(CtkWidgetsPlugin)
            .init_resource::<Cancelled>()
            .add_observer(record);
        let control = app
            .world_mut()
            .spawn(fader(NumericControlProps::new(
                "test.disable-during-drag",
                0.0,
                ControlRange {
                    min: -1.0,
                    max: 1.0,
                    step: 0.1,
                    detent: Some(0.0),
                },
                ValueMapping::linear(-1.0, 1.0).unwrap(),
            )))
            .id();
        app.world_mut().trigger(ValueChange::<f32> {
            source: control,
            value: 0.75,
            is_final: false,
        });
        app.update();
        app.world_mut()
            .entity_mut(control)
            .get_mut::<SliderDragState>()
            .unwrap()
            .dragging = true;
        app.world_mut().entity_mut(control).insert(Pressed);

        app.world_mut()
            .entity_mut(control)
            .insert(InteractionDisabled);
        app.update();

        assert_eq!(app.world().resource::<Cancelled>().0, [control]);
        assert!(!app
            .world()
            .entity(control)
            .contains::<ActiveControlGesture>());
        assert!(
            !app.world()
                .get::<SliderDragState>(control)
                .unwrap()
                .dragging
        );
        assert!(!app.world().entity(control).contains::<Pressed>());
    }

    #[test]
    fn disabling_a_pressed_control_before_value_change_cancels_it() {
        use bevy::camera::NormalizedRenderTarget;
        use bevy::math::Vec2;
        use bevy::picking::backend::HitData;
        use bevy::picking::pointer::{Location, PointerButton, PointerId};
        use bevy::window::WindowRef;

        #[derive(bevy::prelude::Resource, Default)]
        struct Cancelled(Vec<Entity>);
        fn record(
            cancel: On<ControlGestureCancel>,
            mut cancelled: bevy::prelude::ResMut<Cancelled>,
        ) {
            cancelled.0.push(cancel.source);
        }

        let mut app = App::new();
        app.add_plugins(CtkWidgetsPlugin)
            .init_resource::<Cancelled>()
            .add_observer(record);
        let control = app
            .world_mut()
            .spawn(fader(NumericControlProps::new(
                "test.disable-before-value-change",
                0.0,
                ControlRange {
                    min: -1.0,
                    max: 1.0,
                    step: 0.1,
                    detent: Some(0.0),
                },
                ValueMapping::linear(-1.0, 1.0).unwrap(),
            )))
            .id();
        app.update();
        let thumb = {
            let world = app.world_mut();
            let mut thumbs = world.query_filtered::<Entity, With<FaderThumb>>();
            thumbs.single(world).unwrap()
        };
        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            Location {
                target: NormalizedRenderTarget::Window(
                    WindowRef::Entity(control).normalize(None).unwrap(),
                ),
                position: Vec2::ZERO,
            },
            Press {
                button: PointerButton::Primary,
                hit: HitData::new(thumb, 0.0, None, None),
                count: 1,
            },
            thumb,
        ));
        app.update();

        assert!(app.world().entity(control).contains::<Pressed>());
        assert!(
            !app.world()
                .get::<SliderDragState>(control)
                .unwrap()
                .dragging
        );
        assert!(!app
            .world()
            .entity(control)
            .contains::<ActiveControlGesture>());

        app.world_mut()
            .entity_mut(control)
            .insert(InteractionDisabled);
        app.update();

        assert_eq!(app.world().resource::<Cancelled>().0, [control]);
        assert!(!app
            .world()
            .entity(control)
            .contains::<ActiveControlGesture>());
        assert!(
            !app.world()
                .get::<SliderDragState>(control)
                .unwrap()
                .dragging
        );
        assert!(!app.world().entity(control).contains::<Pressed>());
    }

    #[test]
    fn direct_final_commit_clears_stationary_click_gesture_state() {
        let mut app = App::new();
        app.add_plugins(CtkWidgetsPlugin);
        let control = app
            .world_mut()
            .spawn(fader(NumericControlProps::new(
                "test.stationary-click",
                0.0,
                ControlRange {
                    min: -1.0,
                    max: 1.0,
                    step: 0.1,
                    detent: Some(0.0),
                },
                ValueMapping::linear(-1.0, 1.0).unwrap(),
            )))
            .id();
        app.world_mut().trigger(ValueChange::<f32> {
            source: control,
            value: 0.75,
            is_final: false,
        });
        app.update();
        assert!(app
            .world()
            .entity(control)
            .contains::<ActiveControlGesture>());

        // Stationary track clicks commit directly as ControlChange rather
        // than receiving Bevy's drag-end ValueChange.
        app.world_mut().trigger(ControlChange {
            source: control,
            value: 0.5,
            is_final: true,
        });
        app.update();

        assert!(!app
            .world()
            .entity(control)
            .contains::<ActiveControlGesture>());
    }

    #[test]
    fn piecewise_keyboard_repeat_emits_one_domain_step() {
        use bevy::input::keyboard::Key;
        use bevy::input::InputPlugin;
        use bevy::input_focus::{InputDispatchPlugin, InputFocusPlugin};
        use bevy::ui_widgets::SliderPlugin;
        use bevy::window::{PrimaryWindow, Window};

        #[derive(bevy::prelude::Resource, Default)]
        struct Changes(Vec<(f32, bool)>);

        fn record(change: On<ControlChange>, mut changes: bevy::prelude::ResMut<Changes>) {
            changes.0.push((change.value, change.is_final));
        }

        let mut app = App::new();
        app.add_plugins((
            InputPlugin,
            InputFocusPlugin,
            InputDispatchPlugin,
            SliderPlugin,
            CtkWidgetsPlugin,
        ))
        .init_resource::<Changes>()
        .add_observer(record);
        let window = app
            .world_mut()
            .spawn((Window::default(), PrimaryWindow))
            .id();
        let control = app
            .world_mut()
            .spawn(fader(NumericControlProps::new(
                "test.keyboard-step",
                -12.0,
                ControlRange {
                    min: -120.0,
                    max: 6.0,
                    step: 0.1,
                    detent: Some(0.0),
                },
                ValueMapping::piecewise([(0.0, -120.0), (0.75, 0.0), (1.0, 6.0)]).unwrap(),
            )))
            .id();
        app.world_mut()
            .insert_resource(InputFocus::from_entity(control));
        app.world_mut().write_message(KeyboardInput {
            key_code: KeyCode::ArrowRight,
            logical_key: Key::ArrowRight,
            state: ButtonState::Pressed,
            text: None,
            repeat: true,
            window,
        });

        app.update();

        let changes = &app.world().resource::<Changes>().0;
        assert_eq!(changes.len(), 1);
        assert!(changes[0].1);
        assert!((changes[0].0 + 11.9).abs() < 1e-5);
        assert!(app
            .world()
            .entity(control)
            .contains::<bevy::ui_widgets::SliderStep>());
    }

    #[test]
    fn linear_vertical_keyboard_accepts_arrow_up() {
        use bevy::input::keyboard::Key;
        use bevy::input::InputPlugin;
        use bevy::input_focus::{InputDispatchPlugin, InputFocusPlugin};
        use bevy::ui_widgets::SliderPlugin;
        use bevy::window::{PrimaryWindow, Window};

        #[derive(bevy::prelude::Resource, Default)]
        struct Changes(Vec<f32>);
        fn record(change: On<ControlChange>, mut changes: bevy::prelude::ResMut<Changes>) {
            changes.0.push(change.value);
        }

        let mut app = App::new();
        app.add_plugins((
            InputPlugin,
            InputFocusPlugin,
            InputDispatchPlugin,
            SliderPlugin,
            CtkWidgetsPlugin,
        ))
        .init_resource::<Changes>()
        .add_observer(record);
        let window = app
            .world_mut()
            .spawn((Window::default(), PrimaryWindow))
            .id();
        let control = app
            .world_mut()
            .spawn(knob(NumericControlProps::new(
                "test.linear-up",
                0.0,
                ControlRange {
                    min: -1.0,
                    max: 1.0,
                    step: 0.1,
                    detent: None,
                },
                ValueMapping::linear(-1.0, 1.0).unwrap(),
            )))
            .id();
        app.world_mut()
            .insert_resource(InputFocus::from_entity(control));
        app.world_mut().write_message(KeyboardInput {
            key_code: KeyCode::ArrowUp,
            logical_key: Key::ArrowUp,
            state: ButtonState::Pressed,
            text: None,
            repeat: false,
            window,
        });

        app.update();

        assert_eq!(app.world().resource::<Changes>().0.len(), 1);
        assert!((app.world().resource::<Changes>().0[0] - 0.1).abs() < 1e-6);
    }

    #[test]
    fn linear_keyboard_right_leaves_a_detent_once() {
        use bevy::input::keyboard::Key;
        use bevy::input::InputPlugin;
        use bevy::input_focus::{InputDispatchPlugin, InputFocusPlugin};
        use bevy::ui_widgets::SliderPlugin;
        use bevy::window::{PrimaryWindow, Window};

        #[derive(bevy::prelude::Resource, Default)]
        struct Changes(Vec<f32>);
        fn record(change: On<ControlChange>, mut changes: bevy::prelude::ResMut<Changes>) {
            changes.0.push(change.value);
        }

        let mut app = App::new();
        app.add_plugins((
            InputPlugin,
            InputFocusPlugin,
            InputDispatchPlugin,
            SliderPlugin,
            CtkWidgetsPlugin,
        ))
        .init_resource::<Changes>()
        .add_observer(record);
        let window = app
            .world_mut()
            .spawn((Window::default(), PrimaryWindow))
            .id();
        let control = app
            .world_mut()
            .spawn(knob(NumericControlProps::new(
                "test.linear-detent-right",
                0.0,
                ControlRange {
                    min: -1.0,
                    max: 1.0,
                    step: 1.0 / 512.0,
                    detent: Some(0.0),
                },
                ValueMapping::linear(-1.0, 1.0).unwrap(),
            )))
            .id();
        app.world_mut()
            .insert_resource(InputFocus::from_entity(control));
        app.world_mut().write_message(KeyboardInput {
            key_code: KeyCode::ArrowRight,
            logical_key: Key::ArrowRight,
            state: ButtonState::Pressed,
            text: None,
            repeat: false,
            window,
        });

        app.update();

        assert_eq!(app.world().resource::<Changes>().0.len(), 1);
        assert!((app.world().resource::<Changes>().0[0] - 1.0 / 512.0).abs() < 1e-6);
    }

    #[test]
    fn disabled_linear_control_ignores_keyboard_input() {
        use bevy::input::keyboard::Key;
        use bevy::input::InputPlugin;
        use bevy::input_focus::{InputDispatchPlugin, InputFocusPlugin};
        use bevy::ui_widgets::SliderPlugin;
        use bevy::window::{PrimaryWindow, Window};

        #[derive(bevy::prelude::Resource, Default)]
        struct ChangeCount(usize);
        fn record(_: On<ControlChange>, mut count: bevy::prelude::ResMut<ChangeCount>) {
            count.0 += 1;
        }

        let mut app = App::new();
        app.add_plugins((
            InputPlugin,
            InputFocusPlugin,
            InputDispatchPlugin,
            SliderPlugin,
            CtkWidgetsPlugin,
        ))
        .init_resource::<ChangeCount>()
        .add_observer(record);
        let window = app
            .world_mut()
            .spawn((Window::default(), PrimaryWindow))
            .id();
        let control = app
            .world_mut()
            .spawn((
                knob(NumericControlProps::new(
                    "test.disabled-keyboard",
                    0.0,
                    ControlRange {
                        min: -1.0,
                        max: 1.0,
                        step: 0.1,
                        detent: Some(0.0),
                    },
                    ValueMapping::linear(-1.0, 1.0).unwrap(),
                )),
                InteractionDisabled,
            ))
            .id();
        app.world_mut()
            .insert_resource(InputFocus::from_entity(control));
        app.world_mut().write_message(KeyboardInput {
            key_code: KeyCode::ArrowRight,
            logical_key: Key::ArrowRight,
            state: ButtonState::Pressed,
            text: None,
            repeat: false,
            window,
        });

        app.update();

        assert_eq!(app.world().resource::<ChangeCount>().0, 0);
        assert_eq!(
            app.world().get::<ControlValue>(control),
            Some(&ControlValue(0.0))
        );
    }

    #[test]
    fn track_click_position_uses_the_current_pointer_location() {
        assert_eq!(vertical_track_position(250.0, 18.0, 0.5), Some(0.0));
        assert_eq!(vertical_track_position(250.0, 18.0, -0.5), Some(1.0));
        assert_eq!(vertical_track_position(18.0, 18.0, 0.0), None);
        assert!(fader_click_is_blocked(false, true, false));
        assert!(fader_click_is_blocked(true, false, false));
        assert!(fader_click_is_blocked(false, false, true));
        assert!(!fader_click_is_blocked(false, false, false));
    }

    #[test]
    fn horizontal_track_click_maps_left_to_zero_and_right_to_one() {
        // Left edge (normalised x = -0.5) seeks to the start, right edge to end.
        assert_eq!(horizontal_track_position(900.0, 14.0, -0.5), Some(0.0));
        assert_eq!(horizontal_track_position(900.0, 14.0, 0.5), Some(1.0));
        // A degenerate track (thumb fills the node) yields no position.
        assert_eq!(horizontal_track_position(14.0, 14.0, 0.0), None);
    }

    #[test]
    fn slider_track_press_drag_start_and_drag_preserve_bevy_pointer_semantics() {
        use bevy::camera::NormalizedRenderTarget;
        use bevy::math::Vec2;
        use bevy::picking::backend::HitData;
        use bevy::picking::events::{Drag, DragStart};
        use bevy::picking::pointer::{Location, PointerButton, PointerId};
        use bevy::ui_widgets::SliderPlugin;
        use bevy::window::WindowRef;

        let mut app = App::new();
        app.add_plugins((SliderPlugin, CtkWidgetsPlugin))
            .init_resource::<UiScale>();
        let control = app
            .world_mut()
            .spawn(hfader_sized(
                NumericControlProps::new(
                    "test.pointer-chain",
                    0.0,
                    ControlRange {
                        min: 0.0,
                        max: 1.0,
                        step: 0.01,
                        detent: None,
                    },
                    ValueMapping::linear(0.0, 1.0).unwrap(),
                ),
                100.0,
                20.0,
            ))
            .id();
        app.update();
        app.world_mut().entity_mut(control).insert(ComputedNode {
            size: Vec2::new(100.0, 20.0),
            inverse_scale_factor: 1.0,
            ..default()
        });
        let thumb = {
            let world = app.world_mut();
            let mut thumbs = world.query_filtered::<Entity, With<SliderThumb>>();
            thumbs.single(world).unwrap()
        };
        app.world_mut().entity_mut(thumb).insert(ComputedNode {
            size: Vec2::new(10.0, 10.0),
            inverse_scale_factor: 1.0,
            ..default()
        });
        let location = Location {
            target: NormalizedRenderTarget::Window(
                WindowRef::Entity(control).normalize(None).unwrap(),
            ),
            position: Vec2::new(36.0, 0.0),
        };
        let hit = HitData::new(control, 0.0, None, None);

        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            location.clone(),
            Press {
                button: PointerButton::Primary,
                hit: hit.clone(),
                count: 1,
            },
            control,
        ));
        app.world_mut().flush();
        let pressed_value = app.world().get::<ControlValue>(control).unwrap().0;
        assert!(pressed_value > 0.85);
        assert!(app.world().entity(control).contains::<Pressed>());

        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            location.clone(),
            DragStart {
                button: PointerButton::Primary,
                hit,
            },
            control,
        ));
        app.world_mut().flush();
        assert!(
            app.world()
                .get::<SliderDragState>(control)
                .unwrap()
                .dragging
        );

        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            location,
            Drag {
                button: PointerButton::Primary,
                distance: Vec2::new(-45.0, 0.0),
                delta: Vec2::new(-45.0, 0.0),
            },
            control,
        ));
        app.world_mut().flush();
        let dragged_value = app.world().get::<ControlValue>(control).unwrap().0;
        assert!((dragged_value - (pressed_value - 0.5)).abs() < 0.02);
    }

    #[test]
    fn toggle_press_focuses_activates_and_survives_drag_off() {
        use bevy::camera::NormalizedRenderTarget;
        use bevy::input::keyboard::Key;
        use bevy::input::InputPlugin;
        use bevy::input_focus::{InputDispatchPlugin, InputFocusPlugin};
        use bevy::math::Vec2;
        use bevy::picking::backend::HitData;
        use bevy::picking::events::Click;
        use bevy::picking::pointer::{Location, PointerButton, PointerId};
        use bevy::window::{PrimaryWindow, Window, WindowRef};
        use core::time::Duration;

        let mut app = App::new();
        app.add_plugins((
            InputPlugin,
            InputFocusPlugin,
            InputDispatchPlugin,
            CtkWidgetsPlugin,
        ))
        .init_resource::<UiScale>();
        let window = app
            .world_mut()
            .spawn((Window::default(), PrimaryWindow))
            .id();
        let toggle = app
            .world_mut()
            .spawn(toggle_button("test.pointer-focus"))
            .id();
        let location = Location {
            target: NormalizedRenderTarget::Window(
                WindowRef::Entity(window).normalize(None).unwrap(),
            ),
            position: Vec2::ZERO,
        };
        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            location.clone(),
            Press {
                button: PointerButton::Primary,
                hit: HitData::new(toggle, 0.0, None, None),
                count: 1,
            },
            toggle,
        ));
        app.world_mut().flush();
        assert_eq!(app.world().resource::<InputFocus>().get(), Some(toggle));
        assert!(app.world().entity(toggle).contains::<Checked>());

        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            location.clone(),
            Click {
                button: PointerButton::Primary,
                hit: HitData::new(toggle, 0.0, None, None),
                duration: Duration::ZERO,
                count: 1,
            },
            toggle,
        ));
        app.world_mut().flush();
        assert!(app.world().entity(toggle).contains::<Checked>());

        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            location,
            DragEnd {
                button: PointerButton::Primary,
                distance: Vec2::new(100.0, 0.0),
            },
            toggle,
        ));
        app.world_mut().flush();
        assert!(app.world().entity(toggle).contains::<Checked>());

        app.world_mut().write_message(KeyboardInput {
            key_code: KeyCode::Space,
            logical_key: Key::Space,
            state: ButtonState::Pressed,
            text: Some(" ".into()),
            repeat: false,
            window,
        });
        app.update();
        assert!(!app.world().entity(toggle).contains::<Checked>());
    }

    #[test]
    fn disabled_ctk_controls_consume_press_and_click_before_parent() {
        use bevy::camera::NormalizedRenderTarget;
        use bevy::math::Vec2;
        use bevy::picking::backend::HitData;
        use bevy::picking::events::Click;
        use bevy::picking::pointer::{Location, PointerButton, PointerId};
        use bevy::window::WindowRef;
        use core::time::Duration;

        #[derive(Component)]
        struct ClickableParent;
        #[derive(Resource, Default)]
        struct ParentEvents(usize);
        fn count_parent_press(
            press: On<Pointer<Press>>,
            parents: Query<(), With<ClickableParent>>,
            mut events: ResMut<ParentEvents>,
        ) {
            if parents.contains(press.entity) {
                events.0 += 1;
            }
        }
        fn count_parent_click(
            click: On<Pointer<Click>>,
            parents: Query<(), With<ClickableParent>>,
            mut events: ResMut<ParentEvents>,
        ) {
            if parents.contains(click.entity) {
                events.0 += 1;
            }
        }

        let mut app = App::new();
        app.add_plugins(CtkWidgetsPlugin)
            .init_resource::<InputFocus>()
            .init_resource::<InputFocusVisible>()
            .init_resource::<UiScale>()
            .init_resource::<ParentEvents>()
            .add_observer(count_parent_press)
            .add_observer(count_parent_click);
        let parent = app.world_mut().spawn(ClickableParent).id();
        let controls = [
            app.world_mut()
                .spawn((
                    action_button("test.disabled-action", 40.0, 20.0),
                    InteractionDisabled,
                ))
                .id(),
            app.world_mut()
                .spawn((toggle_button("test.disabled-toggle"), InteractionDisabled))
                .id(),
        ];
        app.world_mut().entity_mut(parent).add_children(&controls);
        for control in controls {
            let location = Location {
                target: NormalizedRenderTarget::Window(
                    WindowRef::Entity(control).normalize(None).unwrap(),
                ),
                position: Vec2::ZERO,
            };
            let hit = HitData::new(control, 0.0, None, None);
            app.world_mut().trigger(Pointer::new(
                PointerId::Mouse,
                location.clone(),
                Press {
                    button: PointerButton::Primary,
                    hit: hit.clone(),
                    count: 1,
                },
                control,
            ));
            app.world_mut().trigger(Pointer::new(
                PointerId::Mouse,
                location,
                Click {
                    button: PointerButton::Primary,
                    hit,
                    duration: Duration::ZERO,
                    count: 1,
                },
                control,
            ));
        }

        assert_eq!(app.world().resource::<ParentEvents>().0, 0);
    }

    #[test]
    fn action_button_activation_emits_one_final_control_change() {
        use bevy::ui_widgets::Activate;

        #[derive(bevy::prelude::Resource, Default)]
        struct Changes(Vec<(f32, bool)>);

        fn record(change: On<ControlChange>, mut changes: bevy::prelude::ResMut<Changes>) {
            changes.0.push((change.value, change.is_final));
        }

        let mut app = App::new();
        app.add_plugins(CtkWidgetsPlugin)
            .init_resource::<Changes>()
            .add_observer(record);
        let button = app
            .world_mut()
            .spawn(action_button("test.play", 40.0, 20.0))
            .id();
        app.world_mut().trigger(Activate { entity: button });
        app.update();

        assert_eq!(app.world().resource::<Changes>().0, [(0.0, true)]);
    }

    #[test]
    fn drain_time_disable_preserves_legacy_activate_but_blocks_canonical_button() {
        use bevy::ui_widgets::Activate;

        #[derive(Resource, Default)]
        struct Activations(Vec<Entity>);
        #[derive(Resource, Default)]
        struct Changes(usize);
        fn record_activate(activate: On<Activate>, mut seen: ResMut<Activations>) {
            seen.0.push(activate.entity);
        }
        fn record_change(_: On<ControlChange>, mut seen: ResMut<Changes>) {
            seen.0 += 1;
        }

        let mut app = App::new();
        app.add_plugins(CtkWidgetsPlugin)
            .init_resource::<Activations>()
            .init_resource::<Changes>()
            .add_observer(record_activate)
            .add_observer(record_change);
        let legacy = app
            .world_mut()
            .spawn(action_button("test.legacy-drain", 40.0, 20.0))
            .id();
        let canonical = crate::button::spawn_button(
            &mut app.world_mut().commands(),
            crate::button::ButtonDef::text("Canonical").bus("test.canonical-drain"),
        );
        app.world_mut().flush();
        {
            let mut queue = app.world_mut().resource_mut::<KeyboardControlQueue>();
            queue.push(1, legacy, DeferredKeyboardControlKind::Activate);
            queue.push(2, canonical, DeferredKeyboardControlKind::Activate);
        }
        app.world_mut()
            .entity_mut(legacy)
            .insert(InteractionDisabled);
        app.world_mut()
            .entity_mut(canonical)
            .insert(InteractionDisabled);
        app.update();

        assert_eq!(app.world().resource::<Activations>().0, [legacy]);
        assert_eq!(app.world().resource::<Changes>().0, 0);
    }

    #[test]
    fn repeated_activation_is_ignored_by_buttons_and_toggles() {
        use bevy::input::keyboard::Key;
        use bevy::input::InputPlugin;
        use bevy::input_focus::{InputDispatchPlugin, InputFocusPlugin};
        use bevy::window::{PrimaryWindow, Window};

        #[derive(Resource, Default)]
        struct Changes(usize);
        fn record(_: On<ControlChange>, mut changes: ResMut<Changes>) {
            changes.0 += 1;
        }

        let mut app = App::new();
        app.add_plugins((
            InputPlugin,
            InputFocusPlugin,
            InputDispatchPlugin,
            CtkWidgetsPlugin,
        ))
        .init_resource::<Changes>()
        .add_observer(record);
        let window = app
            .world_mut()
            .spawn((Window::default(), PrimaryWindow))
            .id();
        for control in [
            app.world_mut()
                .spawn(action_button("test.repeat-action", 40.0, 20.0))
                .id(),
            app.world_mut()
                .spawn(toggle_button("test.repeat-toggle"))
                .id(),
        ] {
            app.world_mut()
                .insert_resource(InputFocus::from_entity(control));
            app.world_mut().write_message(KeyboardInput {
                key_code: KeyCode::Space,
                logical_key: Key::Space,
                state: ButtonState::Pressed,
                text: Some(" ".into()),
                repeat: true,
                window,
            });
            app.update();
        }

        assert_eq!(app.world().resource::<Changes>().0, 0);
    }

    #[test]
    fn same_frame_space_and_click_flip_a_toggle_twice() {
        use bevy::input::keyboard::Key;
        use bevy::input::InputPlugin;
        use bevy::input_focus::{InputDispatchPlugin, InputFocusPlugin};
        use bevy::window::{PrimaryWindow, Window};

        let mut app = App::new();
        app.add_plugins((
            InputPlugin,
            InputFocusPlugin,
            InputDispatchPlugin,
            CtkWidgetsPlugin,
        ));
        let window = app
            .world_mut()
            .spawn((Window::default(), PrimaryWindow))
            .id();
        let toggle = app
            .world_mut()
            .spawn(toggle_button("test.space-click"))
            .id();
        app.world_mut()
            .insert_resource(InputFocus::from_entity(toggle));
        app.world_mut().write_message(KeyboardInput {
            key_code: KeyCode::Space,
            logical_key: Key::Space,
            state: ButtonState::Pressed,
            text: Some(" ".into()),
            repeat: false,
            window,
        });
        // This is the semantic event emitted by CTK's pointer-press handler.
        app.world_mut().trigger(ValueChange {
            source: toggle,
            value: true,
            is_final: true,
        });

        app.update();

        assert!(!app.world().entity(toggle).contains::<Checked>());
    }

    #[test]
    fn pointer_slider_claim_wins_on_either_side_of_a_queued_key() {
        let entity = Entity::from_bits(1 << 32 | 1);
        for pointer_first in [false, true] {
            let mut queue = KeyboardControlQueue::default();
            if pointer_first {
                queue.claim_pointer_slider(entity);
            }
            queue.push(
                1,
                entity,
                DeferredKeyboardControlKind::SliderKey(KeyCode::ArrowRight),
            );
            if !pointer_first {
                queue.claim_pointer_slider(entity);
            }
            assert!(queue.pending.is_empty());
        }
    }

    #[test]
    fn keyboard_control_queue_drops_oldest_at_capacity() {
        let entity = Entity::from_bits(1 << 32 | 2);
        let mut queue = KeyboardControlQueue::default();
        for order in 0..=MAX_DEFERRED_KEYBOARD_CONTROLS as u64 {
            queue.push(order, entity, DeferredKeyboardControlKind::Activate);
        }

        assert_eq!(queue.pending.len(), MAX_DEFERRED_KEYBOARD_CONTROLS);
        assert_eq!(queue.pending.front().unwrap().order, 1);
    }

    #[test]
    fn disabled_action_button_ignores_activation() {
        use bevy::ui_widgets::Activate;

        #[derive(bevy::prelude::Resource, Default)]
        struct ChangeCount(usize);

        fn count(_: On<ControlChange>, mut count: bevy::prelude::ResMut<ChangeCount>) {
            count.0 += 1;
        }

        let mut app = App::new();
        app.add_plugins(CtkWidgetsPlugin)
            .init_resource::<ChangeCount>()
            .add_observer(count);
        let button = app
            .world_mut()
            .spawn((action_button("test.stop", 40.0, 20.0), InteractionDisabled))
            .id();
        app.world_mut().trigger(Activate { entity: button });
        app.update();

        assert_eq!(app.world().resource::<ChangeCount>().0, 0);
    }

    #[test]
    fn authoritative_value_is_not_pulled_into_the_local_detent() {
        let mut app = App::new();
        app.add_plugins(CtkWidgetsPlugin);
        let control = app
            .world_mut()
            .spawn(fader(NumericControlProps::new(
                "test.authoritative",
                0.0,
                ControlRange {
                    min: -120.0,
                    max: 6.0,
                    step: 0.1,
                    detent: Some(0.0),
                },
                ValueMapping::piecewise([(0.0, -120.0), (0.1, -60.0), (0.75, 0.0), (1.0, 6.0)])
                    .unwrap(),
            )))
            .id();
        app.world_mut().trigger(SetControlValue {
            source: control,
            value: 0.5,
        });
        app.update();

        assert_eq!(
            app.world().get::<ControlValue>(control),
            Some(&ControlValue(0.5))
        );
    }

    #[test]
    fn decoration_ignores_hits_but_the_fader_thumb_remains_hittable() {
        let mut world = bevy::ecs::world::World::new();
        world.spawn(fader(NumericControlProps::new(
            "test.fader",
            0.0,
            ControlRange {
                min: -1.0,
                max: 1.0,
                step: 0.1,
                detent: None,
            },
            ValueMapping::linear(-1.0, 1.0).unwrap(),
        )));
        world.spawn(knob(NumericControlProps::new(
            "test.knob",
            0.0,
            ControlRange {
                min: -1.0,
                max: 1.0,
                step: 0.1,
                detent: None,
            },
            ValueMapping::linear(-1.0, 1.0).unwrap(),
        )));
        world.flush();

        let mut fader_fills = world.query_filtered::<&Pickable, With<FaderFill>>();
        assert!(fader_fills
            .iter(&world)
            .all(|pickable| *pickable == Pickable::IGNORE));
        let mut fader_thumbs = world.query_filtered::<&Pickable, With<FaderThumb>>();
        assert!(fader_thumbs
            .iter(&world)
            .all(|pickable| *pickable == Pickable::default()));
        let mut knob_parts = world.query_filtered::<&Pickable, With<KnobIndicator>>();
        assert!(knob_parts
            .iter(&world)
            .all(|pickable| *pickable == Pickable::IGNORE));
    }

    #[test]
    fn toggle_true_is_checked_and_uses_the_active_colour() {
        let mut app = App::new();
        app.add_plugins(CtkWidgetsPlugin);
        let toggle = app.world_mut().spawn(toggle_button("test.toggle")).id();
        app.world_mut().trigger(ValueChange {
            source: toggle,
            value: true,
            is_final: true,
        });
        app.update();

        assert!(app.world().entity(toggle).contains::<Checked>());
        assert_eq!(
            app.world()
                .entity(toggle)
                .get::<BackgroundColor>()
                .unwrap()
                .0,
            app.world()
                .resource::<UiTheme>()
                .color(&crate::theme::tokens::CONTROL_ACTIVE)
        );

        app.world_mut().trigger(ValueChange {
            source: toggle,
            value: false,
            is_final: true,
        });
        app.update();
        assert!(!app.world().entity(toggle).contains::<Checked>());
    }

    #[test]
    fn authoritative_toggle_rollback_wins_after_local_activation() {
        fn rollback(change: On<ControlChange>, mut commands: Commands) {
            commands.trigger(SetToggleValue {
                source: change.source,
                value: false,
            });
        }

        let mut app = App::new();
        app.add_plugins(CtkWidgetsPlugin).add_observer(rollback);
        let toggle = app.world_mut().spawn(toggle_button("test.rollback")).id();
        app.world_mut().trigger(ValueChange {
            source: toggle,
            value: true,
            is_final: true,
        });
        app.update();

        assert!(!app.world().entity(toggle).contains::<Checked>());
    }

    #[test]
    fn unrelated_checkbox_does_not_emit_a_ctk_control_change() {
        #[derive(bevy::prelude::Resource, Default)]
        struct ChangeCount(usize);

        fn count_change(_: On<ControlChange>, mut count: bevy::prelude::ResMut<ChangeCount>) {
            count.0 += 1;
        }

        let mut app = App::new();
        app.add_plugins(CtkWidgetsPlugin)
            .init_resource::<ChangeCount>()
            .add_observer(count_change);
        let checkbox = app.world_mut().spawn(bevy::ui_widgets::Checkbox).id();
        app.world_mut().trigger(ValueChange {
            source: checkbox,
            value: true,
            is_final: true,
        });
        app.update();

        assert_eq!(app.world().resource::<ChangeCount>().0, 0);
    }
}
