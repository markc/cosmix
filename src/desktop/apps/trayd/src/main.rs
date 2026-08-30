//! CosMix Tray Daemon: session state and allowlisted actions over D-Bus.

mod bus;
mod desktop_apps;
mod icons;
mod mix;
mod node;
mod process;
mod ssh;
mod systemd;

use std::env;
use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use bus::{BusController, BusError, WireBusSnapshot, WireTraffic};
use desktop_apps::DesktopApp;
use mix::{MixController, MixError, WireMixOutput, WireMixSnapshot, XDG_OPEN};
use process::ProcessLauncher;
use ssh::{SshController, SshError, WireSshSnapshot};
use systemd::{DaemonUnit, Manager};
use zbus::blocking::connection::Builder as ConnectionBuilder;
use zbus::blocking::{Connection, MessageIterator};
use zbus::message::{Header, Type as MessageType};
use zbus::object_server::SignalEmitter;
use zbus::{
    names::{BusName, UniqueName},
    MatchRule,
};

const BUS_NAME: &str = "dev.cosmix.trayd";
const OBJECT_PATH: &str = "/dev/cosmix/trayd";
#[cfg(test)]
const INTERFACE_NAME: &str = "dev.cosmix.trayd";
const STALLED_REFRESH_AFTER: Duration = Duration::from_secs(30);

type WireApp = (String, String, String, bool);
type WireDaemon = (String, String, String);
#[derive(serde::Serialize, zbus::zvariant::Type)]
struct WireSnapshot {
    revision: u64,
    noded_checked: bool,
    noded_reachable: bool,
    apps: Vec<WireApp>,
    apps_error: String,
    daemons: Vec<WireDaemon>,
    daemons_error: String,
    refresh_error: String,
}

#[derive(Clone, Debug, Default)]
struct Snapshot {
    revision: u64,
    noded_reachable: bool,
    apps: Vec<DesktopApp>,
    apps_error: String,
    daemons: Vec<DaemonUnit>,
    daemons_error: String,
    refresh_error: String,
}

impl Snapshot {
    fn discover(next_revision: u64) -> Self {
        let (apps, apps_error) = match desktop_apps::discover() {
            Ok(apps) => {
                // Drop any `Icon=` the theme cannot resolve, so clients can use
                // their generic fallback. A name that does not resolve renders
                // as blank space in the menu, which is worse than the generic
                // icon it replaced — and three of the four CosMix apps ship no
                // artwork yet, so this is the live case, not a hypothetical.
                // Filtered here rather than in desktop_apps so its parser stays
                // a pure function of the file's bytes. The theme is detected
                // per snapshot, not once per process, so switching icon theme
                // at runtime re-filters against the theme now in force.
                let theme = icons::IconTheme::detect();
                (
                    apps.into_iter()
                        .map(|mut app| {
                            app.icon = theme.keep_if_resolvable(app.icon);
                            app
                        })
                        .collect(),
                    String::new(),
                )
            }
            Err(error) => (Vec::new(), concise(&error)),
        };
        let daemon_discovery = systemd::discover();
        Self {
            revision: next_revision,
            noded_reachable: node::noded_is_reachable(),
            apps,
            apps_error,
            daemons: daemon_discovery.units,
            daemons_error: concise(&daemon_discovery.error),
            refresh_error: String::new(),
        }
    }

    fn wire_apps(&self) -> Vec<WireApp> {
        self.apps
            .iter()
            .map(|app| {
                (
                    app.slug.clone(),
                    app.label.clone(),
                    app.icon.clone().unwrap_or_default(),
                    app.argv.is_some(),
                )
            })
            .collect()
    }

    fn wire_daemons(&self) -> Vec<WireDaemon> {
        self.daemons
            .iter()
            .map(|daemon| {
                (
                    daemon.manager.label().into(),
                    daemon.unit.clone(),
                    daemon.status.label().into(),
                )
            })
            .collect()
    }

    fn wire(&self) -> WireSnapshot {
        WireSnapshot {
            revision: self.revision,
            noded_checked: self.revision != 0,
            noded_reachable: self.noded_reachable,
            apps: self.wire_apps(),
            apps_error: self.apps_error.clone(),
            daemons: self.wire_daemons(),
            daemons_error: self.daemons_error.clone(),
            refresh_error: self.refresh_error.clone(),
        }
    }
}

#[derive(Debug)]
enum RefreshState {
    Idle,
    Running { pending: bool, started: Instant },
}

struct RefreshControl {
    state: Mutex<RefreshState>,
}

impl RefreshControl {
    fn new() -> Self {
        Self {
            state: Mutex::new(RefreshState::Idle),
        }
    }

    fn state(&self) -> MutexGuard<'_, RefreshState> {
        match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn request(&self) -> RefreshDecision {
        self.request_at(Instant::now())
    }

    fn request_at(&self, now: Instant) -> RefreshDecision {
        let mut state = self.state();
        match &mut *state {
            RefreshState::Idle => {
                *state = RefreshState::Running {
                    pending: false,
                    started: now,
                };
                RefreshDecision::Start
            }
            RefreshState::Running { pending, started } => {
                *pending = true;
                let elapsed = now.saturating_duration_since(*started);
                RefreshDecision::Queued {
                    stalled_for: (elapsed >= STALLED_REFRESH_AFTER).then_some(elapsed),
                }
            }
        }
    }

    fn complete_pass(&self) -> bool {
        let mut state = self.state();
        match &mut *state {
            RefreshState::Running { pending: true, .. } => {
                *state = RefreshState::Running {
                    pending: false,
                    started: Instant::now(),
                };
                true
            }
            RefreshState::Running { pending: false, .. } => {
                *state = RefreshState::Idle;
                false
            }
            RefreshState::Idle => false,
        }
    }

    fn abort(&self) {
        *self.state() = RefreshState::Idle;
    }
}

#[derive(Debug, PartialEq, Eq)]
enum RefreshDecision {
    Start,
    Queued { stalled_for: Option<Duration> },
}

/// Holds the single refresh slot for as long as it lives.
///
/// A guard rather than a paired `finish()` call because the worker can unwind:
/// a panic anywhere in discovery would leave the slot claimed forever, and the
/// daemon would then never refresh again for the session — a permanent failure
/// produced by a transient one.
struct RefreshGuard {
    control: Arc<RefreshControl>,
    armed: bool,
}

impl RefreshGuard {
    fn new(control: &Arc<RefreshControl>) -> Self {
        Self {
            control: Arc::clone(control),
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for RefreshGuard {
    fn drop(&mut self) {
        if self.armed {
            self.control.abort();
        }
    }
}

fn run_coalesced_refreshes(control: &Arc<RefreshControl>, mut pass: impl FnMut()) {
    let mut guard = RefreshGuard::new(control);
    loop {
        pass();
        if !control.complete_pass() {
            break;
        }
    }
    guard.disarm();
}

#[derive(Debug, zbus::DBusError, PartialEq, Eq)]
#[zbus(prefix = "dev.cosmix.trayd.Error", impl_display = true)]
enum ActionError {
    NotReady(String),
    UnknownApp(String),
    NotLaunchable(String),
    BadVerb(String),
    UnknownUnit(String),
}

fn require_ready(snapshot: &Snapshot) -> Result<(), ActionError> {
    if snapshot.revision == 0 {
        Err(ActionError::NotReady(
            "no discovery snapshot is installed yet; call Refresh first".into(),
        ))
    } else {
        Ok(())
    }
}

fn allowed_app<'a>(snapshot: &'a Snapshot, slug: &str) -> Result<&'a [String], ActionError> {
    require_ready(snapshot)?;
    let app = snapshot
        .apps
        .iter()
        .find(|app| app.slug == slug)
        .ok_or_else(|| ActionError::UnknownApp(format!("unknown application slug: {slug}")))?;
    app.argv.as_deref().ok_or_else(|| {
        ActionError::NotLaunchable(format!("application {slug} has no runnable argv"))
    })
}

fn allowed_unit(snapshot: &Snapshot, manager: &str, unit: &str) -> Result<Manager, ActionError> {
    require_ready(snapshot)?;
    let manager = Manager::parse(manager).ok_or_else(|| {
        ActionError::UnknownUnit(format!("unknown daemon manager/unit: {manager}/{unit}"))
    })?;
    snapshot
        .daemons
        .iter()
        .any(|daemon| daemon.manager == manager && daemon.unit == unit)
        .then_some(manager)
        .ok_or_else(|| {
            ActionError::UnknownUnit(format!(
                "unknown daemon manager/unit: {}/{unit}",
                manager.label()
            ))
        })
}

fn allowed_control(
    snapshot: &Snapshot,
    manager: &str,
    unit: &str,
    verb: &str,
) -> Result<Manager, ActionError> {
    require_ready(snapshot)?;
    if !matches!(verb, "start" | "stop" | "restart") {
        return Err(ActionError::BadVerb(format!(
            "unsupported daemon verb: {verb}"
        )));
    }
    allowed_unit(snapshot, manager, unit)
}

#[derive(Clone)]
struct TrayDaemon {
    snapshot: Arc<Mutex<Snapshot>>,
    refresh: Arc<RefreshControl>,
    launcher: ProcessLauncher,
    bus: Arc<BusController>,
    mix: Arc<MixController>,
    ssh: Arc<SshController>,
}

impl TrayDaemon {
    fn new() -> Self {
        Self::with_mix(MixController::new_default())
    }

