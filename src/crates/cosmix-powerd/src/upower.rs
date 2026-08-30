//! Event-driven UPower adapter.
//!
//! The tracker task polls the raw UPower signal stream while every multi-read
//! snapshot is in progress. This makes the signal stream position the scan
//! fence: a signal already received from zbus cannot sit in another task while
//! a mixed snapshot is accepted. Name-owner changes have their own watcher;
//! that watcher advances the shared epoch before awaiting delivery to the
//! reducer, so an in-progress scan always observes an ownership edge at its
//! final fence.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::future::{Future, pending};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use futures_util::{Stream, StreamExt};
use tokio::sync::mpsc;
use zbus::names::{BusName, WellKnownName};
use zbus::zvariant::OwnedObjectPath;
use zbus::{Connection, MatchRule, MessageStream, Proxy, fdo, message};

use crate::core::{BatterySnapshot, BatteryState, DeviceKind, PowerSnapshot};

const DBUS_DEST: &str = "org.freedesktop.DBus";
const DBUS_IFACE: &str = "org.freedesktop.DBus";
const UPOWER_DEST: &str = "org.freedesktop.UPower";
const UPOWER_PATH: &str = "/org/freedesktop/UPower";
const UPOWER_IFACE: &str = "org.freedesktop.UPower";
const UPOWER_DEVICE_IFACE: &str = "org.freedesktop.UPower.Device";
const RESYNC_INTERVAL: Duration = Duration::from_secs(5 * 60);
const READ_RETRY_INTERVAL: Duration = Duration::from_secs(60);
const DBUS_RECONNECT_DELAY: Duration = Duration::from_secs(60);
const SIGNAL_QUEUE_CAPACITY: usize = 256;
const MAX_SCAN_ATTEMPTS: usize = 3;

type EventStream = Pin<Box<dyn Stream<Item = Result<(), String>> + Send>>;

/// Owner lifecycle and snapshots share one ordered lane. The epoch atomic is
/// advanced on the owner signal's receive path before this update is awaited.
#[derive(Debug, Clone, PartialEq)]
pub enum TrackerUpdate {
    OwnerEpoch(u64),
    Snapshot {
        owner_epoch: u64,
        snapshot: PowerSnapshot,
    },
}

#[derive(Debug)]
enum OwnerWake {
    Changed,
    Ended(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScanOutcome {
    Clean,
    RacedOut,
    OwnerChanged,
}

/// Aborts a watcher when the connection tracker is cancelled or returns.
struct AbortTask(tokio::task::JoinHandle<()>);

impl Drop for AbortTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The UPower transport seam. The real implementation returns raw zbus-backed
/// streams; scripted tests drive the same attachment, scan-fence, activation
/// and owner-turnover loop without standing up a private D-Bus daemon.
trait UPowerBus: Clone + Send + Sync + 'static {
    async fn owner_changes(&self) -> Result<EventStream>;
    async fn signals(&self) -> Result<EventStream>;
    async fn current_owner(&self) -> Result<Option<String>>;
    async fn activate(&self) -> Result<()>;
    async fn read_snapshot(&self, owner: &str) -> Result<PowerSnapshot>;

    fn report_scan_storm(&self) {
        eprintln!(
            "cosmix-powerd: UPower changed during {MAX_SCAN_ATTEMPTS} consecutive scans; retaining the previous snapshot and rescanning"
        );
    }

    fn report_drained_signals(&self, _count: usize) {}
}

#[derive(Clone)]
struct ZbusUPowerBus {
    connection: Connection,
}

impl ZbusUPowerBus {
    fn new(connection: Connection) -> Self {
        Self { connection }
    }

    async fn message_stream(&self, rule: MatchRule<'_>, capacity: usize) -> Result<EventStream> {
        let stream = MessageStream::for_match_rule(rule, &self.connection, Some(capacity)).await?;
        Ok(Box::pin(stream.map(|message| {
            message.map(|_| ()).map_err(|error| error.to_string())
        })))
    }
}

impl UPowerBus for ZbusUPowerBus {
    async fn owner_changes(&self) -> Result<EventStream> {
        let rule = MatchRule::builder()
            .msg_type(message::Type::Signal)
            .sender(DBUS_DEST)?
            .interface(DBUS_IFACE)?
            .member("NameOwnerChanged")?
            .add_arg(UPOWER_DEST)?
            .build();
        self.message_stream(rule, 16).await
    }

    async fn signals(&self) -> Result<EventStream> {
        let rule = MatchRule::builder()
            .msg_type(message::Type::Signal)
            .sender(UPOWER_DEST)?
            .build();
        self.message_stream(rule, SIGNAL_QUEUE_CAPACITY).await
    }

    async fn current_owner(&self) -> Result<Option<String>> {
        let dbus = fdo::DBusProxy::new(&self.connection).await?;
        let name = BusName::try_from(UPOWER_DEST)?;
        if !dbus.name_has_owner(name.clone()).await? {
            return Ok(None);
        }
        Ok(Some(dbus.get_name_owner(name).await?.to_string()))
    }

