//! Bus citizen for the `power.*` read namespace and power event topics.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use cosmix_client::{IncomingCommand, NodedClient};
use cosmix_props_core::PropTree;
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};

use crate::core::{BatterySnapshot, PowerEvent, PowerSnapshot};
use crate::props::PowerProps;
use crate::upower::TrackerUpdate;

pub const BUS_SERVICE: &str = "power";
pub const TOPIC_BATTERY_CHANGED: &str = "power.battery.changed";
pub const TOPIC_DEVICE_ADDED: &str = "power.device.added";
pub const TOPIC_DEVICE_REMOVED: &str = "power.device.removed";
pub const TOPIC_ON_BATTERY_CHANGED: &str = "power.on_battery.changed";
pub const TOPIC_PROPS_CHANGED: &str = "power.props.changed";

const BROKER_RECONNECT_DELAY: Duration = Duration::from_secs(60);
const PUBLISH_TIMEOUT: Duration = Duration::from_secs(60);
const SNAPSHOT_QUEUE_CAPACITY: usize = 64;
const EVENT_QUEUE_CAPACITY: usize = 64;
const STATE_KEY_CAPACITY: usize = 1024;
const LOSS_EPISODE_LOG_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Default)]
struct CitizenState {
    snapshot: PowerSnapshot,
    event_seq: u64,
    owner_epoch: u64,
    publisher_loss: Arc<AtomicU64>,
}