    fn with_mix(mix: Arc<MixController>) -> Self {
        Self::with_controllers(mix, BusController::new(), SshController::new_default())
    }

    fn with_controllers(
        mix: Arc<MixController>,
        bus: Arc<BusController>,
        ssh: Arc<SshController>,
    ) -> Self {
        Self {
            snapshot: Arc::new(Mutex::new(Snapshot::default())),
            refresh: Arc::new(RefreshControl::new()),
            launcher: ProcessLauncher::new(),
            bus,
            mix,
            ssh,
        }
    }

    #[cfg(test)]
    fn new_test() -> Self {
        static SEQUENCE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "cosmix-trayd-main-test-{}-{sequence}/mix",
            std::process::id()
        ));
        let ssh_path = std::env::temp_dir().join(format!(
            "cosmix-trayd-main-test-{}-{sequence}/ssh",
            std::process::id()
        ));
        Self::with_controllers(
            MixController::new_test(path),
            BusController::new_test(),
            SshController::new_test(ssh_path),
        )
    }

    fn snapshot(&self) -> MutexGuard<'_, Snapshot> {
        match self.snapshot.lock() {
            Ok(snapshot) => snapshot,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    async fn bus_owner_is_live(
        connection: &zbus::Connection,
        owner: &str,
    ) -> Result<bool, BusError> {
        let name = BusName::try_from(owner)
            .map_err(|error| BusError::BusUnavailable(format!("invalid D-Bus owner: {error}")))?;
        let proxy = zbus::fdo::DBusProxy::new(connection)
            .await
            .map_err(|error| {
                BusError::BusUnavailable(format!("cannot create D-Bus owner proxy: {error}"))
            })?;
        proxy.name_has_owner(name).await.map_err(|error| {
            BusError::BusUnavailable(format!("cannot validate D-Bus owner: {error}"))
        })
    }

    fn install_discovery(&self) -> u64 {
        let next_revision = self
            .snapshot()
            .revision
            .checked_add(1)
            .expect("snapshot revision exhausted");
        let discovered = Snapshot::discover(next_revision);
        *self.snapshot() = discovered;
        next_revision
    }

    fn emit_snapshot_changed(&self, emitter: &SignalEmitter<'_>, revision: u64) {
        if let Err(error) = zbus::block_on(async {
            self.revision_changed(emitter).await?;
            self.noded_checked_changed(emitter).await?;
            self.noded_reachable_changed(emitter).await?;
            self.apps_changed(emitter).await?;
            self.apps_error_changed(emitter).await?;
            self.daemons_changed(emitter).await?;
            self.daemons_error_changed(emitter).await?;
            self.refresh_error_changed(emitter).await?;
            Self::changed(emitter, revision).await
        }) {
            eprintln!("cosmix-trayd: cannot emit snapshot change: {error}");
        }
    }

    async fn publish_stalled_refresh(&self, emitter: &SignalEmitter<'_>, stalled_for: Duration) {
        let seconds = stalled_for.as_secs();
        let warning =
            format!("refresh has been running for {seconds}s; filesystem discovery may be stalled");
        let revision = {
            let mut snapshot = self.snapshot();
            snapshot.refresh_error = warning;
            snapshot.revision
        };
        if let Err(error) = async {
            self.refresh_error_changed(emitter).await?;
            Self::changed(emitter, revision).await
        }
        .await
        {
            eprintln!("cosmix-trayd: cannot emit stalled refresh warning: {error}");
        }
    }

    fn run_refreshes(&self, emitter: &SignalEmitter<'_>) {
        run_coalesced_refreshes(&self.refresh, || {
            let revision = self.install_discovery();
            self.emit_snapshot_changed(emitter, revision);
            if self.ssh.status() != "watching" {
                self.ssh.refresh();
            }
        });
    }

    async fn start_refresh(&self, emitter: SignalEmitter<'static>) {
        match self.refresh.request() {
            RefreshDecision::Queued {
                stalled_for: Some(stalled_for),
            } => {
                self.publish_stalled_refresh(&emitter, stalled_for).await;
                return;
            }
            RefreshDecision::Queued { stalled_for: None } => return,
            RefreshDecision::Start => {}
        }
        let service = self.clone();
        if let Err(error) = thread::Builder::new()
            .name("cosmix-trayd-refresh".into())
            .spawn(move || service.run_refreshes(&emitter))
        {
            self.refresh.abort();
            eprintln!("cosmix-trayd: cannot start refresh worker: {error}");
        }
    }
}

#[zbus::interface(name = "dev.cosmix.trayd")]
impl TrayDaemon {
    #[zbus(property)]
    fn revision(&self) -> u64 {
        self.snapshot().revision
    }

    #[zbus(property)]
    fn noded_checked(&self) -> bool {
        self.snapshot().revision != 0
    }

    #[zbus(property)]
    fn noded_reachable(&self) -> bool {
        self.snapshot().noded_reachable
    }

    #[zbus(property)]
    fn apps(&self) -> Vec<WireApp> {
        self.snapshot().wire_apps()
    }

    #[zbus(property)]
    fn apps_error(&self) -> String {
        self.snapshot().apps_error.clone()
    }

    #[zbus(property)]
    fn daemons(&self) -> Vec<WireDaemon> {
        self.snapshot().wire_daemons()
    }

    #[zbus(property)]
    fn daemons_error(&self) -> String {
        self.snapshot().daemons_error.clone()
    }

    #[zbus(property)]
    fn refresh_error(&self) -> String {
        self.snapshot().refresh_error.clone()
    }

    #[zbus(out_args("snapshot"))]
    fn get_snapshot(&self) -> (WireSnapshot,) {
        // This is deliberately one lock acquisition. Individual properties
        // remain useful for inspection, but clients must use this method. The
        // one-element return tuple makes the message body one D-Bus struct,
        // matching the published XML and QtDBus decoder.
        (self.snapshot().wire(),)
    }

    async fn refresh(&self, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) {
        self.start_refresh(emitter.to_owned()).await;
    }

    async fn launch_app(&self, slug: &str) -> Result<(), ActionError> {
        let argv = {
            let snapshot = self.snapshot();
            allowed_app(&snapshot, slug)?.to_vec()
        };
        let launcher = self.launcher.clone();
        let slug = slug.to_owned();
        blocking::unblock(move || {
            launcher.launch(&slug, &argv, "Could not launch application");
        })
        .await;
        Ok(())
    }

    async fn control_daemon(
        &self,
        manager: &str,
        unit: &str,
        verb: &str,
    ) -> Result<(), ActionError> {
        let manager = {
            let snapshot = self.snapshot();
            allowed_control(&snapshot, manager, unit, verb)?
        };
        let launcher = self.launcher.clone();
        let verb = verb.to_owned();
        let unit = unit.to_owned();
        blocking::unblock(move || launcher.control_daemon(manager, &verb, &unit)).await;
        Ok(())
    }

    async fn open_logs(&self, manager: &str, unit: &str) -> Result<(), ActionError> {
        let manager = {
            let snapshot = self.snapshot();
            allowed_unit(&snapshot, manager, unit)?
        };
        let launcher = self.launcher.clone();
        let unit = unit.to_owned();
        blocking::unblock(move || launcher.open_logs(manager, &unit)).await;
        Ok(())
    }

    #[zbus(property)]
    fn bus_active(&self) -> bool {
        self.bus.active()
    }

    #[zbus(property)]
    fn bus_state(&self) -> String {
        self.bus.status()
    }

    #[zbus(property)]
    fn bus_revision(&self) -> u64 {
        self.bus.revision()
    }

