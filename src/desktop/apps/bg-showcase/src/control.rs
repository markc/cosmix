//! Bounded native ABP control for the input-free Boing scene.

use bevy::prelude::*;
use cosmix_bg_showcase::boids;
use ctk::bus::BusBridge;
use serde_json::json;
use std::time::{Duration, Instant};

#[derive(Resource, Default)]
pub(super) struct KickState {
    last: Option<Instant>,
    kicks: u64,
}

pub fn configure(app: &mut App) {
    boids::bus::configure_named(app, "bg-showcase");
    app.init_resource::<KickState>().add_systems(
        Update,
        (boids::bus::service, service)
            .chain()
            .run_if(resource_exists::<BusBridge>)
            .before(super::reconcile),
    );
}

fn valid_empty_body(body: &str) -> bool {
    body.len() <= 64
        && (body.trim().is_empty()
            || serde_json::from_str::<serde_json::Value>(body)
                .is_ok_and(|value| value.as_object().is_some_and(|object| object.is_empty())))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn service(
    bridge: Res<BusBridge>,
    mut requests: ResMut<boids::bus::OtherRequests>,
    mut options: ResMut<super::Options>,
    mut commands: Commands,
    instances: Res<super::Instances>,
    mut state: ResMut<KickState>,
    mut simulations: NonSendMut<super::boing::Simulations>,
    mut presentation: ResMut<cosmix_shell_host::scene::SceneControl>,
) {
    for request in requests.0.drain(..) {
        let now = Instant::now();
        let (rc, result) = match request.command.as_str() {
            "background.list" => (
                0,
                json!({"scenes":[
                    {"id":"boing","title":"Boing","camera":true},
                    {"id":"bloom","title":"Bloom","camera":true},
                    {"id":"shapes","title":"Shapes","camera":true},
                    {"id":"boids","title":"Boids","camera":false}
                ]}),
            ),
            "background.status" => (
                0,
                json!({"scene":options.scene.name(),
                "camera":options.camera.name(),"msaa":options.msaa,"outputs":instances.0.len()}),
            ),
            "background.select" if options.capture.is_some() => (
                10,
                json!({"error":"selection is disabled during a capture preview"}),
            ),
            "background.select" => match selection(&request.body) {
                Err(error) => (10, json!({"error":error})),
                Ok((scene, camera, msaa)) => {
                    if options.scene != scene || options.camera != camera {
                        options.scene = scene;
                        options.camera = camera;
                        commands.queue(super::reset_scene);
                        state.last = None;
                    }
                    if let Some(msaa) = msaa {
                        options.msaa = msaa;
                    }
                    (
                        0,
                        json!({"accepted":true,"scene":scene.name(),"camera":camera.name(),"msaa":options.msaa}),
                    )
                }
            },
            "boing.status" => (
                0,
                json!({"outputs":simulations.0.len(),"kicks":state.kicks}),
            ),
            "boing.kick" if !valid_empty_body(&request.body) => {
                (10, json!({"error":"kick accepts an empty object only"}))
            }
            "boing.kick" if options.scene != super::Scene::Boing || simulations.0.is_empty() => {
                (10, json!({"error":"no Boing output is ready"}))
            }
            "boing.kick"
                if state
                    .last
                    .is_some_and(|last| now.duration_since(last) < Duration::from_millis(250)) =>
            {
                (10, json!({"error":"kick rate limited"}))
            }
            "boing.kick" => {
                for simulation in simulations.0.values_mut() {
                    simulation.kick();
                }
                state.last = Some(now);
                state.kicks = state.kicks.saturating_add(1);
                info!(
                    outputs = simulations.0.len(),
                    kicks = state.kicks,
                    "BG_BOING_KICK"
                );
                (
                    0,
                    json!({"accepted":true,"outputs":simulations.0.len(),"kicks":state.kicks}),
                )
            }
            _ => (10, json!({"error":"unknown command"})),
        };
        if bridge
            .try_respond(&request, rc, result.to_string())
            .is_err()
        {
            warn!("BG_BOING_REPLY_QUEUE_FULL");
        }
    }
    // None means "retain the current host rate", not "restore its default".
    // Leaving boids must therefore explicitly restore the showcase CLI rate.
    if options.scene != super::Scene::Boids {
        presentation.fps_limit = Some(options.fps);
    }
}

fn selection(body: &str) -> Result<(super::Scene, super::CameraMotion, Option<u32>), &'static str> {
    if body.len() > 256 {
        return Err("selection exceeds 256 bytes");
    }
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Selection {
        scene: String,
        camera: Option<String>,
        msaa: Option<u32>,
    }
    let request: Selection =
        serde_json::from_str(body).map_err(|_| "expected scene and optional camera/msaa")?;
    if request
        .msaa
        .is_some_and(|samples| !matches!(samples, 1 | 4))
    {
        return Err("msaa must be 1 (off) or 4");
    }
    let scene = super::Scene::keeper(&request.scene).ok_or("unknown catalogue scene")?;
    let camera = match request.camera.as_deref().unwrap_or("fixed") {
        "fixed" => super::CameraMotion::Fixed,
        "orbit" => super::CameraMotion::Orbit,
        _ => return Err("camera must be fixed or orbit"),
    };
    if scene == super::Scene::Boids && camera != super::CameraMotion::Fixed {
        return Err("boids has a fixed camera");
    }
    Ok((scene, camera, request.msaa))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn kick_has_no_unbounded_or_arbitrary_impulse_input() {
        assert!(valid_empty_body(""));
        assert!(valid_empty_body(" {}\n"));
        assert!(!valid_empty_body("{\"force\":1000000}"));
        assert!(!valid_empty_body("[]"));
        assert!(!valid_empty_body(&" ".repeat(65)));
    }

    #[test]
    fn selection_is_bounded_and_only_accepts_catalogue_contract() {
        for name in ["boing", "bloom", "shapes", "boids"] {
            let (scene, camera, msaa) = selection(&json!({"scene":name}).to_string()).unwrap();
            assert_eq!(scene.name(), name);
            assert_eq!(camera, super::super::CameraMotion::Fixed);
            assert_eq!(msaa, None);
        }
        for body in [
            r#"{"scene":"coast"}"#,
            r#"{"scene":"boids","camera":"orbit"}"#,
            r#"{"scene":"boing","camera":"fly"}"#,
            r#"{"scene":"boing","fps":500}"#,
            r#"{"scene":false}"#,
            r#"{"scene":"shapes","msaa":0}"#,
            r#"{"scene":"shapes","msaa":2}"#,
            r#"{"scene":"shapes","msaa":8}"#,
            r#"{"scene":"shapes","msaa":"1"}"#,
            "[]",
        ] {
            assert!(selection(body).is_err(), "{body}");
        }
        assert!(selection(&" ".repeat(257)).is_err());
        for samples in [1, 4] {
            assert_eq!(
                selection(&json!({"scene":"shapes","msaa":samples}).to_string())
                    .unwrap()
                    .2,
                Some(samples)
            );
        }
        assert_eq!(
            selection(r#"{"scene":"shapes","camera":"orbit"}"#)
                .unwrap()
                .1,
            super::super::CameraMotion::Orbit
        );
    }
}
