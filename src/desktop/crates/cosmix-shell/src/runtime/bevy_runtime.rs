//! Bevy adapter around the pure shell model.
//!
//! Input systems emit semantic commands; this adapter is the only code which
//! mutates [`ShellModel`]. Presentation and hosts consume [`ShellFrameState`]
//! and therefore cannot reach back into model or window state.

use bevy::app::{App, AppExit, Plugin, Update};
use bevy::ecs::message::{MessageReader, MessageWriter};
use bevy::ecs::schedule::{IntoScheduleConfigs, SystemSet};
use bevy::prelude::{Mut, Res, ResMut, Resource, Time, World};
use bevy::time::Real;
use std::time::{Duration, SystemTime};

use crate::chrome::QuoinCommittedMotionModes;
use crate::core::{Edge, PanelInput, ShellModel, SubPanelRegistry, SubPanelSeat};
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

/// The process-wide sub-panel registry: every live sub-panel name with its
/// `(output, edge, owner)` seat, held above the one live [`ShellModel`]
/// because names are globally unique across outputs while a model is not.
///
/// Inserted by [`ShellRuntimePlugin`], so exactly one instance exists per
/// process (the Quoin host and each embedded host run one runtime plugin);
/// output replacement swaps the model underneath without touching it. Hosts
/// feed it through [`reseat_shell_subpanel`] / [`forget_shell_subpanel`];
/// citizen disconnect is applied with [`remove_all_owned_subpanels`].
#[derive(Resource, Clone, Debug, Default)]
pub struct SubPanelRegistryState(pub SubPanelRegistry);

/// Semantic effects emitted during the current model update only.
/// The first list records panel transitions; the second records edges whose
/// active page changed. Rejected and no-op page selections emit nothing.
#[derive(Resource, Clone, Debug, Default)]
pub struct ShellEffects(pub Vec<ShellEffect>, pub Vec<Edge>);