    #[zbus(out_args("session_id"))]
    async fn open_bus_session(
        &self,
        directions: Vec<String>,
        verb_glob: &str,
        body_mode: &str,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> Result<String, BusError> {
        let owner = caller(&header)?;
        if !Self::bus_owner_is_live(connection, &owner).await? {
            return Err(BusError::BusUnavailable(
                "D-Bus caller disappeared before lease creation".into(),
            ));
        }
        let bus = Arc::clone(&self.bus);
        let open_owner = owner.clone();
        let verb_glob = verb_glob.to_owned();
        let body_mode = body_mode.to_owned();
        let session_id =
            blocking::unblock(move || bus.open(open_owner, directions, verb_glob, body_mode))
                .await?;
        // NameOwnerChanged is already subscribed before BUS_NAME is published.
        // This second check closes the insertion race if the sender vanished
        // between method dispatch and the lease entering BusState.
        match Self::bus_owner_is_live(connection, &owner).await {
            Ok(true) => {}
            Ok(false) => {
                self.bus.owner_lost(&owner);
                return Err(BusError::BusUnavailable(
                    "D-Bus caller disappeared during lease creation".into(),
                ));
            }
            Err(error) => {
                self.bus.owner_lost(&owner);
                return Err(error);
            }
        }
        Ok(session_id)
    }

    fn update_bus_session(
        &self,
        session_id: &str,
        directions: Vec<String>,
        verb_glob: &str,
        body_mode: &str,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<(), BusError> {
        self.bus.update(
            &caller(&header)?,
            session_id,
            directions,
            verb_glob.into(),
            body_mode.into(),
        )
    }

    fn keep_bus_session_alive(
        &self,
        session_id: &str,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<(), BusError> {
        self.bus.keep_alive(&caller(&header)?, session_id)
    }

    fn close_bus_session(
        &self,
        session_id: &str,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<(), BusError> {
        self.bus.close(&caller(&header)?, session_id)
    }

    fn refresh_bus_roster(
        &self,
        session_id: &str,
        #[zbus(header)] header: Header<'_>,
    ) -> Result<(), BusError> {
        self.bus.refresh_roster(&caller(&header)?, session_id)
    }

    #[zbus(out_args("snapshot"))]
    fn get_bus_snapshot(&self) -> (WireBusSnapshot,) {
        // Keep the snapshot as one top-level D-Bus struct; see get_snapshot.
        (self.bus.snapshot(),)
    }

    #[zbus(property)]
    fn mix_revision(&self) -> u64 {
        self.mix.revision()
    }

    #[zbus(property)]
    fn mix_state(&self) -> String {
        self.mix.status()
    }

    #[zbus(property)]
    fn mix_error(&self) -> String {
        self.mix.error()
    }

    #[zbus(property)]
    fn mix_active_runs(&self) -> u32 {
        self.mix.active_runs()
    }

    #[zbus(out_args("snapshot"))]
    fn get_mix_snapshot(&self) -> (WireMixSnapshot,) {
        (self.mix.snapshot(),)
    }

    #[zbus(out_args("script_id"))]
    async fn create_mix_script(&self, name: &str, description: &str) -> Result<String, MixError> {
        // These calls can occupy blocking-pool threads while MixController's
        // operations mutex serialises them. That bounded pool exposure is
        // accepted here because it keeps the sole zbus executor responsive;
        // a dedicated request worker would add queue and shutdown semantics.
        let mix = Arc::clone(&self.mix);
        let name = name.to_owned();
        let description = description.to_owned();
        blocking::unblock(move || mix.create(&name, &description)).await
    }

    async fn update_mix_script(
        &self,
        script_id: &str,
        name: &str,
        description: &str,
    ) -> Result<(), MixError> {
        let mix = Arc::clone(&self.mix);
        let script_id = script_id.to_owned();
        let name = name.to_owned();
        let description = description.to_owned();
        blocking::unblock(move || mix.update(&script_id, &name, &description)).await
    }

    async fn trash_mix_script(&self, script_id: &str) -> Result<(), MixError> {
        let mix = Arc::clone(&self.mix);
        let script_id = script_id.to_owned();
        blocking::unblock(move || mix.trash(&script_id)).await
    }

    async fn restore_mix_script(&self, script_id: &str) -> Result<(), MixError> {
        let mix = Arc::clone(&self.mix);
        let script_id = script_id.to_owned();
        blocking::unblock(move || mix.restore(&script_id)).await
    }

    async fn purge_mix_script(&self, script_id: &str) -> Result<(), MixError> {
        let mix = Arc::clone(&self.mix);
        let script_id = script_id.to_owned();
        blocking::unblock(move || mix.purge(&script_id)).await
    }

    async fn edit_mix_script(&self, script_id: &str) -> Result<(), MixError> {
        let mix = Arc::clone(&self.mix);
        let script_id = script_id.to_owned();
        let script = blocking::unblock(move || mix.edit_path(&script_id)).await?;
        // xdg-open has no descriptor hand-off protocol: the desktop opener
        // ultimately requires a pathname. This is deliberately inside the
        // same-user session trust boundary. That user can already replace
        // their own script store and execute arbitrary Mix code, so a swap
        // between this authority check and xdg-open grants no new authority.
        // Store mutations and Mix execution remain descriptor-pinned.
        let launcher = self.launcher.clone();
        let argv = vec![XDG_OPEN.into(), script.to_string_lossy().into_owned()];
        blocking::unblock(move || {
            launcher.launch("mix-editor", &argv, "Could not open Mix script");
        })
        .await;
        Ok(())
    }

    #[zbus(out_args("run_id"))]
    async fn run_mix_script(&self, script_id: &str) -> Result<String, MixError> {
        let mix = Arc::clone(&self.mix);
        let script_id = script_id.to_owned();
        blocking::unblock(move || mix.run(&script_id)).await
    }

    async fn stop_mix_run(&self, run_id: &str) -> Result<(), MixError> {
        let mix = Arc::clone(&self.mix);
        let run_id = run_id.to_owned();
        blocking::unblock(move || mix.stop(&run_id)).await
    }

    #[zbus(property)]
    fn ssh_revision(&self) -> u64 {
        self.ssh.revision()
    }

    #[zbus(property)]
    fn ssh_state(&self) -> String {
        self.ssh.status()
    }

    #[zbus(property)]
    fn ssh_error(&self) -> String {
        self.ssh.error()
    }

    #[zbus(property)]
    fn ssh_active_probes(&self) -> u32 {
        self.ssh.active_probes()
    }

    #[zbus(out_args("snapshot"))]
    fn get_ssh_snapshot(&self) -> (WireSshSnapshot,) {
        (self.ssh.snapshot(),)
    }

    async fn connect_ssh_host(&self, id: &str) -> Result<(), SshError> {
        let argv = self.ssh.connect_argv(id)?;
        let launcher = self.launcher.clone();
        blocking::unblock(move || {
            launcher.launch("ssh", &argv, "Could not connect to SSH host");
        })
        .await;
        Ok(())
    }

    async fn probe_ssh_hosts(&self, ids: Vec<String>) -> Result<(), SshError> {
        let ssh = Arc::clone(&self.ssh);
        blocking::unblock(move || ssh.probe(ids)).await
    }

    async fn create_ssh_host(
        &self,
        name: &str,
        hostname: &str,
        port: u32,
        user: &str,
        key_id: &str,
    ) -> Result<(), SshError> {
        let ssh = Arc::clone(&self.ssh);
        let name = name.to_owned();
        let hostname = hostname.to_owned();
        let user = user.to_owned();
        let key_id = key_id.to_owned();
        blocking::unblock(move || ssh.create(&name, &hostname, port, &user, &key_id)).await
    }

    async fn edit_ssh_host(&self, id: &str) -> Result<(), SshError> {
        let ssh = Arc::clone(&self.ssh);
        let id = id.to_owned();
        let path = blocking::unblock(move || ssh.edit_path(&id)).await?;
        // As with Mix editing, xdg-open ultimately needs a pathname. The
        // same-user session can already replace its own SSH fragments; the
        // authority check itself remains descriptor-pinned.
        let launcher = self.launcher.clone();
        let argv = vec![XDG_OPEN.into(), path.to_string_lossy().into_owned()];
        blocking::unblock(move || {
            launcher.launch("ssh-editor", &argv, "Could not open SSH host");
        })
        .await;
        Ok(())
    }

    async fn trash_ssh_host(&self, id: &str) -> Result<(), SshError> {
        let ssh = Arc::clone(&self.ssh);
        let id = id.to_owned();
        blocking::unblock(move || ssh.trash(&id)).await
    }

    async fn restore_ssh_host(&self, id: &str) -> Result<(), SshError> {
        let ssh = Arc::clone(&self.ssh);
        let id = id.to_owned();
        blocking::unblock(move || ssh.restore(&id)).await
    }

    async fn purge_ssh_host(&self, id: &str) -> Result<(), SshError> {
        let ssh = Arc::clone(&self.ssh);
        let id = id.to_owned();
        blocking::unblock(move || ssh.purge(&id)).await
    }

    #[zbus(signal)]
    async fn changed(emitter: &SignalEmitter<'_>, revision: u64) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn bus_changed(emitter: &SignalEmitter<'_>, revision: u64) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn bus_traffic_batch(
        emitter: &SignalEmitter<'_>,
        revision: u64,
        filter_epoch: u64,
        events: Vec<WireTraffic>,
        server_dropped: u64,
        bridge_dropped: u64,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn mix_changed(emitter: &SignalEmitter<'_>, revision: u64) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn mix_run_changed(
        emitter: &SignalEmitter<'_>,
        revision: u64,
        run_id: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn mix_run_output(
        emitter: &SignalEmitter<'_>,
        revision: u64,
        run_id: &str,
        chunks: Vec<WireMixOutput>,
        stdout_dropped: u64,
        stderr_dropped: u64,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn ssh_changed(emitter: &SignalEmitter<'_>, revision: u64) -> zbus::Result<()>;
}

fn caller(header: &Header<'_>) -> Result<String, BusError> {
    header
        .sender()
        .map(ToString::to_string)
        .ok_or_else(|| BusError::BusUnavailable("D-Bus caller has no unique sender".into()))
}

fn concise(message: &str) -> String {
    let single_line = message.split_whitespace().collect::<Vec<_>>().join(" ");
    const LIMIT: usize = 180;
    if single_line.chars().count() <= LIMIT {
        return single_line;
    }
    let mut shortened = single_line.chars().take(LIMIT).collect::<String>();
    shortened.push('…');
    shortened
}

fn warn_about_legacy_control_host() {
    let path = cosmix_config::store::config_dir().join("tray.conf.mix");
    if let Ok(text) = fs::read_to_string(&path) {
        if text.contains("control_host") {
            eprintln!(
                "cosmix-trayd: control_host in {} is no longer supported; this daemon controls the local systemd only",
                path.display()
            );
        }
    }
}

fn bus_owner_signals(connection: &Connection) -> zbus::Result<MessageIterator> {
    let rule = MatchRule::builder()
        .msg_type(MessageType::Signal)
        .sender("org.freedesktop.DBus")?
        .path("/org/freedesktop/DBus")?
        .interface("org.freedesktop.DBus")?
        .member("NameOwnerChanged")?
        .build();
    MessageIterator::for_match_rule(rule, connection, Some(256))
}

fn start_bus_owner_watcher(bus: Arc<BusController>, connection: &Connection) -> Result<(), String> {
    let mut signals = bus_owner_signals(connection)
        .map_err(|error| format!("cannot subscribe to D-Bus owner changes: {error}"))?;
    thread::Builder::new()
        .name("cosmix-trayd-bus-owners".into())
        .spawn(move || {
            for message in &mut signals {
                let message = match message {
                    Ok(message) => message,
                    Err(error) => {
                        eprintln!("cosmix-trayd: Bus owner subscription failed: {error}");
                        return;
                    }
                };
                let Ok((name, old_owner, new_owner)) =
                    message.body().deserialize::<(String, String, String)>()
                else {
                    continue;
                };
                if !old_owner.is_empty()
                    && new_owner.is_empty()
                    && UniqueName::try_from(name.as_str()).is_ok()
                {
                    bus.owner_lost(&name);
                }
            }
        })
        .map(|_| ())
        .map_err(|error| format!("cannot start Bus owner watcher: {error}"))
}

fn start_bus_publisher(service: TrayDaemon, connection: &Connection) -> Result<(), String> {
    let receiver = service.bus.take_publish_receiver();
    let emitter = SignalEmitter::new(connection.inner(), OBJECT_PATH)
        .map(SignalEmitter::into_owned)
        .map_err(|error| format!("cannot create Bus signal emitter: {error}"))?;
    thread::Builder::new()
        .name("cosmix-trayd-bus-publish".into())
        .spawn(move || {
            while receiver.recv().is_ok() {
                while let Some(publication) = service.bus.take_publication() {
                    if let Err(error) = zbus::block_on(async {
                        service.bus_active_changed(&emitter).await?;
                        service.bus_state_changed(&emitter).await?;
                        service.bus_revision_changed(&emitter).await?;
                        TrayDaemon::bus_changed(&emitter, publication.revision).await?;
                        if !publication.events.is_empty() {
                            TrayDaemon::bus_traffic_batch(
                                &emitter,
                                publication.revision,
                                publication.filter_epoch,
                                publication.events,
                                publication.server_dropped,
                                publication.bridge_dropped,
                            )
                            .await?;
                        }
                        Ok::<(), zbus::Error>(())
                    }) {
                        eprintln!("cosmix-trayd: cannot publish Bus change: {error}");
                    }
                }
            }
        })
        .map(|_| ())
        .map_err(|error| format!("cannot start Bus publisher: {error}"))
}

fn start_mix_publisher(service: TrayDaemon, connection: &Connection) -> Result<(), String> {
    let receiver = service.mix.take_publish_receiver();
    let emitter = SignalEmitter::new(connection.inner(), OBJECT_PATH)
        .map(SignalEmitter::into_owned)
        .map_err(|error| format!("cannot create Mix signal emitter: {error}"))?;
    thread::Builder::new()
        .name("cosmix-trayd-mix-publish".into())
        .spawn(move || {
            while receiver.recv().is_ok() {
                while let Some(publication) = service.mix.take_publication() {
                    if let Err(error) = zbus::block_on(async {
                        service.mix_revision_changed(&emitter).await?;
                        service.mix_state_changed(&emitter).await?;
                        service.mix_error_changed(&emitter).await?;
                        service.mix_active_runs_changed(&emitter).await?;
                        if publication.catalogue_changed {
                            TrayDaemon::mix_changed(&emitter, publication.revision).await?;
                        }
                        for run_id in &publication.run_ids {
                            TrayDaemon::mix_run_changed(&emitter, publication.revision, run_id)
                                .await?;
                        }
                        if !publication.output.is_empty() {
                            TrayDaemon::mix_run_output(
                                &emitter,
                                publication.revision,
                                &publication.output_run_id,
                                publication.output,
                                publication.stdout_dropped,
                                publication.stderr_dropped,
                            )
                            .await?;
                        }
                        Ok::<(), zbus::Error>(())
                    }) {
                        eprintln!("cosmix-trayd: cannot publish Mix change: {error}");
                    }
                }
            }
        })
        .map(|_| ())
        .map_err(|error| format!("cannot start Mix publisher: {error}"))
}

fn start_ssh_publisher(service: TrayDaemon, connection: &Connection) -> Result<(), String> {
    let receiver = service.ssh.take_publish_receiver();
    let emitter = SignalEmitter::new(connection.inner(), OBJECT_PATH)
        .map(SignalEmitter::into_owned)
        .map_err(|error| format!("cannot create SSH signal emitter: {error}"))?;
    thread::Builder::new()
        .name("cosmix-trayd-ssh-publish".into())
        .spawn(move || {
            while receiver.recv().is_ok() {
                while let Some(publication) = service.ssh.take_publication() {
                    if let Err(error) = zbus::block_on(async {
                        service.ssh_revision_changed(&emitter).await?;
                        service.ssh_state_changed(&emitter).await?;
                        service.ssh_error_changed(&emitter).await?;
                        service.ssh_active_probes_changed(&emitter).await?;
                        TrayDaemon::ssh_changed(&emitter, publication.revision).await?;
                        Ok::<(), zbus::Error>(())
                    }) {
                        eprintln!("cosmix-trayd: cannot publish SSH change: {error}");
                    }
                }
            }
        })
        .map(|_| ())
        .map_err(|error| format!("cannot start SSH publisher: {error}"))
}

fn main() {
    let _instance_lock = match acquire_instance_lock() {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            eprintln!("cosmix-trayd: already running");
            return;
        }
        Err(error) => {
            eprintln!("cosmix-trayd: cannot acquire instance lock: {error}");
            std::process::exit(1);
        }
    };
    let service = TrayDaemon::new();
    let builder = match ConnectionBuilder::session()
        .and_then(|builder| builder.serve_at(OBJECT_PATH, service.clone()))
    {
        Ok(builder) => builder,
        Err(error) => {
            eprintln!("cosmix-trayd: cannot configure session bus service: {error}");
            std::process::exit(1);
        }
    };
    let connection = match builder.build() {
        Ok(connection) => connection,
        Err(zbus::Error::NameTaken) => {
            eprintln!("cosmix-trayd: already running");
            return;
        }
        Err(error) => {
            eprintln!("cosmix-trayd: cannot register session bus service: {error}");
            std::process::exit(1);
        }
    };
    let bus_proxy = match zbus::blocking::fdo::DBusProxy::new(&connection) {
        Ok(proxy) => proxy,
        Err(error) => {
            eprintln!("cosmix-trayd: cannot inspect session bus ownership: {error}");
            std::process::exit(1);
        }
    };
    let bus_name = BusName::try_from(BUS_NAME).expect("static D-Bus name is valid");
    match bus_proxy.name_has_owner(bus_name) {
        Ok(true) => {
            eprintln!("cosmix-trayd: already running");
            return;
        }
        Ok(false) => {}
        Err(error) => {
            eprintln!("cosmix-trayd: cannot inspect session bus ownership: {error}");
            std::process::exit(1);
        }
    }
    if let Err(error) = start_bus_owner_watcher(Arc::clone(&service.bus), &connection) {
        eprintln!("cosmix-trayd: {error}");
        std::process::exit(1);
    }
    // The private process lock establishes singleton ownership before the
    // wildcard reconciliation. No callable well-known D-Bus service exists
    // yet, so a legitimate new run cannot be created inside this window.
    if let Err(error) = service.mix.reconcile_orphans() {
        eprintln!("cosmix-trayd: cannot reconcile orphan Mix units: {error}");
        std::process::exit(1);
    }
    if let Err(error) = connection.request_name(BUS_NAME) {
        if matches!(error, zbus::Error::NameTaken) {
            eprintln!("cosmix-trayd: already running");
            return;
        }
        eprintln!("cosmix-trayd: cannot publish session bus service: {error}");
        std::process::exit(1);
    }
    if let Err(error) = start_bus_publisher(service.clone(), &connection)
        .and_then(|()| start_mix_publisher(service.clone(), &connection))
        .and_then(|()| start_ssh_publisher(service.clone(), &connection))
    {
        eprintln!("cosmix-trayd: {error}");
        std::process::exit(1);
    }
    warn_about_legacy_control_host();

    // zbus owns the session-bus callbacks. The main executor has no sampling
    // loop; Bus connection work and its five-minute lease backstop are isolated
    // on the dedicated Tokio thread.
    zbus::block_on(std::future::pending::<()>());
}

fn acquire_instance_lock() -> Result<Option<File>, String> {
    let runtime = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| "XDG_RUNTIME_DIR is not set".to_owned())?;
    if !runtime.is_absolute() {
        return Err("XDG_RUNTIME_DIR is not absolute".into());
    }
    let path = runtime.join("cosmix-trayd.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .map_err(|error| format!("opening {}: {error}", path.display()))?;
    // SAFETY: flock only reads the valid descriptor and does not retain it.
    let result = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        return Ok(Some(lock));
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::WouldBlock {
        Ok(None)
    } else {
        Err(format!("locking {}: {error}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc::{sync_channel, RecvTimeoutError};
    use std::thread::JoinHandle;
    use systemd::UnitStatus;
    use zbus::object_server::Interface;

    struct TestGateRelease(Arc<bus::TestOpenGate>);

    impl Drop for TestGateRelease {
        fn drop(&mut self) {
            self.0.release();
        }
    }

    fn join_with_timeout<T>(handle: JoinHandle<T>, context: &str) -> std::thread::Result<T> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !handle.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            handle.is_finished(),
            "{context} did not finish within five seconds"
        );
        handle.join()
    }

    struct PrivateSessionBus {
        child: Child,
        reader: Option<JoinHandle<()>>,
        address: String,
    }

    #[derive(Debug)]
    struct PrivateBusStartError {
        message: String,
        pid: Option<u32>,
    }

    impl PrivateSessionBus {
        fn start() -> Self {
            Self::start_with_args(
                &["--session", "--nofork", "--nopidfile", "--print-address=1"],
                Duration::from_secs(3),
            )
            .unwrap_or_else(|error| panic!("{}", error.message))
        }

        fn start_with_args(args: &[&str], timeout: Duration) -> Result<Self, PrivateBusStartError> {
            let child = Command::new("dbus-daemon")
                .args(args)
                .stdout(Stdio::piped())
                .spawn()
                .map_err(|error| PrivateBusStartError {
                    message: format!(
                        "dbus-daemon is required for the serving-connection regression test: {error}"
                    ),
                    pid: None,
                })?;
            let mut bus = Self {
                child,
                reader: None,
                address: String::new(),
            };
            let pid = bus.child.id();
            let stdout = bus
                .child
                .stdout
                .take()
                .ok_or_else(|| PrivateBusStartError {
                    message: "cannot capture private dbus-daemon address".into(),
                    pid: Some(pid),
                })?;
            let (address_tx, address_rx) = sync_channel(1);
            bus.reader = Some(std::thread::spawn(move || {
                let mut stdout = BufReader::new(stdout);
                let mut address = String::new();
                let result = stdout.read_line(&mut address).map(|_| address);
                let _ = address_tx.send(result);
            }));

            let address = match address_rx.recv_timeout(timeout) {
                Ok(Ok(address)) => address,
                Ok(Err(error)) => {
                    return Err(PrivateBusStartError {
                        message: format!("cannot read private dbus-daemon address: {error}"),
                        pid: Some(pid),
                    });
                }
                Err(RecvTimeoutError::Timeout) => {
                    return Err(PrivateBusStartError {
                        message: format!(
                            "private dbus-daemon did not print an address within {}s",
                            timeout.as_secs_f64()
                        ),
                        pid: Some(pid),
                    });
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(PrivateBusStartError {
                        message: "private dbus-daemon address reader stopped unexpectedly".into(),
                        pid: Some(pid),
                    });
                }
            };
            if let Some(reader) = bus.reader.take() {
                reader.join().map_err(|_| PrivateBusStartError {
                    message: "private dbus-daemon address reader panicked".into(),
                    pid: Some(pid),
                })?;
            }
            bus.address = address.trim().to_owned();
            if bus.address.is_empty() {
                return Err(PrivateBusStartError {
                    message: "private dbus-daemon returned an empty address".into(),
                    pid: Some(pid),
                });
            }
            Ok(bus)
        }
    }

    impl Drop for PrivateSessionBus {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            if let Some(reader) = self.reader.take() {
                let _ = reader.join();
            }
        }
    }

    fn ready_snapshot() -> Snapshot {
        Snapshot {
            revision: 1,
            apps: vec![
                DesktopApp {
                    slug: "tower".into(),
                    label: "CosMix Tower".into(),
                    icon: Some("dev.cosmix.tower".into()),
                    argv: Some(vec!["/opt/cosmix/bin/cosmix-tower".into()]),
                },
                DesktopApp {
                    slug: "broken".into(),
                    label: "Broken".into(),
                    icon: None,
                    argv: None,
                },
            ],
            daemons: vec![DaemonUnit {
                manager: Manager::System,
                unit: "cosmix-noded.service".into(),
                status: UnitStatus::Active,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn open_bus_session_keeps_the_serving_connection_responsive() {
        let private_bus = PrivateSessionBus::start();
        let service_name = format!("dev.cosmix.trayd.test.p{}", std::process::id());
        let service = TrayDaemon::new_test();
        let open_gate = service.bus.block_next_open();
        let open_gate_release = TestGateRelease(Arc::clone(&open_gate));
        let server = ConnectionBuilder::address(private_bus.address.as_str())
            .expect("configure trayd test bus")
            .serve_at(OBJECT_PATH, service)
            .expect("serve trayd test interface")
            // Bound the broken implementation's nested owner query so this
            // regression fails instead of permanently wedging the test process.
            .method_timeout(Duration::from_secs(3))
            .name(service_name.as_str())
            .expect("configure trayd test name")
            .build()
            .expect("build trayd test connection");
        let client = ConnectionBuilder::address(private_bus.address.as_str())
            .expect("configure trayd test client")
            .method_timeout(Duration::from_secs(8))
            .build()
            .expect("build trayd test client");

        let open_client = client.clone();
        let open_service_name = service_name.clone();
        let open_call = std::thread::spawn(move || {
            open_client.call_method(
                Some(open_service_name.as_str()),
                OBJECT_PATH,
                Some(INTERFACE_NAME),
                "OpenBusSession",
                &(vec!["local".to_owned()], "*", "none"),
            )
        });
        open_gate.wait_until_entered();

        client
            .call_method(
                Some(service_name.as_str()),
                OBJECT_PATH,
                Some("org.freedesktop.DBus.Peer"),
                "Ping",
                &(),
            )
            .expect("Peer.Ping must remain responsive while OpenBusSession is blocked");
        let introspection = client
            .call_method(
                Some(service_name.as_str()),
                OBJECT_PATH,
                Some("org.freedesktop.DBus.Introspectable"),
                "Introspect",
                &(),
            )
            .expect("Introspect must remain responsive while OpenBusSession is blocked")
            .body()
            .deserialize::<String>()
            .expect("decode introspection XML");
        assert!(introspection.contains("OpenBusSession"));

        open_gate.release();
        drop(open_gate_release);
        let open_reply = join_with_timeout(open_call, "OpenBusSession caller")
            .expect("join OpenBusSession caller")
            .expect("OpenBusSession must complete without starving the connection");
        let session_id = open_reply
            .body()
            .deserialize::<String>()
            .expect("decode Bus session identity");
        assert!(!session_id.is_empty());

        client
            .call_method(
                Some(service_name.as_str()),
                OBJECT_PATH,
                Some("org.freedesktop.DBus.Peer"),
                "Ping",
                &(),
            )
            .expect("Peer.Ping must remain responsive after OpenBusSession");

        client
            .call_method(
                Some(service_name.as_str()),
                OBJECT_PATH,
                Some(INTERFACE_NAME),
                "CloseBusSession",
                &session_id,
            )
            .expect("close Bus test session");
        drop(server);
    }

    #[test]
    fn create_mix_script_keeps_the_serving_connection_responsive() {
        let private_bus = PrivateSessionBus::start();
        let service_name = format!("dev.cosmix.trayd.mix.test.p{}", std::process::id());
        let service = TrayDaemon::new_test();
        let create_gate = service.mix.block_next_create();
        let create_gate_release = TestGateRelease(Arc::clone(&create_gate));
        let server = ConnectionBuilder::address(private_bus.address.as_str())
            .expect("configure trayd Mix test bus")
            .serve_at(OBJECT_PATH, service)
            .expect("serve trayd Mix test interface")
            .name(service_name.as_str())
            .expect("configure trayd Mix test name")
            .build()
            .expect("build trayd Mix test connection");
        let create_client = ConnectionBuilder::address(private_bus.address.as_str())
            .expect("configure trayd Mix create client")
            .method_timeout(Duration::from_secs(5))
            .build()
            .expect("build trayd Mix create client");
        let responsive_client = ConnectionBuilder::address(private_bus.address.as_str())
            .expect("configure trayd Mix responsiveness client")
            .method_timeout(Duration::from_millis(500))
            .build()
            .expect("build trayd Mix responsiveness client");

        let create_service_name = service_name.clone();
        let create_call = std::thread::spawn(move || {
            create_client.call_method(
                Some(create_service_name.as_str()),
                OBJECT_PATH,
                Some(INTERFACE_NAME),
                "CreateMixScript",
                &("Slow-script", "blocked test store"),
            )
        });
        create_gate.wait_until_entered();

        let ping = responsive_client.call_method(
            Some(service_name.as_str()),
            OBJECT_PATH,
            Some("org.freedesktop.DBus.Peer"),
            "Ping",
            &(),
        );
        create_gate.release();
        drop(create_gate_release);
        let create_reply = join_with_timeout(create_call, "CreateMixScript caller")
            .expect("join CreateMixScript caller")
            .expect("CreateMixScript completes after the test store is released");
        let script_id = create_reply
            .body()
            .deserialize::<String>()
            .expect("decode Mix script identity");

        assert!(
            ping.is_ok(),
            "Peer.Ping must remain responsive while CreateMixScript is blocked: {ping:?}"
        );
        assert!(!script_id.is_empty());
        drop(server);
    }

    #[test]
    fn edit_mix_script_keeps_the_serving_connection_responsive_during_launch() {
        let private_bus = PrivateSessionBus::start();
        let service_name = format!("dev.cosmix.trayd.mix.edit.test.p{}", std::process::id());
        let service = TrayDaemon::new_test();
        let script_id = service
            .mix
            .create("Editable-script", "launcher responsiveness test")
            .expect("create editable Mix script");
        let launch_gate = service.launcher.block_next_launch();
        let launch_gate_release = TestGateRelease(Arc::clone(&launch_gate));
        let server = ConnectionBuilder::address(private_bus.address.as_str())
            .expect("configure trayd Mix edit test bus")
            .serve_at(OBJECT_PATH, service)
            .expect("serve trayd Mix edit test interface")
            .name(service_name.as_str())
            .expect("configure trayd Mix edit test name")
            .build()
            .expect("build trayd Mix edit test connection");
        let edit_client = ConnectionBuilder::address(private_bus.address.as_str())
            .expect("configure trayd Mix edit client")
            .method_timeout(Duration::from_secs(5))
            .build()
            .expect("build trayd Mix edit client");
        let responsive_client = ConnectionBuilder::address(private_bus.address.as_str())
            .expect("configure trayd Mix edit responsiveness client")
            .method_timeout(Duration::from_millis(500))
            .build()
            .expect("build trayd Mix edit responsiveness client");

        let edit_service_name = service_name.clone();
        let edit_call = std::thread::spawn(move || {
            edit_client.call_method(
                Some(edit_service_name.as_str()),
                OBJECT_PATH,
                Some(INTERFACE_NAME),
                "EditMixScript",
                &script_id,
            )
        });
        launch_gate.wait_until_entered();

        let ping = responsive_client.call_method(
            Some(service_name.as_str()),
            OBJECT_PATH,
            Some("org.freedesktop.DBus.Peer"),
            "Ping",
            &(),
        );
        launch_gate.release();
        drop(launch_gate_release);
        join_with_timeout(edit_call, "EditMixScript caller")
            .expect("join EditMixScript caller")
            .expect("EditMixScript completes after the launcher is released");

        assert!(
            ping.is_ok(),
            "Peer.Ping must remain responsive while EditMixScript launch is blocked: {ping:?}"
        );
        drop(server);
    }

    #[test]
    fn private_bus_startup_timeout_reaps_a_silent_daemon() {
        let started = Instant::now();
        let error = match PrivateSessionBus::start_with_args(
            &["--session", "--nofork", "--nopidfile"],
            Duration::from_millis(250),
        ) {
            Ok(_) => panic!("a silent dbus-daemon must not pass startup"),
            Err(error) => error,
        };

        assert!(
            error.message.contains("did not print an address"),
            "unexpected startup error: {}",
            error.message
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "silent dbus-daemon startup was not bounded"
        );
        let pid = error.pid.expect("the silent child was spawned");
        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "silent dbus-daemon child {pid} was not reaped"
        );
    }

    #[test]
    fn overlapping_refresh_runs_one_coalesced_follow_up_pass() {
        let refresh = Arc::new(RefreshControl::new());
        assert_eq!(refresh.request(), RefreshDecision::Start);
        let passes = Arc::new(AtomicUsize::new(0));
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let worker_control = Arc::clone(&refresh);
        let worker_passes = Arc::clone(&passes);
        let worker = thread::spawn(move || {
            run_coalesced_refreshes(&worker_control, || {
                let pass = worker_passes.fetch_add(1, Ordering::SeqCst);
                if pass == 0 {
                    started_tx.send(()).expect("announce first pass");
                    release_rx.recv().expect("release first pass");
                }
            });
        });
        started_rx.recv().expect("first pass started");
        assert_eq!(
            refresh.request(),
            RefreshDecision::Queued { stalled_for: None }
        );
        release_tx.send(()).expect("release worker");
        join_with_timeout(worker, "refresh worker").expect("refresh worker");
        assert_eq!(passes.load(Ordering::SeqCst), 2);
        assert_eq!(refresh.request(), RefreshDecision::Start);
    }

    #[test]
    fn a_panicking_refresh_still_releases_the_slot() {
        let refresh = Arc::new(RefreshControl::new());
        assert_eq!(refresh.request(), RefreshDecision::Start);
        let worker = Arc::clone(&refresh);
        let panicked = join_with_timeout(
            thread::spawn(move || {
                run_coalesced_refreshes(&worker, || panic!("discovery blew up"));
            }),
            "panicking refresh worker",
        );
        assert!(panicked.is_err(), "the worker really did unwind");
        assert!(
            matches!(refresh.request(), RefreshDecision::Start),
            "the slot is free again after a panicking refresh"
        );
    }

    #[test]
    fn overlapping_stalled_refresh_is_observable_without_a_timer() {
        let refresh = RefreshControl::new();
        let started = Instant::now();
        assert_eq!(refresh.request_at(started), RefreshDecision::Start);
        assert_eq!(
            refresh.request_at(started + STALLED_REFRESH_AFTER),
            RefreshDecision::Queued {
                stalled_for: Some(STALLED_REFRESH_AFTER)
            }
        );
    }

    #[test]
    fn app_allowlist_rejects_unknown_and_non_launchable_slugs() {
        let snapshot = ready_snapshot();
        assert_eq!(
            allowed_app(&snapshot, "missing"),
            Err(ActionError::UnknownApp(
                "unknown application slug: missing".into()
            ))
        );
        assert_eq!(
            allowed_app(&snapshot, "broken"),
            Err(ActionError::NotLaunchable(
                "application broken has no runnable argv".into()
            ))
        );
        assert_eq!(
            allowed_app(&snapshot, "tower").unwrap(),
            ["/opt/cosmix/bin/cosmix-tower"]
        );
    }

    #[test]
    fn daemon_allowlist_accepts_only_the_three_control_verbs() {
        let snapshot = ready_snapshot();
        for verb in ["start", "stop", "restart"] {
            assert_eq!(
                allowed_control(&snapshot, "system", "cosmix-noded.service", verb),
                Ok(Manager::System)
            );
        }
        for verb in ["; rm -rf /", "reload"] {
            assert!(matches!(
                allowed_control(&snapshot, "system", "cosmix-noded.service", verb),
                Err(ActionError::BadVerb(_))
            ));
        }
    }

    #[test]
    fn daemon_allowlist_rejects_unknown_units() {
        let snapshot = ready_snapshot();
        assert!(matches!(
            allowed_unit(&snapshot, "system", "cosmix-unknown.service"),
            Err(ActionError::UnknownUnit(_))
        ));
        assert!(matches!(
            allowed_unit(&snapshot, "user", "cosmix-noded.service"),
            Err(ActionError::UnknownUnit(_))
        ));
    }

    #[test]
    fn every_action_is_not_ready_before_the_first_snapshot() {
        let snapshot = Snapshot::default();
        assert!(matches!(
            allowed_app(&snapshot, "tower"),
            Err(ActionError::NotReady(message)) if message.contains("call Refresh first")
        ));
        assert!(matches!(
            allowed_control(&snapshot, "system", "cosmix-noded.service", "start"),
            Err(ActionError::NotReady(message)) if message.contains("call Refresh first")
        ));
        assert!(matches!(
            allowed_unit(&snapshot, "system", "cosmix-noded.service"),
            Err(ActionError::NotReady(message)) if message.contains("call Refresh first")
        ));
    }

    #[test]
    fn get_snapshot_is_atomic_across_all_published_fields() {
        fn tagged(revision: u64) -> Snapshot {
            let tag = revision.to_string();
            Snapshot {
                revision,
                noded_reachable: revision.is_multiple_of(2),
                apps: vec![DesktopApp {
                    slug: tag.clone(),
                    label: tag.clone(),
                    icon: Some(tag.clone()),
                    argv: Some(vec![tag.clone()]),
                }],
                apps_error: format!("apps-{tag}"),
                daemons: vec![DaemonUnit {
                    manager: if revision.is_multiple_of(2) {
                        Manager::System
                    } else {
                        Manager::User
                    },
                    unit: format!("{tag}.service"),
                    status: UnitStatus::Active,
                }],
                daemons_error: format!("daemons-{tag}"),
                refresh_error: format!("refresh-{tag}"),
            }
        }

        let service = TrayDaemon::new_test();
        *service.snapshot() = tagged(1);
        let writer_service = service.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let writer_stop = Arc::clone(&stop);
        let writer = thread::spawn(move || {
            let mut revision = 2;
            while !writer_stop.load(Ordering::Relaxed) {
                *writer_service.snapshot() = tagged(revision);
                revision = if revision == 2 { 1 } else { 2 };
            }
        });

        for _ in 0..10_000 {
            let snapshot = service.get_snapshot().0;
            let revision = snapshot.revision;
            let tag = revision.to_string();
            assert!(snapshot.noded_checked);
            assert_eq!(snapshot.noded_reachable, revision.is_multiple_of(2));
            assert_eq!(snapshot.apps[0].0, tag);
            assert_eq!(snapshot.apps[0].1, tag);
            assert_eq!(snapshot.apps[0].2, tag);
            assert_eq!(snapshot.apps_error, format!("apps-{tag}"));
            assert_eq!(
                snapshot.daemons[0].0,
                if revision.is_multiple_of(2) {
                    "system"
                } else {
                    "user"
                }
            );
            assert_eq!(snapshot.daemons[0].1, format!("{tag}.service"));
            assert_eq!(snapshot.daemons_error, format!("daemons-{tag}"));
            assert_eq!(snapshot.refresh_error, format!("refresh-{tag}"));
        }
        stop.store(true, Ordering::Relaxed);
        join_with_timeout(writer, "snapshot writer").expect("snapshot writer");
    }

    fn parse_sections(input: &str) -> HashMap<(String, String), String> {
        let mut section = String::new();
        let mut values = HashMap::new();
        for raw_line in input.lines() {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            if let Some(name) = line
                .strip_prefix('[')
                .and_then(|line| line.strip_suffix(']'))
            {
                section = name.to_owned();
                continue;
            }
            let (key, value) = line.split_once('=').expect("key=value line");
            let value = value.trim();
            let value = value
                .strip_prefix('"')
                .and_then(|value| value.strip_suffix('"'))
                .unwrap_or(value);
            values.insert((section.clone(), key.trim().to_owned()), value.to_owned());
        }
        values
    }

    #[test]
    fn deployment_identity_is_derived_from_the_package_slug() {
        let package = env!("CARGO_PKG_NAME");
        let slug = package
            .strip_prefix("cosmix-")
            .expect("cosmix package prefix");
        let binary = format!("cosmix-{slug}");
        let bus = format!("dev.cosmix.{slug}");
        let unit = format!("{binary}.service");
        let executable = format!("/opt/cosmix/bin/{binary}");

        assert_eq!(env!("CARGO_BIN_NAME"), binary);
        assert_eq!(BUS_NAME, INTERFACE_NAME);
        assert_eq!(BUS_NAME, bus);
        assert_eq!(OBJECT_PATH, "/dev/cosmix/trayd");

        let manifest = parse_sections(include_str!("../Cargo.toml"));
        assert_eq!(
            manifest.get(&("package.metadata.cosmix".into(), "component".into())),
            Some(&slug.to_owned())
        );
        assert_eq!(
            manifest.get(&("[bin]".into(), "name".into())),
            Some(&binary)
        );

        let activation = parse_sections(include_str!("../dbus/dev.cosmix.trayd.service"));
        assert_eq!(
            activation.get(&("D-BUS Service".into(), "Name".into())),
            Some(&bus)
        );
        assert_eq!(
            activation
                .get(&("D-BUS Service".into(), "Exec".into()))
                .and_then(|value| value.split_whitespace().next()),
            Some(executable.as_str())
        );
        assert_eq!(
            activation.get(&("D-BUS Service".into(), "SystemdService".into())),
            Some(&unit)
        );

        let systemd = parse_sections(include_str!("../systemd/cosmix-trayd.service"));
        assert_eq!(
            systemd
                .get(&("Service".into(), "Type".into()))
                .map(String::as_str),
            Some("dbus")
        );
        assert_eq!(
            systemd.get(&("Service".into(), "BusName".into())),
            Some(&bus)
        );
        assert_eq!(
            systemd
                .get(&("Service".into(), "ExecStart".into()))
                .and_then(|value| value.split_whitespace().next()),
            Some(executable.as_str())
        );

        let xml = include_str!("../dbus/dev.cosmix.trayd.xml");
        assert!(xml.contains(&format!("<node name=\"{OBJECT_PATH}\">")));
        assert!(xml.contains(&format!("<interface name=\"{bus}\">")));
        assert!(xml.contains("type=\"(tbba(sssb)sa(sss)ss)\" direction=\"out\""));
        assert!(xml.contains("<method name=\"OpenBusSession\">"));
        assert!(xml.contains("<method name=\"UpdateBusSession\">"));
        assert!(xml.contains("<method name=\"KeepBusSessionAlive\">"));
        assert!(xml.contains("<method name=\"CloseBusSession\">"));
        assert!(xml.contains("<method name=\"RefreshBusRoster\">"));
        assert!(
            xml.contains("type=\"(tssbtasasssa(ssbs)asa(tssssssssbxttss)tt)\" direction=\"out\"")
        );
        assert!(xml.contains("<signal name=\"BusChanged\">"));
        assert!(xml.contains("<signal name=\"BusTrafficBatch\">"));
        assert!(xml.contains("name=\"filter_epoch\" type=\"t\""));
        assert!(xml.contains("name=\"events\" type=\"a(tssssssssbxttss)\""));
        assert!(xml.contains("<property name=\"BusActive\" type=\"b\" access=\"read\"/>"));
        assert!(xml.contains("<property name=\"BusState\" type=\"s\" access=\"read\"/>"));
        assert!(xml.contains("<property name=\"BusRevision\" type=\"t\" access=\"read\"/>"));
        assert!(xml.contains("<method name=\"GetMixSnapshot\">"));
        assert!(xml.contains("<method name=\"CreateMixScript\">"));
        assert!(xml.contains("<method name=\"UpdateMixScript\">"));
        assert!(xml.contains("<method name=\"TrashMixScript\">"));
        assert!(xml.contains("<method name=\"RestoreMixScript\">"));
        assert!(xml.contains("<method name=\"PurgeMixScript\">"));
        assert!(xml.contains("<method name=\"EditMixScript\">"));
        assert!(xml.contains("<method name=\"RunMixScript\">"));
        assert!(xml.contains("<method name=\"StopMixRun\">"));
        assert!(xml.contains("type=\"(tssa(sssbtt)a(ssssttbissttt)u)\" direction=\"out\""));
        assert!(xml.contains("<signal name=\"MixChanged\">"));
        assert!(xml.contains("<signal name=\"MixRunChanged\">"));
        assert!(xml.contains("<signal name=\"MixRunOutput\">"));
        assert!(xml.contains("name=\"chunks\" type=\"a(tss)\""));
        assert!(xml.contains("<property name=\"MixRevision\" type=\"t\" access=\"read\"/>"));
        assert!(xml.contains("<property name=\"MixState\" type=\"s\" access=\"read\"/>"));
        assert!(xml.contains("<property name=\"MixError\" type=\"s\" access=\"read\"/>"));
        assert!(xml.contains("<property name=\"MixActiveRuns\" type=\"u\" access=\"read\"/>"));
        assert!(xml.contains("<method name=\"GetSshSnapshot\">"));
        assert!(xml.contains("<method name=\"ConnectSshHost\">"));
        assert!(xml.contains("<method name=\"ProbeSshHosts\">"));
        assert!(xml.contains("<method name=\"CreateSshHost\">"));
        assert!(xml.contains("<method name=\"EditSshHost\">"));
        assert!(xml.contains("<method name=\"TrashSshHost\">"));
        assert!(xml.contains("<method name=\"RestoreSshHost\">"));
        assert!(xml.contains("<method name=\"PurgeSshHost\">"));
        assert!(xml.contains("name=\"port\" type=\"u\" direction=\"in\""));
        assert!(xml.contains("type=\"(tssa(ssssqssbsstt)a(sss)u)\" direction=\"out\""));
        assert!(xml.contains("<signal name=\"SshChanged\">"));
        assert!(xml.contains("<property name=\"SshRevision\" type=\"t\" access=\"read\"/>"));
        assert!(xml.contains("<property name=\"SshState\" type=\"s\" access=\"read\"/>"));
        assert!(xml.contains("<property name=\"SshError\" type=\"s\" access=\"read\"/>"));
        assert!(xml.contains("<property name=\"SshActiveProbes\" type=\"u\" access=\"read\"/>"));

        let mut generated = String::new();
        TrayDaemon::new_test().introspect_to_writer(&mut generated, 0);
        for required in [
            r#"<method name="OpenBusSession">"#,
            r#"<method name="UpdateBusSession">"#,
            r#"<method name="KeepBusSessionAlive">"#,
            r#"<method name="CloseBusSession">"#,
            r#"<method name="RefreshBusRoster">"#,
            r#"<method name="GetBusSnapshot">"#,
            r#"<signal name="BusChanged">"#,
            r#"<signal name="BusTrafficBatch">"#,
            r#"<property name="BusActive" type="b" access="read"/>"#,
            r#"<property name="BusState" type="s" access="read"/>"#,
            r#"<property name="BusRevision" type="t" access="read"/>"#,
            r#"<method name="GetMixSnapshot">"#,
            r#"<method name="CreateMixScript">"#,
            r#"<method name="UpdateMixScript">"#,
            r#"<method name="TrashMixScript">"#,
            r#"<method name="RestoreMixScript">"#,
            r#"<method name="PurgeMixScript">"#,
            r#"<method name="EditMixScript">"#,
            r#"<method name="RunMixScript">"#,
            r#"<method name="StopMixRun">"#,
            r#"<signal name="MixChanged">"#,
            r#"<signal name="MixRunChanged">"#,
            r#"<signal name="MixRunOutput">"#,
            r#"<property name="MixRevision" type="t" access="read"/>"#,
            r#"<property name="MixState" type="s" access="read"/>"#,
            r#"<property name="MixError" type="s" access="read"/>"#,
            r#"<property name="MixActiveRuns" type="u" access="read"/>"#,
            r#"<method name="GetSshSnapshot">"#,
            r#"<method name="ConnectSshHost">"#,
            r#"<method name="ProbeSshHosts">"#,
            r#"<method name="CreateSshHost">"#,
            r#"<method name="EditSshHost">"#,
            r#"<method name="TrashSshHost">"#,
            r#"<method name="RestoreSshHost">"#,
            r#"<method name="PurgeSshHost">"#,
            r#"<signal name="SshChanged">"#,
            r#"<property name="SshRevision" type="t" access="read"/>"#,
            r#"<property name="SshState" type="s" access="read"/>"#,
            r#"<property name="SshError" type="s" access="read"/>"#,
            r#"<property name="SshActiveProbes" type="u" access="read"/>"#,
        ] {
            assert!(
                generated.contains(required),
                "generated D-Bus interface is missing {required}"
            );
        }
        assert!(
            generated.contains(
                r#"<arg name="snapshot" type="(tssbtasasssa(ssbs)asa(tssssssssbxttss)tt)" direction="out"/>"#
            ),
            "{generated}"
        );
        assert!(generated.contains(r#"<arg name="session_id" type="s" direction="out"/>"#));
        assert!(generated.contains(r#"<arg name="events" type="a(tssssssssbxttss)"/>"#));
        assert!(generated.contains(r#"<arg name="filter_epoch" type="t"/>"#));
        assert!(generated.contains(
            r#"<arg name="snapshot" type="(tssa(sssbtt)a(ssssttbissttt)u)" direction="out"/>"#
        ));
        assert!(generated.contains(r#"<arg name="chunks" type="a(tss)"/>"#));
        assert!(generated.contains(r#"<arg name="script_id" type="s" direction="out"/>"#));
        assert!(generated.contains(r#"<arg name="run_id" type="s" direction="out"/>"#));
        assert!(generated.contains(
            r#"<arg name="snapshot" type="(tssa(ssssqssbsstt)a(sss)u)" direction="out"/>"#
        ));
        assert!(generated.contains(r#"<arg name="ids" type="as" direction="in"/>"#));
        assert!(generated.contains(r#"<arg name="port" type="u" direction="in"/>"#));
        assert!(generated.contains(r#"<arg name="key_id" type="s" direction="in"/>"#));
    }

    #[test]
    fn action_errors_keep_the_published_dbus_names() {
        for (error, expected) in [
            (
                ActionError::NotReady(String::new()),
                "dev.cosmix.trayd.Error.NotReady",
            ),
            (
                ActionError::UnknownApp(String::new()),
                "dev.cosmix.trayd.Error.UnknownApp",
            ),
            (
                ActionError::NotLaunchable(String::new()),
                "dev.cosmix.trayd.Error.NotLaunchable",
            ),
            (
                ActionError::BadVerb(String::new()),
                "dev.cosmix.trayd.Error.BadVerb",
            ),
            (
                ActionError::UnknownUnit(String::new()),
                "dev.cosmix.trayd.Error.UnknownUnit",
            ),
        ] {
            assert_eq!(zbus::DBusError::name(&error).as_str(), expected);
        }
    }
}
