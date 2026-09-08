//! Pipeline settlement observed after drawing, before KMS presentation.
//! This is frame evidence, not a complete scene-readiness or idle certificate.

use bevy::{
    render::render_resource::{CachedPipelineState, PipelineCache},
    shader::ShaderCacheError,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PipelineReadinessSnapshot {
    pub(crate) pipelines: usize,
    pub(crate) pending: usize,
    pub(crate) failed: usize,
    /// New entries or newly ready pipelines missed the draw just completed.
    /// Only a later genuine render/presentation can satisfy that work.
    pub(crate) changed_after_draw: bool,
}

pub(crate) fn process_after_draw(cache: &mut PipelineCache) -> PipelineReadinessSnapshot {
    let (before_count, before_ready) =
        cache.pipelines().fold((0, 0), |(count, ready), pipeline| {
            (
                count + 1,
                ready + usize::from(matches!(pipeline.state, CachedPipelineState::Ok(_))),
            )
        });
    // Public iterators omit the private new-pipeline queue. One supported
    // processing pass exposes it and polls completed asynchronous work. Never
    // loop until ready: compilation may depend on future shader asset events.
    cache.process_queue();
    let mut snapshot = PipelineReadinessSnapshot {
        pipelines: 0,
        pending: 0,
        failed: 0,
        changed_after_draw: false,
    };
    let mut ready = 0;
    for pipeline in cache.pipelines() {
        snapshot.pipelines += 1;
        match &pipeline.state {
            CachedPipelineState::Ok(_) => ready += 1,
            CachedPipelineState::Queued | CachedPipelineState::Creating(_) => snapshot.pending += 1,
            CachedPipelineState::Err(
                ShaderCacheError::ShaderNotLoaded(_)
                | ShaderCacheError::ShaderImportNotYetAvailable,
            ) => snapshot.pending += 1,
            CachedPipelineState::Err(_) => snapshot.failed += 1,
        }
    }
    // Within this exclusive process_queue call existing Ok entries cannot
    // become unready and entries cannot disappear. Compare within this call,
    // not across frames, where shader reload can replace an existing pipeline.
    snapshot.changed_after_draw = snapshot.pipelines != before_count || ready != before_ready;
    snapshot
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::{prelude::*, render::render_resource::ComputePipelineDescriptor};

    fn shader() -> Shader {
        Shader::from_wgsl(
            "@compute @workgroup_size(1) fn main() {}",
            "readiness-test.wgsl",
        )
    }

    #[test]
    fn hidden_new_queue_is_observed_and_late_ready_work_requires_another_draw() {
        let mut cache = crate::backend::render::tests::noop_pipeline_cache();
        let handle = Handle::<Shader>::default();
        cache.set_shader(handle.id(), shader());
        let id = cache.queue_compute_pipeline(ComputePipelineDescriptor {
            shader: handle.clone(),
            entry_point: Some("main".into()),
            ..Default::default()
        });
        assert_eq!(cache.pipelines().count(), 0);
        assert_eq!(cache.waiting_pipelines().count(), 0);
        let late = process_after_draw(&mut cache);
        assert_eq!((late.pipelines, late.pending, late.failed), (1, 0, 0));
        assert!(late.changed_after_draw);
        assert!(cache.get_compute_pipeline(id).is_some());
        // Represents the next frame's ordinary pre-draw processing. The
        // observer itself never acknowledges a render or presentation.
        cache.process_queue();
        let next = process_after_draw(&mut cache);
        assert!(!next.changed_after_draw);
        assert_eq!(next.pending, 0);
        cache.set_shader(handle.id(), shader());
        let reloaded = process_after_draw(&mut cache);
        assert_eq!(
            (reloaded.pipelines, reloaded.pending, reloaded.failed),
            (1, 0, 0)
        );
        assert!(
            reloaded.changed_after_draw,
            "same-ID shader reload still needs a new draw"
        );
    }

    #[test]
    fn missing_shader_stays_pending_and_shader_arrival_settles_the_same_id() {
        let mut cache = crate::backend::render::tests::noop_pipeline_cache();
        let handle = Handle::<Shader>::default();
        let id = cache.queue_compute_pipeline(ComputePipelineDescriptor {
            shader: handle.clone(),
            entry_point: Some("main".into()),
            ..Default::default()
        });
        for _ in 0..3 {
            let waiting = process_after_draw(&mut cache);
            assert_eq!((waiting.pending, waiting.failed), (1, 0));
            assert!(cache.get_compute_pipeline(id).is_none());
        }
        cache.set_shader(handle.id(), shader());
        // Bevy first changes the cached missing-shader error back to Queued;
        // compilation happens on the following processing pass.
        let requeued = process_after_draw(&mut cache);
        assert_eq!((requeued.pending, requeued.failed), (1, 0));
        assert!(cache.get_compute_pipeline(id).is_none());
        let ready = process_after_draw(&mut cache);
        assert_eq!((ready.pipelines, ready.pending, ready.failed), (1, 0, 0));
        assert!(ready.changed_after_draw);
        assert!(cache.get_compute_pipeline(id).is_some());
    }

    #[test]
    fn permanent_shader_failure_is_not_success_or_a_missing_dependency() {
        let mut cache = crate::backend::render::tests::noop_pipeline_cache();
        let handle = Handle::<Shader>::default();
        cache.set_shader(
            handle.id(),
            Shader::from_wgsl("invalid shader", "invalid-test.wgsl"),
        );
        cache.queue_compute_pipeline(ComputePipelineDescriptor {
            shader: handle,
            entry_point: Some("main".into()),
            ..Default::default()
        });
        let failed = process_after_draw(&mut cache);
        assert_eq!((failed.pending, failed.failed), (0, 1));
        let still_failed = process_after_draw(&mut cache);
        assert_eq!((still_failed.pending, still_failed.failed), (0, 1));
    }
}
