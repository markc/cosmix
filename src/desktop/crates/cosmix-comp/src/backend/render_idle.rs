//! Live render admission. Every idle decision follows Main and requires a
//! settled, genuinely presented frame for the exact current output generation.

use super::*;
use crate::{
    render_asset_readiness::{AssetPreparationSnapshot, AssetPreparationStatus},
    render_pipeline_readiness::PipelineReadinessSnapshot,
};
use bevy::{app::AppLabel, render::extract_plugin::ExtractPlugin, time::TimeSender};

#[derive(Resource)]
pub(super) struct ContinuousRendering;

#[derive(Resource)]
struct IdleConfiguration {
    pre_extract: fn(&mut World, &mut World),
}

#[derive(Resource, Default)]
struct RenderSettlement {
    since: Option<Instant>,
    generation: Option<u64>,
}

impl RenderSettlement {
    fn observe(
        &mut self,
        generation: u64,
        unsettled: bool,
        now: Instant,
    ) -> Result<(), super::super::kms_live::KmsLiveError> {
        if self.generation != Some(generation) {
            self.generation = Some(generation);
            self.since = None;
        }
        if !unsettled {
            self.since = None;
            return Ok(());
        }
        let since = *self.since.get_or_insert(now);
        if now.saturating_duration_since(since) >= Duration::from_secs(30) {
            return Err(super::super::kms_live::KmsLiveError::Setup(
                "kms-live-render-settling-timeout: asset or pipeline preparation remained unsettled for 30s".into()));
        }
        Ok(())
    }
}

#[derive(Resource, Clone, Debug)]
struct PresentedScene {
    generation: u64,
    key: OutputKey,
    revision: Option<u64>,
    assets: Option<AssetPreparationSnapshot>,
    pipelines: Option<PipelineReadinessSnapshot>,
}

#[cfg(test)]
pub(super) fn diagnostic(app: &App, key: &OutputKey, generation: u64) -> String {
    let main = app.world();
    let world = app.sub_app(RenderApp).world();
    let source_id = crate::backend::CaptureSourceId::Kms {
        key: key.clone(),
        generation,
    };
    let targets = world.resource::<KmsRenderTargets>();
    let source = targets.sources.get(key).unwrap();
    format!(
        "config={} revision={:?} presented={:?} dma={:?} capture={} security={:?} assets={:?} lifecycle={:?} source=({},{:?},{:?},{:?},{})",
        main.contains_resource::<IdleConfiguration>(),
        main.get_resource::<crate::compositor_scene::SceneContentRevision>()
            .map(|revision| revision.0),
        main.get_resource::<PresentedScene>(),
        main.get_resource::<ImportedDmabufImages>()
            .map(ImportedDmabufImages::has_pending_render_work),
        crate::capture::kms_render_demand(main, &source_id, Instant::now()),
        main.get_resource::<NestedSecurityPresentation>()
            .map(|p| p.snapshot()),
        world
            .get_resource::<AssetPreparationStatus>()
            .map(|p| p.snapshot()),
        targets.lifecycle.state(),
        source.generation,
        source.ready_generation,
        source.current_ready_generation,
        source.pending_frame_token,
        source.pending_present.is_some()
    )
}

pub(super) fn configure(app: &mut App) {
    // Probe builds keep their per-frame instrumentation semantics.
    if app.world().contains_resource::<ContinuousRendering>() {
        return;
    }
    #[cfg(feature = "frame-capture")]
    if !app
        .get_added_plugins::<crate::frame_capture::FrameCapturePlugin>()
        .is_empty()
    {
        return;
    }
    let Some(pre_extract) = app
        .get_added_plugins::<ExtractPlugin>()
        .first()
        .map(|plugin| plugin.pre_extract)
    else {
        return;
    };
    app.insert_resource(IdleConfiguration { pre_extract });
}

pub(super) fn observe_presentations(
    app: &mut App,
    execution: LiveUpdateExecution,
    events: &[KmsRenderFrameEvent],
) -> Result<(), super::super::kms_live::KmsLiveError> {
    if !app.world().contains_resource::<IdleConfiguration>()
        || app.world().get_resource::<LiveSceneMode>() != Some(&LiveSceneMode::ClientContent)
    {
        return Ok(());
    }
    if !matches!(execution, LiveUpdateExecution::HealthyIdle { .. }) {
        app.world_mut().remove_resource::<PresentedScene>();
    }
    for event in events {
        match event {
            KmsRenderFrameEvent::FrameSubmitted {
                generation,
                key,
                scene_revision,
                asset_preparation,
                pipeline_readiness,
                ..
            } => {
                let unsettled = asset_preparation.is_none_or(|assets| {
                    assets.pending_preparations > 0 || assets.pending_removals > 0
                }) || pipeline_readiness.is_none_or(|pipelines| {
                    pipelines.pending > 0 || pipelines.failed > 0 || pipelines.changed_after_draw
                });
                app.init_resource::<RenderSettlement>();
                app.world_mut().resource_mut::<RenderSettlement>().observe(
                    *generation,
                    unsettled,
                    Instant::now(),
                )?;
                app.insert_resource(PresentedScene {
                    generation: *generation,
                    key: key.clone(),
                    revision: *scene_revision,
                    assets: *asset_preparation,
                    pipelines: *pipeline_readiness,
                });
            }
            KmsRenderFrameEvent::PresentationCancelled { .. }
            | KmsRenderFrameEvent::TerminalFailure(_) => {
                app.world_mut().remove_resource::<PresentedScene>();
            }
        }
    }
    Ok(())
}