#[derive(Debug)]
struct Publication {
    topic: &'static str,
    class: PublicationClass,
    event_seq: u64,
    owner_epoch: u64,
    message: cosmix_bus::bus::BusMessage,
    gap_loss: Option<PendingLoss>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum PublicationClass {
    State(String),
    Event,
    Gap,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct PendingLoss {
    lost_count: u64,
    through_seq: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LossEpisodeLog {
    Started,
    Continuing { lost_count: u64, elapsed: Duration },
    Ended { lost_count: u64, elapsed: Duration },
}

#[derive(Debug)]
struct ActiveLossEpisode {
    started_at: Instant,
    last_log_at: Instant,
    lost_count: u64,
}

#[derive(Debug, Default)]
struct LossEpisodeLimiter {
    active: Option<ActiveLossEpisode>,
}

impl LossEpisodeLimiter {
    fn record_loss(&mut self, count: u64, now: Instant) -> Option<LossEpisodeLog> {
        if count == 0 {
            return None;
        }
        let Some(active) = self.active.as_mut() else {
            self.active = Some(ActiveLossEpisode {
                started_at: now,
                last_log_at: now,
                lost_count: count,
            });
            return Some(LossEpisodeLog::Started);
        };

        active.lost_count = active.lost_count.saturating_add(count);
        if now.saturating_duration_since(active.last_log_at) < LOSS_EPISODE_LOG_INTERVAL {
            return None;
        }
        active.last_log_at = now;
        Some(LossEpisodeLog::Continuing {
            lost_count: active.lost_count,
            elapsed: now.saturating_duration_since(active.started_at),
        })
    }

    fn publication_succeeded(&mut self, now: Instant) -> Option<LossEpisodeLog> {
        let active = self.active.take()?;
        Some(LossEpisodeLog::Ended {
            lost_count: active.lost_count,
            elapsed: now.saturating_duration_since(active.started_at),
        })
    }
}

#[derive(Default)]
struct PendingPublications {
    state: BTreeMap<(&'static str, String), Publication>,
    events: VecDeque<Publication>,
    loss: PendingLoss,
    loss_episode: LossEpisodeLimiter,
}

enum LossCause {
    LatestWins {
        topic: &'static str,
        key: String,
        event_seq: u64,
    },
    StateKeyCap {
        topic: &'static str,
        key: String,
        event_seq: u64,
    },
    EventFifo {
        dropped_seq: u64,
        event_seq: u64,
    },
    StaleOwnerQueue {
        discarded: usize,
    },
    PublisherBacklog {
        discarded: usize,
    },
}

impl fmt::Display for LossCause {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LatestWins {
                topic,
                key,
                event_seq,
            } => write!(
                formatter,
                "latest-wins replacement on {topic}/{key} through seq {event_seq}"
            ),
            Self::StateKeyCap {
                topic,
                key,
                event_seq,
            } => write!(
                formatter,
                "state-key cap dropped oldest {topic}/{key} at seq {event_seq}"
            ),
            Self::EventFifo {
                dropped_seq,
                event_seq,
            } => write!(
                formatter,
                "event FIFO overflow dropped oldest seq {dropped_seq} through seq {event_seq}"
            ),
            Self::StaleOwnerQueue { discarded } => write!(
                formatter,
                "stale owner epoch forced gap and discarded {discarded} queued update(s)"
            ),
            Self::PublisherBacklog { discarded } => write!(
                formatter,
                "publisher loss forced gap and discarded {discarded} queued update(s)"
            ),
        }
    }
}

/// Synchronous publication ingress. State topics coalesce latest-wins by
/// `(topic, path/device)`; add/remove events retain wire order in a bounded
/// FIFO. UPower reduction therefore never awaits a broker-owned future.
#[derive(Clone)]
struct PublicationIngress {
    pending: Arc<Mutex<PendingPublications>>,
    wake: mpsc::Sender<()>,
    dropped_updates: Arc<AtomicU64>,
    #[cfg(test)]
    loss_logs: Arc<Mutex<Vec<String>>>,
}

impl PublicationIngress {
    fn channel() -> (Self, mpsc::Receiver<()>) {
        let (wake, wake_rx) = mpsc::channel(1);
        (
            Self {
                pending: Arc::new(Mutex::new(PendingPublications::default())),
                wake,
                dropped_updates: Arc::new(AtomicU64::new(0)),
                #[cfg(test)]
                loss_logs: Arc::new(Mutex::new(Vec::new())),
            },
            wake_rx,
        )
    }

    fn enqueue(&self, publication: Publication) {
        let topic = publication.topic;
        let event_seq = publication.event_seq;
        let class = publication.class.clone();
        let mut diagnostic = None;
        {
            let mut pending = self.pending.lock().expect("publication queue poisoned");
            match class {
                PublicationClass::State(key) => {
                    if let Some(replaced) = pending.state.insert((topic, key.clone()), publication)
                    {
                        diagnostic = self
                            .record_loss_locked(&mut pending, 1, replaced.event_seq, Instant::now())
                            .map(|admission| {
                                (
                                    admission,
                                    LossCause::LatestWins {
                                        topic,
                                        key,
                                        event_seq,
                                    },
                                )
                            });
                    } else if pending.state.len() > STATE_KEY_CAPACITY {
                        let oldest = pending
                            .state
                            .iter()
                            .min_by_key(|(_, publication)| publication.event_seq)
                            .map(|(key, _)| key.clone())
                            .expect("over-cap state map is non-empty");
                        let dropped = pending
                            .state
                            .remove(&oldest)
                            .expect("oldest state key still exists");
                        diagnostic = self
                            .record_loss_locked(&mut pending, 1, dropped.event_seq, Instant::now())
                            .map(|admission| {
                                (
                                    admission,
                                    LossCause::StateKeyCap {
                                        topic: oldest.0,
                                        key: oldest.1,
                                        event_seq: dropped.event_seq,
                                    },
                                )
                            });
                    }
                }
                PublicationClass::Event => {
                    if pending.events.len() == EVENT_QUEUE_CAPACITY {
                        let dropped = pending
                            .events
                            .pop_front()
                            .expect("full event FIFO is non-empty");
                        diagnostic = self
                            .record_loss_locked(&mut pending, 1, dropped.event_seq, Instant::now())
                            .map(|admission| {
                                (
                                    admission,
                                    LossCause::EventFifo {
                                        dropped_seq: dropped.event_seq,
                                        event_seq,
                                    },
                                )
                            });
                    }
                    pending.events.push_back(publication);
                }
                PublicationClass::Gap => unreachable!("gap frames bypass publication ingress"),
            }
        }
        if let Some((admission, cause)) = diagnostic {
            self.log_loss(admission, format_args!("{cause}"));
        }
        let _ = self.wake.try_send(());
    }

    fn take_next(&self, current_epoch: u64) -> Option<Publication> {
        let (next, diagnostic) = {
            let mut pending = self.pending.lock().expect("publication queue poisoned");
            let stale_state = pending
                .state
                .values()
                .filter(|publication| publication.owner_epoch != current_epoch)
                .count();
            let stale_events = pending
                .events
                .iter()
                .filter(|publication| publication.owner_epoch != current_epoch)
                .count();
            let stale = stale_state + stale_events;
            if pending.loss.lost_count != 0 || stale != 0 {
                let discarded = pending.state.len().saturating_add(pending.events.len());
                let discarded_through = pending
                    .state
                    .values()
                    .map(|publication| publication.event_seq)
                    .chain(
                        pending
                            .events
                            .iter()
                            .map(|publication| publication.event_seq),
                    )
                    .max()
                    .unwrap_or(0);
                pending.state.clear();
                pending.events.clear();
                let admission = (discarded != 0)
                    .then(|| {
                        self.record_loss_locked(
                            &mut pending,
                            discarded as u64,
                            discarded_through,
                            Instant::now(),
                        )
                    })
                    .flatten();
                let loss = std::mem::take(&mut pending.loss);
                let message = gap_publication(loss, current_epoch);
                let diagnostic = if stale != 0 {
                    admission.map(|admission| (admission, LossCause::StaleOwnerQueue { discarded }))
                } else if discarded != 0 {
                    admission
                        .map(|admission| (admission, LossCause::PublisherBacklog { discarded }))
                } else {
                    None
                };
                (Some(message), diagnostic)
            } else {
                let next_state = pending
                    .state
                    .iter()
                    .min_by_key(|(_, publication)| publication.event_seq)
                    .map(|(key, publication)| (key.clone(), publication.event_seq));
                let next_event_seq = pending
                    .events
                    .front()
                    .map(|publication| publication.event_seq);
                let next = match (next_state, next_event_seq) {
                    (Some((key, state_seq)), Some(event_seq)) if state_seq <= event_seq => {
                        pending.state.remove(&key)
                    }
                    (Some((key, _)), None) => pending.state.remove(&key),
                    (_, Some(_)) => pending.events.pop_front(),
                    (None, None) => None,
                };
                (next, None)
            }
        };
        if let Some((admission, cause)) = diagnostic {
            self.log_loss(admission, format_args!("{cause}"));
        }
        next
    }

    fn record_loss(&self, count: u64, through_seq: u64, cause: fmt::Arguments<'_>) {
        self.record_loss_at(count, through_seq, cause, Instant::now());
    }

    fn record_loss_at(
        &self,
        count: u64,
        through_seq: u64,
        cause: fmt::Arguments<'_>,
        now: Instant,
    ) {
        let admission = {
            let mut pending = self.pending.lock().expect("publication queue poisoned");
            self.record_loss_locked(&mut pending, count, through_seq, now)
        };
        if let Some(admission) = admission {
            self.log_loss(admission, cause);
        }
        let _ = self.wake.try_send(());
    }

    fn record_publication_loss(&self, publication: &Publication, cause: fmt::Arguments<'_>) {
        {
            let mut pending = self.pending.lock().expect("publication queue poisoned");
            if let Some(loss) = publication.gap_loss {
                pending.loss.lost_count = pending.loss.lost_count.saturating_add(loss.lost_count);
                pending.loss.through_seq = pending.loss.through_seq.max(loss.through_seq);
            }
        }
        self.record_loss(1, publication.event_seq, cause);
    }

    fn publication_succeeded(&self) {
        self.publication_succeeded_at(Instant::now());
    }

    fn publication_succeeded_at(&self, now: Instant) {
        let admission = {
            let mut pending = self.pending.lock().expect("publication queue poisoned");
            pending.loss_episode.publication_succeeded(now)
        };
        if let Some(LossEpisodeLog::Ended {
            lost_count,
            elapsed,
        }) = admission
        {
            self.emit_loss_log(format_args!(
                "cosmix-powerd: publisher loss episode ended: {lost_count} lost over {:.3} s",
                elapsed.as_secs_f64()
            ));
        }
    }

    fn record_loss_locked(
        &self,
        pending: &mut PendingPublications,
        count: u64,
        through_seq: u64,
        now: Instant,
    ) -> Option<LossEpisodeLog> {
        pending.loss.lost_count = pending.loss.lost_count.saturating_add(count);
        pending.loss.through_seq = pending.loss.through_seq.max(through_seq);
        let _ = self
            .dropped_updates
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |total| {
                Some(total.saturating_add(count))
            });
        pending.loss_episode.record_loss(count, now)
    }

    fn log_loss(&self, admission: LossEpisodeLog, cause: fmt::Arguments<'_>) {
        match admission {
            LossEpisodeLog::Started => self.emit_loss_log(format_args!(
                "cosmix-powerd: publisher loss episode started: {cause}"
            )),
            LossEpisodeLog::Continuing {
                lost_count,
                elapsed,
            } => self.emit_loss_log(format_args!(
                "cosmix-powerd: publisher loss episode continuing: {lost_count} lost over {:.3} s",
                elapsed.as_secs_f64()
            )),
            LossEpisodeLog::Ended { .. } => {
                unreachable!("loss recording cannot end an episode")
            }
        }
    }

    fn emit_loss_log(&self, arguments: fmt::Arguments<'_>) {
        #[cfg(not(test))]
        eprintln!("{arguments}");
        #[cfg(test)]
        self.loss_logs
            .lock()
            .expect("loss log capture poisoned")
            .push(arguments.to_string());
    }

    #[cfg(test)]
    fn dropped_updates(&self) -> u64 {
        self.dropped_updates.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    fn pending_counts(&self) -> (usize, usize) {
        let pending = self.pending.lock().expect("publication queue poisoned");
        (pending.state.len(), pending.events.len())
    }

    #[cfg(test)]
    fn loss_logs(&self) -> Vec<String> {
        self.loss_logs
            .lock()
            .expect("loss log capture poisoned")
            .clone()
    }
}

/// Start the UPower adapter, state reducer, publisher and reconnecting Bus
/// citizen. This does not return during normal operation.
pub async fn serve() -> Result<()> {
    let (snapshot_tx, snapshot_rx) = mpsc::channel(SNAPSHOT_QUEUE_CAPACITY);
    let (publications, publication_wakes) = PublicationIngress::channel();
    let state = Arc::new(Mutex::new(CitizenState {
        publisher_loss: Arc::clone(&publications.dropped_updates),
        ..CitizenState::default()
    }));
    let (publisher_fault_tx, publisher_fault_rx) = mpsc::channel(1);
    let (client_tx, client_rx) = watch::channel::<Option<Arc<NodedClient>>>(None);
    let owner_epoch = Arc::new(AtomicU64::new(0));
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let mut tracker = tokio::spawn(crate::upower::run(snapshot_tx, Arc::clone(&owner_epoch)));
    let mut reducer = tokio::spawn(apply_snapshots(
        snapshot_rx,
        Arc::clone(&state),
        publications.clone(),
    ));
    let mut publisher = tokio::spawn(run_publisher(
        publications,
        publication_wakes,
        client_rx,
        Arc::clone(&owner_epoch),
        publisher_fault_tx,
    ));

    let build = cosmix_buildinfo::build_info!();
    let provenance = cosmix_bus::RegisterProvenance::from_parts(
        build.pkg,
        build.version,
        build.git_sha,
        build.git_dirty,
        build.build_time,
        cosmix_buildinfo::now_rfc3339(),
    );

    let mut broker = tokio::spawn(run_broker(
        Arc::clone(&state),
        client_tx,
        publisher_fault_rx,
        provenance,
        shutdown_rx,
    ));

    enum Exit {
        Shutdown(Result<()>),
        Task(&'static str, Result<Result<()>, tokio::task::JoinError>),
    }

    let exit = tokio::select! {
        signal = shutdown_signal() => Exit::Shutdown(signal),
        result = &mut tracker => Exit::Task("UPower tracker", result),
        result = &mut reducer => Exit::Task("snapshot reducer", result),
        result = &mut publisher => Exit::Task("publisher", result),
        result = &mut broker => Exit::Task("broker loop", result),
    };

    let _ = shutdown_tx.send(true);
    let graceful = matches!(exit, Exit::Shutdown(Ok(())));
    if graceful && !broker.is_finished() {
        match broker.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                eprintln!("cosmix-powerd: broker shutdown failed; continuing teardown: {error:#}")
            }
            Err(error) => eprintln!(
                "cosmix-powerd: broker task failed during shutdown; continuing teardown: {error}"
            ),
        }
    } else {
        broker.abort();
    }
    tracker.abort();
    reducer.abort();
    publisher.abort();

    match exit {
        Exit::Shutdown(result) => result,
        Exit::Task(name, Ok(Ok(()))) => {
            eprintln!("cosmix-powerd: supervised {name} exited unexpectedly");
            Err(anyhow!("supervised {name} exited unexpectedly"))
        }
        Exit::Task(name, Ok(Err(error))) => {
            eprintln!("cosmix-powerd: supervised {name} failed: {error:#}");
            Err(error).with_context(|| format!("supervised {name} failed"))
        }
        Exit::Task(name, Err(error)) => {
            eprintln!("cosmix-powerd: supervised {name} panicked or was cancelled: {error}");
            Err(anyhow!(
                "supervised {name} panicked or was cancelled: {error}"
            ))
        }
    }
}

async fn run_broker(
    state: Arc<Mutex<CitizenState>>,
    client_tx: watch::Sender<Option<Arc<NodedClient>>>,
    mut publisher_fault_rx: mpsc::Receiver<()>,
    provenance: cosmix_bus::RegisterProvenance,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    loop {
        let connection = tokio::select! {
            biased;
            changed = shutdown.changed() => {
                changed.map_err(|_| anyhow!("shutdown coordinator ended"))?;
                return Ok(());
            }
            connection = tokio::time::timeout(
                PUBLISH_TIMEOUT,
                cosmix_config::client_helpers::connect_default_with_provenance(
                    BUS_SERVICE,
                    provenance.clone(),
                ),
            ) => connection,
        };
        match connection {
            Ok(Ok(client)) => {
                let client = Arc::new(client);
                let _ = client_tx.send(Some(Arc::clone(&client)));
                eprintln!("cosmix-powerd: registered as '{BUS_SERVICE}'");
                let stopping = tokio::select! {
                    biased;
                    changed = shutdown.changed() => {
                        changed.map_err(|_| anyhow!("shutdown coordinator ended"))?;
                        true
                    }
                    _ = serve_bus(Arc::clone(&client), &state) => false,
                    fault = publisher_fault_rx.recv() => {
                        if fault.is_none() {
                            return Err(anyhow!("publisher task ended unexpectedly"));
                        }
                        // Deliberately outside the loss-episode limiter: this
                        // line fires at most once per reconnect cycle, and the
                        // cycle below sleeps >= 60s, so it is bounded by
                        // construction. Per-loss logging goes through the
                        // episode limiter; connection lifecycle does not.
                        eprintln!("cosmix-powerd: publisher fault; reconnecting broker client");
                        false
                    }
                };
                let _ = client_tx.send(None);
                if tokio::time::timeout(PUBLISH_TIMEOUT, client.close())
                    .await
                    .is_err()
                {
                    eprintln!("cosmix-powerd: broker client close timed out");
                }
                while publisher_fault_rx.try_recv().is_ok() {}
                if stopping {
                    return Ok(());
                }
                eprintln!("cosmix-powerd: broker disconnected; retrying in 60s");
            }
            Ok(Err(error)) => {
                eprintln!("cosmix-powerd: broker unavailable; retrying in 60s: {error}");
            }
            Err(_) => {
                eprintln!("cosmix-powerd: broker connection timed out; retrying in 60s");
            }
        }
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                changed.map_err(|_| anyhow!("shutdown coordinator ended"))?;
                return Ok(());
            }
            _ = tokio::time::sleep(BROKER_RECONNECT_DELAY) => {}
        }
    }
}

