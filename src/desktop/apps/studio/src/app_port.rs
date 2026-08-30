//! Studio's Bus app port: direct typed document/transport verbs plus CTK's
//! registered `action.invoke` and `actions.*` surface. No widget-control plugin
//! is installed. Picker-opening actions remain metadata-marked interactive and
//! direct callers to their explicit-path verb instead of opening local UI.

use bevy::app::{App, AppExit, Plugin, Update};
use bevy::ecs::message::MessageWriter;
use bevy::ecs::schedule::IntoScheduleConfigs;
use bevy::ecs::system::In;
use ctk::prelude::{
    validate_action_direct_verbs, ActionPortPlugin, BusBridgeConfig, BusBridgePlugin,
    AppPortAppExt, AppPortPlugin, AppPortReply, AppPortRequest, AppPortSystems, Mode, Scheme,
    APP_ENGINE, THEME_CHANGED_TOPIC,
};
use serde_json::{json, Map, Value};

use crate::action::{ActionApply, ActionProduce, ActionRoute, ThemeSelectionRequest};
use crate::song_bus::{
    song_load_verb, soundfont_load_verb, SongBusPolicy, SONG_LOAD_VERB, SOUNDFONT_LOAD_VERB,
};
use crate::transport_bus::{
    transport_pause_verb, transport_start_verb, transport_state_verb, transport_stop_verb,
    TRANSPORT_PAUSE_VERB, TRANSPORT_START_VERB, TRANSPORT_STATE_VERB, TRANSPORT_STOP_VERB,
};
use crate::IDENTITY;

pub(crate) const THEME_SET_VERB: &str = "app.theme.set";

pub(crate) struct StudioAppPortPlugin {
    noded_url: String,
}

impl StudioAppPortPlugin {
    pub(crate) fn new(noded_url: impl Into<String>) -> Self {
        Self {
            noded_url: noded_url.into(),
        }
    }
}

impl Plugin for StudioAppPortPlugin {
    fn build(&self, app: &mut App) {
        let service_name = format!("{}-{APP_ENGINE}-{}", IDENTITY.slug, std::process::id());
        let mut bridge = BusBridgeConfig::new(service_name, &self.noded_url);
        // build_info!() must expand HERE (the app crate) so the registered
        // provenance carries Studio's version, not ctk's.
        bridge.provenance = ctk::prelude::provenance_from_build(cosmix_buildinfo::build_info!());
        bridge.subscriptions = vec![THEME_CHANGED_TOPIC.to_string()];
        app.add_plugins((
            BusBridgePlugin::new(bridge),
            AppPortPlugin::new(IDENTITY.display_name, IDENTITY.slug),
            ActionPortPlugin,
        ));
        // Default policy denies every remote load; main.rs overrides it from
        // --song-root / --soundfont-root. init (not insert) so a real config
        // supplied afterwards wins.
        app.init_resource::<SongBusPolicy>();
        app.register_app_verb(SONG_LOAD_VERB, song_load_verb);
        app.register_app_verb(SOUNDFONT_LOAD_VERB, soundfont_load_verb);
        // Transport control (session-agnostic, no policy) + a state read.
        app.register_app_verb(TRANSPORT_START_VERB, transport_start_verb);
        app.register_app_verb(TRANSPORT_STOP_VERB, transport_stop_verb);
        app.register_app_verb(TRANSPORT_PAUSE_VERB, transport_pause_verb);
        app.register_app_verb(TRANSPORT_STATE_VERB, transport_state_verb);
        app.register_app_verb(THEME_SET_VERB, theme_set_verb);
        // App lifecycle: quit the process gracefully (vs killing it from
        // outside). Distinct from app.transport.stop (which halts playback).
        app.register_app_verb("app.quit", app_quit_verb);
        if let Err(missing) = validate_action_direct_verbs(app) {
            panic!(
                "Studio action metadata advertises unregistered direct Bus verbs: {}",
                missing.join(", ")
            );
        }
        // Keyboard/menu production and availability mirrors settle first;
        // then Bus ingress observes capture barriers and still reaches routing
        // and application in this same frame.
        app.configure_sets(
            Update,
            (ActionProduce, AppPortSystems, ActionRoute, ActionApply).chain(),
        );
    }
}

/// `app.quit`: request a graceful Bevy shutdown — the same clean exit the
/// window-close triggers, but driven over Bus. rc=0 acks the request; the
/// process winds down its RT thread + bridge on the next update.
fn app_quit_verb(
    In(_request): In<AppPortRequest>,
    mut exit: MessageWriter<AppExit>,
) -> AppPortReply {
    exit.write(AppExit::Success);
    (0, "{\"quitting\":true}".to_string())
}

fn theme_set_verb(
    In(input): In<AppPortRequest>,
    mut selections: MessageWriter<ThemeSelectionRequest>,
) -> AppPortReply {
    let payload = match request_payload(&input.request) {
        Ok(payload) => payload,
        Err(error) => return error,
    };
    let Some(scheme) = payload
        .get("scheme")
        .and_then(Value::as_str)
        .and_then(Scheme::from_name)
    else {
        return error_reply(
            "invalid_args",
            "argument scheme must name a supported theme scheme",
        );
    };
    let Some(mode) = payload
        .get("mode")
        .and_then(Value::as_str)
        .and_then(Mode::from_name)
    else {
        return error_reply("invalid_args", "argument mode must be light or dark");
    };
    selections.write(ThemeSelectionRequest { scheme, mode });
    (
        0,
        json!({"scheme": scheme.name(), "mode": mode.name()}).to_string(),
    )
}

