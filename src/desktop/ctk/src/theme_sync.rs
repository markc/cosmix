//! Event-driven convergence for the desktop-wide colour-theme selection.
//!
//! The shared theme file is the sole authority. This lane broadcasts an
//! invalidation/wake only after successful persistence so already-running
//! local apps re-read durable state immediately; focus-gained file reload
//! remains the missed-wake backstop.

use bevy::ecs::message::MessageReader;
use bevy::ecs::system::Res;
use serde::Deserialize;

use crate::bus::{BusBridge, BusMessage};
use crate::theme::{shared_theme_path, Mode, Scheme, ThemeReloadSignal, ThemeWriteCompleted};

pub const THEME_CHANGED_TOPIC: &str = "theme.changed";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThemeChanged {
    pub scheme: Scheme,
    pub mode: Mode,
}

impl ThemeChanged {
    pub fn encode(self) -> String {
        let mut inner = cosmix_bus::bus::BusMessage::new();
        inner.set("command", THEME_CHANGED_TOPIC);
        inner.body = self.json_body();
        inner.to_wire()
    }

    pub fn decode(wire: &str) -> Option<Self> {
        let inner = cosmix_bus::bus::parse(wire).ok()?;
        if inner.get("command") != Some(THEME_CHANGED_TOPIC) {
            return None;
        }
        Self::decode_body(&inner.body)
    }

    fn json_body(self) -> String {
        format!(
            r#"{{"scheme":"{}","mode":"{}"}}"#,
            self.scheme.name(),
            self.mode.name()
        )
    }

    fn decode_body(body: &str) -> Option<Self> {
        #[derive(Deserialize)]
        struct WireThemeChanged {
            scheme: String,
            mode: String,
        }

        let decoded: WireThemeChanged = serde_json::from_str(body).ok()?;
        Some(Self {
            scheme: Scheme::from_name(&decoded.scheme)?,
            mode: Mode::from_name(&decoded.mode)?,
        })
    }
}

pub(crate) fn publish_shared_theme_changes(
    mut completions: MessageReader<ThemeWriteCompleted>,
    bridge: Option<Res<BusBridge>>,
) {
    let shared_path = shared_theme_path();
    let mut final_selection = None;
    for completion in completions.read() {
        if completion.path == shared_path && completion.result.is_ok() {
            final_selection = Some(ThemeChanged {
                scheme: completion.scheme,
                mode: completion.mode,
            });
        }
    }

    let (Some(bridge), Some(selection)) = (bridge, final_selection) else {
        return;
    };
    if let Err(error) = bridge.try_publish_topic(THEME_CHANGED_TOPIC, false, selection.encode()) {
        bevy::log::warn!("desktop theme broadcast was not queued: {error}");
    }
}

pub(crate) fn receive_theme_changed(
    bridge: Option<Res<BusBridge>>,
    reload: Res<ThemeReloadSignal>,
) {
    let Some(message) = bridge.and_then(|bridge| bridge.drain_theme_changed()) else {
        return;
    };
    if !is_valid_local_delivery(&message) {
        return;
    }
    reload.request_reload();
}

#[cfg(all(feature = "theme", feature = "bus"))]
pub(crate) fn is_valid_local_delivery(message: &BusMessage) -> bool {
    message.topic() == Some(THEME_CHANGED_TOPIC)
        && message.command == THEME_CHANGED_TOPIC
        && message.headers.get("broker_origin").map(String::as_str) == Some("local")
        && ThemeChanged::decode_body(&message.body).is_some()
}

#[cfg(test)]
mod tests {
    use bevy::app::{App, Last, Update};
    use bevy::ecs::message::Messages;
    use bevy::ecs::schedule::IntoScheduleConfigs;
    use bevy::feathers::theme::UiTheme;
    use bevy::MinimalPlugins;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::bus::{test_bridge, TestBusPeer};
    use crate::theme::{
        apply_theme, apply_theme_requests, reload_theme_files, ApplyTheme, ThemeReloadSignal,
        ThemeRuntimeConfig, ThemeSpec, ThemeState, ThemeWriteRequest, THEME_FILE,
    };