#[cfg(unix)]
async fn shutdown_signal() -> Result<()> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("install SIGTERM handler")?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result.context("listen for SIGINT"),
        signal = terminate.recv() => signal
            .ok_or_else(|| anyhow!("SIGTERM stream ended"))
            .map(|_| ()),
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() -> Result<()> {
    tokio::signal::ctrl_c().await.context("listen for Ctrl-C")
}

async fn apply_snapshots(
    mut updates: mpsc::Receiver<TrackerUpdate>,
    state: Arc<Mutex<CitizenState>>,
    publications: PublicationIngress,
) -> Result<()> {
    while let Some(update) = updates.recv().await {
        let (owner_epoch, next) = match update {
            TrackerUpdate::OwnerEpoch(owner_epoch) => {
                let mut state = state.lock().expect("power state poisoned");
                if owner_epoch > state.owner_epoch {
                    state.owner_epoch = owner_epoch;
                }
                continue;
            }
            TrackerUpdate::Snapshot {
                owner_epoch,
                snapshot,
            } => (owner_epoch, snapshot),
        };
        let publication_batch = {
            let mut state = state.lock().expect("power state poisoned");
            if owner_epoch < state.owner_epoch {
                publications.record_loss(
                    1,
                    state.event_seq,
                    format_args!("stale UPower snapshot before reduction"),
                );
                continue;
            }
            if owner_epoch > state.owner_epoch {
                state.owner_epoch = owner_epoch;
            }
            let publisher_loss = state.publisher_loss.load(Ordering::Acquire);
            let old_tree = PowerProps::new(&state.snapshot, state.event_seq, publisher_loss);
            let old_props = old_tree.snapshot();
            let events = state.snapshot.diff(&next);
            state.snapshot = next;
            let mut publications = events
                .into_iter()
                .map(|event| {
                    state.event_seq = state.event_seq.saturating_add(1);
                    publication_for(&event, state.event_seq, owner_epoch)
                })
                .collect::<Vec<_>>();
            let new_tree = PowerProps::new(&state.snapshot, state.event_seq, publisher_loss);
            let new_props = new_tree.snapshot();
            for (path, old, new) in cosmix_props_core::diff(&old_props, &new_props) {
                let description = new_tree
                    .describe(&path)
                    .or_else(|| old_tree.describe(&path));
                if description.is_none_or(|description| description.transient) {
                    continue;
                }
                let message = cosmix_props_core::publish::build_props_changed_message(
                    &path,
                    &old,
                    &new,
                    "upower.signal",
                );
                publications.push(Publication {
                    topic: TOPIC_PROPS_CHANGED,
                    class: PublicationClass::State(path.to_string()),
                    event_seq: state.event_seq,
                    owner_epoch,
                    message,
                    gap_loss: None,
                });
            }
            publications
        };
        for publication in publication_batch {
            publications.enqueue(publication);
        }
    }
    Ok(())
}