pub(super) fn eligible(app: &App, key: &OutputKey, generation: u64, revision: Option<u64>) -> bool {
    let main = app.world();
    if !main.contains_resource::<IdleConfiguration>()
        || main.get_resource::<LiveSceneMode>() != Some(&LiveSceneMode::ClientContent)
        || main.contains_resource::<ContinuousRendering>()
    {
        return false;
    }
    let Some(presented) = main.get_resource::<PresentedScene>() else {
        return false;
    };
    if presented.key != *key
        || presented.generation != generation
        || revision.is_none()
        || presented.revision != revision
    {
        return false;
    }
    let Some(assets) = presented.assets else {
        return false;
    };
    let Some(pipelines) = presented.pipelines else {
        return false;
    };
    if assets.revision.is_none()
        || assets.tracked_types == 0
        || assets.pending_preparations != 0
        || assets.pending_removals != 0
        || pipelines.pending != 0
        || pipelines.failed != 0
        || pipelines.changed_after_draw
    {
        return false;
    }
    if main
        .get_resource::<ImportedDmabufImages>()
        .is_none_or(ImportedDmabufImages::has_pending_render_work)
        || main
            .get_resource::<NestedSecurityPresentation>()
            .is_none_or(|pending| !pending.snapshot().is_empty())
    {
        return false;
    }
    let source_id = crate::backend::CaptureSourceId::Kms {
        key: key.clone(),
        generation,
    };
    if crate::capture::kms_render_demand(main, &source_id, Instant::now()) {
        return false;
    }
    let Some(render) = app.get_sub_app(RenderApp) else {
        return false;
    };
    let world = render.world();
    if !world.contains_resource::<RenderDevice>()
        || !world.contains_resource::<TimeSender>()
        || world
            .get_resource::<AssetPreparationStatus>()
            .is_none_or(|status| status.snapshot() != assets)
    {
        return false;
    }
    let Some(commands) = world.get_resource::<KmsRenderCommands>() else {
        return false;
    };
    if commands
        .commands
        .lock()
        .map_or(true, |mut commands| commands.has_pending())
    {
        return false;
    }
    let Some(targets) = world.get_resource::<KmsRenderTargets>() else {
        return false;
    };
    if targets.lifecycle.state() != KmsRenderLifecycleState::Active
        || !targets.pending_quiescence.is_empty()
        || targets.sources.len() != 1
    {
        return false;
    }
    let Some(source) = targets.sources.get(key) else {
        return false;
    };
    source.generation == generation
        && source.ready_generation == Some(generation)
        && source.current_ready_generation == Some(generation)
        && source.pending_frame_token.is_none()
        && source.pending_scene_revision.is_none()
        && source.pending_present.is_none()
        && source.pending_resume_first_flip.is_none()
        && source.pending_presentation_timestamp.is_none()
        && source.pending_security_presentations.is_empty()
        && source.pending_capture_presentations.is_empty()
}

pub(super) fn finish_idle_update(
    app: &mut App,
) -> Result<(), super::super::kms_live::KmsLiveError> {
    let hook = app.world().resource::<IdleConfiguration>().pre_extract;
    let apps = app.sub_apps_mut();
    for (label, sub_app) in &mut apps.sub_apps {
        if *label == RenderApp.intern() {
            // Polling services completion/error callbacks without blocking on
            // GPU work. The installed pre-extract hook owns error propagation.
            sub_app
                .world()
                .resource::<RenderDevice>()
                .poll(bevy::render::render_resource::PollType::Poll)
                .map_err(|error| {
                    super::super::kms_live::KmsLiveError::Setup(format!(
                        "kms-live-idle-device-poll: {error}"
                    ))
                })?;
            hook(apps.main.world_mut(), sub_app.world_mut());
            sub_app
                .world()
                .resource::<TimeSender>()
                .0
                .try_send(Instant::now())
                .map_err(|error| {
                    super::super::kms_live::KmsLiveError::Setup(format!(
                        "kms-live-idle-time-handoff: {error}"
                    ))
                })?;
        } else {
            sub_app.extract(apps.main.world_mut());
            sub_app.update();
        }
    }
    apps.main.world_mut().clear_trackers();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_presentations_cannot_extend_unsettled_preparation_forever() {
        let started = Instant::now();
        let mut settlement = RenderSettlement::default();
        for seconds in 0..30 {
            settlement
                .observe(1, true, started + Duration::from_secs(seconds))
                .unwrap();
        }
        assert!(
            settlement
                .observe(1, true, started + Duration::from_secs(30))
                .unwrap_err()
                .to_string()
                .contains("kms-live-render-settling-timeout")
        );
        settlement
            .observe(1, false, started + Duration::from_secs(31))
            .unwrap();
        settlement
            .observe(1, true, started + Duration::from_secs(32))
            .unwrap();
        assert_eq!(settlement.since, Some(started + Duration::from_secs(32)));
        settlement
            .observe(2, true, started + Duration::from_secs(90))
            .unwrap();
        assert_eq!(
            settlement.since,
            Some(started + Duration::from_secs(90)),
            "resume has its own preparation budget"
        );
    }
}