    static TEST_DIR_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn delivery(command: &str, body: &str, origin: &str) -> BusMessage {
        BusMessage {
            connection_generation: 1,
            from: "peer-sub".into(),
            command: command.into(),
            body: body.into(),
            headers: BTreeMap::from([
                ("topic".into(), THEME_CHANGED_TOPIC.into()),
                ("broker_origin".into(), origin.into()),
            ]),
        }
    }

    fn receiver_app(
        initial: ThemeChanged,
        durable: ThemeChanged,
    ) -> (App, TestBusPeer, std::path::PathBuf) {
        let (bridge, peer) = test_bridge("theme-receiver");
        let mut theme = UiTheme::default();
        let mut state = ThemeState::default();
        assert!(apply_theme(
            &mut theme,
            &mut state,
            &ThemeSpec::from_scheme(initial.scheme, initial.mode)
        ));

        let sequence = TEST_DIR_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let app_config_dir =
            std::env::temp_dir().join(format!("ctk-theme-sync-{}-{sequence}", std::process::id()));
        std::fs::create_dir_all(&app_config_dir).unwrap();
        write_durable_theme(&app_config_dir, durable);

        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .insert_resource(theme)
            .insert_resource(state)
            .insert_resource(ThemeRuntimeConfig {
                shared_path: app_config_dir.join("missing-shared-theme.conf.mix"),
                app_config_dir: Some(app_config_dir.clone()),
            })
            .init_resource::<crate::theme::ThemeLayerLastGood>()
            .insert_resource(ThemeReloadSignal::default())
            .insert_resource(bridge)
            .add_message::<ApplyTheme>()
            .add_message::<ThemeWriteRequest>()
            .add_message::<ThemeWriteCompleted>()
            .add_systems(
                Update,
                (
                    receive_theme_changed,
                    reload_theme_files,
                    apply_theme_requests,
                )
                    .chain(),
            )
            .add_systems(Last, publish_shared_theme_changes);
        crate::design::init_design_resources(&mut app);
        (app, peer, app_config_dir)
    }

    fn write_durable_theme(directory: &std::path::Path, selection: ThemeChanged) {
        std::fs::write(
            directory.join(THEME_FILE),
            format!(
                r#"{{ scheme: "{}", mode: "{}" }}"#,
                selection.scheme.name(),
                selection.mode.name()
            ),
        )
        .unwrap();
    }

    fn assert_no_theme_write(app: &App) {
        let messages = app.world().resource::<Messages<ThemeWriteRequest>>();
        let mut cursor = messages.get_cursor();
        assert_eq!(cursor.read(messages).count(), 0);
    }

    #[test]
    fn all_scheme_mode_pairs_round_trip_with_stable_lowercase_wire() {
        for scheme in Scheme::ALL {
            for mode in [Mode::Light, Mode::Dark] {
                let changed = ThemeChanged { scheme, mode };
                let wire = changed.encode();
                assert_eq!(ThemeChanged::decode(&wire), Some(changed));
                let inner = cosmix_bus::bus::parse(&wire).unwrap();
                assert_eq!(inner.get("command"), Some(THEME_CHANGED_TOPIC));
                assert_eq!(
                    inner.body,
                    format!(
                        r#"{{"scheme":"{}","mode":"{}"}}"#,
                        scheme.name(),
                        mode.name()
                    )
                );
            }
        }
    }