trait EpochSource: Send + Sync + 'static {
    fn current(&self) -> u64;
}

impl EpochSource for AtomicU64 {
    fn current(&self) -> u64 {
        self.load(Ordering::Acquire)
    }
}

trait PublicationClient: Send + Sync + 'static {
    async fn send_publication(&self, headers: &BTreeMap<String, String>, wire: &str) -> Result<()>;
}

impl PublicationClient for NodedClient {
    async fn send_publication(&self, headers: &BTreeMap<String, String>, wire: &str) -> Result<()> {
        self.send_with_headers("noded", "topic.publish", headers, wire)
            .await
    }
}

async fn run_publisher<C, E>(
    publications: PublicationIngress,
    mut wakes: mpsc::Receiver<()>,
    mut clients: watch::Receiver<Option<Arc<C>>>,
    owner_epoch: Arc<E>,
    faults: mpsc::Sender<()>,
) -> Result<()>
where
    C: PublicationClient,
    E: EpochSource,
{
    loop {
        tokio::select! {
            wake = wakes.recv() => {
                if wake.is_none() {
                    return Err(anyhow!("publication wake channel closed"));
                }
            }
            changed = clients.changed() => {
                if changed.is_err() {
                    return Err(anyhow!("publisher client channel closed"));
                }
            }
        }

        'flush: loop {
            let Some(client) = clients.borrow_and_update().clone() else {
                // Keep at most the newest item per topic while disconnected.
                // A future ingress replaces it and accounts for the loss.
                break;
            };
            let Some(mut publication) = publications.take_next(owner_epoch.current()) else {
                break;
            };
            publication
                .message
                .set("event_seq", &publication.event_seq.to_string());
            let headers = BTreeMap::from([
                ("name".to_string(), publication.topic.to_string()),
                ("retain".to_string(), "false".to_string()),
            ]);
            let wire = publication.message.to_wire();
            // This is the last operation before the one sink await. Epoch
            // changes never cancel that await: the sink may already have
            // accepted the frame. At most one already-sent old-epoch frame can
            // therefore be in flight; every not-yet-sent item is skipped.
            if publication.owner_epoch != owner_epoch.current() {
                publications.record_publication_loss(
                    &publication,
                    format_args!(
                        "stale owner epoch at send point for {} seq {}",
                        publication.topic, publication.event_seq
                    ),
                );
                continue;
            }
            match tokio::time::timeout(PUBLISH_TIMEOUT, client.send_publication(&headers, &wire))
                .await
            {
                Ok(Ok(())) => publications.publication_succeeded(),
                Ok(Err(error)) => {
                    publications.record_publication_loss(
                        &publication,
                        format_args!(
                            "broker publish failure for {} seq {}: {error}",
                            publication.topic, publication.event_seq
                        ),
                    );
                    let _ = faults.try_send(());
                    break 'flush;
                }
                Err(_) => {
                    publications.record_publication_loss(
                        &publication,
                        format_args!(
                            "broker publish timeout for {} seq {}",
                            publication.topic, publication.event_seq
                        ),
                    );
                    let _ = faults.try_send(());
                    break 'flush;
                }
            }
        }
    }
}

