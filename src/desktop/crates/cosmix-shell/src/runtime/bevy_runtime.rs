//! Bevy adapter around the pure shell model.
//!
//! Input systems emit semantic commands; this adapter is the only code which
//! mutates [`ShellModel`]. Presentation and hosts consume [`ShellFrameState`]
//! and therefore cannot reach back into model or window state.

use bevy::app::{App, Plugin, Update};
use bevy::ecs::message::MessageReader;
use bevy::ecs::schedule::{IntoScheduleConfigs, SystemSet};
use bevy::prelude::{Res, ResMut, Resource, Time, World};
use bevy::time::Real;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::chrome::QuoinCommittedMotionModes;
use crate::core::{Edge, ShellModel};
use crate::runtime::{
    CarouselInput, ShellCommand, ShellCommandKind, ShellEffect, ShellFrame, WakePolicy,
};

#[derive(Resource)]
struct ShellRuntime {
    model: ShellModel,
    clock_text: String,
    clock_deadline: Option<Duration>,
}

/// Current renderer-neutral output. This is the sole presentation input.
#[derive(Resource, Clone, Debug)]
pub struct ShellFrameState(pub ShellFrame);

/// Semantic effects emitted during the current model update only.
#[derive(Resource, Clone, Debug, Default)]
pub struct ShellEffects(pub Vec<ShellEffect>);

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
            .init_resource::<ShellEffects>()
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

/// Replace the singleton v1 model after its selected output disappears.
///
/// The layer host drains and destroys the old surfaces first, then calls this
/// before mapping fresh surfaces on the replacement output.
pub fn replace_shell_model(world: &mut World, model: ShellModel) {
    let frame = ShellFrame::from_model(&model);
    *world.resource_mut::<ShellRuntime>() = ShellRuntime {
        model,
        clock_text: String::new(),
        clock_deadline: None,
    };
    world.resource_mut::<ShellFrameState>().0 = frame;
    if let Some(mut modes) = world.get_resource_mut::<QuoinCommittedMotionModes>() {
        *modes = QuoinCommittedMotionModes::hidden();
    }
}

fn update_model(
    time: Res<Time<Real>>,
    mut commands: MessageReader<ShellCommand>,
    mut runtime: ResMut<ShellRuntime>,
    mut frame: ResMut<ShellFrameState>,
    mut effects: ResMut<ShellEffects>,
) {
    let now = time.elapsed();
    effects.0.clear();
    for command in commands.read() {
        if command.output != *runtime.model.output() {
            continue;
        }
        let at = command.at.clamp(runtime.model.last_update(), now);
        match &command.kind {
            ShellCommandKind::Geometry(size) => runtime.model.set_geometry(*size),
            ShellCommandKind::Corner(event) => {
                if let Ok(Some(update)) = runtime.model.corner_event(at, *event)
                    && let Some(effect) = update.effect
                {
                    effects.0.push(ShellEffect {
                        edge: event.corner().summoned_edge(),
                        effect,
                    });
                }
            }
            ShellCommandKind::Panel { edge, input } => {
                if let Ok(update) = runtime.model.panel_input(*edge, at, *input)
                    && let Some(effect) = update.effect
                {
                    effects.0.push(ShellEffect {
                        edge: *edge,
                        effect,
                    });
                }
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
    if let Ok(updates) = runtime.model.tick(now) {
        for (edge, update) in Edge::ALL.into_iter().zip(updates) {
            if let Some(effect) = update.effect {
                effects.0.push(ShellEffect { edge, effect });
            }
        }
    }
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
            next_frame.wake_deadline = Some(
                next_frame
                    .wake_deadline
                    .map_or(deadline, |current| current.min(deadline)),
            );
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{
        Corner, CornerEvent, CornerTrigger, LogicalSize, OutputKey, PanelEffect, PanelMode,
        RevealTrigger,
    };
    use bevy::MinimalPlugins;
    use bevy::prelude::App;

    fn app() -> App {
        let output = OutputKey::new("DP-1").unwrap();
        let model = ShellModel::new(
            output,
            LogicalSize::new(1_000.0, 800.0).unwrap(),
            Duration::ZERO,
            Duration::from_millis(800),
            Duration::from_millis(200),
        )
        .unwrap();
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, ShellRuntimePlugin::new(model)));
        app
    }

    #[test]
    fn foreign_output_corner_is_ignored_and_effects_are_current_update_only() {
        let mut app = app();
        let entered = CornerEvent::Entered {
            corner: Corner::TopLeft,
            dwell: Duration::from_secs(5),
            trigger: CornerTrigger::Compositor,
        };
        app.world_mut().write_message(ShellCommand {
            output: OutputKey::new("HDMI-A-1").unwrap(),
            at: Duration::ZERO,
            kind: ShellCommandKind::Corner(entered),
        });
        app.update();
        assert_eq!(
            app.world()
                .resource::<ShellFrameState>()
                .0
                .panel(Edge::Left)
                .mode,
            PanelMode::Hidden
        );
        assert!(app.world().resource::<ShellEffects>().0.is_empty());

        app.world_mut().write_message(ShellCommand {
            output: OutputKey::new("DP-1").unwrap(),
            at: Duration::ZERO,
            kind: ShellCommandKind::Corner(entered),
        });
        app.update();
        assert_eq!(
            app.world().resource::<ShellEffects>().0,
            vec![ShellEffect {
                edge: Edge::Left,
                effect: PanelEffect::Reveal {
                    trigger: RevealTrigger::Corner,
                },
            }]
        );
        app.update();
        assert!(app.world().resource::<ShellEffects>().0.is_empty());
    }

    #[test]
    fn output_model_replacement_reseeds_the_committed_motion_latch() {
        let mut app = app();
        let mut modes = QuoinCommittedMotionModes::hidden();
        modes.set(Edge::Left, PanelMode::Pinned);
        app.insert_resource(modes);
        let replacement = ShellModel::new(
            OutputKey::new("HDMI-A-1").unwrap(),
            LogicalSize::new(1_920.0, 1_080.0).unwrap(),
            Duration::ZERO,
            Duration::from_millis(800),
            Duration::from_millis(200),
        )
        .unwrap();
        replace_shell_model(app.world_mut(), replacement);
        assert_eq!(
            app.world()
                .resource::<QuoinCommittedMotionModes>()
                .get(Edge::Left),
            PanelMode::Hidden
        );
    }
}
