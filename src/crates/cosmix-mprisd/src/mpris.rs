//! Event-driven session-bus MPRIS adapter and private transport seam.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use futures_util::{Stream, StreamExt, stream};
use tokio::sync::{mpsc, oneshot};
use zbus::names::BusName;
use zbus::proxy::{Builder as ProxyBuilder, CacheProperties};
use zbus::zvariant::{DynamicTuple, OwnedValue};
use zbus::{Connection, DBusError, MatchRule, MessageStream, Proxy, fdo, message};

use crate::core::{PlaybackStatus, PlayerMetadata, PlayerSnapshot, PlayingObservation, player_key};

const DBUS_DEST: &str = "org.freedesktop.DBus";
const DBUS_IFACE: &str = "org.freedesktop.DBus";
const MPRIS_PREFIX: &str = "org.mpris.MediaPlayer2.";
const MPRIS_PATH: &str = "/org/mpris/MediaPlayer2";
const ROOT_IFACE: &str = "org.mpris.MediaPlayer2";
const PLAYER_IFACE: &str = "org.mpris.MediaPlayer2.Player";
const PROPERTIES_IFACE: &str = "org.freedesktop.DBus.Properties";
const RESYNC_INTERVAL: Duration = Duration::from_secs(5 * 60);
const DBUS_RECONNECT_DELAY: Duration = Duration::from_secs(60);
/// Deadline for one request/reply exchange with a player. This bounds a
/// request; it is not a polling or scheduling interval.
const PLAYER_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const SIGNAL_QUEUE_CAPACITY: usize = 256;
const MAX_SCAN_ATTEMPTS: usize = 3;
const MAX_CONCURRENT_PLAYER_READS: usize = 8;
const CONTROL_CALL_TIMEOUT: Duration = Duration::from_secs(15);

static OWNER_EPOCH_COUNTER: AtomicU64 = AtomicU64::new(0);
static SCAN_REVISION_COUNTER: AtomicU64 = AtomicU64::new(0);

type OwnerStream = Pin<Box<dyn Stream<Item = Result<OwnerChange, String>> + Send>>;
type SignalStream = Pin<Box<dyn Stream<Item = Result<SignalEvent, String>> + Send>>;
type PlayerStream = Pin<Box<dyn Stream<Item = (String, Result<PlayerSnapshot, String>)> + Send>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OwnerChange {
    pub name: String,
    pub old_owner: String,
    pub new_owner: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingSeek {
    sender: String,
    position_us: i64,
    observed_at_us: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingPlaying {
    sender: String,
    observed_at_us: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SeekUpdate {
    pub key: String,
    pub position_us: i64,
    pub observed_at_us: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SignalEvent {
    Properties {
        sender: String,
        playing_observed_at_us: Option<u64>,
    },
    Seeked {
        sender: String,
        position_us: i64,
    },
}

#[derive(Debug, Clone, PartialEq)]
struct ScannedPlayers {
    players: BTreeMap<String, PlayerSnapshot>,
    owners: BTreeMap<String, String>,
}

struct PlayerScan {
    owners: BTreeMap<String, String>,
    players: PlayerStream,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TrackerUpdate {
    Generation(u64),
    ScanStarted {
        generation: u64,
        scan_revision: u64,
        cause: &'static str,
    },
    PartialSnapshot {
        generation: u64,
        scan_revision: u64,
        players: BTreeMap<String, PlayerSnapshot>,
        cause: &'static str,
    },
    Snapshot {
        generation: u64,
        scan_revision: u64,
        players: BTreeMap<String, PlayerSnapshot>,
        cause: &'static str,
        owner_changes: Vec<OwnerChange>,
        seeked: Vec<SeekUpdate>,
        playing: Vec<PlayingObservation>,
        adapter_loss: u64,
    },
}

#[derive(Debug)]
enum OwnerWake {
    Changed,
    Ended(String),
}

#[derive(Debug, Default)]
struct PendingOwnerChanges {
    changes: VecDeque<OwnerChange>,
    lost: u64,
}

struct AbortTask(tokio::task::JoinHandle<()>);

impl Drop for AbortTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Private D-Bus seam used by the production tracker and control worker.
trait MprisBus: Clone + Send + Sync + 'static {
    async fn owner_changes(&self) -> Result<OwnerStream>;
    async fn signals(&self) -> Result<SignalStream>;
    async fn scan_players(&self) -> Result<PlayerScan>;
    async fn owners(&self) -> Result<BTreeMap<String, String>>;
    async fn owner_for(&self, name: &str) -> Result<Option<String>>;
    async fn control(
        &self,
        target: &ControlTarget,
        action: &ControlAction,
    ) -> std::result::Result<ControlResult, ControlError>;

    fn report_scan_storm(&self) {
        eprintln!(
            "cosmix-mprisd: MPRIS changed during {MAX_SCAN_ATTEMPTS} consecutive scans; retaining the previous snapshot and rescanning"
        );
    }

    fn report_drained_signals(&self, _count: usize) {}

    fn report_ignored_signal(&self, sender: &str) {
        eprintln!("cosmix-mprisd: ignoring MPRIS-path signal from unknown sender {sender}");
    }
}

#[derive(Clone)]
struct ZbusMprisBus {
    connection: Connection,
}

impl ZbusMprisBus {
    async fn session() -> Result<Self> {
        Ok(Self {
            connection: Connection::session().await?,
        })
    }

    async fn proxy<'a>(&'a self, owner: &'a str, interface: &'a str) -> Result<Proxy<'a>> {
        Ok(ProxyBuilder::<Proxy<'a>>::new(&self.connection)
            .destination(owner)?
            .path(MPRIS_PATH)?
            .interface(interface)?
            .cache_properties(CacheProperties::No)
            .build()
            .await?)
    }
}

impl MprisBus for ZbusMprisBus {
    async fn owner_changes(&self) -> Result<OwnerStream> {
        let rule = MatchRule::builder()
            .msg_type(message::Type::Signal)
            .sender(DBUS_DEST)?
            .interface(DBUS_IFACE)?
            .member("NameOwnerChanged")?
            .arg0ns("org.mpris.MediaPlayer2")?
            .build();
        let stream =
            MessageStream::for_match_rule(rule, &self.connection, Some(SIGNAL_QUEUE_CAPACITY))
                .await?;
        Ok(Box::pin(stream.map(|message| {
            let message = message.map_err(|error| error.to_string())?;
            let (name, old_owner, new_owner) = message
                .body()
                .deserialize::<(String, String, String)>()
                .map_err(|error| error.to_string())?;
            Ok(OwnerChange {
                name,
                old_owner,
                new_owner,
            })
        })))
    }

    async fn signals(&self) -> Result<SignalStream> {
        let properties_rule = MatchRule::builder()
            .msg_type(message::Type::Signal)
            .path(MPRIS_PATH)?
            .interface(PROPERTIES_IFACE)?
            .member("PropertiesChanged")?
            .build();
        let seeked_rule = MatchRule::builder()
            .msg_type(message::Type::Signal)
            .path(MPRIS_PATH)?
            .interface(PLAYER_IFACE)?
            .member("Seeked")?
            .build();
        let (properties, seeked) = tokio::try_join!(
            MessageStream::for_match_rule(
                properties_rule,
                &self.connection,
                Some(SIGNAL_QUEUE_CAPACITY),
            ),
            MessageStream::for_match_rule(
                seeked_rule,
                &self.connection,
                Some(SIGNAL_QUEUE_CAPACITY),
            ),
        )?;
        let properties = properties.map(|message| {
            let message = message.map_err(|error| error.to_string())?;
            let sender = message
                .header()
                .sender()
                .ok_or_else(|| "PropertiesChanged signal has no sender".to_string())?
                .to_string();
            let (interface, changed, _invalidated) = message
                .body()
                .deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
                .map_err(|error| error.to_string())?;
            Ok(SignalEvent::Properties {
                sender,
                playing_observed_at_us: properties_started_playing(&interface, &changed)
                    .then(monotonic_us),
            })
        });
        let seeked = seeked.map(|message| {
            let message = message.map_err(|error| error.to_string())?;
            let sender = message
                .header()
                .sender()
                .ok_or_else(|| "Seeked signal has no sender".to_string())?
                .to_string();
            let position_us = message
                .body()
                .deserialize::<i64>()
                .map_err(|error| error.to_string())?;
            Ok(SignalEvent::Seeked {
                sender,
                position_us,
            })
        });
        Ok(Box::pin(stream::select(properties, seeked)))
    }

    async fn scan_players(&self) -> Result<PlayerScan> {
        let owners = self.owners().await?;
        let owner_pairs = owners
            .iter()
            .map(|(name, owner)| (name.clone(), owner.clone()))
            .collect::<Vec<_>>();
        let bus = self.clone();
        let players = stream::iter(owner_pairs.into_iter().map(move |(name, owner)| {
            let bus = bus.clone();
            async move {
                let result = read_player(&bus, &name, &owner)
                    .await
                    .map_err(|error| format!("{error:#}"));
                (name, result)
            }
        }))
        .buffer_unordered(MAX_CONCURRENT_PLAYER_READS);
        Ok(PlayerScan {
            owners,
            players: Box::pin(players),
        })
    }

    async fn owners(&self) -> Result<BTreeMap<String, String>> {
        let dbus = fdo::DBusProxy::new(&self.connection).await?;
        let mut owners = BTreeMap::new();
        for name in dbus.list_names().await? {
            let name = name.to_string();
            if !name.starts_with(MPRIS_PREFIX) {
                continue;
            }
            let bus_name = BusName::try_from(name.as_str())?;
            if let Ok(owner) = dbus.get_name_owner(bus_name).await {
                owners.insert(name, owner.to_string());
            }
        }
        Ok(owners)
    }

    async fn owner_for(&self, name: &str) -> Result<Option<String>> {
        let dbus = fdo::DBusProxy::new(&self.connection).await?;
        let name = BusName::try_from(name)?;
        if !dbus.name_has_owner(name.clone()).await? {
            return Ok(None);
        }
        Ok(Some(dbus.get_name_owner(name).await?.to_string()))
    }

    async fn control(
        &self,
        target: &ControlTarget,
        action: &ControlAction,
    ) -> std::result::Result<ControlResult, ControlError> {
        let proxy = self
            .proxy(&target.owner, PLAYER_IFACE)
            .await
            .map_err(ControlError::transport)?;
        let result = match action {
            ControlAction::Play => proxy
                .call::<_, _, ()>("Play", &())
                .await
                .map_err(ControlError::from_zbus),
            ControlAction::Pause => proxy
                .call::<_, _, ()>("Pause", &())
                .await
                .map_err(ControlError::from_zbus),
            ControlAction::PlayPause => proxy
                .call::<_, _, ()>("PlayPause", &())
                .await
                .map_err(ControlError::from_zbus),
            ControlAction::Next => proxy
                .call::<_, _, ()>("Next", &())
                .await
                .map_err(ControlError::from_zbus),
            ControlAction::Previous => proxy
                .call::<_, _, ()>("Previous", &())
                .await
                .map_err(ControlError::from_zbus),
            ControlAction::Stop => proxy
                .call::<_, _, ()>("Stop", &())
                .await
                .map_err(ControlError::from_zbus),
            ControlAction::Seek { offset_us } => proxy
                .call::<_, _, ()>("Seek", &DynamicTuple((*offset_us,)))
                .await
                .map_err(ControlError::from_zbus),
            ControlAction::SetVolume { volume } => proxy
                .set_property("Volume", *volume)
                .await
                .map_err(ControlError::from_fdo),
        };
        result
            .map(|()| ControlResult {
                result: None,
                executed_by: target.owner.clone(),
            })
            .map_err(|mut error| {
                error.executed_by = Some(target.owner.clone());
                error
            })
    }
}

fn properties_started_playing(interface: &str, changed: &HashMap<String, OwnedValue>) -> bool {
    interface == PLAYER_IFACE
        && owned_string(changed.get("PlaybackStatus")).as_deref() == Some("Playing")
}

async fn read_player(bus: &ZbusMprisBus, name: &str, owner: &str) -> Result<PlayerSnapshot> {
    let root = bus.proxy(owner, ROOT_IFACE).await?;
    let player = bus.proxy(owner, PLAYER_IFACE).await?;
    macro_rules! property {
        ($proxy:expr, $ty:ty, $name:literal) => {
            match player_request($proxy.get_property::<$ty>($name)).await {
                PlayerRequest::Value(value) => Some(value),
                PlayerRequest::Failed => None,
                PlayerRequest::TimedOut => return Ok(unresponsive_player(name, owner)),
            }
        };
    }
    let identity = property!(root, String, "Identity").unwrap_or_else(|| name.to_string());
    let desktop_entry = property!(root, String, "DesktopEntry");
    let status = property!(player, String, "PlaybackStatus")
        .map(|value| PlaybackStatus::from_mpris(&value))
        .unwrap_or_default();
    let metadata = property!(player, HashMap<String, OwnedValue>, "Metadata")
        .map(metadata_from_values)
        .unwrap_or_default();
    let position_us = property!(player, i64, "Position").unwrap_or(0);
    let position_observed_at_us = monotonic_us();
    let rate = finite_or(property!(player, f64, "Rate"), 1.0);
    let volume = finite_or(property!(player, f64, "Volume"), 1.0).max(0.0);
    Ok(PlayerSnapshot {
        key: player_key(name),
        name: name.to_string(),
        owner: owner.to_string(),
        owner_epoch: 0,
        unresponsive: false,
        stale: false,
        identity,
        desktop_entry,
        playback_status: status,
        metadata,
        position_us,
        position_observed_at_us,
        rate,
        volume,
        can_play: property!(player, bool, "CanPlay").unwrap_or(false),
        can_pause: property!(player, bool, "CanPause").unwrap_or(false),
        can_go_next: property!(player, bool, "CanGoNext").unwrap_or(false),
        can_go_previous: property!(player, bool, "CanGoPrevious").unwrap_or(false),
        can_seek: property!(player, bool, "CanSeek").unwrap_or(false),
        can_control: property!(player, bool, "CanControl").unwrap_or(false),
    })
}

enum PlayerRequest<T> {
    Value(T),
    Failed,
    TimedOut,
}

async fn player_request<F, T, E>(future: F) -> PlayerRequest<T>
where
    F: Future<Output = std::result::Result<T, E>>,
{
    player_request_with_timeout(PLAYER_REQUEST_TIMEOUT, future).await
}

async fn player_request_with_timeout<F, T, E>(timeout: Duration, future: F) -> PlayerRequest<T>
where
    F: Future<Output = std::result::Result<T, E>>,
{
    match tokio::time::timeout(timeout, future).await {
        Ok(Ok(value)) => PlayerRequest::Value(value),
        Ok(Err(_)) => PlayerRequest::Failed,
        Err(_) => PlayerRequest::TimedOut,
    }
}

fn unresponsive_player(name: &str, owner: &str) -> PlayerSnapshot {
    PlayerSnapshot {
        key: player_key(name),
        name: name.to_string(),
        owner: owner.to_string(),
        owner_epoch: 0,
        unresponsive: true,
        stale: false,
        identity: name.to_string(),
        desktop_entry: None,
        playback_status: PlaybackStatus::Unknown,
        metadata: PlayerMetadata::default(),
        position_us: 0,
        position_observed_at_us: monotonic_us(),
        rate: 1.0,
        volume: 1.0,
        can_play: false,
        can_pause: false,
        can_go_next: false,
        can_go_previous: false,
        can_seek: false,
        can_control: false,
    }
}

fn metadata_from_values(values: HashMap<String, OwnedValue>) -> PlayerMetadata {
    PlayerMetadata {
        title: owned_string(values.get("xesam:title")),
        artists: owned_strings(values.get("xesam:artist")).unwrap_or_default(),
        album: owned_string(values.get("xesam:album")),
        length_us: owned_i64(values.get("mpris:length")).filter(|value| *value >= 0),
        art_url: owned_string(values.get("mpris:artUrl")),
    }
}

fn owned_string(value: Option<&OwnedValue>) -> Option<String> {
    value
        .and_then(|value| value.try_clone().ok())
        .and_then(|value| String::try_from(value).ok())
}

fn owned_strings(value: Option<&OwnedValue>) -> Option<Vec<String>> {
    value
        .and_then(|value| value.try_clone().ok())
        .and_then(|value| Vec::<String>::try_from(value).ok())
}

fn owned_i64(value: Option<&OwnedValue>) -> Option<i64> {
    value
        .and_then(|value| value.try_clone().ok())
        .and_then(|value| i64::try_from(value).ok())
}

fn finite_or(value: Option<f64>, fallback: f64) -> f64 {
    value.filter(|value| value.is_finite()).unwrap_or(fallback)
}

pub(crate) fn monotonic_us() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    let elapsed = START.get_or_init(Instant::now).elapsed().as_micros();
    u64::try_from(elapsed).unwrap_or(u64::MAX)
}

#[derive(Default)]
struct SignalEpisodeLimiter {
    active: bool,
    last_log_at: Option<Instant>,
}

impl SignalEpisodeLimiter {
    fn rejected(&mut self, now: Instant) -> bool {
        let should_log = !self.active
            || self
                .last_log_at
                .is_none_or(|last| now.saturating_duration_since(last) >= Duration::from_secs(60));
        self.active = true;
        if should_log {
            self.last_log_at = Some(now);
        }
        should_log
    }

    fn accepted(&mut self) {
        self.active = false;
        self.last_log_at = None;
    }
}

struct SignalState {
    epochs: HashMap<String, u64>,
    known_owners: BTreeSet<String>,
    classification_ready: bool,
    unclassified: VecDeque<SignalEvent>,
    pending_seeked: VecDeque<PendingSeek>,
    pending_playing: VecDeque<PendingPlaying>,
    adapter_loss: u64,
    pending_ledger: Arc<AtomicU64>,
    ignored_limiter: SignalEpisodeLimiter,
}

impl SignalState {
    fn new(adapter_loss: u64, pending_ledger: Arc<AtomicU64>) -> Self {
        Self {
            epochs: HashMap::new(),
            known_owners: BTreeSet::new(),
            classification_ready: false,
            unclassified: VecDeque::new(),
            pending_seeked: VecDeque::new(),
            pending_playing: VecDeque::new(),
            adapter_loss,
            pending_ledger,
            ignored_limiter: SignalEpisodeLimiter::default(),
        }
    }

    fn observe<B: MprisBus>(&mut self, bus: &B, event: SignalEvent) {
        if !self.classification_ready {
            let _ =
                self.pending_ledger
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                        Some(value.saturating_add(1))
                    });
            if self.unclassified.len() == SIGNAL_QUEUE_CAPACITY {
                self.unclassified.pop_front();
                self.adapter_loss = self.adapter_loss.saturating_add(1);
            }
            self.unclassified.push_back(event);
            return;
        }
        self.classify(bus, event);
    }

    fn learn_owners<B: MprisBus>(&mut self, bus: &B, owners: &BTreeMap<String, String>) {
        self.known_owners.extend(owners.values().cloned());
        self.classification_ready = true;
        for event in std::mem::take(&mut self.unclassified) {
            let _ =
                self.pending_ledger
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                        Some(value.saturating_sub(1))
                    });
            self.classify(bus, event);
        }
    }

    fn finish_owners(&mut self, owners: &BTreeMap<String, String>) {
        self.known_owners = owners.values().cloned().collect();
        self.epochs
            .retain(|owner, _| self.known_owners.contains(owner));
    }

    fn classify<B: MprisBus>(&mut self, bus: &B, event: SignalEvent) {
        let sender = match &event {
            SignalEvent::Properties { sender, .. } | SignalEvent::Seeked { sender, .. } => sender,
        };
        if !self.known_owners.contains(sender) {
            if self.ignored_limiter.rejected(Instant::now()) {
                bus.report_ignored_signal(sender);
            }
            return;
        }
        self.ignored_limiter.accepted();
        let epoch = self.epochs.entry(sender.clone()).or_default();
        *epoch = epoch.saturating_add(1);
        match event {
            SignalEvent::Properties {
                sender,
                playing_observed_at_us: Some(observed_at_us),
            } => {
                let _ = self.pending_ledger.fetch_update(
                    Ordering::AcqRel,
                    Ordering::Acquire,
                    |value| Some(value.saturating_add(1)),
                );
                if self.pending_playing.len() == SIGNAL_QUEUE_CAPACITY {
                    self.pending_playing.pop_front();
                    self.adapter_loss = self.adapter_loss.saturating_add(1);
                }
                self.pending_playing.push_back(PendingPlaying {
                    sender,
                    observed_at_us,
                });
            }
            SignalEvent::Seeked {
                sender,
                position_us,
            } => {
                let _ = self.pending_ledger.fetch_update(
                    Ordering::AcqRel,
                    Ordering::Acquire,
                    |value| Some(value.saturating_add(1)),
                );
                if self.pending_seeked.len() == SIGNAL_QUEUE_CAPACITY {
                    self.pending_seeked.pop_front();
                    self.adapter_loss = self.adapter_loss.saturating_add(1);
                }
                self.pending_seeked.push_back(PendingSeek {
                    sender,
                    position_us,
                    observed_at_us: monotonic_us(),
                });
            }
            SignalEvent::Properties { .. } => {}
        }
    }
}

