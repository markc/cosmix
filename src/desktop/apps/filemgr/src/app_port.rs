//! FileMgr's Bus app port and direct desktop-theme verb.

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
use crate::IDENTITY;

pub(crate) const THEME_SET_VERB: &str = "app.theme.set";

pub(crate) struct FileMgrAppPortPlugin {
    noded_url: String,
}

impl FileMgrAppPortPlugin {
    pub(crate) fn new(noded_url: impl Into<String>) -> Self {
        Self {
            noded_url: noded_url.into(),
        }
    }
}

impl Plugin for FileMgrAppPortPlugin {
    fn build(&self, app: &mut App) {
        let service_name = format!("{}-{APP_ENGINE}-{}", IDENTITY.slug, std::process::id());
        let mut bridge = BusBridgeConfig::new(service_name, &self.noded_url);
        // build_info!() must expand HERE (the app crate) so the registered
        // provenance carries FileMgr's version, not ctk's.
        bridge.provenance = ctk::prelude::provenance_from_build(cosmix_buildinfo::build_info!());
        bridge.subscriptions = vec![THEME_CHANGED_TOPIC.to_string()];
        app.add_plugins((
            BusBridgePlugin::new(bridge),
            AppPortPlugin::new(IDENTITY.display_name, IDENTITY.slug),
            ActionPortPlugin,
        ))
        .register_app_verb(THEME_SET_VERB, theme_set_verb)
        .register_app_verb("app.quit", app_quit_verb)
        .configure_sets(
            Update,
            (ActionProduce, AppPortSystems, ActionRoute, ActionApply).chain(),
        );
        if let Err(missing) = validate_action_direct_verbs(app) {
            panic!(
                "FileMgr action metadata advertises unregistered direct Bus verbs: {}",
                missing.join(", ")
            );
        }
    }
}

fn app_quit_verb(
    In(_request): In<AppPortRequest>,
    mut exit: MessageWriter<AppExit>,
) -> AppPortReply {
    // Phase 2 accepts only in-process DnD deliveries, so no completion
    // observer survives this process. Do not synthesize an AppExit failure for
    // a worker that could still report its real result; the external bridge
    // phase must add explicit shutdown accounting before accepting deliveries
    // owned outside the process.
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
            "request body must be a JSON object",
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
    use ctk::prelude::{BusBridgeConfig, AppControlInfo};
    use std::collections::BTreeMap;

    #[test]
    fn provenance_reports_this_apps_version() {
        let mut app = App::new();
        app.add_plugins(FileMgrAppPortPlugin::new("ws://test.invalid/ws"));
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
    }

    #[test]
    fn describes_filemgrs_stable_port_identity() {
        let mut app = App::new();
        app.add_plugins(FileMgrAppPortPlugin::new("ws://test.invalid/ws"));
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
                        body: r#"{"scheme":"crimson","mode":"light"}"#.into(),
                        reply_id: Some("1".into()),
                    },
                    app_name: "filemgr-bevy-test".into(),
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
                scheme: Scheme::Crimson,
                mode: Mode::Light,
            }]
        );
    }
}