/// An embedded shell delegates session exit to its host's lifecycle owner.
/// Without this resource the standalone shell emits Bevy's normal AppExit.
#[derive(Resource, Clone)]
pub struct ShellQuitHandler(pub std::sync::Arc<dyn Fn() + Send + Sync>);

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
            .init_resource::<SubPanelRegistryState>()
            .add_message::<super::ShellResizeResult>()
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
/// Live pins, pages and dimensions win over the replacement factory's seeds.
/// The layer host drains and destroys the old surfaces first, then calls this
/// before mapping fresh surfaces on the replacement output.
pub fn replace_shell_model(world: &mut World, mut model: ShellModel) {
    model.carry_live_state(&world.resource::<ShellRuntime>().model);
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

/// Apply an authored page extent without persisting a pointer resize preference.
pub fn set_page_thickness(world: &mut World, edge: Edge, thickness: f32) {
    let Some(mut runtime) = world.get_resource_mut::<ShellRuntime>() else {
        return;
    };
    // Authored page sizes are not pointer-grip gestures (which have a 500px cap).
    if runtime.model.restore_thickness(edge, thickness).is_ok() {
        let frame = ShellFrame::from_model(&runtime.model);
        world.resource_mut::<ShellFrameState>().0 = frame;
    }
}

/// Update a dynamic carousel while retaining its active page when possible.
pub fn set_shell_pages(world: &mut World, edge: Edge, ids: Vec<String>, select: Option<&str>) {
    let Some(mut runtime) = world.get_resource_mut::<ShellRuntime>() else {
        return;
    };
    let selected = select
        .map(str::to_owned)
        .or_else(|| runtime.model.carousel(edge).active_id().map(str::to_owned));
    let Ok(mut carousel) = crate::core::Carousel::new(ids) else {
        return;
    };
    if let Some(selected) = selected {
        carousel.select_id(&selected);
    }
    runtime.model.set_carousel(edge, carousel);
    let frame = ShellFrame::from_model(&runtime.model);
    world.resource_mut::<ShellFrameState>().0 = frame;
}

/// Record or refresh the sub-panel registry seat for a page a host just
/// mounted under `name`, owned by `owner` on the live frame's output.
///
/// A no-op until the registry exists (every runtime plugin inserts one), so
/// hosts and tests without the resource skip the feed cleanly.
pub fn reseat_shell_subpanel(world: &mut World, edge: Edge, name: &str, owner: &str) {
    let Some(output) = world
        .get_resource::<ShellFrameState>()
        .map(|frame| frame.0.geometry.output.clone())
    else {
        return;
    };
    if let Some(mut registry) = world.get_resource_mut::<SubPanelRegistryState>() {
        registry.0.reseat(name, output, edge, owner);
    }
}

/// Drop the sub-panel registry seat for a page a host unmounted, without
/// touching any carousel (the host unmount drives its own page removal).
pub fn forget_shell_subpanel(world: &mut World, name: &str) {
    if let Some(mut registry) = world.get_resource_mut::<SubPanelRegistryState>() {
        registry.0.forget(name);
    }
}

/// Citizen disconnect: remove every sub-panel seat `owner` holds plus its
/// carousel content in the live model, landing per the carousel's removal
/// rule, and refresh the frame. Returns the removed seats.
///
/// Runs before the scene reconcile that tears the owner's mounted content
/// down; the carousel removal decides the landing first so the page-list
/// rebuild the unmount performs preserves it.
pub fn remove_all_owned_subpanels(world: &mut World, owner: &str) -> Vec<SubPanelSeat> {
    if !world.contains_resource::<SubPanelRegistryState>()
        || !world.contains_resource::<ShellRuntime>()
    {
        return Vec::new();
    }
    let seats = world.resource_scope(|world, mut runtime: Mut<ShellRuntime>| {
        let Some(mut registry) = world.get_resource_mut::<SubPanelRegistryState>() else {
            return Vec::new();
        };
        registry.0.remove_all_owned(owner, &mut runtime.model)
    });
    if !seats.is_empty()
        && let Some(mut runtime) = world.get_resource_mut::<ShellRuntime>()
    {
        let frame = ShellFrame::from_model(&runtime.model);
        world.resource_mut::<ShellFrameState>().0 = frame;
    }
    seats
}

fn update_model(
    time: Res<Time<Real>>,
    mut commands: MessageReader<ShellCommand>,
    mut runtime: ResMut<ShellRuntime>,
    mut frame: ResMut<ShellFrameState>,
    mut effects: ResMut<ShellEffects>,
    mut replies: (
        MessageWriter<AppExit>,
        MessageWriter<super::ShellResizeResult>,
    ),
    quit_handler: Option<Res<ShellQuitHandler>>,
) {
    let now = time.elapsed();
    effects.0.clear();
    effects.1.clear();
    for command in commands.read() {
        if command.output != *runtime.model.output() {
            if let ShellCommandKind::ResizeChecked {
                edge,
                thickness_px,
                request_id,
            } = command.kind
            {
                replies.1.write(super::ShellResizeResult {
                    request_id,
                    edge,
                    requested: thickness_px,
                    max: runtime.model.max_thickness(edge),
                    result: Err(super::ShellResizeError::OutputChanged),
                });
            }
            continue;
        }
        let at = command.at.clamp(runtime.model.last_update(), now);
        match &command.kind {
            // Scene content is owned by the host adapter; it has no motion effect.
            ShellCommandKind::Scene(_) => {}
            ShellCommandKind::Resize { edge, thickness_px } => {
                let thickness_px = if thickness_px.is_finite() {
                    thickness_px.min(runtime.model.max_thickness(*edge))
                } else {
                    *thickness_px
                };
                if let Err(error) = runtime.model.resize_thickness(*edge, thickness_px) {
                    bevy::log::warn!("panel drag rejected: {error}");
                }
            }
            ShellCommandKind::ResizeCommit { edge, thickness_px }
            | ShellCommandKind::ResizeChecked {
                edge, thickness_px, ..
            } => {
                // Atomic scripted resize: start records the pre-resize
                // thickness (so settled_thickness_px is correct if the apply
                // is rejected), apply, then complete — which settles the new
                // value and emits ResizeCompleted so persistence writes once,
                // exactly as the grip gesture does on release.
                let _ = runtime
                    .model
                    .panel_input(*edge, at, PanelInput::ResizeStarted);
                let result = runtime.model.resize_thickness(*edge, *thickness_px);
                if let ShellCommandKind::ResizeChecked { request_id, .. } = command.kind {
                    replies.1.write(super::ShellResizeResult {
                        request_id,
                        edge: *edge,
                        requested: *thickness_px,
                        max: runtime.model.max_thickness(*edge),
                        result: result.map_err(super::ShellResizeError::Configuration),
                    });
                }
                if result.is_ok()
                    && let Ok(update) =
                        runtime
                            .model
                            .panel_input(*edge, at, PanelInput::ResizeCompleted)
                    && let Some(effect) = update.effect
                {
                    effects.0.push(ShellEffect {
                        edge: *edge,
                        effect,
                    });
                } else {
                    // Rejected size: cancel so the recorded start is dropped
                    // and no stale resize_start lingers.
                    let _ = runtime
                        .model
                        .panel_input(*edge, at, PanelInput::ResizeCancelled);
                }
            }
            ShellCommandKind::Quit => {
                if let Some(handler) = &quit_handler {
                    (handler.0)();
                } else {
                    replies.0.write(AppExit::Success);
                }
            }
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
                let before = carousel.active_index();
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
                if carousel.active_index() != before {
                    effects.1.push(*edge);
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
            runtime.clock_text = local_clock_text_at(SystemTime::now());
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

fn local_clock_text_at(wall: SystemTime) -> String {
    // Honour the machine timezone (or its process TZ override). Numeric
    // offsets remain unambiguous across daylight saving and half-hour zones.
    let local = chrono::DateTime::<chrono::Local>::from(wall);
    local.format("%H:%M:%S %:z").to_string()
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
    #[test]
    fn fast_drag_lands_at_budget_and_checked_resize_reports_refusal() {
        let mut app = app();
        let output = {
            let mut runtime = app.world_mut().resource_mut::<ShellRuntime>();
            let model = &mut runtime.model;
            model.set_geometry(LogicalSize::new(600.0, 600.0).unwrap());
            model.restore_thickness(Edge::Right, 350.0).unwrap();
            model
                .panel_input(Edge::Right, Duration::ZERO, PanelInput::Dock)
                .unwrap();
            assert_eq!(model.max_thickness(Edge::Left), 249.0);
            model.output().clone()
        };
        app.world_mut().write_message(ShellCommand {
            output: output.clone(),
            at: Duration::ZERO,
            kind: ShellCommandKind::Resize {
                edge: Edge::Left,
                thickness_px: 500.0,
            },
        });
        app.update();
        assert_eq!(
            app.world()
                .resource::<ShellFrameState>()
                .0
                .panel(Edge::Left)
                .thickness_px,
            249.0
        );
        app.world_mut().write_message(ShellCommand {
            output,
            at: Duration::ZERO,
            kind: ShellCommandKind::ResizeChecked {
                edge: Edge::Left,
                thickness_px: 300.0,
                request_id: 42,
            },
        });
        app.update();
        let mut results = app
            .world_mut()
            .resource_mut::<bevy::ecs::message::Messages<super::super::ShellResizeResult>>();
        let result = results.drain().next().unwrap();
        assert_eq!(result.request_id, 42);
        assert!(result.result.is_err());
        assert_eq!(result.max, 249.0);
    }
    #[test]
    fn resizing_pinned_panel_cannot_exceed_output_budget() {
        let mut app = app();
        let mut runtime = app.world_mut().resource_mut::<ShellRuntime>();
        let model = &mut runtime.model;
        model.set_geometry(LogicalSize::new(800.0, 600.0).unwrap());
        for (edge, thickness) in [(Edge::Left, 798.0), (Edge::Right, 1.0)] {
            model.restore_thickness(edge, thickness).unwrap();
            model
                .panel_input(edge, Duration::ZERO, PanelInput::Dock)
                .unwrap();
        }
        assert_eq!(model.panel(Edge::Left).exclusive_zone_px, 798.0);
        assert_eq!(model.panel(Edge::Right).exclusive_zone_px, 1.0);
        assert_eq!(
            model.resize_thickness(Edge::Right, 120.0),
            Err(crate::core::PanelConfigError::ThicknessBudget {
                edge: Edge::Right,
                requested: 120.0,
                max: 1.0
            })
        );
        assert_eq!(model.panel(Edge::Right).thickness_px, 1.0);
        assert_eq!(model.panel(Edge::Right).exclusive_zone_px, 1.0);
    }

    #[test]
    fn unmounting_last_dynamic_page_clears_carousel() {
        let mut app = app();
        set_shell_pages(app.world_mut(), Edge::Top, vec!["scene-only".into()], None);
        set_shell_pages(app.world_mut(), Edge::Top, vec![], None);
        let runtime = app.world().resource::<ShellRuntime>();
        assert!(runtime.model.carousel(Edge::Top).page_ids().is_empty());
        assert_eq!(runtime.model.carousel(Edge::Top).active_id(), None);
    }

    #[test]
    fn authored_opposing_panels_fit_on_pin_and_output_change() {
        let mut app = app();
        for edge in [Edge::Left, Edge::Right] {
            set_page_thickness(app.world_mut(), edge, 100_000.0);
            let mut runtime = app.world_mut().resource_mut::<ShellRuntime>();
            runtime
                .model
                .panel_input(edge, Duration::ZERO, PanelInput::Dock)
                .unwrap();
            assert!(
                runtime.model.panel(Edge::Left).exclusive_zone_px
                    + runtime.model.panel(Edge::Right).exclusive_zone_px
                    < runtime.model.geometry().width()
            );
        }
        let mut runtime = app.world_mut().resource_mut::<ShellRuntime>();
        for width in [800.0, 320.0, 100.0] {
            runtime
                .model
                .set_geometry(LogicalSize::new(width, 600.0).unwrap());
            let left = runtime.model.panel(Edge::Left);
            let right = runtime.model.panel(Edge::Right);
            assert!(left.exclusive_zone_px + right.exclusive_zone_px < width);
            assert!(left.thickness_px > 0.0 && right.thickness_px > 0.0);
        }
    }
    #[test]
    fn local_clock_timezone_probe() {
        let Ok(expected) = std::env::var("QUOIN_CLOCK_TEST_EXPECTED") else {
            return;
        };
        let wall = std::time::UNIX_EPOCH + Duration::from_secs(86_399);
        assert_eq!(local_clock_text_at(wall), expected);
    }

    #[test]
    fn local_clock_uses_process_timezone() {
        // Separate processes avoid mutating TZ in a multithreaded test runner
        // and exercise the same local-time conversion as the visible clock.
        for (zone, expected) in [
            ("UTC", "23:59:59 +00:00"),
            ("Australia/Brisbane", "09:59:59 +10:00"),
            ("Asia/Kolkata", "05:29:59 +05:30"),
            ("America/New_York", "18:59:59 -05:00"),
        ] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "runtime::bevy_runtime::tests::local_clock_timezone_probe",
                    "--nocapture",
                ])
                .env("TZ", zone)
                .env("QUOIN_CLOCK_TEST_EXPECTED", expected)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "zone={zone}: {}",
                String::from_utf8_lossy(&output.stdout)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        }
    }

    use crate::core::{
        Corner, CornerEvent, CornerTrigger, LogicalSize, OutputKey, PanelEffect, PanelMode,
        RevealTrigger,
    };
    use crate::runtime::{ShellSemanticVerb, semantic_shell_command};
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
    fn embedded_quit_uses_host_lifecycle_without_exiting_render_app() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let requested = Arc::new(AtomicBool::new(false));
        let flag = requested.clone();
        let mut app = app();
        app.insert_resource(ShellQuitHandler(Arc::new(move || {
            flag.store(true, Ordering::SeqCst);
        })));
        app.world_mut().write_message(ShellCommand {
            output: OutputKey::new("DP-1").unwrap(),
            at: Duration::ZERO,
            kind: ShellCommandKind::Quit,
        });
        app.update();
        assert!(requested.load(Ordering::SeqCst));
        assert!(app.should_exit().is_none());
    }

    #[test]
    fn resize_commit_settles_the_new_thickness_and_emits_the_persist_effect() {
        let mut app = app();
        let output = OutputKey::new("DP-1").unwrap();
        app.world_mut().write_message(ShellCommand {
            output: output.clone(),
            at: Duration::ZERO,
            kind: ShellCommandKind::ResizeCommit {
                edge: Edge::Left,
                thickness_px: 260.0,
            },
        });
        app.update();
        let panel = app
            .world()
            .resource::<ShellFrameState>()
            .0
            .panel(Edge::Left)
            .clone();
        // Atomic commit settles the NEW value (not the pre-resize one), which
        // is what persistence records.
        assert_eq!(panel.settled_thickness_px, 260.0);
        assert!(!panel.resize_active, "no gesture is left open");
        // The ResizeCompleted effect fired, so persist_transitions writes once.
        assert!(
            app.world()
                .resource::<ShellEffects>()
                .0
                .iter()
                .any(|e| { e.edge == Edge::Left && e.effect == PanelEffect::ResizeCompleted })
        );

        // An out-of-range commit leaves the settled size and opens no gesture.
        app.world_mut().write_message(ShellCommand {
            output,
            at: Duration::ZERO,
            kind: ShellCommandKind::ResizeCommit {
                edge: Edge::Left,
                thickness_px: 9_000.0,
            },
        });
        app.update();
        let panel = app
            .world()
            .resource::<ShellFrameState>()
            .0
            .panel(Edge::Left)
            .clone();
        assert_eq!(
            panel.settled_thickness_px, 260.0,
            "rejected size is a no-op"
        );
        assert!(
            !panel.resize_active,
            "a rejected commit leaves no open gesture"
        );
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
        modes.set(Edge::Left, PanelMode::Docked);
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

    #[test]
    fn output_migration_carries_live_pin_page_and_thickness() {
        let mut app = app();
        {
            let mut runtime = app.world_mut().resource_mut::<ShellRuntime>();
            runtime.model.restore_thickness(Edge::Left, 137.0).unwrap();
            runtime.model.set_carousel(
                Edge::Left,
                crate::core::Carousel::new(["nav", "places"]).unwrap(),
            );
            runtime.model.carousel_mut(Edge::Left).select_id("places");
            runtime
                .model
                .panel_input(Edge::Left, Duration::ZERO, crate::core::PanelInput::Dock)
                .unwrap();
        }
        let mut replacement = ShellModel::new(
            OutputKey::new("HDMI-A-1").unwrap(),
            LogicalSize::new(1920.0, 1080.0).unwrap(),
            Duration::ZERO,
            Duration::from_millis(800),
            Duration::from_millis(200),
        )
        .unwrap();
        replacement.start_intro(Duration::from_secs(2));
        replace_shell_model(app.world_mut(), replacement);
        let frame = &app.world().resource::<ShellFrameState>().0;
        assert_eq!(frame.geometry.output.as_str(), "HDMI-A-1");
        assert_eq!(frame.panel(Edge::Left).mode, PanelMode::Docked);
        assert_eq!(frame.panel(Edge::Left).thickness_px, 137.0);
        assert_eq!(
            frame.panel(Edge::Left).active_page_id.as_deref(),
            Some("places")
        );
        assert_eq!(frame.panel(Edge::Right).mode, PanelMode::Hidden);
    }

    #[test]
    fn only_changed_pages_emit_persistence_effects() {
        let mut app = app();
        app.world_mut()
            .resource_mut::<ShellRuntime>()
            .model
            .set_carousel(
                Edge::Left,
                crate::core::Carousel::new(["nav", "places"]).unwrap(),
            );
        for (id, changed) in [
            ("unknown", false),
            ("nav", false),
            ("places", true),
            ("places", false),
        ] {
            app.world_mut().write_message(semantic_shell_command(
                OutputKey::new("DP-1").unwrap(),
                Duration::ZERO,
                Edge::Left,
                ShellSemanticVerb::PageSet(id.into()),
            ));
            app.update();
            assert_eq!(
                !app.world().resource::<ShellEffects>().1.is_empty(),
                changed
            );
        }
    }

    #[test]
    fn quit_command_emits_success_only_for_selected_output() {
        let mut app = app();
        for (output, expected) in [("foreign", None), ("DP-1", Some(AppExit::Success))] {
            app.world_mut().write_message(ShellCommand {
                output: OutputKey::new(output).unwrap(),
                at: Duration::ZERO,
                kind: ShellCommandKind::Quit,
            });
            app.update();
            assert_eq!(app.should_exit(), expected);
        }
    }

    #[test]
    fn semantic_verbs_reproduce_direct_shell_command_results() {
        let cases = [
            (
                ShellSemanticVerb::PanelShow,
                ShellCommandKind::Panel {
                    edge: Edge::Left,
                    input: crate::core::PanelInput::Reveal,
                },
            ),
            (
                ShellSemanticVerb::PanelHide,
                ShellCommandKind::Panel {
                    edge: Edge::Left,
                    input: crate::core::PanelInput::Hide,
                },
            ),
            // `PanelToggle` maps to core `Toggle`, NOT to `Reveal`: the
            // direction binds at Model time. This fixture starts Hidden,
            // where the two coincide, so the row records the mapping rather
            // than discriminating it — the tests that DO discriminate are
            // `semantic_toggle_on_a_mapped_panel_reproduces_direct_hide` and
            // `semantic_toggle_reopens_a_panel_animating_shut` below.
            (
                ShellSemanticVerb::PanelToggle,
                ShellCommandKind::Panel {
                    edge: Edge::Left,
                    input: crate::core::PanelInput::Toggle,
                },
            ),
            (
                ShellSemanticVerb::PanelPin,
                ShellCommandKind::Panel {
                    edge: Edge::Left,
                    input: crate::core::PanelInput::Dock,
                },
            ),
            (
                ShellSemanticVerb::PanelUnpin,
                ShellCommandKind::Panel {
                    edge: Edge::Left,
                    input: crate::core::PanelInput::Release,
                },
            ),
            (
                ShellSemanticVerb::PageNext,
                ShellCommandKind::Carousel {
                    edge: Edge::Left,
                    input: CarouselInput::Next,
                },
            ),
            (
                ShellSemanticVerb::PagePrevious,
                ShellCommandKind::Carousel {
                    edge: Edge::Left,
                    input: CarouselInput::Previous,
                },
            ),
            (
                ShellSemanticVerb::PageSet("places".to_owned()),
                ShellCommandKind::Carousel {
                    edge: Edge::Left,
                    input: CarouselInput::SelectId("places".to_owned()),
                },
            ),
        ];
        for (verb, direct_kind) in cases {
            let mut adapted = app();
            let mut direct = app();
            let frame = adapted.world().resource::<ShellFrameState>().0.clone();
            let output = frame.geometry.output.clone();
            adapted.world_mut().write_message(semantic_shell_command(
                output.clone(),
                Duration::ZERO,
                Edge::Left,
                verb,
            ));
            direct.world_mut().write_message(ShellCommand {
                output,
                at: Duration::ZERO,
                kind: direct_kind,
            });
            adapted.update();
            direct.update();
            assert_eq!(
                adapted.world().resource::<ShellFrameState>().0,
                direct.world().resource::<ShellFrameState>().0,
            );
            assert_eq!(
                adapted.world().resource::<ShellEffects>().0,
                direct.world().resource::<ShellEffects>().0,
            );
        }
    }

    /// The other toggle direction: on a MAPPED panel the semantic toggle must
    /// reproduce a direct Hide, not a second Reveal.
    #[test]
    fn semantic_toggle_on_a_mapped_panel_reproduces_direct_hide() {
        let mut adapted = app();
        let mut direct = app();
        for app in [&mut adapted, &mut direct] {
            let output = app
                .world()
                .resource::<ShellFrameState>()
                .0
                .geometry
                .output
                .clone();
            app.world_mut().write_message(ShellCommand {
                output,
                at: Duration::ZERO,
                kind: ShellCommandKind::Panel {
                    edge: Edge::Left,
                    input: crate::core::PanelInput::Reveal,
                },
            });
            app.update();
        }
        let frame = adapted.world().resource::<ShellFrameState>().0.clone();
        assert!(frame.panel(Edge::Left).mapped, "precondition: mapped");
        let output = frame.geometry.output.clone();
        adapted.world_mut().write_message(semantic_shell_command(
            output.clone(),
            Duration::ZERO,
            Edge::Left,
            ShellSemanticVerb::PanelToggle,
        ));
        direct.world_mut().write_message(ShellCommand {
            output,
            at: Duration::ZERO,
            kind: ShellCommandKind::Panel {
                edge: Edge::Left,
                input: crate::core::PanelInput::Hide,
            },
        });
        adapted.update();
        direct.update();
        assert_eq!(
            adapted.world().resource::<ShellFrameState>().0,
            direct.world().resource::<ShellFrameState>().0,
        );
        assert_eq!(
            adapted.world().resource::<ShellEffects>().0,
            direct.world().resource::<ShellEffects>().0,
        );
    }

    /// A panel animating shut still has `mapped == true` but mode `Hidden`.
    /// The toggle direction binds at Model time against the mode, so the user
    /// can toggle a mid-conceal panel straight back open — the old
    /// snapshot-keyed adapter sent `Hide` here (a no-op) and locked the panel
    /// out until the unmap completed.
    #[test]
    fn semantic_toggle_reopens_a_panel_animating_shut() {
        let mut app = app();
        let output = app
            .world()
            .resource::<ShellFrameState>()
            .0
            .geometry
            .output
            .clone();
        for (at, verb) in [
            (Duration::ZERO, ShellSemanticVerb::PanelShow),
            (Duration::from_millis(400), ShellSemanticVerb::PanelHide),
        ] {
            app.world_mut().write_message(semantic_shell_command(
                output.clone(),
                at,
                Edge::Left,
                verb,
            ));
            app.update();
        }
        // Mid-conceal: the panel is still mapped but semantically Hidden.
        // Assert it, or a fixture timing change silently degrades this into a
        // plain toggle-from-Hidden and stops testing the thing it names.
        {
            let panel = app
                .world()
                .resource::<ShellFrameState>()
                .0
                .panel(Edge::Left);
            assert!(panel.mapped, "precondition: mid-conceal panel is mapped");
            assert!(!panel.transient_revealed);
            assert_eq!(
                panel.mode,
                PanelMode::Hidden,
                "precondition: mid-conceal panel is semantically Hidden"
            );
        }
        app.world_mut().write_message(semantic_shell_command(
            output.clone(),
            Duration::from_millis(450),
            Edge::Left,
            ShellSemanticVerb::PanelToggle,
        ));
        app.update();
        let panel = app
            .world()
            .resource::<ShellFrameState>()
            .0
            .panel(Edge::Left)
            .clone();
        assert_eq!(
            panel.mode,
            PanelMode::Hidden,
            "toggle must reopen a mid-conceal panel"
        );
        assert!(panel.transient_revealed);
        assert!(panel.mapped);
    }

    /// Two toggles drained in one Bus batch both apply within one update; with
    /// the direction bound at Model time they net to identity instead of to a
    /// single toggle read off the same stale frame.
    #[test]
    fn two_semantic_toggles_in_one_batch_net_to_identity() {
        let mut app = app();
        let output = app
            .world()
            .resource::<ShellFrameState>()
            .0
            .geometry
            .output
            .clone();
        app.world_mut().write_message(semantic_shell_command(
            output.clone(),
            Duration::ZERO,
            Edge::Left,
            ShellSemanticVerb::PanelShow,
        ));
        app.update();
        for _ in 0..2 {
            app.world_mut().write_message(semantic_shell_command(
                output.clone(),
                Duration::from_millis(400),
                Edge::Left,
                ShellSemanticVerb::PanelToggle,
            ));
        }
        app.update();
        assert_eq!(
            app.world()
                .resource::<ShellFrameState>()
                .0
                .panel(Edge::Left)
                .mode,
            PanelMode::Hidden,
            "toggle+toggle in one batch must be identity, not one toggle"
        );
        assert!(
            app.world()
                .resource::<ShellFrameState>()
                .0
                .panel(Edge::Left)
                .transient_revealed
        );
    }
}