async fn serve_bus(client: Arc<NodedClient>, state: &Arc<Mutex<CitizenState>>) {
    let Some(mut incoming) = client.incoming_async().await else {
        return;
    };
    while let Some(command) = incoming.recv().await {
        let (rc, body) = dispatch(&command, state);
        match tokio::time::timeout(
            PUBLISH_TIMEOUT,
            client.respond_parts(
                &command.from,
                &command.command,
                command.id.as_deref(),
                rc,
                &body,
            ),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                eprintln!("cosmix-powerd: Bus response failed; reconnecting: {error}");
                return;
            }
            Err(_) => {
                eprintln!("cosmix-powerd: Bus response timed out; reconnecting");
                return;
            }
        }
    }
}

fn dispatch(command: &IncomingCommand, state: &Arc<Mutex<CitizenState>>) -> (u8, String) {
    if let Some(suffix) = command.command.strip_prefix("power.props.") {
        let state = state.lock().expect("power state poisoned");
        if suffix == "watch" {
            return (
                0,
                json!({
                    "topic": TOPIC_PROPS_CHANGED,
                    "domain_topics": [
                        TOPIC_BATTERY_CHANGED,
                        TOPIC_DEVICE_ADDED,
                        TOPIC_DEVICE_REMOVED,
                        TOPIC_ON_BATTERY_CHANGED,
                    ],
                    "event_seq": state.event_seq,
                    "event_sequence": "daemon_session_monotonic",
                    "loss_signal": "gap_and_lost_count",
                    "bootstrap": "subscribe on this connection, then read power.props.get",
                })
                .to_string(),
            );
        }
        let props = PowerProps::new(
            &state.snapshot,
            state.event_seq,
            state.publisher_loss.load(Ordering::Acquire),
        );
        let args = resolve_args(command);
        let response = cosmix_props_core::bus::dispatch_props(&props, suffix, args.as_ref(), true);
        return (response.rc.clamp(0, 255) as u8, response.body);
    }

    match command.command.as_str() {
        "power.ping" => (
            0,
            json!({"pong": true, "service": BUS_SERVICE, "schema": "power.v1"}).to_string(),
        ),
        "power.info" => {
            let state = state.lock().expect("power state poisoned");
            let build = cosmix_buildinfo::build_info!();
            (
                0,
                json!({
                    "name": BUS_SERVICE,
                    "schema": "power.v1",
                    "props_level": "L2",
                    "binary": build.pkg,
                    "version": build.version,
                    "git_sha": build.git_sha,
                    "git_dirty": build.git_dirty,
                    "build_time": build.build_time,
                    "present": state.snapshot.present,
                    "on_battery": state.snapshot.on_battery,
                    "device_count": state.snapshot.devices.len(),
                    "event_seq": state.event_seq,
                    "publisher_loss": state.publisher_loss.load(Ordering::Acquire),
                })
                .to_string(),
            )
        }
        _ => (
            10,
            json!({"error": format!("unknown power verb: {}", command.command)}).to_string(),
        ),
    }
}

fn publication_for(event: &PowerEvent, event_seq: u64, owner_epoch: u64) -> Publication {
    let (topic, class, event_name, body) = match event {
        PowerEvent::BatteryChanged { id, old, new } => (
            TOPIC_BATTERY_CHANGED,
            PublicationClass::State(id.clone()),
            "battery.changed",
            json!({"id": id, "old": old.as_ref().map(battery_json), "new": new.as_ref().map(battery_json)}),
        ),
        PowerEvent::DeviceAdded { id, device } => (
            TOPIC_DEVICE_ADDED,
            PublicationClass::Event,
            "device.added",
            json!({"id": id, "device": battery_json(device)}),
        ),
        PowerEvent::DeviceRemoved { id, device } => (
            TOPIC_DEVICE_REMOVED,
            PublicationClass::Event,
            "device.removed",
            json!({"id": id, "device": battery_json(device)}),
        ),
        PowerEvent::OnBatteryChanged { old, new } => (
            TOPIC_ON_BATTERY_CHANGED,
            PublicationClass::State("source".into()),
            "on_battery.changed",
            json!({"old": old, "new": new}),
        ),
    };
    let mut message = cosmix_bus::bus::BusMessage::new();
    message.set("command", topic);
    message.body = json!({
        "event": event_name,
        "event_seq": event_seq,
        "data": body,
    })
    .to_string();
    Publication {
        topic,
        class,
        event_seq,
        owner_epoch,
        message,
        gap_loss: None,
    }
}

fn gap_publication(loss: PendingLoss, owner_epoch: u64) -> Publication {
    let mut message = cosmix_bus::bus::BusMessage::new();
    message.set("command", "props.changed");
    message.set("event_seq", &loss.through_seq.to_string());
    message.set("gap", "true");
    message.set("cause", "publisher.loss");
    message.body = json!({
        "seq": loss.through_seq,
        "gap": true,
        "lost_count": loss.lost_count,
        "cause": "publisher.loss",
    })
    .to_string();
    Publication {
        topic: TOPIC_PROPS_CHANGED,
        class: PublicationClass::Gap,
        event_seq: loss.through_seq,
        owner_epoch,
        message,
        gap_loss: Some(loss),
    }
}

