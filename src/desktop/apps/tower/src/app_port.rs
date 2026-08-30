//! Tower's Bus identity and fixed local telemetry subscriptions.

use bevy::app::{App, Plugin};
use ctk::prelude::{BusBridgeConfig, BusBridgePlugin, AppPortPlugin, APP_ENGINE};

use crate::bus::SUBSCRIPTIONS;
use crate::identity::IDENTITY;

pub(crate) struct TowerAppPortPlugin {
    noded_url: String,
}

impl TowerAppPortPlugin {
    pub(crate) fn new(noded_url: impl Into<String>) -> Self {
        Self {
            noded_url: noded_url.into(),
        }
    }
}

impl Plugin for TowerAppPortPlugin {
    fn build(&self, app: &mut App) {
        let service_name = format!("{}-{APP_ENGINE}-{}", IDENTITY.slug, std::process::id());
        let mut bridge = BusBridgeConfig::new(service_name, &self.noded_url);
        // build_info!() must expand HERE (the app crate) so the registered
        // provenance carries Tower's version, not ctk's.
        bridge.provenance = ctk::prelude::provenance_from_build(cosmix_buildinfo::build_info!());
        bridge.subscriptions = SUBSCRIPTIONS.iter().map(ToString::to_string).collect();
        bridge.latest_topics = crate::bus::SUBSCRIPTIONS[..3]
            .iter()
            .map(ToString::to_string)
            .collect();
        bridge.observation = true;
        app.add_plugins((
            BusBridgePlugin::new(bridge),
            AppPortPlugin::new(IDENTITY.display_name, IDENTITY.slug),
        ));
    }
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

    #[test]
    fn provenance_reports_this_apps_version() {
        let mut app = App::new();
        app.add_plugins(TowerAppPortPlugin::new("ws://test.invalid/ws"));
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
    fn configures_towers_identity_and_real_local_topics() {
        let mut app = App::new();
        app.add_plugins(TowerAppPortPlugin::new("ws://test.invalid/ws"));
        let bridge = app.world().resource::<BusBridgeConfig>();
        assert_eq!(
            bridge.service_name,
            format!("{}-{APP_ENGINE}-{}", IDENTITY.slug, std::process::id())
        );
        assert_eq!(bridge.subscriptions, SUBSCRIPTIONS);
        assert!(bridge.subscriptions.contains(&"theme.changed".to_string()));
        assert_eq!(
            bridge.latest_topics,
            ["world.noded", "world.indexd", "world.musicd"]
        );
        assert!(
            bridge.observation,
            "Tower requires the dedicated observe plane"
        );
        let info = app.world().resource::<AppControlInfo>();
        assert_eq!(info.title, IDENTITY.display_name);
        assert_eq!(info.view, IDENTITY.slug);
    }
}