async fn await_bus_reply<B, F, T>(
    bus: &B,
    signals: &mut SignalStream,
    signal_state: &mut SignalState,
    future: F,
) -> Result<(Result<T>, usize)>
where
    B: MprisBus,
    F: Future<Output = Result<T>>,
{
    tokio::pin!(future);
    let mut drained = 0usize;
    loop {
        if drained < 64 {
            tokio::select! {
                biased;
                signal = signals.next() => match signal {
                    Some(Ok(event)) => {
                        drained = drained.saturating_add(1);
                        signal_state.observe(bus, event);
                    }
                    Some(Err(error)) => {
                        bus.report_drained_signals(drained);
                        return Err(anyhow!(error));
                    }
                    None => {
                        bus.report_drained_signals(drained);
                        return Err(anyhow!("MPRIS signal stream ended"));
                    }
                },
                result = &mut future => {
                    bus.report_drained_signals(drained);
                    return Ok((result, drained));
                },
            }
        } else {
            tokio::select! {
                biased;
                result = &mut future => {
                    bus.report_drained_signals(drained);
                    return Ok((result, drained));
                },
                signal = signals.next() => match signal {
                    Some(Ok(event)) => {
                        drained = drained.saturating_add(1);
                        signal_state.observe(bus, event);
                    }
                    Some(Err(error)) => {
                        bus.report_drained_signals(drained);
                        return Err(anyhow!(error));
                    }
                    None => {
                        bus.report_drained_signals(drained);
                        return Err(anyhow!("MPRIS signal stream ended"));
                    }
                },
            }
        }
    }
}

