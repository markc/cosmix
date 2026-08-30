//! Blocking D-Bus client for the tray daemon.

use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

use ksni::blocking::Handle as TrayHandle;
use zbus::blocking::proxy::Builder as ProxyBuilder;
use zbus::blocking::{Connection, MessageIterator, Proxy};
use zbus::message::Type as MessageType;
use zbus::proxy::CacheProperties;
use zbus::MatchRule;

use crate::{CosmixTray, DaemonUnit, DesktopApp, MenuSnapshot, MenuView, UnitManager, UnitStatus};

const BUS_NAME: &str = "dev.cosmix.trayd";
const OBJECT_PATH: &str = "/dev/cosmix/trayd";
const INTERFACE_NAME: &str = "dev.cosmix.trayd";

type WireApp = (String, String, String, bool);
type WireDaemon = (String, String, String);
type WireSnapshot = (
    u64,
    bool,
    bool,
    Vec<WireApp>,
    String,
    Vec<WireDaemon>,
    String,
    String,
);

enum DeliveryState<H> {
    Pending(Option<MenuView>),
    Connected(H),
}

fn connect_delivery<H>(state: &mut DeliveryState<H>, handle: H) -> Option<MenuView> {
    let pending = match state {
        DeliveryState::Pending(pending) => pending.take(),
        DeliveryState::Connected(_) => None,
    };
    *state = DeliveryState::Connected(handle);
    pending
}

fn route_delivery<H: Clone>(state: &mut DeliveryState<H>, view: MenuView) -> Option<(H, MenuView)> {
    match state {
        DeliveryState::Pending(pending) => {
            *pending = Some(view);
            None
        }
        DeliveryState::Connected(handle) => Some((handle.clone(), view)),
    }
}

struct Shared {
    connection: Option<Connection>,
    delivery: Mutex<DeliveryState<TrayHandle<CosmixTray>>>,
}

#[derive(Clone)]
pub(crate) struct TrayDaemonClient {
    shared: Arc<Shared>,
}

impl TrayDaemonClient {
    pub(crate) fn connect() -> (Self, MenuView) {
        let connection = match Connection::session() {
            Ok(connection) => connection,
            Err(error) => {
                let message = format!("cannot connect to the session bus: {error}");
                return (Self::without_connection(), MenuView::Unavailable(message));
            }
        };
        let changed = match changed_signals(&connection) {
            Ok(signals) => signals,
            Err(error) => {
                let message = format!("cannot subscribe to trayd Changed: {error}");
                return (Self::without_connection(), MenuView::Unavailable(message));
            }
        };
        let owners = match owner_signals(&connection) {
            Ok(signals) => signals,
            Err(error) => {
                let message = format!("cannot subscribe to trayd ownership: {error}");
                return (Self::without_connection(), MenuView::Unavailable(message));
            }
        };
        let client = Self {
            shared: Arc::new(Shared {
                connection: Some(connection),
                delivery: Mutex::new(DeliveryState::Pending(None)),
            }),
        };

        let changed_listener = client.clone();
        if let Err(error) = thread::Builder::new()
            .name("cosmix-tray-trayd-changed".into())
            .spawn(move || changed_listener.listen_changed(changed))
        {
            client.install(MenuView::Unavailable(format!(
                "cannot start trayd Changed listener: {error}"
            )));
        }

        let owner_listener = client.clone();
        if let Err(error) = thread::Builder::new()
            .name("cosmix-tray-trayd-owner".into())
            .spawn(move || owner_listener.listen_owners(owners))
        {
            client.install(MenuView::Unavailable(format!(
                "cannot start trayd ownership listener: {error}"
            )));
        }
        (client, MenuView::Waiting)
    }

    fn without_connection() -> Self {
        Self {
            shared: Arc::new(Shared {
                connection: None,
                delivery: Mutex::new(DeliveryState::Pending(None)),
            }),
        }
    }

    #[cfg(test)]
    pub(crate) fn disconnected_for_test() -> Self {
        Self::without_connection()
    }

