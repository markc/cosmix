//! Bevy adapter around the pure shell model.
//!
//! Input systems emit semantic commands; this adapter is the only code which
//! mutates [`ShellModel`]. Presentation and hosts consume [`ShellFrameState`]
//! and therefore cannot reach back into model or window state.

use bevy::app::{App, Plugin, Update};
use bevy::ecs::message::MessageReader;
use bevy::ecs::schedule::{IntoScheduleConfigs, SystemSet};
use bevy::prelude::{Res, ResMut, Resource, Time};
use bevy::time::Real;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::core::{Edge, ShellModel};
use crate::runtime::{CarouselInput, ShellCommand, ShellCommandKind, ShellFrame, WakePolicy};

#[derive(Resource)]
struct ShellRuntime {
    model: ShellModel,
    clock_text: String,
    clock_deadline: Option<Duration>,
}

/// Current renderer-neutral output. This is the sole presentation input.
#[derive(Resource, Clone, Debug)]
pub struct ShellFrameState(pub ShellFrame);

/// Ordering seam used by chrome and hosts without exposing model internals.
#[derive(SystemSet, Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ShellRuntimeSet {
    Input,
    Model,
    Presentation,
    Host,
}

pub struct ShellRuntimePlugin {
    model: ShellModel,
}

impl ShellRuntimePlugin {
    pub const fn new(model: ShellModel) -> Self {
        Self { model }
    }
}

impl Plugin for ShellRuntimePlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<ShellCommand>()
            .insert_resource(ShellRuntime {
                model: self.model.clone(),
                clock_text: String::new(),
                clock_deadline: None,
            })
            .insert_resource(ShellFrameState(ShellFrame::from_model(&self.model)))
            .configure_sets(
                Update,
                (
                    ShellRuntimeSet::Input,
                    ShellRuntimeSet::Model,
                    ShellRuntimeSet::Presentation,
                    ShellRuntimeSet::Host,
                )
                    .chain(),
            )
            .add_systems(Update, update_model.in_set(ShellRuntimeSet::Model));
    }
}

fn update_model(
    time: Res<Time<Real>>,
    mut commands: MessageReader<ShellCommand>,
    mut runtime: ResMut<ShellRuntime>,
    mut frame: ResMut<ShellFrameState>,
) {
    let now = time.elapsed();
    for command in commands.read() {
        if command.output != *runtime.model.output() {
            continue;
        }
        let at = command.at.clamp(runtime.model.last_update(), now);
        match &command.kind {
            ShellCommandKind::Geometry(size) => runtime.model.set_geometry(*size),
            ShellCommandKind::Corner(event) => {
                let _ = runtime.model.corner_event(at, *event);
            }
            ShellCommandKind::Panel { edge, input } => {
                let _ = runtime.model.panel_input(*edge, at, *input);
            }
            ShellCommandKind::Carousel { edge, input } => {
                let carousel = runtime.model.carousel_mut(*edge);
                match input {
                    CarouselInput::Next => {
                        carousel.next_page();
                    }
                    CarouselInput::Previous => {
                        carousel.previous_page();
                    }
                    CarouselInput::SelectId(id) => {
                        carousel.select_id(id);
                    }
                }
            }
        }
    }
    let _ = runtime.model.tick(now);
    let mut next_frame = ShellFrame::from_model(&runtime.model);
    if next_frame.panel(Edge::Bottom).mapped {
        if runtime
            .clock_deadline
            .is_none_or(|deadline| now >= deadline)
        {
            runtime.clock_text = utc_clock_text();
            runtime.clock_deadline = Some(now + Duration::from_secs(1));
        }
        next_frame.content.bottom_clock_text = Some(runtime.clock_text.clone());
        if let Some(deadline) = runtime.clock_deadline {
            next_frame.wake = merge_wake(next_frame.wake, deadline);
        }
    } else {
        runtime.clock_deadline = None;
    }
    frame.0 = next_frame;
}

fn utc_clock_text() -> String {
    let wall = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let day = wall % 86_400;
    format!("{:02}:{:02}:{:02} UTC", day / 3600, day / 60 % 60, day % 60)
}

fn merge_wake(current: WakePolicy, deadline: Duration) -> WakePolicy {
    match current {
        WakePolicy::Animate => WakePolicy::Animate,
        WakePolicy::WakeAt(current) => WakePolicy::WakeAt(current.min(deadline)),
        WakePolicy::Idle => WakePolicy::WakeAt(deadline),
    }
}