async fn reserve_update<'a, B: MprisBus>(
    bus: &B,
    update_tx: &'a mpsc::Sender<TrackerUpdate>,
    signals: &mut SignalStream,
    signal_state: &mut SignalState,
    owner_wakes: &mut mpsc::Receiver<OwnerWake>,
    signal_caused: &mut bool,
) -> Result<mpsc::Permit<'a, TrackerUpdate>> {
    let reserve = update_tx.reserve();
    tokio::pin!(reserve);
    loop {
        tokio::select! {
            permit = &mut reserve => {
                return permit.map_err(|_| anyhow!("tracker update receiver closed"));
            }
            signal = signals.next() => match signal {
                Some(Ok(event)) => {
                    signal_state.observe(bus, event);
                    *signal_caused = true;
                }
                Some(Err(error)) => return Err(anyhow!(error)),
                None => return Err(anyhow!("MPRIS signal stream ended")),
            },
            wake = owner_wakes.recv() => match wake {
                Some(OwnerWake::Changed) => *signal_caused = true,
                Some(OwnerWake::Ended(error)) => return Err(anyhow!(error)),
                None => return Err(anyhow!("MPRIS owner watcher ended")),
            },
        }
    }
}

fn spawn_owner_watcher(
    mut changes: OwnerStream,
    generation: Arc<AtomicU64>,
    pending: Arc<Mutex<PendingOwnerChanges>>,
    pending_ledger: Arc<AtomicU64>,
) -> (AbortTask, mpsc::Receiver<OwnerWake>) {
    let (wake_tx, wake_rx) = mpsc::channel(1);
    let task = tokio::spawn(async move {
        while let Some(change) = changes.next().await {
            match change {
                Ok(change) => {
                    {
                        let mut pending = pending.lock().expect("owner-change queue poisoned");
                        let _ =
                            generation.fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                                Some(value.saturating_add(1))
                            });
                        if pending.changes.len() == SIGNAL_QUEUE_CAPACITY {
                            pending.changes.pop_front();
                            pending.lost = pending.lost.saturating_add(1);
                        }
                        pending.changes.push_back(change);
                        let _ = pending_ledger.fetch_update(
                            Ordering::AcqRel,
                            Ordering::Acquire,
                            |value| Some(value.saturating_add(1)),
                        );
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
            .send(OwnerWake::Ended(
                "MPRIS NameOwnerChanged stream ended".into(),
            ))
            .await;
    });
    (AbortTask(task), wake_rx)
}

pub(crate) async fn run(
    update_tx: mpsc::Sender<TrackerUpdate>,
    generation: Arc<AtomicU64>,
) -> Result<()> {
    let pending_ledger = Arc::new(AtomicU64::new(0));
    let mut restart_loss = 0_u64;
    loop {
        match ZbusMprisBus::session().await {
            Ok(bus) => {
                if let Err(error) = run_tracker_with_state(
                    bus,
                    update_tx.clone(),
                    Arc::clone(&generation),
                    Arc::clone(&pending_ledger),
                    std::mem::take(&mut restart_loss),
                )
                .await
                {
                    restart_loss =
                        restart_loss.saturating_add(pending_ledger.swap(0, Ordering::AcqRel));
                    if update_tx.is_closed() {
                        return Err(error);
                    }
                    eprintln!("cosmix-mprisd: session D-Bus monitor ended: {error:#}");
                }
            }
            Err(error) => {
                let next = generation.fetch_add(1, Ordering::AcqRel).saturating_add(1);
                update_tx
                    .send(TrackerUpdate::Snapshot {
                        generation: next,
                        scan_revision: mint_scan_revision(),
                        players: BTreeMap::new(),
                        cause: "dbus.unavailable",
                        owner_changes: Vec::new(),
                        seeked: Vec::new(),
                        playing: Vec::new(),
                        adapter_loss: std::mem::take(&mut restart_loss),
                    })
                    .await
                    .map_err(|_| anyhow!("tracker update receiver closed"))?;
                eprintln!("cosmix-mprisd: session D-Bus unavailable; retrying in 60s: {error}");
            }
        }
        tokio::time::sleep(DBUS_RECONNECT_DELAY).await;
    }
}

async fn run_tracker_with_state<B: MprisBus>(
    bus: B,
    update_tx: mpsc::Sender<TrackerUpdate>,
    generation: Arc<AtomicU64>,
    pending_ledger: Arc<AtomicU64>,
    initial_adapter_loss: u64,
) -> Result<()> {
    if initial_adapter_loss != 0 {
        let _ = pending_ledger.fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
            Some(value.saturating_add(initial_adapter_loss))
        });
    }
    let owner_changes = bus.owner_changes().await?;
    let initial = generation.fetch_add(1, Ordering::AcqRel).saturating_add(1);
    update_tx.send(TrackerUpdate::Generation(initial)).await?;
    let pending_owner_changes = Arc::new(Mutex::new(PendingOwnerChanges::default()));
    let (_owner_watcher, mut owner_wakes) = spawn_owner_watcher(
        owner_changes,
        Arc::clone(&generation),
        Arc::clone(&pending_owner_changes),
        Arc::clone(&pending_ledger),
    );
    let signal_setup = bus.signals();
    tokio::pin!(signal_setup);
    let mut signals = loop {
        tokio::select! {
            result = &mut signal_setup => break result?,
            wake = owner_wakes.recv() => match wake {
                Some(OwnerWake::Changed) => {},
                Some(OwnerWake::Ended(error)) => return Err(anyhow!(error)),
                None => return Err(anyhow!("MPRIS owner watcher ended")),
            },
        }
    };

    let mut owner_epochs = BTreeMap::<String, (String, u64)>::new();
    let mut signal_state = SignalState::new(initial_adapter_loss, Arc::clone(&pending_ledger));
    let mut previous_players = BTreeMap::<String, PlayerSnapshot>::new();
    let mut refresh_pending = true;
    let mut signal_caused = false;
    let mut storm_logged = false;
    let resync = tokio::time::sleep(RESYNC_INTERVAL);
    tokio::pin!(resync);

    loop {
        if !refresh_pending {
            tokio::select! {
                signal = signals.next() => match signal {
                    Some(Ok(event)) => {
                        signal_state.observe(&bus, event);
                        refresh_pending = true;
                        signal_caused = true;
                    }
                    Some(Err(error)) => return Err(anyhow!(error)),
                    None => return Err(anyhow!("MPRIS signal stream ended")),
                },
                wake = owner_wakes.recv() => match wake {
                    Some(OwnerWake::Changed) => {
                        refresh_pending = true;
                        signal_caused = true;
                    }
                    Some(OwnerWake::Ended(error)) => return Err(anyhow!(error)),
                    None => return Err(anyhow!("MPRIS owner watcher ended")),
                },
                _ = &mut resync => {
                    resync.as_mut().reset(tokio::time::Instant::now() + RESYNC_INTERVAL);
                    refresh_pending = true;
                },
            }
            continue;
        }

        refresh_pending = false;
        let scan_revision = mint_scan_revision();
        let mut permit = Some(
            reserve_update(
                &bus,
                &update_tx,
                &mut signals,
                &mut signal_state,
                &mut owner_wakes,
                &mut signal_caused,
            )
            .await?,
        );
        let started_generation = generation.load(Ordering::Acquire);
        permit
            .take()
            .expect("tracker update permit available")
            .send(TrackerUpdate::ScanStarted {
                generation: started_generation,
                scan_revision,
                cause: if signal_caused {
                    "mpris.signal"
                } else {
                    "mpris.resync"
                },
            });
        permit = Some(
            reserve_update(
                &bus,
                &update_tx,
                &mut signals,
                &mut signal_state,
                &mut owner_wakes,
                &mut signal_caused,
            )
            .await?,
        );
        let mut accepted = BTreeMap::new();
        let mut accepted_epochs = BTreeMap::<String, (String, u64)>::new();
        let mut unresolved = BTreeSet::<String>::new();
        let mut final_owners = BTreeMap::new();
        let mut captured_generation = generation.load(Ordering::Acquire);
        let mut had_consistent_scan = false;
        for attempt in 0..MAX_SCAN_ATTEMPTS {
            captured_generation = generation.load(Ordering::Acquire);
            let (player_scan, _drained) =
                await_bus_reply(&bus, &mut signals, &mut signal_state, bus.scan_players()).await?;
            let mut player_scan = player_scan?;
            signal_state.learn_owners(&bus, &player_scan.owners);
            if generation.load(Ordering::Acquire) != captured_generation {
                signal_caused = true;
                accepted.clear();
                accepted_epochs.clear();
                unresolved.clear();
                final_owners.clear();
                continue;
            }
            if attempt == 0 || player_scan.owners != final_owners {
                accepted.clear();
                accepted_epochs.clear();
                unresolved = player_scan.owners.keys().cloned().collect();
                final_owners = player_scan.owners.clone();
            }

            let raced_accepted = accepted_epochs
                .iter()
                .filter(|(_, (owner, epoch))| {
                    signal_state.epochs.get(owner).copied().unwrap_or(0) != *epoch
                })
                .map(|(name, _)| name.clone())
                .collect::<Vec<_>>();
            for name in raced_accepted {
                accepted.remove(&player_key(&name));
                accepted_epochs.remove(&name);
                unresolved.insert(name);
                signal_caused = true;
            }

            let captured_signal_epochs = signal_state.epochs.clone();
            while let Some((name, result)) =
                await_bus_reply(&bus, &mut signals, &mut signal_state, async {
                    Ok(player_scan.players.next().await)
                })
                .await?
                .0?
            {
                if generation.load(Ordering::Acquire) != captured_generation {
                    signal_caused = true;
                    accepted.clear();
                    accepted_epochs.clear();
                    unresolved.clear();
                    final_owners.clear();
                    break;
                }
                if !unresolved.contains(&name) {
                    continue;
                }
                let Some(owner) = final_owners.get(&name) else {
                    continue;
                };
                if signal_state.epochs.get(owner).copied().unwrap_or(0)
                    != captured_signal_epochs.get(owner).copied().unwrap_or(0)
                {
                    signal_caused = true;
                    continue;
                }
                match result {
                    Ok(player) => {
                        accepted.insert(player.key.clone(), player);
                        accepted_epochs.insert(
                            name.clone(),
                            (
                                owner.clone(),
                                signal_state.epochs.get(owner).copied().unwrap_or(0),
                            ),
                        );
                    }
                    Err(error) => {
                        eprintln!(
                            "cosmix-mprisd: marking unreadable MPRIS player {name} unresponsive: {error}"
                        );
                        accepted.insert(player_key(&name), unresponsive_player(&name, owner));
                        accepted_epochs.insert(
                            name.clone(),
                            (
                                owner.clone(),
                                signal_state.epochs.get(owner).copied().unwrap_or(0),
                            ),
                        );
                    }
                }
                unresolved.remove(&name);

                if !unresolved.is_empty() {
                    let mut partial_players = previous_players
                        .values()
                        .filter(|player| final_owners.get(&player.name) == Some(&player.owner))
                        .map(|player| (player.key.clone(), player.clone()))
                        .collect::<BTreeMap<_, _>>();
                    partial_players.extend(accepted.clone());
                    apply_owner_epochs(&mut partial_players, &final_owners, &mut owner_epochs);
                    permit
                        .take()
                        .expect("tracker update permit available")
                        .send(TrackerUpdate::PartialSnapshot {
                            generation: captured_generation,
                            scan_revision,
                            players: partial_players,
                            cause: if signal_caused {
                                "mpris.signal"
                            } else {
                                "mpris.resync"
                            },
                        });
                    permit = Some(
                        reserve_update(
                            &bus,
                            &update_tx,
                            &mut signals,
                            &mut signal_state,
                            &mut owner_wakes,
                            &mut signal_caused,
                        )
                        .await?,
                    );
                }
            }

            if generation.load(Ordering::Acquire) != captured_generation {
                continue;
            }
            let (owners, _drained) =
                await_bus_reply(&bus, &mut signals, &mut signal_state, bus.owners()).await?;
            let owners = owners?;
            signal_state.learn_owners(&bus, &owners);
            if owners != final_owners || generation.load(Ordering::Acquire) != captured_generation {
                signal_caused = true;
                accepted.clear();
                accepted_epochs.clear();
                unresolved.clear();
                final_owners.clear();
                continue;
            }
            had_consistent_scan = true;

            let raced_accepted = accepted_epochs
                .iter()
                .filter(|(_, (owner, epoch))| {
                    signal_state.epochs.get(owner).copied().unwrap_or(0) != *epoch
                })
                .map(|(name, _)| name.clone())
                .collect::<Vec<_>>();
            for name in raced_accepted {
                accepted.remove(&player_key(&name));
                accepted_epochs.remove(&name);
                unresolved.insert(name);
                signal_caused = true;
            }
            if unresolved.is_empty() {
                break;
            }
        }

        if !had_consistent_scan {
            refresh_pending = true;
            signal_caused = true;
            continue;
        }

        if !unresolved.is_empty() {
            if !storm_logged {
                bus.report_scan_storm();
                storm_logged = true;
            }
            for name in &unresolved {
                let key = player_key(name);
                if let Some(mut player) = previous_players.get(&key).cloned()
                    && final_owners.get(name) == Some(&player.owner)
                {
                    player.stale = true;
                    accepted.insert(key, player);
                } else if let Some(owner) = final_owners.get(name) {
                    accepted.insert(key, unresponsive_player(name, owner));
                }
            }
        } else {
            storm_logged = false;
        }
        let mut scan = ScannedPlayers {
            players: accepted,
            owners: final_owners,
        };

        apply_owner_epochs(&mut scan.players, &scan.owners, &mut owner_epochs);
        let mut pending_owners = pending_owner_changes
            .lock()
            .expect("owner-change queue poisoned");
        if generation.load(Ordering::Acquire) != captured_generation {
            refresh_pending = true;
            signal_caused = true;
            continue;
        }
        let owner_changes = pending_owners.changes.drain(..).collect::<Vec<_>>();
        signal_state.adapter_loss = signal_state
            .adapter_loss
            .saturating_add(std::mem::take(&mut pending_owners.lost));
        let mut seeked = Vec::new();
        for observation in std::mem::take(&mut signal_state.pending_seeked) {
            let mut matched = false;
            for player in scan.players.values_mut() {
                if player.owner == observation.sender {
                    matched = true;
                    player.position_us = observation.position_us;
                    player.position_observed_at_us = observation.observed_at_us;
                    seeked.push(SeekUpdate {
                        key: player.key.clone(),
                        position_us: observation.position_us,
                        observed_at_us: observation.observed_at_us,
                    });
                }
            }
            if !matched {
                signal_state.adapter_loss = signal_state.adapter_loss.saturating_add(1);
            }
        }
        let mut playing = Vec::new();
        for observation in std::mem::take(&mut signal_state.pending_playing) {
            let Some(player) = scan
                .players
                .values()
                .find(|player| player.owner == observation.sender)
            else {
                signal_state.adapter_loss = signal_state.adapter_loss.saturating_add(1);
                continue;
            };
            playing.push(PlayingObservation {
                key: player.key.clone(),
                observed_at_us: observation.observed_at_us,
            });
        }
        let cause = if !seeked.is_empty() {
            "mpris.seeked"
        } else if signal_caused {
            "mpris.signal"
        } else {
            "mpris.resync"
        };
        previous_players = scan.players.clone();
        pending_ledger.store(0, Ordering::Release);
        let adapter_loss = std::mem::take(&mut signal_state.adapter_loss);
        permit
            .take()
            .expect("tracker update permit available")
            .send(TrackerUpdate::Snapshot {
                generation: captured_generation,
                scan_revision,
                players: scan.players,
                cause,
                owner_changes,
                seeked,
                playing,
                adapter_loss,
            });
        signal_state.finish_owners(&scan.owners);
        owner_epochs.retain(|name, _| scan.owners.contains_key(name));
        drop(pending_owners);
        signal_caused = false;
    }
}