    fn delivery(&self) -> MutexGuard<'_, DeliveryState<TrayHandle<CosmixTray>>> {
        match self.shared.delivery.lock() {
            Ok(delivery) => delivery,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    pub(crate) fn set_handle(&self, handle: TrayHandle<CosmixTray>) {
        let pending = connect_delivery(&mut self.delivery(), handle.clone());
        if let Some(view) = pending {
            let _ = handle.update(move |tray| tray.install_view(view));
        }
    }

    pub(crate) fn refresh(&self) -> Result<(), String> {
        self.proxy()?
            .call("Refresh", &())
            .map_err(|error| format!("Refresh failed: {error}"))
    }

    pub(crate) fn launch_app(&self, slug: &str) -> Result<(), String> {
        self.proxy()?
            .call("LaunchApp", &(slug,))
            .map_err(|error| format!("LaunchApp failed: {error}"))
    }

    pub(crate) fn control_daemon(
        &self,
        manager: &str,
        unit: &str,
        verb: &str,
    ) -> Result<(), String> {
        self.proxy()?
            .call("ControlDaemon", &(manager, unit, verb))
            .map_err(|error| format!("ControlDaemon failed: {error}"))
    }

    pub(crate) fn open_logs(&self, manager: &str, unit: &str) -> Result<(), String> {
        self.proxy()?
            .call("OpenLogs", &(manager, unit))
            .map_err(|error| format!("OpenLogs failed: {error}"))
    }

    fn proxy(&self) -> Result<Proxy<'static>, String> {
        let connection = self
            .shared
            .connection
            .as_ref()
            .ok_or_else(|| "trayd connection is unavailable".to_owned())?;
        ProxyBuilder::<Proxy<'static>>::new(connection)
            .destination(BUS_NAME)
            .and_then(|builder| builder.path(OBJECT_PATH))
            .and_then(|builder| builder.interface(INTERFACE_NAME))
            .map(|builder| builder.cache_properties(CacheProperties::No))
            .and_then(ProxyBuilder::build)
            .map_err(|error| format!("cannot create trayd proxy: {error}"))
    }

    fn listen_changed(&self, mut signals: MessageIterator) {
        for message in &mut signals {
            if let Err(error) = message {
                self.install(MenuView::Unavailable(format!(
                    "trayd Changed subscription failed: {error}"
                )));
                return;
            }
            match self.read_view() {
                Ok(view) => self.install(view),
                Err(error) => self.install(MenuView::Unavailable(error)),
            }
        }
        self.install(MenuView::Unavailable(
            "trayd Changed subscription ended".into(),
        ));
    }

    fn listen_owners(&self, mut signals: MessageIterator) {
        let connection = self
            .shared
            .connection
            .as_ref()
            .expect("listener starts only with a connection");
        match name_has_owner(connection) {
            Ok(true) => self.owner_acquired(),
            Ok(false) => {
                // A method call asks D-Bus activation to start trayd. The
                // resulting NameOwnerChanged acquisition performs the normal
                // Waiting + Refresh transition again, coalescing if needed.
                self.install(MenuView::Waiting);
                if let Err(error) = self.refresh() {
                    self.install(MenuView::Unavailable(error));
                }
            }
            Err(error) => self.install(MenuView::Unavailable(error)),
        }

        for message in &mut signals {
            let message = match message {
                Ok(message) => message,
                Err(error) => {
                    self.install(MenuView::Unavailable(format!(
                        "trayd ownership subscription failed: {error}"
                    )));
                    return;
                }
            };
            let body: (String, String, String) = match message.body().deserialize() {
                Ok(body) => body,
                Err(error) => {
                    self.install(MenuView::Unavailable(format!(
                        "invalid trayd ownership signal: {error}"
                    )));
                    continue;
                }
            };
            match owner_change(&body.0, &body.1, &body.2) {
                OwnerChange::Acquired => self.owner_acquired(),
                OwnerChange::Lost => self.install(MenuView::Unavailable(
                    "tray daemon stopped or lost its bus name".into(),
                )),
                OwnerChange::Irrelevant => {}
            }
        }
        self.install(MenuView::Unavailable(
            "trayd ownership subscription ended".into(),
        ));
    }

    fn owner_acquired(&self) {
        self.install(MenuView::Waiting);
        if let Err(error) = self.refresh() {
            self.install(MenuView::Unavailable(error));
        }
    }

    fn read_view(&self) -> Result<MenuView, String> {
        let wire: WireSnapshot = self
            .proxy()?
            .call("GetSnapshot", &())
            .map_err(|error| format!("GetSnapshot failed: {error}"))?;
        decode_snapshot(wire)
    }

    fn install(&self, view: MenuView) {
        let target = route_delivery(&mut self.delivery(), view);
        if let Some((handle, view)) = target {
            // Installing through update() serialises the swap with menu() on
            // ksni's thread and emits LayoutUpdated.
            let _ = handle.update(move |tray| tray.install_view(view));
        }
    }
}