fn battery_json(battery: &BatterySnapshot) -> Value {
    json!({
        "id": battery.id,
        "kind": battery.kind.as_str(),
        "power_supply": battery.power_supply,
        "present": battery.present,
        "percentage": battery.percentage,
        "state": battery.state.as_str(),
        "time_to_empty_s": battery.time_to_empty_s,
        "time_to_full_s": battery.time_to_full_s,
        "energy_rate_w": battery.energy_rate_w,
        "health_percent": battery.health_percent,
    })
}

fn resolve_args(command: &IncomingCommand) -> Option<Value> {
    if let Some(args) = command.header("args")
        && let Ok(value) = serde_json::from_str(args)
    {
        return Some(value);
    }
    if !command.args.is_null() {
        return Some(command.args.clone());
    }
    if !command.body.is_empty()
        && let Ok(value) = serde_json::from_str(&command.body)
    {
        return Some(value);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{BatteryState, DeviceKind};

    #[derive(Default)]
    struct ScriptedClient {
        sent: Mutex<Vec<(String, String)>>,
        sends: AtomicU64,
        failures_remaining: AtomicU64,
    }

    impl PublicationClient for ScriptedClient {
        async fn send_publication(
            &self,
            headers: &BTreeMap<String, String>,
            wire: &str,
        ) -> Result<()> {
            if self
                .failures_remaining
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
                    (remaining != 0).then(|| remaining - 1)
                })
                .is_ok()
            {
                return Err(anyhow!("scripted broker failure"));
            }
            self.sent.lock().unwrap().push((
                headers.get("name").cloned().unwrap_or_default(),
                wire.to_string(),
            ));
            self.sends.fetch_add(1, Ordering::Release);
            Ok(())
        }
    }

    struct SequencedEpoch {
        values: Mutex<VecDeque<u64>>,
        last: AtomicU64,
        reads: AtomicU64,
    }

    impl SequencedEpoch {
        fn new(values: impl IntoIterator<Item = u64>) -> Self {
            Self {
                values: Mutex::new(values.into_iter().collect()),
                last: AtomicU64::new(0),
                reads: AtomicU64::new(0),
            }
        }
    }

    impl EpochSource for SequencedEpoch {
        fn current(&self) -> u64 {
            self.reads.fetch_add(1, Ordering::Release);
            if let Some(next) = self.values.lock().unwrap().pop_front() {
                self.last.store(next, Ordering::Release);
                next
            } else {
                self.last.load(Ordering::Acquire)
            }
        }
    }

    fn command(verb: &str) -> IncomingCommand {
        IncomingCommand {
            from: "alpha".into(),
            command: verb.into(),
            id: Some("1".into()),
            args: Value::Null,
            body: String::new(),
            headers: BTreeMap::new(),
        }
    }

    fn battery() -> BatterySnapshot {
        BatterySnapshot {
            id: "battery_BAT0".into(),
            kind: DeviceKind::Battery,
            power_supply: true,
            present: true,
            percentage: Some(50.0),
            state: BatteryState::Discharging,
            time_to_empty_s: Some(3_000),
            time_to_full_s: None,
            energy_rate_w: Some(10.0),
            health_percent: Some(90.0),
        }
    }

    fn present_snapshot(on_battery: bool, percentage: f64) -> PowerSnapshot {
        let mut device = battery();
        device.percentage = Some(percentage);
        let display = BatterySnapshot {
            id: "display".into(),
            ..device.clone()
        };
        PowerSnapshot::from_parts(
            on_battery,
            Some(display),
            BTreeMap::from([(device.id.clone(), device)]),
        )
    }

    fn assert_gap(publication: &Publication, lost_count: u64) {
        assert_eq!(publication.topic, TOPIC_PROPS_CHANGED);
        assert_eq!(publication.class, PublicationClass::Gap);
        assert_eq!(publication.message.get("gap"), Some("true"));
        assert_eq!(publication.message.get("cause"), Some("publisher.loss"));
        let body: Value = serde_json::from_str(&publication.message.body).unwrap();
        assert_eq!(body["gap"], true);
        assert_eq!(body["lost_count"], lost_count);
        assert_eq!(body["cause"], "publisher.loss");
    }

    #[test]
    fn delegated_props_get_and_list_are_available() {
        let state = Arc::new(Mutex::new(CitizenState::default()));
        let (rc, get) = dispatch(&command("power.props.get"), &state);
        assert_eq!(rc, 0);
        assert_eq!(
            serde_json::from_str::<Value>(&get).unwrap()["present"],
            false
        );

        let (rc, list) = dispatch(&command("power.props.list"), &state);
        assert_eq!(rc, 0);
        assert!(
            serde_json::from_str::<Vec<String>>(&list)
                .unwrap()
                .contains(&"present".to_string())
        );
    }

    #[test]
    fn info_and_props_expose_cumulative_publisher_loss() {
        let state = Arc::new(Mutex::new(CitizenState::default()));
        state
            .lock()
            .unwrap()
            .publisher_loss
            .store(4, Ordering::Release);

        let (rc, info) = dispatch(&command("power.info"), &state);
        assert_eq!(rc, 0);
        assert_eq!(
            serde_json::from_str::<Value>(&info).unwrap()["publisher_loss"],
            4
        );
        let (rc, props) = dispatch(&command("power.props.get"), &state);
        assert_eq!(rc, 0);
        assert_eq!(
            serde_json::from_str::<Value>(&props).unwrap()["lifecycle"]["publisher_loss"],
            4
        );
    }

    #[test]
    fn event_topics_and_sequence_are_exact() {
        let event = PowerEvent::DeviceAdded {
            id: "battery_BAT0".into(),
            device: battery(),
        };
        let publication = publication_for(&event, 7, 3);
        assert_eq!(publication.topic, TOPIC_DEVICE_ADDED);
        assert_eq!(publication.owner_epoch, 3);
        let body: Value = serde_json::from_str(&publication.message.body).unwrap();
        assert_eq!(body["event"], "device.added");
        assert_eq!(body["event_seq"], 7);
        assert_eq!(body["data"]["device"]["state"], "discharging");
    }

    #[test]
    fn owner_turnover_discards_stale_epoch_publications() {
        let (ingress, _wakes) = PublicationIngress::channel();
        ingress.enqueue(publication_for(
            &PowerEvent::OnBatteryChanged {
                old: false,
                new: true,
            },
            1,
            4,
        ));

        let gap = ingress.take_next(5).unwrap();
        assert_gap(&gap, 1);
        assert_eq!(ingress.dropped_updates(), 1);
        assert!(ingress.take_next(5).is_none());
    }

    #[test]
    fn publication_ingress_replacement_queues_gap_with_loss_count() {
        let (ingress, _wakes) = PublicationIngress::channel();
        for (seq, new) in [(1, true), (2, false)] {
            ingress.enqueue(publication_for(
                &PowerEvent::OnBatteryChanged { old: !new, new },
                seq,
                1,
            ));
        }

        assert_eq!(ingress.dropped_updates(), 1);
        let gap = ingress.take_next(1).unwrap();
        assert_gap(&gap, 2);
        assert_eq!(ingress.dropped_updates(), 2);
        assert!(ingress.take_next(1).is_none());
    }

    #[test]
    fn event_fifo_overflow_fast_forwards_to_gap_with_lost_count() {
        let (ingress, _wakes) = PublicationIngress::channel();
        for seq in 1..=(EVENT_QUEUE_CAPACITY as u64 + 1) {
            ingress.enqueue(publication_for(
                &PowerEvent::DeviceAdded {
                    id: format!("battery_{seq}"),
                    device: battery(),
                },
                seq,
                1,
            ));
        }

        assert_eq!(ingress.dropped_updates(), 1);
        assert_eq!(ingress.pending_counts(), (0, EVENT_QUEUE_CAPACITY));
        let gap = ingress.take_next(1).unwrap();
        assert_gap(&gap, EVENT_QUEUE_CAPACITY as u64 + 1);
        assert!(ingress.take_next(1).is_none());
    }

    #[test]
    fn state_key_cap_drops_oldest_and_uses_the_same_gap_path() {
        let (ingress, _wakes) = PublicationIngress::channel();
        for seq in 1..=(STATE_KEY_CAPACITY as u64 + 1) {
            ingress.enqueue(publication_for(
                &PowerEvent::BatteryChanged {
                    id: format!("battery_{seq}"),
                    old: None,
                    new: Some(battery()),
                },
                seq,
                1,
            ));
        }

        assert_eq!(ingress.dropped_updates(), 1);
        assert_eq!(ingress.pending_counts(), (STATE_KEY_CAPACITY, 0));
        let gap = ingress.take_next(1).unwrap();
        assert_gap(&gap, STATE_KEY_CAPACITY as u64 + 1);
        assert!(ingress.take_next(1).is_none());
    }

    #[test]
    fn loss_logging_is_limited_to_one_start_until_recovery() {
        let (ingress, _wakes) = PublicationIngress::channel();
        let started_at = Instant::now();

        for seq in 1..=10_000 {
            ingress.record_loss_at(1, seq, format_args!("state storm loss {seq}"), started_at);
        }

        assert_eq!(ingress.dropped_updates(), 10_000);
        assert_eq!(
            ingress.loss_logs(),
            ["cosmix-powerd: publisher loss episode started: state storm loss 1"]
        );

        ingress.publication_succeeded_at(started_at + Duration::from_secs(2));
        assert_eq!(
            ingress.loss_logs(),
            [
                "cosmix-powerd: publisher loss episode started: state storm loss 1",
                "cosmix-powerd: publisher loss episode ended: 10000 lost over 2.000 s",
            ]
        );

        ingress.record_loss_at(
            1,
            10_001,
            format_args!("second state storm"),
            started_at + Duration::from_secs(3),
        );
        assert_eq!(ingress.dropped_updates(), 10_001);
        assert_eq!(
            ingress.loss_logs(),
            [
                "cosmix-powerd: publisher loss episode started: state storm loss 1",
                "cosmix-powerd: publisher loss episode ended: 10000 lost over 2.000 s",
                "cosmix-powerd: publisher loss episode started: second state storm",
            ]
        );
    }

    #[test]
    fn continuing_loss_episode_logs_no_more_than_once_per_minute() {
        let (ingress, _wakes) = PublicationIngress::channel();
        let started_at = Instant::now();

        ingress.record_loss_at(1, 1, format_args!("first"), started_at);
        ingress.record_loss_at(
            1,
            2,
            format_args!("before interval"),
            started_at + Duration::from_secs(59),
        );
        ingress.record_loss_at(
            1,
            3,
            format_args!("at interval"),
            started_at + LOSS_EPISODE_LOG_INTERVAL,
        );
        ingress.record_loss_at(
            1,
            4,
            format_args!("before second interval"),
            started_at + Duration::from_secs(119),
        );
        ingress.record_loss_at(
            1,
            5,
            format_args!("at second interval"),
            started_at + Duration::from_secs(120),
        );

        assert_eq!(
            ingress.loss_logs(),
            [
                "cosmix-powerd: publisher loss episode started: first",
                "cosmix-powerd: publisher loss episode continuing: 3 lost over 60.000 s",
                "cosmix-powerd: publisher loss episode continuing: 5 lost over 120.000 s",
            ]
        );
    }

    #[tokio::test]
    async fn production_publisher_skips_epoch_that_changes_at_send_point() {
        let (ingress, wakes) = PublicationIngress::channel();
        ingress.enqueue(publication_for(
            &PowerEvent::OnBatteryChanged {
                old: false,
                new: true,
            },
            1,
            1,
        ));
        let client = Arc::new(ScriptedClient::default());
        let (client_tx, client_rx) = watch::channel(Some(Arc::clone(&client)));
        let (fault_tx, _fault_rx) = mpsc::channel(1);
        let epoch = Arc::new(SequencedEpoch::new([1, 2]));
        let publisher = tokio::spawn(run_publisher(
            ingress.clone(),
            wakes,
            client_rx,
            Arc::clone(&epoch),
            fault_tx,
        ));

        while client.sends.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
        let sent = client.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].1.contains("gap: true"));
        assert!(sent[0].1.contains("\"lost_count\":1"));
        drop(sent);
        assert_eq!(ingress.dropped_updates(), 1);

        drop(client_tx);
        publisher.abort();
    }

    #[tokio::test]
    async fn broker_send_failure_reconnects_with_gap_and_cumulative_counter() {
        let (ingress, wakes) = PublicationIngress::channel();
        ingress.enqueue(publication_for(
            &PowerEvent::DeviceAdded {
                id: "battery_BAT0".into(),
                device: battery(),
            },
            7,
            1,
        ));
        let client = Arc::new(ScriptedClient {
            failures_remaining: AtomicU64::new(1),
            ..ScriptedClient::default()
        });
        let (client_tx, client_rx) = watch::channel(Some(Arc::clone(&client)));
        let (fault_tx, mut fault_rx) = mpsc::channel(1);
        let publisher = tokio::spawn(run_publisher(
            ingress.clone(),
            wakes,
            client_rx,
            Arc::new(AtomicU64::new(1)),
            fault_tx,
        ));

        assert_eq!(fault_rx.recv().await, Some(()));
        assert_eq!(ingress.dropped_updates(), 1);
        client_tx.send(None).unwrap();
        tokio::task::yield_now().await;
        client_tx.send(Some(Arc::clone(&client))).unwrap();
        while client.sends.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }

        let sent = client.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].1.contains("gap: true"));
        assert!(sent[0].1.contains("\"lost_count\":1"));
        drop(sent);
        assert_eq!(ingress.dropped_updates(), 1);

        drop(client_tx);
        publisher.abort();
    }

    #[tokio::test]
    async fn production_publisher_preserves_sequence_without_loss() {
        let (ingress, wakes) = PublicationIngress::channel();
        ingress.enqueue(publication_for(
            &PowerEvent::BatteryChanged {
                id: "battery_BAT0".into(),
                old: None,
                new: Some(battery()),
            },
            1,
            1,
        ));
        ingress.enqueue(publication_for(
            &PowerEvent::BatteryChanged {
                id: "battery_BAT1".into(),
                old: None,
                new: Some(battery()),
            },
            2,
            1,
        ));
        for (seq, id) in [(3, "first"), (4, "second")] {
            ingress.enqueue(publication_for(
                &PowerEvent::DeviceAdded {
                    id: id.into(),
                    device: battery(),
                },
                seq,
                1,
            ));
        }

        let client = Arc::new(ScriptedClient::default());
        let (client_tx, client_rx) = watch::channel(Some(Arc::clone(&client)));
        let (fault_tx, _fault_rx) = mpsc::channel(1);
        let publisher = tokio::spawn(run_publisher(
            ingress,
            wakes,
            client_rx,
            Arc::new(AtomicU64::new(1)),
            fault_tx,
        ));

        while client.sends.load(Ordering::Acquire) < 4 {
            tokio::task::yield_now().await;
        }
        let sent = client.sent.lock().unwrap();
        assert_eq!(sent.len(), 4);
        assert_eq!(sent[0].0, TOPIC_BATTERY_CHANGED);
        assert!(sent[0].1.contains("event_seq: 1"));
        assert_eq!(sent[1].0, TOPIC_BATTERY_CHANGED);
        assert!(sent[1].1.contains("event_seq: 2"));
        assert_eq!(sent[2].0, TOPIC_DEVICE_ADDED);
        assert!(sent[2].1.contains("\"id\":\"first\""));
        assert_eq!(sent[3].0, TOPIC_DEVICE_ADDED);
        assert!(sent[3].1.contains("\"id\":\"second\""));
        drop(sent);

        drop(client_tx);
        publisher.abort();
    }

    #[tokio::test]
    async fn absent_to_present_reattachment_maps_battery_and_source_topics() {
        let state = Arc::new(Mutex::new(CitizenState::default()));
        let (update_tx, update_rx) = mpsc::channel(8);
        let (ingress, _wakes) = PublicationIngress::channel();
        let reducer = tokio::spawn(apply_snapshots(
            update_rx,
            Arc::clone(&state),
            ingress.clone(),
        ));

        update_tx.send(TrackerUpdate::OwnerEpoch(1)).await.unwrap();
        update_tx
            .send(TrackerUpdate::Snapshot {
                owner_epoch: 1,
                snapshot: PowerSnapshot::default(),
            })
            .await
            .unwrap();
        update_tx.send(TrackerUpdate::OwnerEpoch(2)).await.unwrap();
        update_tx
            .send(TrackerUpdate::Snapshot {
                owner_epoch: 2,
                snapshot: present_snapshot(true, 50.0),
            })
            .await
            .unwrap();
        drop(update_tx);
        reducer.await.unwrap().unwrap();

        let reduced = state.lock().expect("power state poisoned").clone();
        assert_eq!(reduced.owner_epoch, 2);
        assert!(reduced.snapshot.present);
        assert!(reduced.snapshot.on_battery);

        let mut by_topic = BTreeMap::new();
        while let Some(publication) = ingress.take_next(2) {
            by_topic.insert(publication.topic, publication.message.body);
        }
        let source: Value =
            serde_json::from_str(by_topic[TOPIC_ON_BATTERY_CHANGED].as_str()).unwrap();
        assert_eq!(source["data"], json!({"old": false, "new": true}));
        let battery: Value =
            serde_json::from_str(by_topic[TOPIC_BATTERY_CHANGED].as_str()).unwrap();
        assert_eq!(battery["data"]["id"], "display");
        assert_eq!(battery["data"]["new"]["percentage"], 50.0);
    }

    #[tokio::test]
    async fn stalled_broker_does_not_block_reducer_and_records_latest_wins_loss() {
        let state = Arc::new(Mutex::new(CitizenState::default()));
        let (update_tx, update_rx) = mpsc::channel(8);
        let (ingress, _wakes) = PublicationIngress::channel();
        let reducer = tokio::spawn(apply_snapshots(
            update_rx,
            Arc::clone(&state),
            ingress.clone(),
        ));

        update_tx.send(TrackerUpdate::OwnerEpoch(1)).await.unwrap();
        for percentage in [40.0, 41.0, 42.0] {
            update_tx
                .send(TrackerUpdate::Snapshot {
                    owner_epoch: 1,
                    snapshot: present_snapshot(true, percentage),
                })
                .await
                .unwrap();
        }
        drop(update_tx);
        reducer.await.unwrap().unwrap();

        assert_eq!(
            state
                .lock()
                .expect("power state poisoned")
                .snapshot
                .display
                .as_ref()
                .and_then(|battery| battery.percentage),
            Some(42.0)
        );
        assert!(ingress.dropped_updates() > 0);
    }
}