    async fn activate(&self) -> Result<()> {
        let dbus = fdo::DBusProxy::new(&self.connection).await?;
        dbus.start_service_by_name(WellKnownName::try_from(UPOWER_DEST)?, 0)
            .await
            .context("activate org.freedesktop.UPower")?;
        Ok(())
    }

    async fn read_snapshot(&self, owner: &str) -> Result<PowerSnapshot> {
        read_snapshot(&self.connection, owner).await
    }
}

/// Keep the bounded zbus signal stream moving while a method reply is pending.
/// The returned count is also a scan fence: any signal observed around a read
/// means that result must not be published as a clean snapshot.
async fn await_bus_reply<B, F, T>(
    bus: &B,
    signals: &mut EventStream,
    future: F,
) -> Result<(Result<T>, usize)>
where
    B: UPowerBus,
    F: Future<Output = Result<T>>,
{
    tokio::pin!(future);
    let mut drained = 0usize;
    loop {
        tokio::select! {
            biased;
            signal = signals.next() => match signal {
                Some(Ok(())) => drained = drained.saturating_add(1),
                Some(Err(error)) => {
                    bus.report_drained_signals(drained);
                    return Err(anyhow!(error));
                }
                None => {
                    bus.report_drained_signals(drained);
                    return Err(anyhow!("UPower signal stream ended"));
                }
            },
            result = &mut future => {
                bus.report_drained_signals(drained);
                return Ok((result, drained));
            },
        }
    }
}

/// Raw UPower values at the adapter boundary, kept separate so mapping can be
/// tested without a live D-Bus or synthetic proxy server.
#[derive(Debug, Clone, PartialEq)]
pub struct RawDeviceProperties {
    pub kind: u32,
    pub power_supply: bool,
    pub present: bool,
    pub percentage: f64,
    pub state: u32,
    pub time_to_empty_s: i64,
    pub time_to_full_s: i64,
    pub energy_rate_w: f64,
    pub capacity_percent: f64,
}

impl RawDeviceProperties {
    pub fn into_snapshot(self, id: String) -> BatterySnapshot {
        // UPower documents zero as "not set" unless a property says otherwise,
        // and IsPresent=false makes the battery-only readings inapplicable.
        let percentage = self
            .present
            .then(|| finite_percent(self.percentage))
            .flatten();
        let time_to_empty_s = self
            .present
            .then(|| positive_seconds(self.time_to_empty_s))
            .flatten();
        let time_to_full_s = self
            .present
            .then(|| positive_seconds(self.time_to_full_s))
            .flatten();
        let energy_rate_w = self
            .present
            .then(|| finite_nonzero(self.energy_rate_w))
            .flatten();
        let health_percent = self
            .present
            .then(|| finite_positive_percent(self.capacity_percent))
            .flatten();
        BatterySnapshot {
            id,
            kind: DeviceKind::from_upower(self.kind),
            power_supply: self.power_supply,
            present: self.present,
            percentage,
            state: BatteryState::from_upower(self.state),
            time_to_empty_s,
            time_to_full_s,
            energy_rate_w,
            health_percent,
        }
    }
}

async fn advance_owner_epoch(
    epoch: &AtomicU64,
    updates: &mpsc::Sender<TrackerUpdate>,
) -> Result<u64> {
    let next = epoch.fetch_add(1, Ordering::AcqRel).saturating_add(1);
    updates
        .send(TrackerUpdate::OwnerEpoch(next))
        .await
        .map_err(|_| anyhow!("tracker update receiver closed"))?;
    Ok(next)
}

fn spawn_owner_watcher(
    mut changes: EventStream,
    epoch: Arc<AtomicU64>,
    updates: mpsc::Sender<TrackerUpdate>,
) -> (AbortTask, mpsc::Receiver<OwnerWake>) {
    let (wake_tx, wake_rx) = mpsc::channel(1);
    let task = tokio::spawn(async move {
        while let Some(change) = changes.next().await {
            match change {
                Ok(()) => {
                    // This increment is on the task that receives the signal
                    // from zbus and happens before either downstream await.
                    let next = epoch.fetch_add(1, Ordering::AcqRel).saturating_add(1);
                    if updates.send(TrackerUpdate::OwnerEpoch(next)).await.is_err() {
                        return;
                    }
                    let _ = wake_tx.try_send(OwnerWake::Changed);
                }
                Err(error) => {
                    let _ = wake_tx.send(OwnerWake::Ended(error)).await;
                    return;
                }
            }
        }
        let _ = wake_tx
            .send(OwnerWake::Ended("NameOwnerChanged stream ended".into()))
            .await;
    });
    (AbortTask(task), wake_rx)
}

/// Run forever, publishing complete logical snapshots through a bounded lane.
/// UPower absence is represented by `PowerSnapshot::default()`. A missing
/// system bus is retried every 60 seconds, never in a busy loop.
pub async fn run(
    update_tx: mpsc::Sender<TrackerUpdate>,
    owner_epoch: Arc<AtomicU64>,
) -> Result<()> {
    loop {
        match Connection::system().await {
            Ok(connection) => {
                let bus = ZbusUPowerBus::new(connection);
                if let Err(error) =
                    run_tracker(bus, update_tx.clone(), Arc::clone(&owner_epoch)).await
                {
                    if update_tx.is_closed() {
                        return Err(error);
                    }
                    eprintln!("cosmix-powerd: system D-Bus monitor ended: {error:#}");
                }
            }
            Err(error) => {
                let epoch = advance_owner_epoch(&owner_epoch, &update_tx).await?;
                update_tx
                    .send(TrackerUpdate::Snapshot {
                        owner_epoch: epoch,
                        snapshot: PowerSnapshot::default(),
                    })
                    .await
                    .map_err(|_| anyhow!("tracker update receiver closed"))?;
                eprintln!("cosmix-powerd: system D-Bus unavailable; retrying in 60s: {error}");
            }
        }
        tokio::time::sleep(DBUS_RECONNECT_DELAY).await;
    }
}

async fn run_tracker<B: UPowerBus>(
    bus: B,
    update_tx: mpsc::Sender<TrackerUpdate>,
    owner_epoch: Arc<AtomicU64>,
) -> Result<()> {
    // Install the match before the initial epoch/query. An edge during setup is
    // queued in this stream and the watcher advances the epoch when it polls it.
    let owner_changes = bus.owner_changes().await?;
    advance_owner_epoch(&owner_epoch, &update_tx).await?;
    let (_owner_watcher, mut owner_wakes) =
        spawn_owner_watcher(owner_changes, Arc::clone(&owner_epoch), update_tx.clone());
    // Install one well-known-name signal match for the tracker lifetime. Its
    // setup is raced against owner edges, then the resulting bounded stream is
    // drained alongside every later D-Bus method call.
    let signal_setup = bus.signals();
    tokio::pin!(signal_setup);
    let mut signals = loop {
        tokio::select! {
            result = &mut signal_setup => break result?,
            wake = owner_wakes.recv() => match wake {
                Some(OwnerWake::Changed) => {},
                Some(OwnerWake::Ended(error)) => return Err(anyhow!(error)),
                None => return Err(anyhow!("NameOwnerChanged watcher ended")),
            },
        }
    };

    let mut absence_published_at = None;
    loop {
        let (current_owner, _) = await_bus_reply(&bus, &mut signals, bus.current_owner()).await?;
        let current_owner = current_owner?;
        let Some(owner) = current_owner else {
            let epoch = owner_epoch.load(Ordering::Acquire);
            if absence_published_at != Some(epoch) {
                update_tx
                    .send(TrackerUpdate::Snapshot {
                        owner_epoch: epoch,
                        snapshot: PowerSnapshot::default(),
                    })
                    .await
                    .map_err(|_| anyhow!("tracker update receiver closed"))?;
                absence_published_at = Some(epoch);
                let (activation, _) = await_bus_reply(&bus, &mut signals, bus.activate()).await?;
                if let Err(error) = activation {
                    eprintln!(
                        "cosmix-powerd: UPower activation failed; waiting for owner edge: {error:#}"
                    );
                }
            }
            loop {
                tokio::select! {
                    signal = signals.next() => match signal {
                        Some(Ok(())) => bus.report_drained_signals(1),
                        Some(Err(error)) => return Err(anyhow!(error)),
                        None => return Err(anyhow!("UPower signal stream ended")),
                    },
                    wake = owner_wakes.recv() => match wake {
                        Some(OwnerWake::Changed) => break,
                        Some(OwnerWake::Ended(error)) => return Err(anyhow!(error)),
                        None => return Err(anyhow!("NameOwnerChanged watcher ended")),
                    },
                }
            }
            continue;
        };

        absence_published_at = None;
        let epoch = owner_epoch.load(Ordering::Acquire);
        match run_attached(
            &bus,
            &mut signals,
            &update_tx,
            &mut owner_wakes,
            &owner,
            epoch,
            &owner_epoch,
        )
        .await
        {
            Ok(()) => {}
            Err(error) => {
                eprintln!("cosmix-powerd: UPower attachment lost: {error:#}");
                let retry = tokio::time::sleep(READ_RETRY_INTERVAL);
                tokio::pin!(retry);
                loop {
                    tokio::select! {
                        signal = signals.next() => match signal {
                            Some(Ok(())) => bus.report_drained_signals(1),
                            Some(Err(error)) => return Err(anyhow!(error)),
                            None => return Err(anyhow!("UPower signal stream ended")),
                        },
                        wake = owner_wakes.recv() => match wake {
                            Some(OwnerWake::Changed) => break,
                            Some(OwnerWake::Ended(error)) => return Err(anyhow!(error)),
                            None => return Err(anyhow!("NameOwnerChanged watcher ended")),
                        },
                        _ = &mut retry => break,
                    }
                }
            }
        }
    }
}

async fn run_attached<B: UPowerBus>(
    bus: &B,
    signals: &mut EventStream,
    update_tx: &mpsc::Sender<TrackerUpdate>,
    owner_wakes: &mut mpsc::Receiver<OwnerWake>,
    owner: &str,
    owner_epoch: u64,
    epoch: &AtomicU64,
) -> Result<()> {
    let mut retry: Option<Pin<Box<tokio::time::Sleep>>> = None;
    let mut refresh_pending = true;
    let mut storm_logged = false;
    let resync = tokio::time::sleep(RESYNC_INTERVAL);
    tokio::pin!(resync);

    loop {
        if !refresh_pending {
            tokio::select! {
                signal = signals.next() => match signal {
                    Some(Ok(())) => refresh_pending = true,
                    Some(Err(error)) => return Err(anyhow!(error)),
                    None => return Err(anyhow!("UPower signal stream ended")),
                },
                wake = owner_wakes.recv() => match wake {
                    Some(OwnerWake::Changed) => return Ok(()),
                    Some(OwnerWake::Ended(error)) => return Err(anyhow!(error)),
                    None => return Err(anyhow!("NameOwnerChanged watcher ended")),
                },
                _ = &mut resync => {
                    resync.as_mut().reset(tokio::time::Instant::now() + RESYNC_INTERVAL);
                    refresh_pending = true;
                },
                _ = async {
                    match retry.as_mut() {
                        Some(timer) => timer.as_mut().await,
                        None => pending().await,
                    }
                } => {
                    retry = None;
                    refresh_pending = true;
                },
            }
            continue;
        }

        refresh_pending = false;
        match publish_current_snapshot(bus, signals, update_tx, owner, owner_epoch, epoch).await {
            Ok(ScanOutcome::Clean) => {
                retry = None;
                storm_logged = false;
            }
            Ok(ScanOutcome::OwnerChanged) => return Ok(()),
            Ok(ScanOutcome::RacedOut) => {
                // The signal(s) consumed while fencing remain represented by
                // this local wake. Do not publish the raced third result.
                refresh_pending = true;
                if !storm_logged {
                    bus.report_scan_storm();
                    storm_logged = true;
                }
            }
            Err(error) => {
                eprintln!("cosmix-powerd: UPower snapshot failed; retrying in 60s: {error:#}");
                retry = Some(Box::pin(tokio::time::sleep(READ_RETRY_INTERVAL)));
            }
        }
    }
}

/// Reserve reducer capacity before reading, fence each attempt directly
/// against the raw signal stream, verify the unique owner, then publish with no
/// await between the final epoch check and `Permit::send`.
async fn publish_current_snapshot<B: UPowerBus>(
    bus: &B,
    signals: &mut EventStream,
    update_tx: &mpsc::Sender<TrackerUpdate>,
    owner: &str,
    owner_epoch: u64,
    epoch: &AtomicU64,
) -> Result<ScanOutcome> {
    let permit = update_tx
        .reserve()
        .await
        .map_err(|_| anyhow!("tracker update receiver closed"))?;
    let Some(snapshot) = scan_with_signal_fence(bus, signals, owner, owner_epoch, epoch).await?
    else {
        return Ok(if epoch.load(Ordering::Acquire) != owner_epoch {
            ScanOutcome::OwnerChanged
        } else {
            ScanOutcome::RacedOut
        });
    };

    let (current_owner, drained) = await_bus_reply(bus, signals, bus.current_owner()).await?;
    let current_owner = current_owner?;
    if current_owner.as_deref() != Some(owner) || epoch.load(Ordering::Acquire) != owner_epoch {
        return Ok(ScanOutcome::OwnerChanged);
    }
    if drained != 0 {
        return Ok(ScanOutcome::RacedOut);
    }
    permit.send(TrackerUpdate::Snapshot {
        owner_epoch,
        snapshot,
    });
    Ok(ScanOutcome::Clean)
}

/// Poll the scan and the raw signal stream in the same task. A signal wins a
/// tie with scan completion, marks that attempt raced, and remains a pending
/// refresh after the three-attempt bound. `None` means no result is safe to
/// publish (storm or owner turnover).
async fn scan_with_signal_fence<B: UPowerBus>(
    bus: &B,
    signals: &mut EventStream,
    owner: &str,
    owner_epoch: u64,
    epoch: &AtomicU64,
) -> Result<Option<PowerSnapshot>> {
    for _ in 0..MAX_SCAN_ATTEMPTS {
        let (snapshot, drained) = await_bus_reply(bus, signals, bus.read_snapshot(owner)).await?;
        let snapshot = snapshot?;
        if epoch.load(Ordering::Acquire) != owner_epoch {
            return Ok(None);
        }
        if drained == 0 {
            return Ok(Some(snapshot));
        }
    }
    Ok(None)
}

async fn read_snapshot(connection: &Connection, owner: &str) -> Result<PowerSnapshot> {
    let manager = Proxy::new(connection, owner, UPOWER_PATH, UPOWER_IFACE).await?;
    let on_battery = manager
        .get_property::<bool>("OnBattery")
        .await
        .context("read UPower.OnBattery")?;
    let display_path: OwnedObjectPath = manager
        .call("GetDisplayDevice", &())
        .await
        .context("call UPower.GetDisplayDevice")?;
    let device_paths: Vec<OwnedObjectPath> = manager
        .call("EnumerateDevices", &())
        .await
        .context("call UPower.EnumerateDevices")?;

    let display = Some(
        read_device(connection, owner, display_path.as_str(), "display".into())
            .await
            .context("read UPower DisplayDevice")?,
    );
    let mut devices = BTreeMap::new();
    for path in device_paths {
        let id = device_id(path.as_str())?;
        // A per-device read error is not proof of removal. The queued signal
        // or the 60-second retry performs a fresh enumeration.
        let device = read_device(connection, owner, path.as_str(), id.clone())
            .await
            .with_context(|| format!("read enumerated UPower device {}", path.as_str()))?;
        devices.insert(id, device);
    }

    Ok(PowerSnapshot::from_parts(on_battery, display, devices))
}

async fn read_device(
    connection: &Connection,
    owner: &str,
    path: &str,
    id: String,
) -> Result<BatterySnapshot> {
    let proxy = Proxy::new(connection, owner, path, UPOWER_DEVICE_IFACE).await?;
    let raw = RawDeviceProperties {
        kind: proxy.get_property("Type").await?,
        power_supply: proxy.get_property("PowerSupply").await?,
        present: proxy.get_property("IsPresent").await?,
        percentage: proxy.get_property("Percentage").await?,
        state: proxy.get_property("State").await?,
        time_to_empty_s: proxy.get_property("TimeToEmpty").await?,
        time_to_full_s: proxy.get_property("TimeToFull").await?,
        energy_rate_w: proxy.get_property("EnergyRate").await?,
        capacity_percent: proxy.get_property("Capacity").await?,
    };
    Ok(raw.into_snapshot(id))
}

fn device_id(path: &str) -> Result<String> {
    if path.is_empty() {
        return Err(anyhow!("UPower returned an empty device object path"));
    }
    let mut id = String::with_capacity(2 + path.len() * 2);
    id.push_str("d_");
    for byte in path.bytes() {
        write!(&mut id, "{byte:02x}").expect("writing to String cannot fail");
    }
    Ok(id)
}

fn finite_percent(value: f64) -> Option<f64> {
    (value.is_finite() && (0.0..=100.0).contains(&value)).then_some(value)
}

fn finite_positive_percent(value: f64) -> Option<f64> {
    (value.is_finite() && value > 0.0 && value <= 100.0).then_some(value)
}

fn finite_nonzero(value: f64) -> Option<f64> {
    (value.is_finite() && value != 0.0).then_some(value)
}

fn positive_seconds(value: i64) -> Option<u64> {
    u64::try_from(value).ok().filter(|value| *value > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize};

    use futures_util::stream;

    type ScriptEventReceiver = mpsc::Receiver<Result<(), String>>;

    fn receiver_stream(rx: ScriptEventReceiver) -> EventStream {
        Box::pin(stream::unfold(rx, |mut rx| async {
            rx.recv().await.map(|item| (item, rx))
        }))
    }

    #[derive(Clone)]
    struct ScriptedBus {
        owner: Arc<Mutex<Option<String>>>,
        owner_changes: Arc<Mutex<Option<ScriptEventReceiver>>>,
        signal_streams: Arc<Mutex<VecDeque<ScriptEventReceiver>>>,
        scan_started: mpsc::Sender<usize>,
        scan_midpoint: mpsc::Sender<usize>,
        scan_permits: Arc<tokio::sync::Mutex<mpsc::Receiver<()>>>,
        scan_calls: Arc<AtomicUsize>,
        snapshots: Arc<Mutex<VecDeque<PowerSnapshot>>>,
        activations: Arc<AtomicUsize>,
        fail_activation: Arc<AtomicBool>,
        storm_reports: Arc<AtomicUsize>,
        owner_calls: Arc<AtomicUsize>,
        gated_owner_call: Arc<AtomicUsize>,
        owner_check_started: mpsc::Sender<usize>,
        owner_reply_permits: Arc<tokio::sync::Mutex<mpsc::Receiver<()>>>,
        drained_signals: Arc<AtomicUsize>,
    }

    impl UPowerBus for ScriptedBus {
        async fn owner_changes(&self) -> Result<EventStream> {
            let rx = self
                .owner_changes
                .lock()
                .expect("owner stream mutex poisoned")
                .take()
                .ok_or_else(|| anyhow!("owner stream already taken"))?;
            Ok(receiver_stream(rx))
        }

        async fn signals(&self) -> Result<EventStream> {
            let rx = self
                .signal_streams
                .lock()
                .expect("signal stream mutex poisoned")
                .pop_front()
                .ok_or_else(|| anyhow!("no scripted signal stream"))?;
            Ok(receiver_stream(rx))
        }

        async fn current_owner(&self) -> Result<Option<String>> {
            let call = self.owner_calls.fetch_add(1, Ordering::Relaxed);
            let _ = self.owner_check_started.try_send(call);
            if call == self.gated_owner_call.load(Ordering::Acquire) {
                self.owner_reply_permits
                    .lock()
                    .await
                    .recv()
                    .await
                    .ok_or_else(|| anyhow!("owner reply permits closed"))?;
            }
            Ok(self.owner.lock().expect("owner mutex poisoned").clone())
        }

        async fn activate(&self) -> Result<()> {
            self.activations.fetch_add(1, Ordering::Relaxed);
            if self.fail_activation.load(Ordering::Relaxed) {
                Err(anyhow!("activation unavailable"))
            } else {
                Ok(())
            }
        }

        async fn read_snapshot(&self, _owner: &str) -> Result<PowerSnapshot> {
            let call = self.scan_calls.fetch_add(1, Ordering::Relaxed);
            self.scan_started
                .send(call)
                .await
                .map_err(|_| anyhow!("scan observer closed"))?;
            self.scan_permits
                .lock()
                .await
                .recv()
                .await
                .ok_or_else(|| anyhow!("scan permits closed"))?;
            self.scan_midpoint
                .send(call)
                .await
                .map_err(|_| anyhow!("scan midpoint observer closed"))?;
            self.scan_permits
                .lock()
                .await
                .recv()
                .await
                .ok_or_else(|| anyhow!("scan permits closed"))?;
            self.snapshots
                .lock()
                .expect("snapshot mutex poisoned")
                .pop_front()
                .ok_or_else(|| anyhow!("no scripted snapshot"))
        }

        fn report_scan_storm(&self) {
            self.storm_reports.fetch_add(1, Ordering::Relaxed);
        }

        fn report_drained_signals(&self, count: usize) {
            self.drained_signals.fetch_add(count, Ordering::Relaxed);
        }
    }

    struct Harness {
        bus: ScriptedBus,
        owner_tx: mpsc::Sender<Result<(), String>>,
        signal_txs: VecDeque<mpsc::Sender<Result<(), String>>>,
        scan_started: mpsc::Receiver<usize>,
        scan_midpoint: mpsc::Receiver<usize>,
        scan_permit_tx: mpsc::Sender<()>,
        owner_check_started: mpsc::Receiver<usize>,
        owner_reply_permit_tx: mpsc::Sender<()>,
    }

    fn harness(owner: Option<&str>, snapshots: Vec<PowerSnapshot>, attachments: usize) -> Harness {
        let (owner_tx, owner_rx) = mpsc::channel(8);
        let (scan_started_tx, scan_started) = mpsc::channel(16);
        let (scan_midpoint_tx, scan_midpoint) = mpsc::channel(16);
        let (scan_permit_tx, scan_permits) = mpsc::channel(16);
        let (owner_check_started_tx, owner_check_started) = mpsc::channel(16);
        let (owner_reply_permit_tx, owner_reply_permits) = mpsc::channel(1);
        let mut signal_txs = VecDeque::new();
        let mut signal_streams = VecDeque::new();
        for _ in 0..attachments {
            let (tx, rx) = mpsc::channel(SIGNAL_QUEUE_CAPACITY);
            signal_txs.push_back(tx);
            signal_streams.push_back(rx);
        }
        Harness {
            bus: ScriptedBus {
                owner: Arc::new(Mutex::new(owner.map(str::to_string))),
                owner_changes: Arc::new(Mutex::new(Some(owner_rx))),
                signal_streams: Arc::new(Mutex::new(signal_streams)),
                scan_started: scan_started_tx,
                scan_midpoint: scan_midpoint_tx,
                scan_permits: Arc::new(tokio::sync::Mutex::new(scan_permits)),
                scan_calls: Arc::new(AtomicUsize::new(0)),
                snapshots: Arc::new(Mutex::new(snapshots.into())),
                activations: Arc::new(AtomicUsize::new(0)),
                fail_activation: Arc::new(AtomicBool::new(false)),
                storm_reports: Arc::new(AtomicUsize::new(0)),
                owner_calls: Arc::new(AtomicUsize::new(0)),
                gated_owner_call: Arc::new(AtomicUsize::new(usize::MAX)),
                owner_check_started: owner_check_started_tx,
                owner_reply_permits: Arc::new(tokio::sync::Mutex::new(owner_reply_permits)),
                drained_signals: Arc::new(AtomicUsize::new(0)),
            },
            owner_tx,
            signal_txs,
            scan_started,
            scan_midpoint,
            scan_permit_tx,
            owner_check_started,
            owner_reply_permit_tx,
        }
    }

    #[test]
    fn raw_upower_properties_map_without_dbus_types() {
        let mapped = RawDeviceProperties {
            kind: 2,
            power_supply: true,
            present: true,
            percentage: 73.25,
            state: 5,
            time_to_empty_s: 0,
            time_to_full_s: 1_800,
            energy_rate_w: -14.2,
            capacity_percent: 88.5,
        }
        .into_snapshot("battery_BAT0".into());

        assert_eq!(mapped.kind, DeviceKind::Battery);
        assert_eq!(mapped.state, BatteryState::PendingCharge);
        assert_eq!(mapped.time_to_empty_s, None);
        assert_eq!(mapped.time_to_full_s, Some(1_800));
        assert_eq!(mapped.energy_rate_w, Some(-14.2));
        assert_eq!(mapped.health_percent, Some(88.5));
    }

    #[test]
    fn absent_values_and_non_finite_numbers_are_not_exposed() {
        let mapped = RawDeviceProperties {
            kind: 2,
            power_supply: false,
            present: false,
            percentage: 0.0,
            state: 3,
            time_to_empty_s: 900,
            time_to_full_s: 1_800,
            energy_rate_w: -12.5,
            capacity_percent: 80.0,
        }
        .into_snapshot("unknown".into());

        assert_eq!(mapped.kind, DeviceKind::Battery);
        assert_eq!(mapped.state, BatteryState::Empty);
        assert_eq!(mapped.percentage, None);
        assert_eq!(mapped.time_to_empty_s, None);
        assert_eq!(mapped.time_to_full_s, None);
        assert_eq!(mapped.energy_rate_w, None);
        assert_eq!(mapped.health_percent, None);
    }

    #[test]
    fn out_of_contract_percentages_are_not_clamped_into_valid_data() {
        assert_eq!(finite_percent(-0.1), None);
        assert_eq!(finite_percent(100.1), None);
        assert_eq!(finite_positive_percent(101.0), None);
    }

    #[tokio::test]
    async fn production_tracker_activates_reattaches_and_fences_a_mid_scan_signal() {
        let mixed = PowerSnapshot {
            on_battery: true,
            ..PowerSnapshot::default()
        };
        let clean = PowerSnapshot::default();
        let mut h = harness(None, vec![mixed, clean.clone()], 1);
        h.bus.fail_activation.store(true, Ordering::Relaxed);
        let signal_tx = h.signal_txs.pop_front().unwrap();
        let owner = Arc::clone(&h.bus.owner);
        let (update_tx, mut update_rx) = mpsc::channel(16);
        let epoch = Arc::new(AtomicU64::new(0));
        let tracker = tokio::spawn(run_tracker(h.bus.clone(), update_tx, Arc::clone(&epoch)));

        assert_eq!(update_rx.recv().await, Some(TrackerUpdate::OwnerEpoch(1)));
        assert_eq!(
            update_rx.recv().await,
            Some(TrackerUpdate::Snapshot {
                owner_epoch: 1,
                snapshot: PowerSnapshot::default(),
            })
        );
        assert_eq!(h.bus.activations.load(Ordering::Relaxed), 1);

        *owner.lock().unwrap() = Some(":1.42".into());
        h.owner_tx.send(Ok(())).await.unwrap();
        assert_eq!(update_rx.recv().await, Some(TrackerUpdate::OwnerEpoch(2)));
        assert_eq!(h.scan_started.recv().await, Some(0));
        h.scan_permit_tx.send(()).await.unwrap();
        assert_eq!(h.scan_midpoint.recv().await, Some(0));
        signal_tx.send(Ok(())).await.unwrap();
        h.scan_permit_tx.send(()).await.unwrap();
        assert_eq!(h.scan_started.recv().await, Some(1));
        h.scan_permit_tx.send(()).await.unwrap();
        assert_eq!(h.scan_midpoint.recv().await, Some(1));
        h.scan_permit_tx.send(()).await.unwrap();
        assert_eq!(
            update_rx.recv().await,
            Some(TrackerUpdate::Snapshot {
                owner_epoch: 2,
                snapshot: clean,
            })
        );

        tracker.abort();
    }

    #[tokio::test]
    async fn final_owner_check_drains_more_than_signal_queue_capacity() {
        let mut h = harness(
            Some(":1.50"),
            vec![PowerSnapshot::default(), PowerSnapshot::default()],
            1,
        );
        h.bus.gated_owner_call.store(1, Ordering::Release);
        let signal_tx = h.signal_txs.pop_front().unwrap();
        let (update_tx, mut update_rx) = mpsc::channel(16);
        let epoch = Arc::new(AtomicU64::new(0));
        let tracker = tokio::spawn(run_tracker(h.bus.clone(), update_tx, epoch));

        assert_eq!(update_rx.recv().await, Some(TrackerUpdate::OwnerEpoch(1)));
        assert_eq!(h.scan_started.recv().await, Some(0));
        h.scan_permit_tx.send(()).await.unwrap();
        assert_eq!(h.scan_midpoint.recv().await, Some(0));
        h.scan_permit_tx.send(()).await.unwrap();
        loop {
            if h.owner_check_started.recv().await == Some(1) {
                break;
            }
        }

        tokio::time::timeout(Duration::from_secs(2), async {
            for _ in 0..(SIGNAL_QUEUE_CAPACITY + 64) {
                signal_tx.send(Ok(())).await.unwrap();
            }
        })
        .await
        .expect("pending owner reply must not let the 256-item signal stream deadlock");
        h.owner_reply_permit_tx.send(()).await.unwrap();

        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), h.scan_started.recv())
                .await
                .expect("owner check should complete and immediately rescan"),
            Some(1)
        );
        assert!(
            h.bus.drained_signals.load(Ordering::Acquire) > SIGNAL_QUEUE_CAPACITY,
            "all burst signals must be counted while the owner reply is pending"
        );
        assert!(update_rx.try_recv().is_err());

        tracker.abort();
    }

    #[tokio::test]
    async fn owner_turnover_during_scan_advances_epoch_before_old_result_can_reduce() {
        let mut h = harness(
            Some(":1.10"),
            vec![PowerSnapshot::default(), PowerSnapshot::default()],
            2,
        );
        let owner = Arc::clone(&h.bus.owner);
        let (update_tx, mut update_rx) = mpsc::channel(16);
        let epoch = Arc::new(AtomicU64::new(0));
        let tracker = tokio::spawn(run_tracker(h.bus.clone(), update_tx, Arc::clone(&epoch)));

        assert_eq!(update_rx.recv().await, Some(TrackerUpdate::OwnerEpoch(1)));
        assert_eq!(h.scan_started.recv().await, Some(0));
        *owner.lock().unwrap() = Some(":1.11".into());
        h.owner_tx.send(Ok(())).await.unwrap();
        assert_eq!(update_rx.recv().await, Some(TrackerUpdate::OwnerEpoch(2)));
        h.scan_permit_tx.send(()).await.unwrap();
        assert_eq!(h.scan_midpoint.recv().await, Some(0));
        h.scan_permit_tx.send(()).await.unwrap();
        assert_eq!(h.scan_started.recv().await, Some(1));
        h.scan_permit_tx.send(()).await.unwrap();
        assert_eq!(h.scan_midpoint.recv().await, Some(1));
        h.scan_permit_tx.send(()).await.unwrap();
        assert_eq!(
            update_rx.recv().await,
            Some(TrackerUpdate::Snapshot {
                owner_epoch: 2,
                snapshot: PowerSnapshot::default(),
            })
        );

        tracker.abort();
    }

    #[tokio::test]
    async fn three_raced_attempts_publish_nothing_and_keep_the_refresh_queued() {
        let snapshots = vec![PowerSnapshot::default(); 5];
        let mut h = harness(Some(":1.20"), snapshots, 1);
        let signal_tx = h.signal_txs.pop_front().unwrap();
        let (update_tx, mut update_rx) = mpsc::channel(16);
        let epoch = Arc::new(AtomicU64::new(0));
        let tracker = tokio::spawn(run_tracker(h.bus.clone(), update_tx, epoch));

        assert_eq!(update_rx.recv().await, Some(TrackerUpdate::OwnerEpoch(1)));
        assert_eq!(h.scan_started.recv().await, Some(0));
        h.scan_permit_tx.send(()).await.unwrap();
        assert_eq!(h.scan_midpoint.recv().await, Some(0));
        h.scan_permit_tx.send(()).await.unwrap();
        assert!(matches!(
            update_rx.recv().await,
            Some(TrackerUpdate::Snapshot { .. })
        ));

        signal_tx.send(Ok(())).await.unwrap();
        for call in 1..=3 {
            assert_eq!(h.scan_started.recv().await, Some(call));
            h.scan_permit_tx.send(()).await.unwrap();
            assert_eq!(h.scan_midpoint.recv().await, Some(call));
            signal_tx.send(Ok(())).await.unwrap();
            h.scan_permit_tx.send(()).await.unwrap();
        }
        // The consumed storm wake immediately starts another scan, but no
        // raced result has entered the reducer lane.
        assert_eq!(h.scan_started.recv().await, Some(4));
        assert!(update_rx.try_recv().is_err());
        assert_eq!(h.bus.storm_reports.load(Ordering::Relaxed), 1);

        tracker.abort();
    }

    #[test]
    fn object_path_tail_is_a_stable_props_segment() {
        let first = device_id("/org/freedesktop/UPower/devices/battery_BAT0").unwrap();
        let second = device_id("/org/freedesktop/UPower/devices/battery_BAT0").unwrap();
        assert_eq!(first, second);
        assert!(first.starts_with("d_"));
        assert!(
            first
                .chars()
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
        );
        assert_ne!(
            first,
            device_id("/org/freedesktop/UPower/other/battery_BAT0").unwrap()
        );
    }
}
