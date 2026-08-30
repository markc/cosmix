//! CosMix Tray: a thin event-driven Plasma StatusNotifierItem skin.

mod singleton;
mod trayd;

use std::process::Command;
use std::sync::mpsc;

use cosmix_app_identity::AppIdentity;
use ksni::blocking::TrayMethods as _;
use ksni::menu::{StandardItem, SubMenu};
use ksni::{Category, MenuItem, Status, ToolTip, Tray};
use singleton::LockOutcome;
use trayd::TrayDaemonClient;

pub(crate) const IDENTITY: AppIdentity = AppIdentity {
    slug: "tray",
    display_name: "CosMix Tray",
};

#[derive(Clone, Debug, PartialEq, Eq)]
struct DesktopApp {
    slug: String,
    label: String,
    icon_name: String,
    launchable: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UnitStatus {
    Active,
    Inactive,
    Failed,
    Transitional,
}

impl UnitStatus {
    fn from_label(label: &str) -> Result<Self, String> {
        match label {
            "active" => Ok(Self::Active),
            "inactive" => Ok(Self::Inactive),
            "failed" => Ok(Self::Failed),
            "changing" => Ok(Self::Transitional),
            _ => Err(format!("trayd returned invalid daemon status {label:?}")),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Inactive => "inactive",
            Self::Failed => "failed",
            Self::Transitional => "changing",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UnitManager {
    System,
    User,
}

impl UnitManager {
    fn from_label(label: &str) -> Result<Self, String> {
        match label {
            "system" => Ok(Self::System),
            "user" => Ok(Self::User),
            _ => Err(format!("trayd returned invalid daemon manager {label:?}")),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DaemonUnit {
    manager: UnitManager,
    unit: String,
    status: UnitStatus,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MenuSnapshot {
    revision: u64,
    noded_checked: bool,
    noded_reachable: bool,
    apps: Result<Vec<DesktopApp>, String>,
    daemons: Vec<DaemonUnit>,
    daemons_error: String,
    refresh_error: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum MenuView {
    Waiting,
    Snapshot(MenuSnapshot),
    Unavailable(String),
}

struct CosmixTray {
    trayd: TrayDaemonClient,
    view: MenuView,
}

impl CosmixTray {
    fn new(trayd: TrayDaemonClient, view: MenuView) -> Self {
        Self { trayd, view }
    }

    fn install_view(&mut self, view: MenuView) {
        self.view = view;
    }

    fn apps_menu(&self, apps: &Result<Vec<DesktopApp>, String>) -> MenuItem<Self> {
        let submenu = match apps {
            Ok(apps) if apps.is_empty() => vec![disabled("No CosMix desktop apps found")],
            Ok(apps) => apps.iter().cloned().map(app_menu_item).collect(),
            Err(error) => vec![disabled(format!("Apps unavailable: {}", concise(error)))],
        };
        SubMenu {
            label: "Apps".into(),
            icon_name: "applications-all".into(),
            submenu,
            ..Default::default()
        }
        .into()
    }

    fn daemons_menu(&self, daemons: &[DaemonUnit], error: &str) -> MenuItem<Self> {
        let mut submenu = Vec::new();
        if !error.is_empty() {
            submenu.push(disabled(format!(
                "Some daemons unavailable: {}",
                concise(error)
            )));
        }
        submenu.extend(daemons.iter().cloned().map(daemon_menu_item));
        if submenu.is_empty() {
            submenu.push(disabled("No cosmix services found"));
        }
        SubMenu {
            label: "Daemons".into(),
            icon_name: "preferences-system-services".into(),
            submenu,
            ..Default::default()
        }
        .into()
    }

    fn waiting_menu(&self, refresh_error: Option<&str>) -> Vec<MenuItem<Self>> {
        let mut menu = vec![
            disabled_with_icon("Waiting for first tray daemon refresh…", "view-refresh"),
            SubMenu {
                label: "Apps".into(),
                icon_name: "applications-all".into(),
                submenu: vec![disabled("Waiting for first refresh")],
                ..Default::default()
            }
            .into(),
            SubMenu {
                label: "Daemons".into(),
                icon_name: "preferences-system-services".into(),
                submenu: vec![disabled("Waiting for first refresh")],
                ..Default::default()
            }
            .into(),
        ];
        if let Some(error) = refresh_error.filter(|error| !error.is_empty()) {
            menu.push(disabled_with_icon(
                format!("Refresh warning: {}", concise(error)),
                "dialog-warning",
            ));
        }
        menu.extend([MenuItem::Separator, quit_item()]);
        menu
    }

    fn unavailable_menu(&self, error: &str) -> Vec<MenuItem<Self>> {
        vec![
            disabled_with_icon(
                format!("Tray daemon unavailable: {}", concise(error)),
                "dialog-error",
            ),
            MenuItem::Separator,
            quit_item(),
        ]
    }

    fn action_failed(&mut self, summary: &str, error: String) {
        notify(summary, &error);
        self.view = MenuView::Unavailable(error);
    }
}

impl Tray for CosmixTray {
    const MENU_ON_ACTIVATE: bool = true;

    fn id(&self) -> String {
        IDENTITY.app_id()
    }

    fn title(&self) -> String {
        IDENTITY.display_name.into()
    }

    fn category(&self) -> Category {
        Category::SystemServices
    }

    fn status(&self) -> Status {
        Status::Active
    }

    fn icon_name(&self) -> String {
        IDENTITY.app_id()
    }

    fn tool_tip(&self) -> ToolTip {
        let description = match &self.view {
            MenuView::Snapshot(snapshot) if snapshot.noded_checked && snapshot.noded_reachable => {
                "Last trayd snapshot: local noded reachable."
            }
            MenuView::Snapshot(snapshot) if snapshot.noded_checked => {
                "Last trayd snapshot: local noded unreachable."
            }
            MenuView::Unavailable(_) => "Tray daemon unavailable.",
            MenuView::Waiting | MenuView::Snapshot(_) => {
                "Local noded reachability has not been checked."
            }
        };
        ToolTip {
            icon_name: IDENTITY.app_id(),
            title: IDENTITY.display_name.into(),
            description: description.into(),
            ..Default::default()
        }
    }

    fn menu_about_to_show(&mut self) {
        if let Err(error) = self.trayd.refresh() {
            self.view = MenuView::Unavailable(error);
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let MenuView::Snapshot(snapshot) = &self.view else {
            return match &self.view {
                MenuView::Waiting => self.waiting_menu(None),
                MenuView::Unavailable(error) => self.unavailable_menu(error),
                MenuView::Snapshot(_) => unreachable!(),
            };
        };
        if snapshot.revision == 0 {
            return self.waiting_menu(Some(&snapshot.refresh_error));
        }
        let mut menu = vec![
            disabled_with_icon(
                if snapshot.noded_reachable {
                    "Local noded reachable (last trayd snapshot)"
                } else {
                    "Local noded unreachable (last trayd snapshot)"
                },
                if snapshot.noded_reachable {
                    "network-connect"
                } else {
                    "network-disconnect"
                },
            ),
            self.apps_menu(&snapshot.apps),
            self.daemons_menu(&snapshot.daemons, &snapshot.daemons_error),
        ];
        if !snapshot.refresh_error.is_empty() {
            menu.push(disabled_with_icon(
                format!("Refresh warning: {}", concise(&snapshot.refresh_error)),
                "dialog-warning",
            ));
        }
        menu.extend([MenuItem::Separator, quit_item()]);
        menu
    }
}

fn app_menu_item(app: DesktopApp) -> MenuItem<CosmixTray> {
    let icon_name = if app.launchable {
        if app.icon_name.is_empty() {
            "application-x-executable".into()
        } else {
            app.icon_name.clone()
        }
    } else {
        "dialog-warning".into()
    };
    StandardItem {
        label: app.label,
        enabled: app.launchable,
        icon_name,
        activate: Box::new(move |tray: &mut CosmixTray| {
            if let Err(error) = tray.trayd.launch_app(&app.slug) {
                tray.action_failed("Could not launch application", error);
            }
        }),
        ..Default::default()
    }
    .into()
}

fn daemon_menu_item(daemon: DaemonUnit) -> MenuItem<CosmixTray> {
    let manager = daemon.manager;
    let unit = daemon.unit;
    let status = daemon.status;
    // State icons, not action icons: media-playback-* belong to Start/Stop below,
    // and reusing them here made an Active unit wear the same glyph as its own
    // (disabled) Start item. `systemctl stop` on a failed unit is valid and is how
    // its leftover processes get cleaned up, so Failed offers both actions.
    // Transitional keeps BOTH actions live: mid-start, Stop is the only way to
    // cancel a hung unit, and mid-stop, Start is how you bring it back.
    let (status_icon, start_enabled, stop_enabled) = match status {
        UnitStatus::Active => ("dialog-ok", false, true),
        UnitStatus::Inactive => ("dialog-cancel", true, false),
        UnitStatus::Failed => ("dialog-error", true, true),
        UnitStatus::Transitional => ("dialog-information", true, true),
    };
    SubMenu {
        label: format!("[{}] {unit} — {}", manager.label(), status.label()),
        icon_name: status_icon.into(),
        submenu: vec![
            daemon_action(
                "Start",
                "media-playback-start",
                "start",
                manager,
                unit.clone(),
                start_enabled,
            ),
            daemon_action(
                "Stop",
                "media-playback-stop",
                "stop",
                manager,
                unit.clone(),
                stop_enabled,
            ),
            daemon_action(
                "Restart",
                "view-refresh",
                "restart",
                manager,
                unit.clone(),
                true,
            ),
            StandardItem {
                label: "Logs".into(),
                icon_name: "utilities-terminal".into(),
                activate: Box::new(move |tray: &mut CosmixTray| {
                    if let Err(error) = tray.trayd.open_logs(manager.label(), &unit) {
                        tray.action_failed("Could not open daemon logs", error);
                    }
                }),
                ..Default::default()
            }
            .into(),
        ],
        ..Default::default()
    }
    .into()
}

fn daemon_action(
    label: &'static str,
    icon_name: &'static str,
    verb: &'static str,
    manager: UnitManager,
    unit: String,
    enabled: bool,
) -> MenuItem<CosmixTray> {
    StandardItem {
        label: label.into(),
        enabled,
        icon_name: icon_name.into(),
        activate: Box::new(move |tray: &mut CosmixTray| {
            if let Err(error) = tray.trayd.control_daemon(manager.label(), &unit, verb) {
                tray.action_failed(&format!("Could not {verb} {unit}"), error);
            }
        }),
        ..Default::default()
    }
    .into()
}

fn quit_item() -> MenuItem<CosmixTray> {
    StandardItem {
        label: "Quit".into(),
        icon_name: "application-exit".into(),
        activate: Box::new(|_| std::process::exit(0)),
        ..Default::default()
    }
    .into()
}

fn disabled(label: impl Into<String>) -> MenuItem<CosmixTray> {
    StandardItem {
        label: label.into(),
        enabled: false,
        ..Default::default()
    }
    .into()
}

fn disabled_with_icon(
    label: impl Into<String>,
    icon_name: impl Into<String>,
) -> MenuItem<CosmixTray> {
    StandardItem {
        label: label.into(),
        enabled: false,
        icon_name: icon_name.into(),
        ..Default::default()
    }
    .into()
}

fn concise(message: &str) -> String {
    let single_line = message.split_whitespace().collect::<Vec<_>>().join(" ");
    const LIMIT: usize = 140;
    if single_line.chars().count() <= LIMIT {
        return single_line;
    }
    let mut shortened = single_line.chars().take(LIMIT).collect::<String>();
    shortened.push('…');
    shortened
}

fn notify(summary: &str, body: &str) {
    let _ = Command::new("timeout")
        .args([
            "--signal=KILL",
            "3s",
            "notify-send",
            "--app-name=CosMix Tray",
            summary,
            &concise(body),
        ])
        .output();
}

fn main() {
    if let Err(error) = IDENTITY.validate() {
        eprintln!("cosmix-tray: invalid identity: {error}");
        std::process::exit(2);
    }
    let _instance_lock = match singleton::acquire() {
        Ok(LockOutcome::Acquired(lock)) => lock,
        Ok(LockOutcome::AlreadyHeld) => {
            eprintln!("cosmix-tray: already running");
            return;
        }
        Err(error) => {
            eprintln!("cosmix-tray: cannot acquire singleton lock: {error}");
            std::process::exit(1);
        }
    };

    let (trayd, initial_view) = TrayDaemonClient::connect();
    let tray = CosmixTray::new(trayd.clone(), initial_view);
    let handle = match tray.assume_sni_available(true).spawn() {
        Ok(handle) => handle,
        Err(error) => {
            eprintln!("cosmix-tray: cannot register StatusNotifierItem: {error}");
            std::process::exit(1);
        }
    };
    trayd.set_handle(handle.clone());

    // The SNI service and its D-Bus callbacks run in ksni's blocking worker.
    // Keep this windowless process alive without a timer or polling loop.
    let (_keepalive, stopped) = mpsc::channel::<()>();
    let _handle = handle;
    let _ = stopped.recv();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_tray() -> CosmixTray {
        CosmixTray::new(TrayDaemonClient::disconnected_for_test(), MenuView::Waiting)
    }

    #[test]
    fn identity_matches_package_and_workspace_path() {
        assert!(IDENTITY.validate().is_ok());
        assert_eq!(env!("CARGO_PKG_NAME"), format!("cosmix-{}", IDENTITY.slug));
        assert!(env!("CARGO_MANIFEST_DIR").ends_with(&format!("/apps/{}", IDENTITY.slug)));
        assert_eq!(IDENTITY.app_id(), "dev.cosmix.tray");
    }

    #[test]
    fn sni_properties_describe_a_menu_only_system_service() {
        let mut tray = test_tray();
        assert_eq!(tray.category(), Category::SystemServices);
        assert_eq!(tray.status(), Status::Active);
        assert_eq!(tray.icon_name(), "dev.cosmix.tray");
        assert_eq!(tray.tool_tip().icon_name, "dev.cosmix.tray");
        assert_eq!(
            tray.tool_tip().description,
            "Local noded reachability has not been checked."
        );

        tray.install_view(MenuView::Snapshot(MenuSnapshot {
            revision: 1,
            noded_checked: true,
            noded_reachable: true,
            apps: Ok(Vec::new()),
            daemons: Vec::new(),
            daemons_error: String::new(),
            refresh_error: String::new(),
        }));
        assert_eq!(
            tray.tool_tip().description,
            "Last trayd snapshot: local noded reachable."
        );
    }

    #[test]
    fn app_menu_uses_the_daemon_supplied_icon() {
        let item = app_menu_item(DesktopApp {
            slug: "tower".into(),
            label: "CosMix Tower".into(),
            icon_name: "dev.cosmix.tower".into(),
            launchable: true,
        });
        let MenuItem::Standard(item) = item else {
            panic!("application item must be a standard menu item");
        };
        assert_eq!(item.icon_name, "dev.cosmix.tower");
    }

    #[test]
    fn daemon_items_show_state_icons_and_offer_only_useful_actions() {
        fn inspect(status: UnitStatus) -> (String, Vec<(String, bool)>) {
            let item = daemon_menu_item(DaemonUnit {
                manager: UnitManager::System,
                unit: "cosmix-noded.service".into(),
                status,
            });
            let MenuItem::SubMenu(submenu) = item else {
                panic!("a daemon entry must be a submenu");
            };
            let actions = submenu
                .submenu
                .iter()
                .filter_map(|entry| match entry {
                    MenuItem::Standard(action) => Some((action.label.clone(), action.enabled)),
                    _ => None,
                })
                .collect();
            (submenu.icon_name.clone(), actions)
        }

        let (active_icon, active_actions) = inspect(UnitStatus::Active);
        assert_eq!(active_icon, "dialog-ok");
        assert_eq!(
            active_actions,
            vec![
                ("Start".to_string(), false),
                ("Stop".to_string(), true),
                ("Restart".to_string(), true),
                ("Logs".to_string(), true),
            ]
        );

        let (inactive_icon, inactive_actions) = inspect(UnitStatus::Inactive);
        assert_eq!(inactive_icon, "dialog-cancel");
        assert_eq!(inactive_actions[0], ("Start".to_string(), true));
        assert_eq!(inactive_actions[1], ("Stop".to_string(), false));

        let (failed_icon, failed_actions) = inspect(UnitStatus::Failed);
        assert_eq!(failed_icon, "dialog-error");
        assert_eq!(failed_actions[0], ("Start".to_string(), true));
        assert_eq!(failed_actions[1], ("Stop".to_string(), true));

        let (changing_icon, changing_actions) = inspect(UnitStatus::Transitional);
        assert_eq!(changing_icon, "dialog-information");
        assert_eq!(changing_actions[0], ("Start".to_string(), true));
        assert_eq!(changing_actions[1], ("Stop".to_string(), true));

        for icon in [
            active_icon,
            inactive_icon,
            failed_icon,
            changing_icon.clone(),
        ] {
            assert!(!icon.starts_with("media-playback-"));
        }
        let icons = [
            "dialog-ok",
            "dialog-cancel",
            "dialog-error",
            changing_icon.as_str(),
        ];
        let unique: std::collections::HashSet<_> = icons.iter().collect();
        assert_eq!(unique.len(), icons.len());
    }

    #[test]
    fn duplicate_unit_names_are_visibly_distinguished_by_manager() {
        fn label(manager: UnitManager) -> String {
            let MenuItem::SubMenu(item) = daemon_menu_item(DaemonUnit {
                manager,
                unit: "cosmix-shared.service".into(),
                status: UnitStatus::Active,
            }) else {
                panic!("daemon submenu");
            };
            item.label
        }

        assert_eq!(
            label(UnitManager::System),
            "[system] cosmix-shared.service — active"
        );
        assert_eq!(
            label(UnitManager::User),
            "[user] cosmix-shared.service — active"
        );
    }

    #[test]
    fn partial_daemon_errors_keep_discovered_units_visible() {
        let tray = test_tray();
        let MenuItem::SubMenu(menu) = tray.daemons_menu(
            &[DaemonUnit {
                manager: UnitManager::System,
                unit: "cosmix-webd.service".into(),
                status: UnitStatus::Active,
            }],
            "user manager unavailable",
        ) else {
            panic!("daemon submenu");
        };
        assert_eq!(menu.submenu.len(), 2);
        let MenuItem::Standard(error) = &menu.submenu[0] else {
            panic!("error row");
        };
        assert!(error.label.contains("user manager unavailable"));
        let MenuItem::SubMenu(unit) = &menu.submenu[1] else {
            panic!("unit row");
        };
        assert!(unit.label.contains("cosmix-webd.service"));
    }

    #[test]
    fn unavailable_daemon_is_rendered_plainly() {
        let tray = CosmixTray::new(
            TrayDaemonClient::disconnected_for_test(),
            MenuView::Unavailable("activation failed".into()),
        );
        let menu = tray.menu();
        let MenuItem::Standard(row) = &menu[0] else {
            panic!("failure must be a visible menu row");
        };
        assert!(!row.enabled);
        assert!(row.label.contains("Tray daemon unavailable"));
        assert!(row.label.contains("activation failed"));
    }
}