    #[test]
    fn publish_uses_exact_unretained_topic_wrapper_and_final_shared_selection() {
        let (bridge, peer) = test_bridge("theme-publisher");
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .insert_resource(bridge)
            .add_message::<ThemeWriteCompleted>()
            .add_systems(Last, publish_shared_theme_changes);
        app.world_mut()
            .resource_mut::<Messages<ThemeWriteCompleted>>()
            .write(ThemeWriteCompleted {
                path: shared_theme_path(),
                scheme: Scheme::Ocean,
                mode: Mode::Dark,
                result: Ok(()),
            });
        app.world_mut()
            .resource_mut::<Messages<ThemeWriteCompleted>>()
            .write(ThemeWriteCompleted {
                path: std::path::PathBuf::from("/example/app-theme.conf.mix"),
                scheme: Scheme::Mono,
                mode: Mode::Dark,
                result: Ok(()),
            });
        app.world_mut()
            .resource_mut::<Messages<ThemeWriteCompleted>>()
            .write(ThemeWriteCompleted {
                path: shared_theme_path(),
                scheme: Scheme::Forest,
                mode: Mode::Light,
                result: Ok(()),
            });
        app.world_mut()
            .resource_mut::<Messages<ThemeWriteCompleted>>()
            .write(ThemeWriteCompleted {
                path: shared_theme_path(),
                scheme: Scheme::Sunset,
                mode: Mode::Dark,
                result: Err("disk full".into()),
            });

        app.update();

        let publishes = peer.drain_publishes();
        assert_eq!(publishes.len(), 1);
        let publish = &publishes[0];
        assert_eq!(publish.to, "noded");
        assert_eq!(publish.command, "topic.publish");
        assert_eq!(
            publish.headers,
            BTreeMap::from([
                ("name".to_string(), THEME_CHANGED_TOPIC.to_string()),
                ("retain".to_string(), "false".to_string()),
            ])
        );
        let inner = cosmix_bus::bus::parse(&publish.body).unwrap();
        assert_eq!(inner.get("command"), Some(THEME_CHANGED_TOPIC));
        assert_eq!(inner.body, r#"{"scheme":"forest","mode":"light"}"#);
    }

    #[test]
    fn failed_shared_and_successful_nonshared_completions_publish_nothing() {
        let (bridge, peer) = test_bridge("theme-publisher-ignored");
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .insert_resource(bridge)
            .add_message::<ThemeWriteCompleted>()
            .add_systems(Last, publish_shared_theme_changes);
        app.world_mut().write_message(ThemeWriteCompleted {
            path: shared_theme_path(),
            scheme: Scheme::Forest,
            mode: Mode::Light,
            result: Err("write failed".into()),
        });
        app.update();
        assert!(peer.drain_publishes().is_empty());

        app.world_mut().write_message(ThemeWriteCompleted {
            path: std::path::PathBuf::from("/example/app-theme.conf.mix"),
            scheme: Scheme::Sunset,
            mode: Mode::Light,
            result: Ok(()),
        });
        app.update();
        assert!(peer.drain_publishes().is_empty());
    }

    #[test]
    fn invalid_or_nonlocal_deliveries_do_not_change_theme_state() {
        let initial = ThemeChanged {
            scheme: Scheme::Ocean,
            mode: Mode::Dark,
        };
        let durable = ThemeChanged {
            scheme: Scheme::Forest,
            mode: Mode::Light,
        };
        let (mut app, peer, directory) = receiver_app(initial, durable);
        let cases = [
            delivery(
                THEME_CHANGED_TOPIC,
                r#"{"scheme":"unknown","mode":"dark"}"#,
                "local",
            ),
            delivery(
                THEME_CHANGED_TOPIC,
                r#"{"scheme":"forest","mode":"unknown"}"#,
                "local",
            ),
            delivery(THEME_CHANGED_TOPIC, "{not-json", "local"),
            delivery(
                THEME_CHANGED_TOPIC,
                r#"{"scheme":"forest","mode":"light"}"#,
                "mesh",
            ),
            delivery(
                "other.command",
                r#"{"scheme":"forest","mode":"light"}"#,
                "local",
            ),
        ];
        for message in cases {
            peer.deliver_theme_changed(message);
            app.update();
            let state = app.world().resource::<ThemeState>();
            assert_eq!(state.scheme, initial.scheme);
            assert_eq!(state.mode, initial.mode);
            assert_eq!(state.revision, 1);
            assert_no_theme_write(&app);
        }
        assert!(ThemeChanged::decode("not an Bus envelope").is_none());
        assert!(ThemeChanged::decode(
            "---\ncommand: wrong\n---\n{\"scheme\":\"forest\",\"mode\":\"light\"}"
        )
        .is_none());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn own_echo_is_idempotent_and_file_change_applies_once_without_writeback() {
        let initial = ThemeChanged {
            scheme: Scheme::Ocean,
            mode: Mode::Dark,
        };
        let (mut app, peer, directory) = receiver_app(initial, initial);
        peer.deliver_theme_changed(delivery(
            THEME_CHANGED_TOPIC,
            r#"{"scheme":"forest","mode":"light"}"#,
            "local",
        ));
        app.update();
        assert_eq!(app.world().resource::<ThemeState>().revision, 1);
        assert!(peer.drain_publishes().is_empty());

        let durable = ThemeChanged {
            scheme: Scheme::Forest,
            mode: Mode::Light,
        };
        write_durable_theme(&directory, durable);
        peer.deliver_theme_changed(delivery(
            THEME_CHANGED_TOPIC,
            r#"{"scheme":"sunset","mode":"dark"}"#,
            "local",
        ));
        app.update();
        let state = app.world().resource::<ThemeState>();
        assert_eq!(state.scheme, Scheme::Forest);
        assert_eq!(state.mode, Mode::Light);
        assert_eq!(state.revision, 2);
        assert_no_theme_write(&app);
        assert!(peer.drain_publishes().is_empty());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn multiple_valid_invalidations_coalesce_to_the_durable_file_value() {
        let initial = ThemeChanged {
            scheme: Scheme::Ocean,
            mode: Mode::Dark,
        };
        let durable = ThemeChanged {
            scheme: Scheme::Sunset,
            mode: Mode::Light,
        };
        let (mut app, peer, directory) = receiver_app(initial, durable);
        peer.deliver_theme_changed(delivery(
            THEME_CHANGED_TOPIC,
            r#"{"scheme":"forest","mode":"dark"}"#,
            "local",
        ));
        peer.deliver_theme_changed(delivery(
            THEME_CHANGED_TOPIC,
            r#"{"scheme":"sunset","mode":"light"}"#,
            "local",
        ));

        app.update();

        let state = app.world().resource::<ThemeState>();
        assert_eq!(state.scheme, Scheme::Sunset);
        assert_eq!(state.mode, Mode::Light);
        assert_eq!(state.revision, 2);
        assert_no_theme_write(&app);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn malformed_delivery_cannot_clobber_a_prior_valid_invalidation() {
        let initial = ThemeChanged {
            scheme: Scheme::Ocean,
            mode: Mode::Dark,
        };
        let durable = ThemeChanged {
            scheme: Scheme::Forest,
            mode: Mode::Light,
        };
        let (mut app, peer, directory) = receiver_app(initial, durable);
        peer.deliver_theme_changed(delivery(
            THEME_CHANGED_TOPIC,
            r#"{"scheme":"sunset","mode":"dark"}"#,
            "local",
        ));
        peer.deliver_theme_changed(delivery(THEME_CHANGED_TOPIC, "{not-json", "local"));

        app.update();

        let state = app.world().resource::<ThemeState>();
        assert_eq!(state.scheme, durable.scheme);
        assert_eq!(state.mode, durable.mode);
        assert_eq!(state.revision, 2);
        assert_no_theme_write(&app);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn two_apps_converge_in_one_receiver_update_without_republishing() {
        let (publisher_bridge, publisher_peer) = test_bridge("theme-app-a");
        let mut publisher = App::new();
        publisher
            .add_plugins(MinimalPlugins)
            .insert_resource(publisher_bridge)
            .add_message::<ThemeWriteCompleted>()
            .add_systems(Last, publish_shared_theme_changes);
        publisher.world_mut().write_message(ThemeWriteCompleted {
            path: shared_theme_path(),
            scheme: Scheme::Forest,
            mode: Mode::Light,
            result: Ok(()),
        });
        publisher.update();
        let mut publishes = publisher_peer.drain_publishes();
        assert_eq!(publishes.len(), 1);
        let publish = publishes.pop().unwrap();

        let inner = cosmix_bus::bus::parse(&publish.body).unwrap();
        let initial = ThemeChanged {
            scheme: Scheme::Ocean,
            mode: Mode::Dark,
        };
        let durable = ThemeChanged {
            scheme: Scheme::Forest,
            mode: Mode::Light,
        };
        let (mut receiver, receiver_peer, directory) = receiver_app(initial, durable);
        receiver_peer.deliver_theme_changed(BusMessage {
            connection_generation: 1,
            from: "theme-app-a-sub".into(),
            command: inner.get("command").unwrap().to_string(),
            body: inner.body,
            headers: BTreeMap::from([
                ("topic".into(), THEME_CHANGED_TOPIC.into()),
                ("broker_origin".into(), "local".into()),
            ]),
        });

        receiver.update();

        let state = receiver.world().resource::<ThemeState>();
        assert_eq!(state.scheme, Scheme::Forest);
        assert_eq!(state.mode, Mode::Light);
        assert_eq!(state.revision, 2);
        assert_no_theme_write(&receiver);
        assert!(receiver_peer.drain_publishes().is_empty());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