fn request_payload(
    request: &ctk::prelude::InboundRequest,
) -> Result<Map<String, Value>, AppPortReply> {
    let raw = request
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("args"))
        .map(|(_, value)| value.as_str())
        .unwrap_or(&request.body);
    match serde_json::from_str(raw) {
        Ok(Value::Object(payload)) => Ok(payload),
        _ => Err(error_reply(
            "invalid_args",
            "request arguments must be a JSON object",
        )),
    }
}

fn error_reply(identifier: &str, detail: &str) -> AppPortReply {
    (
        10,
        json!({"error": identifier, "detail": detail}).to_string(),
    )
}

/// Explicit `--noded-url` override, or `None` — the caller falls back to
/// `ctk::prelude::resolve_noded_url()` (node.conf.mix-derived; loopback only
/// when unconfigured).
pub(crate) fn parse_noded_url(args: &[String]) -> Result<Option<String>, String> {
    let mut noded_url = None;
    let mut index = 0;
    while index < args.len() {
        if args[index] != "--noded-url" {
            index += 1;
            continue;
        }
        let value = args
            .get(index + 1)
            .filter(|value| !value.starts_with("--"))
            .ok_or_else(|| "--noded-url requires a URL".to_string())?;
        if noded_url.replace(value.clone()).is_some() {
            return Err("--noded-url may only be supplied once".into());
        }
        index += 2;
    }
    Ok(noded_url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ctk::prelude::{ActionRegistryResource, BusBridgeConfig, AppControlInfo};
    use std::collections::BTreeMap;

    #[test]
    fn provenance_reports_this_apps_version() {
        let mut app = App::new();
        app.add_plugins(StudioAppPortPlugin::new("ws://test.invalid/ws"));
        let bridge = app.world().resource::<BusBridgeConfig>();
        assert_eq!(
            bridge.provenance.version.as_deref(),
            Some(env!("CARGO_PKG_VERSION")),
            "registered provenance must carry the app's version, not ctk's"
        );
        assert!(
            option_env!("COSMIX_GIT_SHA").is_some(),
            "COSMIX_GIT_SHA absent at compile time: build.rs no longer calls cosmix_buildinfo::emit()"
        );
        assert_eq!(
            bridge.provenance.git_sha.as_deref(),
            option_env!("COSMIX_GIT_SHA"),
            "bridge provenance git sha must come from this crate's own build stamp"
        );
    }

    #[test]
    fn defaults_and_validates_noded_url() {
        assert_eq!(parse_noded_url(&[]).unwrap(), None);
        assert_eq!(
            parse_noded_url(&["--noded-url".into(), "wss://node.example/ws".into()]).unwrap(),
            Some("wss://node.example/ws".into())
        );
        assert!(parse_noded_url(&["--noded-url".into()]).is_err());
        assert!(parse_noded_url(&[
            "--noded-url".into(),
            "ws://one/ws".into(),
            "--noded-url".into(),
            "ws://two/ws".into(),
        ])
        .is_err());
    }

    #[test]
    fn describe_configuration_uses_studios_stable_identity() {
        let mut app = App::new();
        app.add_plugins(StudioAppPortPlugin::new("ws://test.invalid/ws"));

        let bridge = app.world().resource::<BusBridgeConfig>();
        assert_eq!(
            bridge.service_name,
            format!("{}-{APP_ENGINE}-{}", IDENTITY.slug, std::process::id())
        );
        assert_eq!(bridge.noded_url, "ws://test.invalid/ws");
        assert_eq!(bridge.subscriptions, [THEME_CHANGED_TOPIC]);

        let info = app.world().resource::<AppControlInfo>();
        assert_eq!(info.title, IDENTITY.display_name);
        assert_eq!(info.view, IDENTITY.slug);
        assert!(
            !app.world()
                .contains_resource::<ctk::prelude::ControlRegistry>(),
            "base port must not install widget-control authority"
        );
    }

    #[test]
    fn every_advertised_interactive_direct_verb_is_live() {
        let mut app = App::new();
        app.add_plugins(crate::action::ActionPlugin)
            .add_plugins(StudioAppPortPlugin::new("ws://test.invalid/ws"));
        assert_eq!(validate_action_direct_verbs(&app), Ok(()));

        let mut direct_verbs: Vec<_> = app
            .world()
            .resource::<ActionRegistryResource>()
            .registry()
            .iter_metadata()
            .filter_map(|meta| meta.interactive.as_ref()?.direct_verb.as_deref())
            .collect();
        direct_verbs.sort_unstable();
        assert_eq!(direct_verbs, [SONG_LOAD_VERB, SOUNDFONT_LOAD_VERB]);
    }

    #[test]
    fn theme_set_queues_the_shared_pending_selection_path() {
        let mut app = App::new();
        app.add_message::<ThemeSelectionRequest>();
        let system = app.world_mut().register_system(theme_set_verb);
        let reply = app
            .world_mut()
            .run_system_with(
                system,
                AppPortRequest {
                    request: ctk::prelude::InboundRequest {
                        connection_generation: 1,
                        from: "automation".into(),
                        command: THEME_SET_VERB.into(),
                        headers: BTreeMap::new(),
                        body: r#"{"scheme":"forest","mode":"dark"}"#.into(),
                        reply_id: Some("1".into()),
                    },
                    app_name: "studio-bevy-test".into(),
                },
            )
            .unwrap();
        assert_eq!(reply.0, 0);
        let queued: Vec<_> = app
            .world_mut()
            .resource_mut::<bevy::ecs::message::Messages<ThemeSelectionRequest>>()
            .drain()
            .collect();
        assert_eq!(
            queued,
            [ThemeSelectionRequest {
                scheme: Scheme::Forest,
                mode: Mode::Dark,
            }]
        );
    }
}