fn decode_snapshot(
    (
        revision,
        noded_checked,
        noded_reachable,
        wire_apps,
        apps_error,
        wire_daemons,
        daemons_error,
        refresh_error,
    ): WireSnapshot,
) -> Result<MenuView, String> {
    let apps = if apps_error.is_empty() {
        Ok(wire_apps
            .into_iter()
            .map(|(slug, label, icon_name, launchable)| DesktopApp {
                slug,
                label,
                icon_name,
                launchable,
            })
            .collect())
    } else {
        Err(apps_error)
    };
    let daemons = wire_daemons
        .into_iter()
        .map(|(manager, unit, status)| {
            Ok(DaemonUnit {
                manager: UnitManager::from_label(&manager)?,
                unit,
                status: UnitStatus::from_label(&status)?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(MenuView::Snapshot(MenuSnapshot {
        revision,
        noded_checked,
        noded_reachable,
        apps,
        daemons,
        daemons_error,
        refresh_error,
    }))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OwnerChange {
    Acquired,
    Lost,
    Irrelevant,
}

fn owner_change(name: &str, old_owner: &str, new_owner: &str) -> OwnerChange {
    if name != BUS_NAME {
        OwnerChange::Irrelevant
    } else if !new_owner.is_empty() {
        OwnerChange::Acquired
    } else if !old_owner.is_empty() {
        OwnerChange::Lost
    } else {
        OwnerChange::Irrelevant
    }
}

fn changed_signals(connection: &Connection) -> zbus::Result<MessageIterator> {
    let rule = MatchRule::builder()
        .msg_type(MessageType::Signal)
        .sender(BUS_NAME)?
        .path(OBJECT_PATH)?
        .interface(INTERFACE_NAME)?
        .member("Changed")?
        .build();
    MessageIterator::for_match_rule(rule, connection, Some(8))
}

fn owner_signals(connection: &Connection) -> zbus::Result<MessageIterator> {
    let rule = MatchRule::builder()
        .msg_type(MessageType::Signal)
        .sender("org.freedesktop.DBus")?
        .path("/org/freedesktop/DBus")?
        .interface("org.freedesktop.DBus")?
        .member("NameOwnerChanged")?
        .arg(0, BUS_NAME)?
        .build();
    MessageIterator::for_match_rule(rule, connection, Some(8))
}

fn name_has_owner(connection: &Connection) -> Result<bool, String> {
    let proxy = Proxy::new(
        connection,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )
    .map_err(|error| format!("cannot create session bus proxy: {error}"))?;
    proxy
        .call("NameHasOwner", &(BUS_NAME,))
        .map_err(|error| format!("cannot query trayd owner: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_view_and_handle_connect_in_one_state_transition() {
        let mut state = DeliveryState::Pending(None);
        assert!(route_delivery(&mut state, MenuView::Waiting).is_none());
        assert_eq!(connect_delivery(&mut state, 7_u8), Some(MenuView::Waiting));

        let view = MenuView::Unavailable("later".into());
        assert_eq!(route_delivery(&mut state, view.clone()), Some((7_u8, view)));
    }

    #[test]
    fn owner_changes_classify_loss_and_restart_acquisition() {
        assert_eq!(owner_change(BUS_NAME, ":1.20", ""), OwnerChange::Lost);
        assert_eq!(owner_change(BUS_NAME, "", ":1.21"), OwnerChange::Acquired);
        assert_eq!(
            owner_change("dev.cosmix.other", ":1.20", ""),
            OwnerChange::Irrelevant
        );
    }

    #[test]
    fn snapshot_decoder_preserves_manager_identity_and_partial_errors() {
        let view = decode_snapshot((
            8,
            true,
            true,
            Vec::new(),
            String::new(),
            vec![
                (
                    "system".into(),
                    "cosmix-shared.service".into(),
                    "active".into(),
                ),
                (
                    "user".into(),
                    "cosmix-shared.service".into(),
                    "inactive".into(),
                ),
            ],
            "user manager also reported a transient error".into(),
            "refresh has been running for 31s".into(),
        ))
        .expect("valid snapshot");
        let MenuView::Snapshot(snapshot) = view else {
            panic!("snapshot view");
        };
        assert_eq!(snapshot.revision, 8);
        assert_eq!(snapshot.daemons.len(), 2);
        assert_eq!(snapshot.daemons[0].manager, UnitManager::System);
        assert_eq!(snapshot.daemons[1].manager, UnitManager::User);
        assert!(!snapshot.daemons_error.is_empty());
        assert!(!snapshot.refresh_error.is_empty());
    }
}