#[cfg(test)]
async fn run_tracker<B: MprisBus>(
    bus: B,
    update_tx: mpsc::Sender<TrackerUpdate>,
    generation: Arc<AtomicU64>,
) -> Result<()> {
    run_tracker_with_state(bus, update_tx, generation, Arc::new(AtomicU64::new(0)), 0).await
}

fn mint_owner_epoch() -> u64 {
    OWNER_EPOCH_COUNTER
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
            Some(value.saturating_add(1))
        })
        .unwrap_or(u64::MAX)
        .saturating_add(1)
}

fn mint_scan_revision() -> u64 {
    SCAN_REVISION_COUNTER
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
            Some(value.saturating_add(1))
        })
        .unwrap_or(u64::MAX)
        .saturating_add(1)
}

fn apply_owner_epochs(
    players: &mut BTreeMap<String, PlayerSnapshot>,
    owners: &BTreeMap<String, String>,
    owner_epochs: &mut BTreeMap<String, (String, u64)>,
) {
    for (name, owner) in owners {
        let epoch = match owner_epochs.get(name) {
            Some((old_owner, epoch)) if old_owner == owner => *epoch,
            _ => {
                let epoch = mint_owner_epoch();
                owner_epochs.insert(name.clone(), (owner.clone(), epoch));
                epoch
            }
        };
        if let Some(player) = players.get_mut(&player_key(name)) {
            player.owner_epoch = epoch;
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ControlAction {
    Play,
    Pause,
    PlayPause,
    Next,
    Previous,
    Stop,
    Seek { offset_us: i64 },
    SetVolume { volume: f64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ControlTarget {
    pub name: String,
    pub owner: String,
    pub owner_epoch: u64,
    pub generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ControlResult {
    pub result: Option<String>,
    pub executed_by: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ControlError {
    pub name: String,
    pub message: String,
    pub executed_by: Option<String>,
}

impl ControlError {
    fn transport(error: impl std::fmt::Display) -> Self {
        Self {
            name: "org.cosmix.Mpris.Transport".into(),
            message: error.to_string(),
            executed_by: None,
        }
    }

    fn from_zbus(error: zbus::Error) -> Self {
        match error {
            zbus::Error::MethodError(name, detail, _) => Self {
                name: name.to_string(),
                message: detail.unwrap_or_else(|| "no details".into()),
                executed_by: None,
            },
            other => Self::transport(other),
        }
    }

    fn from_fdo(error: fdo::Error) -> Self {
        match error {
            fdo::Error::ZBus(error) => Self::from_zbus(error),
            other => Self {
                name: other.name().to_string(),
                message: other.description().unwrap_or("no details").to_string(),
                executed_by: None,
            },
        }
    }
}

pub(crate) type ControlOwners = Arc<Mutex<BTreeMap<String, ControlTarget>>>;

pub(crate) struct ControlJob {
    pub target: ControlTarget,
    pub action: ControlAction,
    pub deadline: tokio::time::Instant,
    pub reply: oneshot::Sender<std::result::Result<ControlResult, ControlError>>,
}

fn expired_control() -> ControlError {
    ControlError {
        name: "org.cosmix.Mpris.Expired".into(),
        message: "MPRIS control deadline passed before dispatch".into(),
        executed_by: None,
    }
}

fn bounded_call_deadline(deadline: tokio::time::Instant) -> Option<tokio::time::Instant> {
    let now = tokio::time::Instant::now();
    (now < deadline).then(|| deadline.min(now + CONTROL_CALL_TIMEOUT))
}

async fn run_control_worker<B: MprisBus>(
    bus: B,
    mut jobs: mpsc::Receiver<ControlJob>,
    owners: ControlOwners,
    generation: Arc<AtomicU64>,
) -> Result<()> {
    while let Some(job) = jobs.recv().await {
        if tokio::time::Instant::now() >= job.deadline {
            let _ = job.reply.send(Err(expired_control()));
            continue;
        }
        let current = owners
            .lock()
            .expect("control owner map poisoned")
            .get(&job.target.name)
            .cloned();
        let result = if current.as_ref() != Some(&job.target)
            || generation.load(Ordering::Acquire) != job.target.generation
        {
            Err(ControlError {
                name: "org.cosmix.Mpris.StalePlayer".into(),
                message: "player owner changed before control dispatch".into(),
                executed_by: None,
            })
        } else {
            let Some(owner_deadline) = bounded_call_deadline(job.deadline) else {
                let _ = job.reply.send(Err(expired_control()));
                continue;
            };
            match tokio::time::timeout_at(owner_deadline, bus.owner_for(&job.target.name)).await {
                Ok(Ok(Some(owner)))
                    if owner == job.target.owner
                        && generation.load(Ordering::Acquire) == job.target.generation =>
                {
                    let Some(control_deadline) = bounded_call_deadline(job.deadline) else {
                        let _ = job.reply.send(Err(expired_control()));
                        continue;
                    };
                    match tokio::time::timeout_at(
                        control_deadline,
                        bus.control(&job.target, &job.action),
                    )
                    .await
                    {
                        Ok(result) => result,
                        Err(_) => Err(ControlError {
                            name: "org.cosmix.Mpris.Timeout".into(),
                            message: "MPRIS control timed out".into(),
                            executed_by: Some(job.target.owner.clone()),
                        }),
                    }
                }
                Ok(Ok(_)) => Err(ControlError {
                    name: "org.cosmix.Mpris.StalePlayer".into(),
                    message: "player owner changed before control dispatch".into(),
                    executed_by: None,
                }),
                Ok(Err(error)) => Err(ControlError::transport(error)),
                Err(_) => Err(ControlError {
                    name: "org.cosmix.Mpris.Timeout".into(),
                    message: "MPRIS control timed out".into(),
                    executed_by: None,
                }),
            }
        };
        let _ = job.reply.send(result);
    }
    Ok(())
}

pub(crate) async fn run_controls(
    mut jobs: mpsc::Receiver<ControlJob>,
    owners: ControlOwners,
    generation: Arc<AtomicU64>,
) -> Result<()> {
    while let Some(first) = jobs.recv().await {
        if tokio::time::Instant::now() >= first.deadline {
            let _ = first.reply.send(Err(expired_control()));
            continue;
        }
        let bus = match tokio::time::timeout_at(first.deadline, ZbusMprisBus::session()).await {
            Ok(Ok(bus)) => bus,
            Ok(Err(error)) => {
                let _ = first.reply.send(Err(ControlError::transport(error)));
                continue;
            }
            Err(error) => {
                let _ = first.reply.send(Err(ControlError {
                    name: "org.cosmix.Mpris.Timeout".into(),
                    message: format!("session D-Bus connection timed out: {error}"),
                    executed_by: None,
                }));
                continue;
            }
        };
        // The outer queue can hold CONTROL_QUEUE_CAPACITY jobs in addition to
        // the one already received, so the transfer queue needs one extra slot.
        let (tx, rx) = mpsc::channel(crate::citizen::CONTROL_QUEUE_CAPACITY + 1);
        tx.send(first)
            .await
            .map_err(|_| anyhow!("control worker receiver closed"))?;
        while let Ok(job) = jobs.try_recv() {
            tx.send(job)
                .await
                .map_err(|_| anyhow!("control worker receiver closed"))?;
        }
        drop(tx);
        run_control_worker(bus, rx, Arc::clone(&owners), Arc::clone(&generation)).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicUsize};

    type ScriptSignalReceiver = mpsc::Receiver<Result<SignalEvent, String>>;
    type ScriptOwnerReceiver = mpsc::Receiver<Result<OwnerChange, String>>;

    fn signal_stream(rx: ScriptSignalReceiver) -> SignalStream {
        Box::pin(stream::unfold(rx, |mut rx| async {
            rx.recv().await.map(|item| (item, rx))
        }))
    }

    fn owner_stream(rx: ScriptOwnerReceiver) -> OwnerStream {
        Box::pin(stream::unfold(rx, |mut rx| async {
            rx.recv().await.map(|item| (item, rx))
        }))
    }

    #[derive(Clone)]
    struct ScriptedBus {
        owner_changes: Arc<Mutex<Option<ScriptOwnerReceiver>>>,
        signals: Arc<Mutex<Option<ScriptSignalReceiver>>>,
        scans: Arc<Mutex<VecDeque<ScannedPlayers>>>,
        owners: Arc<Mutex<BTreeMap<String, String>>>,
        scan_started: mpsc::Sender<usize>,
        scan_permits: Arc<tokio::sync::Mutex<mpsc::Receiver<()>>>,
        scan_calls: Arc<AtomicUsize>,
        storm_reports: Arc<AtomicUsize>,
        drained: Arc<AtomicUsize>,
        ignored_signals: Arc<AtomicUsize>,
        controls: Arc<Mutex<Vec<(ControlTarget, ControlAction)>>>,
        fail_control: Arc<AtomicBool>,
        block_next_control: Arc<AtomicBool>,
        control_gate: Arc<tokio::sync::Notify>,
        block_after_first_player: Arc<AtomicBool>,
        player_gate: Arc<tokio::sync::Notify>,
    }

    impl MprisBus for ScriptedBus {
        async fn owner_changes(&self) -> Result<OwnerStream> {
            let rx = self
                .owner_changes
                .lock()
                .unwrap()
                .take()
                .ok_or_else(|| anyhow!("owner stream already taken"))?;
            Ok(owner_stream(rx))
        }

        async fn signals(&self) -> Result<SignalStream> {
            let rx = self
                .signals
                .lock()
                .unwrap()
                .take()
                .ok_or_else(|| anyhow!("signal stream already taken"))?;
            Ok(signal_stream(rx))
        }

        async fn scan_players(&self) -> Result<PlayerScan> {
            let call = self.scan_calls.fetch_add(1, Ordering::Relaxed);
            self.scan_started.send(call).await.unwrap();
            let scan = self
                .scans
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| anyhow!("no scripted scan"))?;
            let owners = scan.owners;
            let players = scan
                .players
                .into_values()
                .map(|player| (player.name.clone(), Ok(player)))
                .collect::<VecDeque<_>>();
            let permits = Arc::clone(&self.scan_permits);
            let block_after_first = self.block_after_first_player.swap(false, Ordering::AcqRel);
            let player_gate = Arc::clone(&self.player_gate);
            let players = stream::once(async move {
                permits
                    .lock()
                    .await
                    .recv()
                    .await
                    .ok_or_else(|| anyhow!("scan permits closed"))?;
                Ok::<_, anyhow::Error>((players, 0usize))
            })
            .map(|result| match result {
                Ok(players) => players,
                Err(error) => (
                    VecDeque::from([(String::new(), Err(format!("{error:#}")))]),
                    0,
                ),
            })
            .flat_map(move |state| {
                let player_gate = Arc::clone(&player_gate);
                stream::unfold(state, move |(mut players, index)| {
                    let player_gate = Arc::clone(&player_gate);
                    async move {
                        if block_after_first && index == 1 {
                            player_gate.notified().await;
                        }
                        players
                            .pop_front()
                            .map(|player| (player, (players, index.saturating_add(1))))
                    }
                })
            });
            Ok(PlayerScan {
                owners,
                players: Box::pin(players),
            })
        }

        async fn owners(&self) -> Result<BTreeMap<String, String>> {
            Ok(self.owners.lock().unwrap().clone())
        }

        async fn owner_for(&self, name: &str) -> Result<Option<String>> {
            Ok(self.owners.lock().unwrap().get(name).cloned())
        }

        async fn control(
            &self,
            target: &ControlTarget,
            action: &ControlAction,
        ) -> std::result::Result<ControlResult, ControlError> {
            self.controls
                .lock()
                .unwrap()
                .push((target.clone(), action.clone()));
            if self.block_next_control.swap(false, Ordering::AcqRel) {
                self.control_gate.notified().await;
            }
            if self.fail_control.load(Ordering::Relaxed) {
                Err(ControlError {
                    name: "org.mpris.MediaPlayer2.Error.Failed".into(),
                    message: "scripted failure".into(),
                    executed_by: Some(target.owner.clone()),
                })
            } else {
                Ok(ControlResult {
                    result: None,
                    executed_by: target.owner.clone(),
                })
            }
        }

        fn report_scan_storm(&self) {
            self.storm_reports.fetch_add(1, Ordering::Relaxed);
        }

        fn report_drained_signals(&self, count: usize) {
            self.drained.fetch_add(count, Ordering::Relaxed);
        }

        fn report_ignored_signal(&self, _sender: &str) {
            self.ignored_signals.fetch_add(1, Ordering::Relaxed);
        }
    }

    struct Harness {
        bus: ScriptedBus,
        owner_tx: mpsc::Sender<Result<OwnerChange, String>>,
        signal_tx: mpsc::Sender<Result<SignalEvent, String>>,
        scan_started: mpsc::Receiver<usize>,
        scan_permit_tx: mpsc::Sender<()>,
    }

    fn player(name: &str, owner: &str, title: &str) -> PlayerSnapshot {
        PlayerSnapshot {
            key: player_key(name),
            name: name.into(),
            owner: owner.into(),
            owner_epoch: 0,
            unresponsive: false,
            stale: false,
            identity: title.into(),
            desktop_entry: None,
            playback_status: PlaybackStatus::Paused,
            metadata: PlayerMetadata {
                title: Some(title.into()),
                ..PlayerMetadata::default()
            },
            position_us: 0,
            position_observed_at_us: 0,
            rate: 1.0,
            volume: 1.0,
            can_play: true,
            can_pause: true,
            can_go_next: false,
            can_go_previous: false,
            can_seek: true,
            can_control: true,
        }
    }

    fn scan(players: Vec<PlayerSnapshot>) -> ScannedPlayers {
        let owners = players
            .iter()
            .map(|player| (player.name.clone(), player.owner.clone()))
            .collect();
        let players = players
            .into_iter()
            .map(|player| (player.key.clone(), player))
            .collect();
        ScannedPlayers { players, owners }
    }

    fn owner_change(name: &str, old_owner: &str, new_owner: &str) -> OwnerChange {
        OwnerChange {
            name: name.into(),
            old_owner: old_owner.into(),
            new_owner: new_owner.into(),
        }
    }

    fn harness_with_signal_capacity(scans: Vec<ScannedPlayers>, signal_capacity: usize) -> Harness {
        let owners = scans
            .last()
            .map(|scan| scan.owners.clone())
            .unwrap_or_default();
        let (owner_tx, owner_rx) = mpsc::channel(16);
        let (signal_tx, signal_rx) = mpsc::channel(signal_capacity);
        let (scan_started_tx, scan_started) = mpsc::channel(16);
        let (scan_permit_tx, scan_permits) = mpsc::channel(16);
        Harness {
            bus: ScriptedBus {
                owner_changes: Arc::new(Mutex::new(Some(owner_rx))),
                signals: Arc::new(Mutex::new(Some(signal_rx))),
                scans: Arc::new(Mutex::new(scans.into())),
                owners: Arc::new(Mutex::new(owners)),
                scan_started: scan_started_tx,
                scan_permits: Arc::new(tokio::sync::Mutex::new(scan_permits)),
                scan_calls: Arc::new(AtomicUsize::new(0)),
                storm_reports: Arc::new(AtomicUsize::new(0)),
                drained: Arc::new(AtomicUsize::new(0)),
                ignored_signals: Arc::new(AtomicUsize::new(0)),
                controls: Arc::new(Mutex::new(Vec::new())),
                fail_control: Arc::new(AtomicBool::new(false)),
                block_next_control: Arc::new(AtomicBool::new(false)),
                control_gate: Arc::new(tokio::sync::Notify::new()),
                block_after_first_player: Arc::new(AtomicBool::new(false)),
                player_gate: Arc::new(tokio::sync::Notify::new()),
            },
            owner_tx,
            signal_tx,
            scan_started,
            scan_permit_tx,
        }
    }

    fn harness(scans: Vec<ScannedPlayers>) -> Harness {
        harness_with_signal_capacity(scans, SIGNAL_QUEUE_CAPACITY)
    }

    async fn recv_snapshot(rx: &mut mpsc::Receiver<TrackerUpdate>) -> TrackerUpdate {
        loop {
            let update = rx
                .recv()
                .await
                .expect("tracker update channel remains open");
            if matches!(update, TrackerUpdate::Snapshot { .. }) {
                return update;
            }
        }
    }

    async fn recv_partial(rx: &mut mpsc::Receiver<TrackerUpdate>) -> TrackerUpdate {
        loop {
            let update = rx
                .recv()
                .await
                .expect("tracker update channel remains open");
            if matches!(update, TrackerUpdate::PartialSnapshot { .. }) {
                return update;
            }
        }
    }

    async fn recv_scan_started(rx: &mut mpsc::Receiver<TrackerUpdate>) -> TrackerUpdate {
        loop {
            let update = rx
                .recv()
                .await
                .expect("tracker update channel remains open");
            if matches!(update, TrackerUpdate::ScanStarted { .. }) {
                return update;
            }
        }
    }

    #[tokio::test]
    async fn production_tracker_fences_signal_during_scan_and_emits_clean_metadata() {
        let name = "org.mpris.MediaPlayer2.alpha";
        let mixed = scan(vec![player(name, ":1.20", "Mixed")]);
        let clean = scan(vec![player(name, ":1.20", "Clean")]);
        let mut h = harness(vec![mixed, clean]);
        let (update_tx, mut update_rx) = mpsc::channel(16);
        let generation = Arc::new(AtomicU64::new(0));
        let tracker = tokio::spawn(run_tracker(
            h.bus.clone(),
            update_tx,
            Arc::clone(&generation),
        ));
        assert_eq!(update_rx.recv().await, Some(TrackerUpdate::Generation(1)));
        assert_eq!(h.scan_started.recv().await, Some(0));
        h.signal_tx
            .send(Ok(SignalEvent::Properties {
                sender: ":1.20".into(),
                playing_observed_at_us: None,
            }))
            .await
            .unwrap();
        h.scan_permit_tx.send(()).await.unwrap();
        assert_eq!(h.scan_started.recv().await, Some(1));
        h.scan_permit_tx.send(()).await.unwrap();
        let TrackerUpdate::Snapshot { players, .. } = recv_snapshot(&mut update_rx).await else {
            panic!("expected clean snapshot");
        };
        assert_eq!(
            players.values().next().unwrap().metadata.title.as_deref(),
            Some("Clean")
        );
        assert_eq!(h.bus.drained.load(Ordering::Acquire), 1);
        tracker.abort();
    }

    #[tokio::test]
    async fn owner_turnover_mints_new_player_epoch() {
        let name = "org.mpris.MediaPlayer2.alpha";
        let first = scan(vec![player(name, ":1.20", "First")]);
        let second = scan(vec![player(name, ":1.21", "Second")]);
        let mut h = harness(vec![first.clone(), second.clone()]);
        *h.bus.owners.lock().unwrap() = first.owners.clone();
        let (update_tx, mut update_rx) = mpsc::channel(16);
        let generation = Arc::new(AtomicU64::new(0));
        let tracker = tokio::spawn(run_tracker(h.bus.clone(), update_tx, generation));
        assert!(matches!(
            update_rx.recv().await,
            Some(TrackerUpdate::Generation(1))
        ));
        assert!(matches!(
            recv_scan_started(&mut update_rx).await,
            TrackerUpdate::ScanStarted { .. }
        ));
        assert_eq!(h.scan_started.recv().await, Some(0));
        h.scan_permit_tx.send(()).await.unwrap();
        let TrackerUpdate::Snapshot { players, .. } = recv_snapshot(&mut update_rx).await else {
            panic!("expected first snapshot");
        };
        let first_epoch = players.values().next().unwrap().owner_epoch;
        *h.bus.owners.lock().unwrap() = second.owners.clone();
        h.owner_tx
            .send(Ok(owner_change(name, ":1.20", ":1.21")))
            .await
            .unwrap();
        assert_eq!(h.scan_started.recv().await, Some(1));
        h.scan_permit_tx.send(()).await.unwrap();
        let TrackerUpdate::Snapshot {
            players,
            owner_changes,
            ..
        } = recv_snapshot(&mut update_rx).await
        else {
            panic!("expected second snapshot");
        };
        assert!(players.values().next().unwrap().owner_epoch > first_epoch);
        assert_eq!(owner_changes, vec![owner_change(name, ":1.20", ":1.21")]);
        tracker.abort();
    }

    #[tokio::test]
    async fn transient_owner_edges_are_preserved_without_blocking_the_watcher() {
        let name = "org.mpris.MediaPlayer2.transient";
        let mut h = harness(vec![scan(Vec::new()), scan(Vec::new()), scan(Vec::new())]);
        let (update_tx, mut update_rx) = mpsc::channel(1);
        let generation = Arc::new(AtomicU64::new(0));
        let tracker = tokio::spawn(run_tracker(h.bus.clone(), update_tx, generation));
        assert!(matches!(
            update_rx.recv().await,
            Some(TrackerUpdate::Generation(1))
        ));
        assert!(matches!(
            recv_scan_started(&mut update_rx).await,
            TrackerUpdate::ScanStarted { .. }
        ));
        assert_eq!(h.scan_started.recv().await, Some(0));
        h.scan_permit_tx.send(()).await.unwrap();
        assert!(matches!(
            recv_snapshot(&mut update_rx).await,
            TrackerUpdate::Snapshot { .. }
        ));
        h.owner_tx
            .send(Ok(owner_change(name, "", ":1.30")))
            .await
            .unwrap();
        assert!(matches!(
            recv_scan_started(&mut update_rx).await,
            TrackerUpdate::ScanStarted { .. }
        ));
        assert_eq!(h.scan_started.recv().await, Some(1));
        h.owner_tx
            .send(Ok(owner_change(name, ":1.30", "")))
            .await
            .unwrap();
        h.scan_permit_tx.send(()).await.unwrap();
        assert_eq!(h.scan_started.recv().await, Some(2));
        h.scan_permit_tx.send(()).await.unwrap();
        let TrackerUpdate::Snapshot { owner_changes, .. } = recv_snapshot(&mut update_rx).await
        else {
            panic!("expected transient-edge snapshot");
        };
        assert_eq!(
            owner_changes,
            vec![
                owner_change(name, "", ":1.30"),
                owner_change(name, ":1.30", ""),
            ]
        );
        tracker.abort();
    }

    #[tokio::test]
    async fn rapid_seeked_edges_keep_order_and_receive_time_basis() {
        let name = "org.mpris.MediaPlayer2.alpha";
        let scans = vec![
            scan(vec![player(name, ":1.20", "Raced")]),
            scan(vec![player(name, ":1.20", "Clean")]),
        ];
        let mut h = harness(scans);
        let (update_tx, mut update_rx) = mpsc::channel(4);
        let generation = Arc::new(AtomicU64::new(0));
        let tracker = tokio::spawn(run_tracker(h.bus.clone(), update_tx, generation));
        assert!(matches!(
            update_rx.recv().await,
            Some(TrackerUpdate::Generation(1))
        ));
        assert!(matches!(
            recv_scan_started(&mut update_rx).await,
            TrackerUpdate::ScanStarted { .. }
        ));
        assert_eq!(h.scan_started.recv().await, Some(0));
        h.signal_tx
            .send(Ok(SignalEvent::Seeked {
                sender: ":1.20".into(),
                position_us: 1_000_000,
            }))
            .await
            .unwrap();
        h.signal_tx
            .send(Ok(SignalEvent::Seeked {
                sender: ":1.20".into(),
                position_us: 2_000_000,
            }))
            .await
            .unwrap();
        h.scan_permit_tx.send(()).await.unwrap();
        assert_eq!(h.scan_started.recv().await, Some(1));
        h.scan_permit_tx.send(()).await.unwrap();
        let TrackerUpdate::Snapshot {
            players, seeked, ..
        } = recv_snapshot(&mut update_rx).await
        else {
            panic!("expected seek snapshot");
        };
        assert_eq!(
            seeked
                .iter()
                .map(|seek| seek.position_us)
                .collect::<Vec<_>>(),
            vec![1_000_000, 2_000_000]
        );
        assert!(seeked[0].observed_at_us <= seeked[1].observed_at_us);
        let tracked = players.values().next().unwrap();
        assert_eq!(tracked.position_us, 2_000_000);
        assert_eq!(tracked.position_observed_at_us, seeked[1].observed_at_us);
        tracker.abort();
    }

    #[tokio::test]
    async fn playing_signal_between_paused_scans_reaches_complete_update() {
        let name = "org.mpris.MediaPlayer2.alpha";
        let scans = vec![
            scan(vec![player(name, ":1.20", "Initial paused")]),
            scan(vec![player(name, ":1.20", "Still paused")]),
        ];
        let mut h = harness(scans);
        let (update_tx, mut update_rx) = mpsc::channel(8);
        let tracker = tokio::spawn(run_tracker(
            h.bus.clone(),
            update_tx,
            Arc::new(AtomicU64::new(0)),
        ));
        assert!(matches!(
            update_rx.recv().await,
            Some(TrackerUpdate::Generation(1))
        ));
        assert_eq!(h.scan_started.recv().await, Some(0));
        h.scan_permit_tx.send(()).await.unwrap();
        let _ = recv_snapshot(&mut update_rx).await;

        h.signal_tx
            .send(Ok(SignalEvent::Properties {
                sender: ":1.20".into(),
                playing_observed_at_us: Some(123),
            }))
            .await
            .unwrap();
        assert_eq!(h.scan_started.recv().await, Some(1));
        h.scan_permit_tx.send(()).await.unwrap();
        let TrackerUpdate::Snapshot {
            players, playing, ..
        } = recv_snapshot(&mut update_rx).await
        else {
            panic!("expected complete update");
        };
        assert_eq!(
            players.get(&player_key(name)).unwrap().playback_status,
            PlaybackStatus::Paused
        );
        assert_eq!(
            playing,
            [PlayingObservation {
                key: player_key(name),
                observed_at_us: 123,
            }]
        );
        tracker.abort();
    }

    #[tokio::test]
    async fn downstream_capacity_wait_keeps_draining_media_signals() {
        let name = "org.mpris.MediaPlayer2.alpha";
        let scans = vec![
            scan(vec![player(name, ":1.20", "Initial")]),
            scan(vec![player(name, ":1.20", "Refreshed")]),
        ];
        let mut h = harness(scans);
        let (update_tx, mut update_rx) = mpsc::channel(1);
        let blocker_tx = update_tx.clone();
        let generation = Arc::new(AtomicU64::new(0));
        let tracker = tokio::spawn(run_tracker(
            h.bus.clone(),
            update_tx,
            Arc::clone(&generation),
        ));
        assert!(matches!(
            update_rx.recv().await,
            Some(TrackerUpdate::Generation(1))
        ));
        assert!(matches!(
            recv_scan_started(&mut update_rx).await,
            TrackerUpdate::ScanStarted { .. }
        ));
        assert_eq!(h.scan_started.recv().await, Some(0));
        h.scan_permit_tx.send(()).await.unwrap();
        assert!(matches!(
            recv_snapshot(&mut update_rx).await,
            TrackerUpdate::Snapshot { generation: 1, .. }
        ));
        blocker_tx.send(TrackerUpdate::Generation(1)).await.unwrap();
        for event in [
            SignalEvent::Properties {
                sender: ":1.20".into(),
                playing_observed_at_us: None,
            },
            SignalEvent::Seeked {
                sender: ":1.20".into(),
                position_us: 3_000_000,
            },
        ] {
            h.signal_tx.send(Ok(event)).await.unwrap();
        }
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert_eq!(generation.load(Ordering::Acquire), 1);
        assert!(matches!(
            update_rx.recv().await,
            Some(TrackerUpdate::Generation(1))
        ));
        assert!(matches!(
            recv_scan_started(&mut update_rx).await,
            TrackerUpdate::ScanStarted { .. }
        ));
        assert_eq!(h.scan_started.recv().await, Some(1));
        h.scan_permit_tx.send(()).await.unwrap();
        let TrackerUpdate::Snapshot { seeked, .. } = recv_snapshot(&mut update_rx).await else {
            panic!("expected refreshed snapshot");
        };
        assert_eq!(seeked.len(), 1);
        assert_eq!(seeked[0].position_us, 3_000_000);
        tracker.abort();
    }

    #[tokio::test]
    async fn seeked_from_an_unknown_owner_is_ignored_and_rate_limited() {
        let name = "org.mpris.MediaPlayer2.alpha";
        let scans = vec![scan(vec![player(name, ":1.21", "Clean")])];
        let mut h = harness(scans);
        let (update_tx, mut update_rx) = mpsc::channel(4);
        let generation = Arc::new(AtomicU64::new(0));
        let tracker = tokio::spawn(run_tracker(h.bus.clone(), update_tx, generation));
        assert!(matches!(
            update_rx.recv().await,
            Some(TrackerUpdate::Generation(1))
        ));
        assert_eq!(h.scan_started.recv().await, Some(0));
        h.signal_tx
            .send(Ok(SignalEvent::Seeked {
                sender: ":1.20".into(),
                position_us: 4_000_000,
            }))
            .await
            .unwrap();
        h.signal_tx
            .send(Ok(SignalEvent::Properties {
                sender: ":1.20".into(),
                playing_observed_at_us: None,
            }))
            .await
            .unwrap();
        h.scan_permit_tx.send(()).await.unwrap();
        let TrackerUpdate::Snapshot {
            seeked,
            adapter_loss,
            ..
        } = recv_snapshot(&mut update_rx).await
        else {
            panic!("expected turnover snapshot");
        };
        assert!(seeked.is_empty());
        assert_eq!(adapter_loss, 0);
        assert_eq!(h.bus.ignored_signals.load(Ordering::Acquire), 1);
        tracker.abort();
    }

    #[tokio::test]
    async fn storm_on_alpha_retains_it_stale_while_beta_updates() {
        let alpha = "org.mpris.MediaPlayer2.alpha";
        let beta = "org.mpris.MediaPlayer2.beta";
        let initial = scan(vec![
            player(alpha, ":1.20", "Alpha old"),
            player(beta, ":1.21", "Beta old"),
        ]);
        let raced = scan(vec![
            player(alpha, ":1.20", "Alpha mixed"),
            player(beta, ":1.21", "Beta new"),
        ]);
        let scans = vec![initial, raced.clone(), raced.clone(), raced];
        let mut h = harness(scans);
        let (update_tx, mut update_rx) = mpsc::channel(16);
        let generation = Arc::new(AtomicU64::new(0));
        let tracker = tokio::spawn(run_tracker(h.bus.clone(), update_tx, generation));
        assert!(matches!(
            update_rx.recv().await,
            Some(TrackerUpdate::Generation(1))
        ));
        assert_eq!(h.scan_started.recv().await, Some(0));
        h.scan_permit_tx.send(()).await.unwrap();
        assert!(matches!(
            recv_partial(&mut update_rx).await,
            TrackerUpdate::PartialSnapshot { .. }
        ));
        assert!(matches!(
            recv_snapshot(&mut update_rx).await,
            TrackerUpdate::Snapshot { .. }
        ));
        h.signal_tx
            .send(Ok(SignalEvent::Properties {
                sender: ":1.20".into(),
                playing_observed_at_us: None,
            }))
            .await
            .unwrap();
        for call in 1..=3 {
            assert_eq!(h.scan_started.recv().await, Some(call));
            h.signal_tx
                .send(Ok(SignalEvent::Properties {
                    sender: ":1.20".into(),
                    playing_observed_at_us: None,
                }))
                .await
                .unwrap();
            h.scan_permit_tx.send(()).await.unwrap();
        }
        let TrackerUpdate::PartialSnapshot {
            players: partial, ..
        } = recv_partial(&mut update_rx).await
        else {
            panic!("expected quiet-player partial snapshot");
        };
        assert_eq!(
            partial
                .get(&player_key(beta))
                .unwrap()
                .metadata
                .title
                .as_deref(),
            Some("Beta new")
        );
        let TrackerUpdate::Snapshot { players, .. } = recv_snapshot(&mut update_rx).await else {
            panic!("expected final storm snapshot");
        };
        let alpha = players.get(&player_key(alpha)).unwrap();
        let beta = players.get(&player_key(beta)).unwrap();
        assert_eq!(alpha.metadata.title.as_deref(), Some("Alpha old"));
        assert!(alpha.stale);
        assert_eq!(beta.metadata.title.as_deref(), Some("Beta new"));
        assert!(!beta.stale);
        assert_eq!(h.bus.storm_reports.load(Ordering::Acquire), 1);
        tracker.abort();
    }

    #[tokio::test]
    async fn stalled_player_does_not_delay_quiet_player_publication() {
        let alpha = "org.mpris.MediaPlayer2.alpha";
        let stalled = "org.mpris.MediaPlayer2.stalled";
        let mut h = harness(vec![scan(vec![
            player(alpha, ":1.20", "Alpha ready"),
            player(stalled, ":1.21", "Never completes"),
        ])]);
        h.bus
            .block_after_first_player
            .store(true, Ordering::Release);
        let (update_tx, mut update_rx) = mpsc::channel(4);
        let tracker = tokio::spawn(run_tracker(
            h.bus.clone(),
            update_tx,
            Arc::new(AtomicU64::new(0)),
        ));
        assert!(matches!(
            update_rx.recv().await,
            Some(TrackerUpdate::Generation(1))
        ));
        assert_eq!(h.scan_started.recv().await, Some(0));
        h.scan_permit_tx.send(()).await.unwrap();

        let TrackerUpdate::PartialSnapshot { players, .. } =
            tokio::time::timeout(Duration::from_millis(100), recv_partial(&mut update_rx))
                .await
                .expect("quiet player must publish before the stalled request deadline")
        else {
            panic!("expected partial player snapshot");
        };
        assert_eq!(players.len(), 1);
        assert_eq!(
            players
                .get(&player_key(alpha))
                .and_then(|player| player.metadata.title.as_deref()),
            Some("Alpha ready")
        );
        assert!(!players.contains_key(&player_key(stalled)));
        tracker.abort();
    }

    #[tokio::test]
    async fn accepted_player_racing_during_later_retry_is_reread() {
        let alpha = "org.mpris.MediaPlayer2.alpha";
        let beta = "org.mpris.MediaPlayer2.beta";
        let mut h = harness(vec![
            scan(vec![
                player(alpha, ":1.20", "Alpha mixed"),
                player(beta, ":1.21", "Beta first"),
            ]),
            scan(vec![
                player(alpha, ":1.20", "Alpha clean"),
                player(beta, ":1.21", "Beta mixed"),
            ]),
            scan(vec![
                player(alpha, ":1.20", "Alpha clean"),
                player(beta, ":1.21", "Beta clean"),
            ]),
        ]);
        let (update_tx, mut update_rx) = mpsc::channel(16);
        let tracker = tokio::spawn(run_tracker(
            h.bus.clone(),
            update_tx,
            Arc::new(AtomicU64::new(0)),
        ));
        assert!(matches!(
            update_rx.recv().await,
            Some(TrackerUpdate::Generation(1))
        ));

        assert_eq!(h.scan_started.recv().await, Some(0));
        h.signal_tx
            .send(Ok(SignalEvent::Properties {
                sender: ":1.20".into(),
                playing_observed_at_us: None,
            }))
            .await
            .unwrap();
        h.scan_permit_tx.send(()).await.unwrap();

        assert_eq!(h.scan_started.recv().await, Some(1));
        h.signal_tx
            .send(Ok(SignalEvent::Properties {
                sender: ":1.21".into(),
                playing_observed_at_us: None,
            }))
            .await
            .unwrap();
        h.scan_permit_tx.send(()).await.unwrap();

        assert_eq!(h.scan_started.recv().await, Some(2));
        h.scan_permit_tx.send(()).await.unwrap();

        let mut final_players = BTreeMap::new();
        while let Ok(Some(update)) =
            tokio::time::timeout(Duration::from_millis(20), update_rx.recv()).await
        {
            if let TrackerUpdate::Snapshot { players, .. } = update {
                final_players = players;
            }
        }
        assert_eq!(
            final_players
                .get(&player_key(beta))
                .and_then(|player| player.metadata.title.as_deref()),
            Some("Beta clean")
        );
        tracker.abort();
    }

    #[tokio::test]
    async fn signal_flood_cannot_starve_a_ready_scan_future_or_lose_signals() {
        let h = harness(Vec::new());
        let (signal_tx, signal_rx) = mpsc::channel(SIGNAL_QUEUE_CAPACITY);
        let mut signals = signal_stream(signal_rx);
        let ledger = Arc::new(AtomicU64::new(0));
        let mut signal_state = SignalState::new(0, ledger);
        signal_state.learn_owners(
            &h.bus,
            &BTreeMap::from([("org.mpris.MediaPlayer2.alpha".into(), ":1.20".into())]),
        );
        for _ in 0..128 {
            signal_tx
                .send(Ok(SignalEvent::Properties {
                    sender: ":1.20".into(),
                    playing_observed_at_us: None,
                }))
                .await
                .unwrap();
        }
        drop(signal_tx);

        let (value, drained) = tokio::time::timeout(Duration::from_secs(1), async {
            let (value, drained) =
                await_bus_reply(&h.bus, &mut signals, &mut signal_state, async {
                    Ok::<_, anyhow::Error>(7)
                })
                .await?;
            Ok::<_, anyhow::Error>((value?, drained))
        })
        .await
        .expect("ready scan future must not starve")
        .unwrap();
        assert_eq!(value, 7);
        assert_eq!(drained, 64);
        let mut remaining = 0;
        while signals.next().await.is_some() {
            remaining += 1;
        }
        assert_eq!(drained + remaining, 128);
    }

    #[test]
    fn signal_epochs_are_pruned_when_the_final_owner_set_drops_a_sender() {
        let h = harness(Vec::new());
        let mut state = SignalState::new(0, Arc::new(AtomicU64::new(0)));
        let owners = BTreeMap::from([("org.mpris.MediaPlayer2.alpha".into(), ":1.20".into())]);
        state.learn_owners(&h.bus, &owners);
        state.observe(
            &h.bus,
            SignalEvent::Properties {
                sender: ":1.20".into(),
                playing_observed_at_us: None,
            },
        );
        assert!(state.epochs.contains_key(":1.20"));
        state.finish_owners(&BTreeMap::new());
        assert!(!state.epochs.contains_key(":1.20"));
    }

    #[test]
    fn properties_changed_playing_is_parsed_only_for_the_player_interface() {
        let changed = HashMap::from([(
            "PlaybackStatus".to_string(),
            OwnedValue::from(zbus::zvariant::Str::from("Playing")),
        )]);
        assert!(properties_started_playing(PLAYER_IFACE, &changed));
        assert!(!properties_started_playing(ROOT_IFACE, &changed));

        let paused = HashMap::from([(
            "PlaybackStatus".to_string(),
            OwnedValue::from(zbus::zvariant::Str::from("Paused")),
        )]);
        assert!(!properties_started_playing(PLAYER_IFACE, &paused));
    }

    #[test]
    fn playing_signal_observations_retain_receive_order() {
        let h = harness(Vec::new());
        let mut state = SignalState::new(0, Arc::new(AtomicU64::new(0)));
        state.learn_owners(
            &h.bus,
            &BTreeMap::from([
                ("org.mpris.MediaPlayer2.alpha".into(), ":1.20".into()),
                ("org.mpris.MediaPlayer2.beta".into(), ":1.21".into()),
            ]),
        );
        state.observe(
            &h.bus,
            SignalEvent::Properties {
                sender: ":1.21".into(),
                playing_observed_at_us: Some(10),
            },
        );
        state.observe(
            &h.bus,
            SignalEvent::Properties {
                sender: ":1.20".into(),
                playing_observed_at_us: Some(20),
            },
        );
        assert_eq!(
            state
                .pending_playing
                .iter()
                .map(|observation| observation.sender.as_str())
                .collect::<Vec<_>>(),
            [":1.21", ":1.20"]
        );
        assert!(state.pending_playing[0].observed_at_us <= state.pending_playing[1].observed_at_us);
    }

    #[tokio::test]
    async fn partial_and_complete_updates_share_one_scan_revision() {
        let alpha = player("org.mpris.MediaPlayer2.alpha", ":1.20", "Alpha");
        let beta = player("org.mpris.MediaPlayer2.beta", ":1.21", "Beta");
        let mut h = harness(vec![scan(vec![alpha, beta])]);
        let (update_tx, mut update_rx) = mpsc::channel(8);
        let tracker = tokio::spawn(run_tracker(
            h.bus.clone(),
            update_tx,
            Arc::new(AtomicU64::new(0)),
        ));
        assert!(matches!(
            update_rx.recv().await,
            Some(TrackerUpdate::Generation(1))
        ));
        let Some(TrackerUpdate::ScanStarted {
            scan_revision: started,
            ..
        }) = update_rx.recv().await
        else {
            panic!("expected scan-start marker");
        };
        assert_eq!(h.scan_started.recv().await, Some(0));
        h.scan_permit_tx.send(()).await.unwrap();
        let TrackerUpdate::PartialSnapshot {
            scan_revision: partial,
            ..
        } = recv_partial(&mut update_rx).await
        else {
            panic!("expected partial snapshot");
        };
        let TrackerUpdate::Snapshot {
            scan_revision: complete,
            ..
        } = recv_snapshot(&mut update_rx).await
        else {
            panic!("expected complete snapshot");
        };
        assert_eq!(partial, started);
        assert_eq!(complete, started);
        tracker.abort();
    }

    #[tokio::test]
    async fn hanging_player_request_does_not_block_a_quiet_peer() {
        let (quick_tx, mut quick_rx) = mpsc::channel(1);
        let requests = tokio::spawn(async move {
            tokio::join!(
                player_request_with_timeout(Duration::from_millis(20), async {
                    std::future::pending::<std::result::Result<(), ()>>().await
                }),
                async {
                    let result = player_request(async { Ok::<_, ()>("quiet") }).await;
                    quick_tx.send(result).await.unwrap();
                }
            )
            .0
        });
        tokio::task::yield_now().await;
        assert!(matches!(
            quick_rx.try_recv(),
            Ok(PlayerRequest::Value("quiet"))
        ));
        assert!(matches!(requests.await.unwrap(), PlayerRequest::TimedOut));
        let snapshot = unresponsive_player("org.mpris.MediaPlayer2.hung", ":1.90");
        assert!(snapshot.unresponsive);
        assert!(!snapshot.stale);
    }

    #[tokio::test]
    async fn tracker_restart_carries_pending_observations_into_one_loss_report() {
        let name = "org.mpris.MediaPlayer2.alpha";
        let mut first = harness(vec![scan(vec![player(name, ":1.20", "Never read")])]);
        let generation = Arc::new(AtomicU64::new(0));
        let ledger = Arc::new(AtomicU64::new(0));
        let (tx, mut rx) = mpsc::channel(4);
        let tracker = tokio::spawn(run_tracker_with_state(
            first.bus.clone(),
            tx,
            Arc::clone(&generation),
            Arc::clone(&ledger),
            0,
        ));
        assert!(matches!(
            rx.recv().await,
            Some(TrackerUpdate::Generation(1))
        ));
        assert_eq!(first.scan_started.recv().await, Some(0));
        first
            .signal_tx
            .send(Ok(SignalEvent::Seeked {
                sender: ":1.20".into(),
                position_us: 5,
            }))
            .await
            .unwrap();
        first.signal_tx.send(Err("restart".into())).await.unwrap();
        assert!(tracker.await.unwrap().is_err());
        assert_eq!(ledger.load(Ordering::Acquire), 1);

        let mut second = harness(vec![scan(vec![player(name, ":1.20", "Recovered")])]);
        let (tx, mut rx) = mpsc::channel(4);
        let carried = ledger.swap(0, Ordering::AcqRel);
        let tracker = tokio::spawn(run_tracker_with_state(
            second.bus.clone(),
            tx,
            generation,
            ledger,
            carried,
        ));
        assert!(matches!(
            rx.recv().await,
            Some(TrackerUpdate::Generation(2))
        ));
        assert_eq!(second.scan_started.recv().await, Some(0));
        second.scan_permit_tx.send(()).await.unwrap();
        let TrackerUpdate::Snapshot { adapter_loss, .. } = recv_snapshot(&mut rx).await else {
            panic!("expected recovered snapshot");
        };
        assert_eq!(adapter_loss, 1);
        tracker.abort();
    }

    #[tokio::test]
    async fn successful_provisional_loss_is_not_recounted_on_later_restart() {
        const PROVISIONAL_LOSS: u64 = 7;
        let name = "org.mpris.MediaPlayer2.alpha";
        let mut first = harness(vec![scan(vec![player(name, ":1.20", "Recovered")])]);
        let generation = Arc::new(AtomicU64::new(0));
        let ledger = Arc::new(AtomicU64::new(0));
        let (tx, mut rx) = mpsc::channel(4);
        let tracker = tokio::spawn(run_tracker_with_state(
            first.bus.clone(),
            tx,
            Arc::clone(&generation),
            Arc::clone(&ledger),
            PROVISIONAL_LOSS,
        ));
        assert!(matches!(
            rx.recv().await,
            Some(TrackerUpdate::Generation(1))
        ));
        assert_eq!(first.scan_started.recv().await, Some(0));
        first.scan_permit_tx.send(()).await.unwrap();
        let TrackerUpdate::Snapshot { adapter_loss, .. } = recv_snapshot(&mut rx).await else {
            panic!("expected recovered snapshot");
        };
        assert_eq!(adapter_loss, PROVISIONAL_LOSS);
        assert_eq!(ledger.load(Ordering::Acquire), 0);

        first
            .signal_tx
            .send(Err("later restart".into()))
            .await
            .unwrap();
        assert!(tracker.await.unwrap().is_err());
        assert_eq!(ledger.load(Ordering::Acquire), 0);

        let mut second = harness(vec![scan(vec![player(name, ":1.20", "Still clean")])]);
        let carried = ledger.swap(0, Ordering::AcqRel);
        let (tx, mut rx) = mpsc::channel(4);
        let tracker = tokio::spawn(run_tracker_with_state(
            second.bus.clone(),
            tx,
            generation,
            ledger,
            carried,
        ));
        assert!(matches!(
            rx.recv().await,
            Some(TrackerUpdate::Generation(2))
        ));
        assert_eq!(second.scan_started.recv().await, Some(0));
        second.scan_permit_tx.send(()).await.unwrap();
        let TrackerUpdate::Snapshot { adapter_loss, .. } = recv_snapshot(&mut rx).await else {
            panic!("expected clean snapshot after later restart");
        };
        assert_eq!(adapter_loss, 0);
        tracker.abort();
    }

    #[tokio::test]
    async fn tracker_restart_counts_preclassification_observations_and_overflow() {
        const OVERFLOW: usize = 3;
        let observation_count = SIGNAL_QUEUE_CAPACITY + OVERFLOW;
        let first = harness_with_signal_capacity(
            vec![scan(vec![player(
                "org.mpris.MediaPlayer2.alpha",
                ":1.20",
                "Never scanned",
            )])],
            observation_count + 1,
        );
        for index in 0..observation_count {
            let event = if index % 2 == 0 {
                SignalEvent::Seeked {
                    sender: ":1.20".into(),
                    position_us: index as i64,
                }
            } else {
                SignalEvent::Properties {
                    sender: ":1.20".into(),
                    playing_observed_at_us: Some(index as u64),
                }
            };
            first.signal_tx.send(Ok(event)).await.unwrap();
        }
        first
            .signal_tx
            .send(Err("startup tracker failure".into()))
            .await
            .unwrap();

        let generation = Arc::new(AtomicU64::new(0));
        let ledger = Arc::new(AtomicU64::new(0));
        let (tx, mut rx) = mpsc::channel(4);
        let tracker = tokio::spawn(run_tracker_with_state(
            first.bus,
            tx,
            Arc::clone(&generation),
            Arc::clone(&ledger),
            0,
        ));
        assert!(matches!(
            rx.recv().await,
            Some(TrackerUpdate::Generation(1))
        ));
        assert!(tracker.await.unwrap().is_err());
        assert_eq!(ledger.load(Ordering::Acquire), observation_count as u64);

        let mut second = harness(vec![scan(vec![player(
            "org.mpris.MediaPlayer2.alpha",
            ":1.20",
            "Recovered",
        )])]);
        let (tx, mut rx) = mpsc::channel(4);
        let carried = ledger.swap(0, Ordering::AcqRel);
        let tracker = tokio::spawn(run_tracker_with_state(
            second.bus, tx, generation, ledger, carried,
        ));
        assert!(matches!(
            rx.recv().await,
            Some(TrackerUpdate::Generation(2))
        ));
        assert_eq!(second.scan_started.recv().await, Some(0));
        second.scan_permit_tx.send(()).await.unwrap();
        let TrackerUpdate::Snapshot { adapter_loss, .. } = recv_snapshot(&mut rx).await else {
            panic!("expected recovered snapshot");
        };
        assert_eq!(adapter_loss, observation_count as u64);
        tracker.abort();
    }

    #[test]
    fn owner_epochs_are_process_global_across_tracker_lifetimes() {
        let first = mint_owner_epoch();
        let second = mint_owner_epoch();
        assert!(second > first);
    }

    #[tokio::test]
    async fn expired_queued_control_is_never_dispatched() {
        let name = "org.mpris.MediaPlayer2.alpha";
        let h = harness(Vec::new());
        h.bus
            .owners
            .lock()
            .unwrap()
            .insert(name.into(), ":1.20".into());
        h.bus.block_next_control.store(true, Ordering::Release);
        let generation = Arc::new(AtomicU64::new(7));
        let target = ControlTarget {
            name: name.into(),
            owner: ":1.20".into(),
            owner_epoch: 3,
            generation: 7,
        };
        let owners = Arc::new(Mutex::new(BTreeMap::from([(name.into(), target.clone())])));
        let (tx, rx) = mpsc::channel(4);
        let worker = tokio::spawn(run_control_worker(
            h.bus.clone(),
            rx,
            owners,
            Arc::clone(&generation),
        ));
        let deadline = tokio::time::Instant::now() + Duration::from_millis(20);
        let (play_tx, play_rx) = oneshot::channel();
        tx.send(ControlJob {
            target: target.clone(),
            action: ControlAction::Play,
            deadline,
            reply: play_tx,
        })
        .await
        .unwrap();
        while h.bus.controls.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
        let (seek_tx, seek_rx) = oneshot::channel();
        tx.send(ControlJob {
            target,
            action: ControlAction::Seek { offset_us: 10 },
            deadline,
            reply: seek_tx,
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        let play_error = play_rx.await.unwrap().unwrap_err();
        assert_eq!(play_error.name, "org.cosmix.Mpris.Timeout");
        assert_eq!(play_error.executed_by.as_deref(), Some(":1.20"));
        assert_eq!(
            seek_rx.await.unwrap().unwrap_err().name,
            "org.cosmix.Mpris.Expired"
        );
        assert_eq!(h.bus.controls.lock().unwrap().len(), 1);
        drop(tx);
        worker.await.unwrap().unwrap();
    }

    #[test]
    fn each_control_dbus_call_is_capped_at_fifteen_seconds_of_remaining_budget() {
        let now = tokio::time::Instant::now();
        let long_deadline = now + Duration::from_secs(60);
        let bounded = bounded_call_deadline(long_deadline).unwrap();
        assert!(bounded <= tokio::time::Instant::now() + CONTROL_CALL_TIMEOUT);
        let short_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        assert_eq!(bounded_call_deadline(short_deadline), Some(short_deadline));
        assert_eq!(bounded_call_deadline(tokio::time::Instant::now()), None);
    }

    #[tokio::test]
    async fn control_worker_forwards_success_and_exact_dbus_error_through_seam() {
        let name = "org.mpris.MediaPlayer2.alpha";
        let h = harness(Vec::new());
        h.bus
            .owners
            .lock()
            .unwrap()
            .insert(name.into(), ":1.20".into());
        let generation = Arc::new(AtomicU64::new(7));
        let target = ControlTarget {
            name: name.into(),
            owner: ":1.20".into(),
            owner_epoch: 3,
            generation: 7,
        };
        let owners = Arc::new(Mutex::new(BTreeMap::from([(name.into(), target.clone())])));
        let (tx, rx) = mpsc::channel(4);
        let worker = tokio::spawn(run_control_worker(
            h.bus.clone(),
            rx,
            owners,
            Arc::clone(&generation),
        ));
        let (reply_tx, reply_rx) = oneshot::channel();
        tx.send(ControlJob {
            target: target.clone(),
            action: ControlAction::PlayPause,
            deadline: tokio::time::Instant::now() + Duration::from_secs(60),
            reply: reply_tx,
        })
        .await
        .unwrap();
        assert_eq!(
            reply_rx.await.unwrap(),
            Ok(ControlResult {
                result: None,
                executed_by: ":1.20".into(),
            })
        );
        h.bus.fail_control.store(true, Ordering::Release);
        let (reply_tx, reply_rx) = oneshot::channel();
        tx.send(ControlJob {
            target,
            action: ControlAction::SetVolume { volume: 0.4 },
            deadline: tokio::time::Instant::now() + Duration::from_secs(60),
            reply: reply_tx,
        })
        .await
        .unwrap();
        assert_eq!(
            reply_rx.await.unwrap(),
            Err(ControlError {
                name: "org.mpris.MediaPlayer2.Error.Failed".into(),
                message: "scripted failure".into(),
                executed_by: Some(":1.20".into()),
            })
        );
        assert_eq!(h.bus.controls.lock().unwrap().len(), 2);
        drop(tx);
        worker.await.unwrap().unwrap();
    }

    #[test]
    fn property_control_preserves_exact_fdo_error_identity() {
        let error = ControlError::from_fdo(fdo::Error::PropertyReadOnly("read only".into()));
        assert_eq!(error.name, "org.freedesktop.DBus.Error.PropertyReadOnly");
        assert_eq!(error.message, "read only");
    }
}
