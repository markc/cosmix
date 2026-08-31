//! Bus citizen for the mpris.* namespace, state topics, and delegated controls.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use cosmix_client::{IncomingCommand, NodedClient};
use cosmix_props_core::PropTree;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinSet;

use crate::core::{MediaModel, MprisEvent, PlayerSnapshot, player_key};
use crate::mpris::{
    ControlAction, ControlJob, ControlOwners, ControlTarget, OwnerChange, TrackerUpdate,
    monotonic_us,
};
use crate::props::MprisProps;

pub const BUS_SERVICE: &str = "mpris";
pub const TOPIC_PROPS_CHANGED: &str = "mpris.props.changed";
pub const TOPIC_PLAYER_APPEARED: &str = "mpris.player.appeared";
pub const TOPIC_PLAYER_VANISHED: &str = "mpris.player.vanished";
pub const TOPIC_ACTIVE_CHANGED: &str = "mpris.active.changed";

const BROKER_RECONNECT_DELAY: Duration = Duration::from_secs(60);
const PUBLISH_TIMEOUT: Duration = Duration::from_secs(60);
/// Leaves ten seconds inside NodedClient's 60-second caller-side request
/// deadline for delivery to the daemon and delivery of the final response.
const CONTROL_JOB_BUDGET: Duration = Duration::from_secs(50);
const CONTROL_RESPONSE_GRACE: Duration = Duration::from_secs(2);
const CONTROL_DRAIN_BUDGET: Duration = Duration::from_secs(60);
const SHUTDOWN_INTAKE_SWEEP_CAP: usize = 4096;
const SNAPSHOT_QUEUE_CAPACITY: usize = 64;
pub(crate) const CONTROL_QUEUE_CAPACITY: usize = 64;
const RESPONSE_TASK_CAPACITY: usize = 64;
const REGULAR_RESPONSE_TASK_CAPACITY: usize = RESPONSE_TASK_CAPACITY - 1;
const EVENT_QUEUE_CAPACITY: usize = 64;
const STATE_KEY_CAPACITY: usize = 1024;
const LOSS_EPISODE_LOG_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Default)]
struct CitizenState {
    model: MediaModel,
    event_seq: u64,
    generation: u64,
    publisher_loss: Arc<AtomicU64>,
    controls_dropped: Arc<AtomicU64>,
}

#[derive(Debug)]
struct Publication {
    topic: &'static str,
    class: PublicationClass,
    event_seq: u64,
    generation: u64,
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
    unknown_tail_logged: bool,
}

impl LossEpisodeLimiter {
    fn record_loss(&mut self, count: u64, now: Instant) -> Option<LossEpisodeLog> {
        if count == 0 {
            return None;
        }
        let Some(active) = self.active.as_mut() else {
            self.unknown_tail_logged = false;
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

    fn loss_ended(&mut self, now: Instant) -> Option<LossEpisodeLog> {
        let active = self.active.take()?;
        self.unknown_tail_logged = false;
        Some(LossEpisodeLog::Ended {
            lost_count: active.lost_count,
            elapsed: now.saturating_duration_since(active.started_at),
        })
    }

    fn note_unknown_tail(&mut self) -> bool {
        if self.unknown_tail_logged {
            return false;
        }
        self.unknown_tail_logged = true;
        true
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
    StaleGeneration {
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
            Self::StaleGeneration { discarded } => write!(
                formatter,
                "stale MPRIS generation forced gap and discarded {discarded} queued update(s)"
            ),
            Self::PublisherBacklog { discarded } => write!(
                formatter,
                "publisher loss forced gap and discarded {discarded} queued update(s)"
            ),
        }
    }
}

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
                        let dropped = pending.state.remove(&oldest).unwrap();
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
                        let dropped = pending.events.pop_front().unwrap();
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
                PublicationClass::Gap => unreachable!("gap frames bypass ingress"),
            }
        }
        if let Some((admission, cause)) = diagnostic {
            self.log_loss(admission, format_args!("{cause}"));
        }
        let _ = self.wake.try_send(());
    }

    fn take_next(&self, generation: u64) -> Option<Publication> {
        let (next, diagnostic) = {
            let mut pending = self.pending.lock().expect("publication queue poisoned");
            let stale = pending
                .state
                .values()
                .filter(|publication| publication.generation != generation)
                .count()
                + pending
                    .events
                    .iter()
                    .filter(|publication| publication.generation != generation)
                    .count();
            if pending.loss.lost_count != 0 || stale != 0 {
                let discarded = pending.state.len().saturating_add(pending.events.len());
                let through = pending
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
                            through,
                            Instant::now(),
                        )
                    })
                    .flatten();
                let loss = std::mem::take(&mut pending.loss);
                let diagnostic = if stale != 0 {
                    admission.map(|admission| (admission, LossCause::StaleGeneration { discarded }))
                } else {
                    admission
                        .map(|admission| (admission, LossCause::PublisherBacklog { discarded }))
                };
                (Some(gap_publication(loss, generation)), diagnostic)
            } else {
                let next_state = pending
                    .state
                    .iter()
                    .min_by_key(|(_, publication)| publication.event_seq)
                    .map(|(key, publication)| (key.clone(), publication.event_seq));
                let next_event = pending.events.front().map(|item| item.event_seq);
                let next = match (next_state, next_event) {
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

    fn publication_succeeded_at(&self, now: Instant) {
        let admission = {
            let mut pending = self.pending.lock().expect("publication queue poisoned");
            pending.loss_episode.loss_ended(now)
        };
        if let Some(LossEpisodeLog::Ended {
            lost_count,
            elapsed,
        }) = admission
        {
            self.emit_loss_log(format_args!(
                "cosmix-mprisd: publisher loss episode ended: {lost_count} lost over {:.3} s",
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
                "cosmix-mprisd: publisher loss episode started: {cause}"
            )),
            LossEpisodeLog::Continuing {
                lost_count,
                elapsed,
            } => self.emit_loss_log(format_args!(
                "cosmix-mprisd: publisher loss episode continuing: {lost_count} lost over {:.3} s",
                elapsed.as_secs_f64()
            )),
            LossEpisodeLog::Ended { .. } => unreachable!(),
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
        self.loss_logs.lock().unwrap().clone()
    }
}

pub async fn serve() -> Result<()> {
    let (snapshot_tx, snapshot_rx) = mpsc::channel(SNAPSHOT_QUEUE_CAPACITY);
    let (control_tx, control_rx) = mpsc::channel(CONTROL_QUEUE_CAPACITY);
    let (publications, publication_wakes) = PublicationIngress::channel();
    let state = Arc::new(Mutex::new(CitizenState {
        publisher_loss: Arc::clone(&publications.dropped_updates),
        ..CitizenState::default()
    }));
    let control_owners: ControlOwners = Arc::new(Mutex::new(BTreeMap::new()));
    let generation = Arc::new(AtomicU64::new(0));
    let (publisher_fault_tx, publisher_fault_rx) = mpsc::channel(1);
    let (client_tx, client_rx) = watch::channel::<Option<Arc<NodedClient>>>(None);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let mut tracker = tokio::spawn(crate::mpris::run(snapshot_tx, Arc::clone(&generation)));
    let mut reducer = tokio::spawn(apply_snapshots(
        snapshot_rx,
        Arc::clone(&state),
        publications.clone(),
        Arc::clone(&control_owners),
    ));
    let mut publisher = tokio::spawn(run_publisher(
        publications,
        publication_wakes,
        client_rx,
        Arc::clone(&generation),
        publisher_fault_tx,
    ));
    let mut controls = tokio::spawn(crate::mpris::run_controls(
        control_rx,
        control_owners,
        Arc::clone(&generation),
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
        control_tx,
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
        result = &mut tracker => Exit::Task("MPRIS tracker", result),
        result = &mut reducer => Exit::Task("snapshot reducer", result),
        result = &mut publisher => Exit::Task("publisher", result),
        result = &mut controls => Exit::Task("control worker", result),
        result = &mut broker => Exit::Task("broker loop", result),
    };

    let _ = shutdown_tx.send(true);
    let graceful = matches!(exit, Exit::Shutdown(Ok(())));
    if graceful && !broker.is_finished() {
        match broker.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                eprintln!("cosmix-mprisd: broker shutdown failed; continuing teardown: {error:#}")
            }
            Err(error) => eprintln!(
                "cosmix-mprisd: broker task failed during shutdown; continuing teardown: {error}"
            ),
        }
    } else {
        broker.abort();
    }
    tracker.abort();
    reducer.abort();
    publisher.abort();
    if graceful && !controls.is_finished() {
        match tokio::time::timeout(CONTROL_RESPONSE_GRACE, &mut controls).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(error))) => eprintln!(
                "cosmix-mprisd: control worker failed during shutdown; continuing teardown: {error:#}"
            ),
            Ok(Err(error)) => eprintln!(
                "cosmix-mprisd: control worker task failed during shutdown; continuing teardown: {error}"
            ),
            Err(_) => {
                eprintln!("cosmix-mprisd: control worker did not finish after response drain");
                controls.abort();
            }
        }
    } else {
        controls.abort();
    }

    match exit {
        Exit::Shutdown(result) => result,
        Exit::Task(name, Ok(Ok(()))) => {
            eprintln!("cosmix-mprisd: supervised {name} exited unexpectedly");
            Err(anyhow!("supervised {name} exited unexpectedly"))
        }
        Exit::Task(name, Ok(Err(error))) => {
            eprintln!("cosmix-mprisd: supervised {name} failed: {error:#}");
            Err(error).with_context(|| format!("supervised {name} failed"))
        }
        Exit::Task(name, Err(error)) => {
            eprintln!("cosmix-mprisd: supervised {name} panicked or was cancelled: {error}");
            Err(anyhow!(
                "supervised {name} panicked or was cancelled: {error}"
            ))
        }
    }
}

async fn run_broker(
    state: Arc<Mutex<CitizenState>>,
    controls: mpsc::Sender<ControlJob>,
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
                eprintln!("cosmix-mprisd: registered as '{BUS_SERVICE}'");
                let outcome = serve_bus(
                    Arc::clone(&client),
                    &state,
                    &controls,
                    &mut publisher_fault_rx,
                    shutdown.clone(),
                )
                .await;
                let stopping = matches!(outcome, ServeBusExit::Shutdown);
                if matches!(outcome, ServeBusExit::PublisherFault) {
                    eprintln!("cosmix-mprisd: publisher fault; reconnecting broker client");
                };
                let _ = client_tx.send(None);
                let close_budget = if stopping {
                    CONTROL_RESPONSE_GRACE
                } else {
                    PUBLISH_TIMEOUT
                };
                if tokio::time::timeout(close_budget, client.close())
                    .await
                    .is_err()
                {
                    eprintln!("cosmix-mprisd: broker client close timed out");
                }
                while publisher_fault_rx.try_recv().is_ok() {}
                if stopping {
                    return Ok(());
                }
                eprintln!("cosmix-mprisd: broker disconnected; retrying in 60s");
            }
            Ok(Err(error)) => {
                eprintln!("cosmix-mprisd: broker unavailable; retrying in 60s: {error}");
            }
            Err(_) => {
                eprintln!("cosmix-mprisd: broker connection timed out; retrying in 60s");
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
    control_owners: ControlOwners,
) -> Result<()> {
    while let Some(update) = updates.recv().await {
        let (
            generation,
            scan_revision,
            players,
            cause,
            owner_changes,
            seeked,
            playing,
            adapter_loss,
            complete,
        ) = match update {
            TrackerUpdate::Generation(generation) => {
                let mut state = state.lock().expect("MPRIS state poisoned");
                state.generation = state.generation.max(generation);
                continue;
            }
            TrackerUpdate::ScanStarted {
                generation,
                scan_revision,
                cause,
            } => (
                generation,
                scan_revision,
                None,
                cause,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                0,
                false,
            ),
            TrackerUpdate::PartialSnapshot {
                generation,
                scan_revision,
                players,
                cause,
            } => (
                generation,
                scan_revision,
                Some(players),
                cause,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                0,
                false,
            ),
            TrackerUpdate::Snapshot {
                generation,
                scan_revision,
                players,
                cause,
                owner_changes,
                seeked,
                playing,
                adapter_loss,
            } => (
                generation,
                scan_revision,
                Some(players),
                cause,
                owner_changes,
                seeked,
                playing,
                adapter_loss,
                true,
            ),
        };
        let (batch, targets) = {
            let mut state = state.lock().expect("MPRIS state poisoned");
            if generation < state.generation {
                publications.record_loss(
                    1,
                    state.event_seq,
                    format_args!("stale MPRIS snapshot before reduction"),
                );
                continue;
            }
            state.generation = generation;
            let now_us = monotonic_us();
            let publisher_loss = state.publisher_loss.load(Ordering::Acquire);
            let old_snapshot = state.model.snapshot().clone();
            let old_complete = state.model.complete_snapshot().clone();
            let controls_dropped = state.controls_dropped.load(Ordering::Acquire);
            let old_tree = MprisProps::new(
                &old_snapshot,
                state.event_seq,
                publisher_loss,
                controls_dropped,
                now_us,
            );
            let old_props = old_tree.snapshot();
            let next = if complete {
                state.model.complete_scan(
                    players.expect("complete tracker update has players"),
                    scan_revision,
                    &playing,
                )
            } else if let Some(players) = players {
                state.model.replace_partial_scan(players, scan_revision)
            } else {
                state.model.begin_scan(scan_revision)
            };
            let mut batch = Vec::new();
            let owner_names = owner_changes
                .iter()
                .map(|change| change.name.as_str())
                .collect::<BTreeSet<_>>();
            let raw_active_turnover = if complete {
                old_complete.active.as_ref().and_then(|key| {
                    let old = old_complete.players.get(key)?;
                    let new = next.players.get(key)?;
                    (old.owner_epoch != new.owner_epoch && owner_names.contains(old.name.as_str()))
                        .then(|| {
                            (
                                old.name.clone(),
                                old.owner.clone(),
                                new.owner.clone(),
                                state.model.active_transitions().to_vec(),
                            )
                        })
                })
            } else {
                None
            };
            for change in &owner_changes {
                if !change.old_owner.is_empty() {
                    let player = old_complete.players.values().find(|player| {
                        player.name == change.name && player.owner == change.old_owner
                    });
                    state.event_seq = state.event_seq.saturating_add(1);
                    batch.push(owner_publication(
                        change,
                        false,
                        player,
                        state.event_seq,
                        generation,
                    ));
                    if let Some((name, old_owner, _, transitions)) = &raw_active_turnover
                        && change.name == *name
                        && change.old_owner == *old_owner
                        && let Some(transition) = transitions.first()
                    {
                        state.event_seq = state.event_seq.saturating_add(1);
                        batch.push(publication_for(transition, state.event_seq, generation));
                    }
                }
                if !change.new_owner.is_empty() {
                    let player = next.players.values().find(|player| {
                        player.name == change.name && player.owner == change.new_owner
                    });
                    state.event_seq = state.event_seq.saturating_add(1);
                    batch.push(owner_publication(
                        change,
                        true,
                        player,
                        state.event_seq,
                        generation,
                    ));
                    if let Some((name, _, new_owner, transitions)) = &raw_active_turnover
                        && change.name == *name
                        && change.new_owner == *new_owner
                    {
                        for transition in transitions.iter().skip(1) {
                            state.event_seq = state.event_seq.saturating_add(1);
                            batch.push(publication_for(transition, state.event_seq, generation));
                        }
                    }
                }
            }
            let lifecycle_events = if complete {
                state.model.lifecycle_events(&old_complete)
            } else {
                Vec::new()
            };
            for event in lifecycle_events {
                let covered_by_owner_signal = match &event {
                    MprisEvent::PlayerAppeared { player }
                    | MprisEvent::PlayerVanished { player } => {
                        owner_names.contains(player.name.as_str())
                    }
                    MprisEvent::ActiveChanged { .. } => raw_active_turnover.is_some(),
                };
                if covered_by_owner_signal {
                    continue;
                }
                state.event_seq = state.event_seq.saturating_add(1);
                batch.push(publication_for(&event, state.event_seq, generation));
            }
            let new_tree = MprisProps::new(
                &next,
                state.event_seq,
                publisher_loss,
                controls_dropped,
                now_us,
            );
            let new_props = new_tree.snapshot();
            for (path, old, new) in cosmix_props_core::diff(&old_props, &new_props) {
                let description = new_tree
                    .describe(&path)
                    .or_else(|| old_tree.describe(&path));
                if description.is_none_or(|description| description.transient) {
                    continue;
                }
                state.event_seq = state.event_seq.saturating_add(1);
                let message = cosmix_props_core::publish::build_props_changed_message(
                    &path, &old, &new, cause,
                );
                batch.push(Publication {
                    topic: TOPIC_PROPS_CHANGED,
                    class: PublicationClass::State(path.to_string()),
                    event_seq: state.event_seq,
                    generation,
                    message,
                    gap_loss: None,
                });
            }
            let mut prior_positions = BTreeMap::new();
            for seek in seeked {
                let Some(new_player) = next.players.get(&seek.key) else {
                    continue;
                };
                let old = prior_positions.remove(&seek.key).unwrap_or_else(|| {
                    old_snapshot
                        .players
                        .get(&seek.key)
                        .map(|player| player.computed_position_us(now_us))
                        .unwrap_or(seek.position_us)
                });
                let mut basis = new_player.clone();
                basis.position_us = seek.position_us;
                basis.position_observed_at_us = seek.observed_at_us;
                let new = basis.computed_position_us(now_us);
                prior_positions.insert(seek.key.clone(), new);
                let path = cosmix_props_core::PropPath::new(format!(
                    "players.by_id.{}.computed_position_us",
                    seek.key
                ))
                .expect("encoded player key is a valid property segment");
                let message = cosmix_props_core::publish::build_props_changed_message(
                    &path,
                    &old.into(),
                    &new.into(),
                    "mpris.seeked",
                );
                state.event_seq = state.event_seq.saturating_add(1);
                batch.push(Publication {
                    topic: TOPIC_PROPS_CHANGED,
                    class: PublicationClass::State(path.to_string()),
                    event_seq: state.event_seq,
                    generation,
                    message,
                    gap_loss: None,
                });
            }
            let targets = next
                .players
                .values()
                .map(|player| {
                    (
                        player.name.clone(),
                        ControlTarget {
                            name: player.name.clone(),
                            owner: player.owner.clone(),
                            owner_epoch: player.owner_epoch,
                            generation,
                        },
                    )
                })
                .collect();
            (batch, targets)
        };
        if adapter_loss != 0 {
            let through_seq = state.lock().expect("MPRIS state poisoned").event_seq;
            publications.record_loss(
                adapter_loss,
                through_seq,
                format_args!("MPRIS adapter queue overflow"),
            );
        }
        *control_owners.lock().expect("control owner map poisoned") = targets;
        for publication in batch {
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
    generation: Arc<E>,
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
                break;
            };
            let Some(mut publication) = publications.take_next(generation.current()) else {
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
            if publication.generation != generation.current() {
                publications.record_publication_loss(
                    &publication,
                    format_args!(
                        "stale generation at send point for {} seq {}",
                        publication.topic, publication.event_seq
                    ),
                );
                continue;
            }
            match tokio::time::timeout(PUBLISH_TIMEOUT, client.send_publication(&headers, &wire))
                .await
            {
                Ok(Ok(())) => publications.publication_succeeded_at(Instant::now()),
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

#[derive(Debug, Clone)]
struct ResponseTarget {
    from: String,
    command: String,
    id: Option<String>,
}

impl ResponseTarget {
    fn from_command(command: &IncomingCommand) -> Self {
        Self {
            from: command.from.clone(),
            command: command.command.clone(),
            id: command.id.clone(),
        }
    }
}

enum ResponsePayload {
    Immediate {
        rc: u8,
        body: String,
    },
    Control {
        deadline: tokio::time::Instant,
        result: oneshot::Receiver<
            std::result::Result<crate::mpris::ControlResult, crate::mpris::ControlError>,
        >,
    },
}

struct ResponseWork {
    target: ResponseTarget,
    payload: ResponsePayload,
}

trait ResponseClient: Send + Sync + 'static {
    fn send_response<'a>(
        &'a self,
        target: &'a ResponseTarget,
        rc: u8,
        body: &'a str,
    ) -> impl Future<Output = Result<()>> + Send + 'a;
}

impl ResponseClient for NodedClient {
    async fn send_response<'a>(
        &'a self,
        target: &'a ResponseTarget,
        rc: u8,
        body: &'a str,
    ) -> Result<()> {
        self.respond_parts(
            &target.from,
            &target.command,
            target.id.as_deref(),
            rc,
            body,
        )
        .await
    }
}

struct ResponseSupervisor<C: ResponseClient> {
    client: Arc<C>,
    tasks: JoinSet<bool>,
    overflow_count: u64,
    dropped: Arc<AtomicU64>,
    drop_episode: LossEpisodeLimiter,
    #[cfg(test)]
    drop_logs: Vec<String>,
}

impl<C: ResponseClient> ResponseSupervisor<C> {
    fn new(client: Arc<C>, dropped: Arc<AtomicU64>) -> Self {
        Self {
            client,
            tasks: JoinSet::new(),
            overflow_count: 0,
            dropped,
            drop_episode: LossEpisodeLimiter::default(),
            #[cfg(test)]
            drop_logs: Vec::new(),
        }
    }

    fn has_regular_capacity(&self) -> bool {
        self.tasks.len() < REGULAR_RESPONSE_TASK_CAPACITY
    }

    fn can_receive(&self) -> bool {
        self.tasks.len() < RESPONSE_TASK_CAPACITY
    }

    fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    fn admit(&mut self, work: ResponseWork) {
        assert!(
            self.has_regular_capacity(),
            "regular response task cap exceeded"
        );
        self.admission_succeeded_at(Instant::now());
        self.spawn(work);
    }

    fn spawn(&mut self, work: ResponseWork) {
        assert!(self.can_receive(), "response task cap exceeded");
        let client = Arc::clone(&self.client);
        self.tasks
            .spawn(async move { execute_response(client, work).await });
    }

    async fn join_next(&mut self) -> bool {
        match self.tasks.join_next().await {
            Some(Ok(sent)) => sent,
            Some(Err(error)) => {
                eprintln!("cosmix-mprisd: supervised response task failed: {error}");
                false
            }
            None => true,
        }
    }

    fn respond_overflow(&mut self, target: ResponseTarget) {
        self.overflow_count = self.overflow_count.saturating_add(1);
        if self.overflow_count == 1 {
            eprintln!("cosmix-mprisd: response capacity exhausted; rejecting command as busy");
        }
        let body = busy_body("MPRIS response capacity is full");
        self.admission_succeeded_at(Instant::now());
        self.spawn(ResponseWork {
            target,
            payload: ResponsePayload::Immediate { rc: 5, body },
        });
    }

    fn drop_overflow(&mut self) {
        self.record_drop_at(Instant::now());
    }

    fn report_unknown_intake_tail(&mut self) {
        if self.drop_episode.note_unknown_tail() {
            self.emit_drop_log(format_args!(
                "cosmix-mprisd: shutdown intake sweep bound reached; remaining queued command count is unknown; dropping receiver"
            ));
        }
    }

    fn record_drop_at(&mut self, now: Instant) {
        let _ = self
            .dropped
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |total| {
                Some(total.saturating_add(1))
            });
        let Some(admission) = self.drop_episode.record_loss(1, now) else {
            return;
        };
        match admission {
            LossEpisodeLog::Started => self.emit_drop_log(format_args!(
                "cosmix-mprisd: control intake loss episode started: response capacity exhausted; dropping commands without reply"
            )),
            LossEpisodeLog::Continuing {
                lost_count,
                elapsed,
            } => self.emit_drop_log(format_args!(
                "cosmix-mprisd: control intake loss episode continuing: {lost_count} dropped over {:.3} s",
                elapsed.as_secs_f64()
            )),
            LossEpisodeLog::Ended { .. } => unreachable!(),
        }
    }

    fn admission_succeeded_at(&mut self, now: Instant) {
        let Some(LossEpisodeLog::Ended {
            lost_count,
            elapsed,
        }) = self.drop_episode.loss_ended(now)
        else {
            return;
        };
        self.emit_drop_log(format_args!(
            "cosmix-mprisd: control intake loss episode ended: {lost_count} dropped over {:.3} s",
            elapsed.as_secs_f64()
        ));
    }

    fn emit_drop_log(&mut self, arguments: fmt::Arguments<'_>) {
        #[cfg(not(test))]
        eprintln!("{arguments}");
        #[cfg(test)]
        self.drop_logs.push(arguments.to_string());
    }

    #[cfg(test)]
    async fn drain(&mut self, deadline: tokio::time::Instant) -> bool {
        while !self.tasks.is_empty() {
            if tokio::time::timeout_at(deadline, self.join_next())
                .await
                .is_err()
            {
                self.tasks.abort_all();
                while self.tasks.join_next().await.is_some() {}
                return false;
            }
        }
        true
    }

    async fn drain_dropping_intake(
        &mut self,
        incoming: &mut mpsc::UnboundedReceiver<IncomingCommand>,
        deadline: tokio::time::Instant,
    ) -> bool {
        let deadline_sleep = tokio::time::sleep_until(deadline);
        tokio::pin!(deadline_sleep);
        let mut intake_open = true;
        while !self.tasks.is_empty() {
            tokio::select! {
                biased;
                _ = &mut deadline_sleep => {
                    self.tasks.abort_all();
                    while self.tasks.join_next().await.is_some() {}
                    if !self.sweep_dropping_intake(incoming, deadline) {
                        self.report_unknown_intake_tail();
                    }
                    return false;
                }
                sent = self.join_next() => {
                    let _ = sent;
                }
                command = incoming.recv(), if intake_open => {
                    if command.is_some() {
                        self.drop_overflow();
                    } else {
                        intake_open = false;
                    }
                }
            }
        }
        if !self.sweep_dropping_intake(incoming, deadline) {
            self.report_unknown_intake_tail();
        }
        true
    }

    fn sweep_dropping_intake(
        &mut self,
        incoming: &mut mpsc::UnboundedReceiver<IncomingCommand>,
        deadline: tokio::time::Instant,
    ) -> bool {
        for _ in 0..SHUTDOWN_INTAKE_SWEEP_CAP {
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            match incoming.try_recv() {
                Ok(_) => self.drop_overflow(),
                Err(mpsc::error::TryRecvError::Empty | mpsc::error::TryRecvError::Disconnected) => {
                    return true;
                }
            }
        }
        false
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServeBusExit {
    Disconnected,
    PublisherFault,
    Shutdown,
}

async fn serve_bus(
    client: Arc<NodedClient>,
    state: &Arc<Mutex<CitizenState>>,
    controls: &mpsc::Sender<ControlJob>,
    publisher_faults: &mut mpsc::Receiver<()>,
    shutdown: watch::Receiver<bool>,
) -> ServeBusExit {
    let Some(incoming) = client.incoming_async().await else {
        return ServeBusExit::Disconnected;
    };
    serve_intake(
        client,
        incoming,
        state,
        controls,
        publisher_faults,
        shutdown,
        CONTROL_DRAIN_BUDGET,
    )
    .await
}

async fn serve_intake<C: ResponseClient>(
    client: Arc<C>,
    mut incoming: mpsc::UnboundedReceiver<IncomingCommand>,
    state: &Arc<Mutex<CitizenState>>,
    controls: &mpsc::Sender<ControlJob>,
    publisher_faults: &mut mpsc::Receiver<()>,
    mut shutdown: watch::Receiver<bool>,
    shutdown_drain_budget: Duration,
) -> ServeBusExit {
    let dropped = Arc::clone(&state.lock().expect("MPRIS state poisoned").controls_dropped);
    let mut responses = ResponseSupervisor::new(client, dropped);
    loop {
        if *shutdown.borrow() {
            break;
        }
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            sent = responses.join_next(), if !responses.is_empty() => {
                if !sent {
                    return ServeBusExit::Disconnected;
                }
            }
            fault = publisher_faults.recv() => {
                let exit = if fault.is_some() {
                    ServeBusExit::PublisherFault
                } else {
                    ServeBusExit::Disconnected
                };
                return exit;
            }
            command = incoming.recv() => {
                let Some(command) = command else {
                    return ServeBusExit::Disconnected;
                };
                if !responses.can_receive() {
                    responses.drop_overflow();
                    continue;
                }
                if !responses.has_regular_capacity() {
                    responses.respond_overflow(ResponseTarget::from_command(&command));
                    continue;
                }
                let work = if command.command.starts_with("mpris.player.") {
                    dispatch_control(&command, state, controls)
                } else {
                    let (rc, body) = dispatch_read(&command, state);
                    ResponseWork {
                        target: ResponseTarget::from_command(&command),
                        payload: ResponsePayload::Immediate { rc, body },
                    }
                };
                responses.admit(work);
            }
        }
    }

    let deadline = tokio::time::Instant::now() + shutdown_drain_budget;
    if !responses
        .drain_dropping_intake(&mut incoming, deadline)
        .await
    {
        eprintln!(
            "cosmix-mprisd: response drain reached its {:.3}s shutdown budget",
            shutdown_drain_budget.as_secs_f64()
        );
    }
    drop(incoming);
    ServeBusExit::Shutdown
}

fn dispatch_control(
    command: &IncomingCommand,
    state: &Arc<Mutex<CitizenState>>,
    controls: &mpsc::Sender<ControlJob>,
) -> ResponseWork {
    let response_target = ResponseTarget::from_command(command);
    let args = resolve_args(command).unwrap_or(Value::Null);
    let suffix = command.command.strip_prefix("mpris.player.").unwrap_or("");
    let action = parse_control_action(suffix, &command.command, &args);
    let action = match action {
        Ok(action) => action,
        Err(error) => {
            return ResponseWork {
                target: response_target,
                payload: ResponsePayload::Immediate {
                    rc: 10,
                    body: control_error_body(&error),
                },
            };
        }
    };
    let control_target = {
        let state = state.lock().expect("MPRIS state poisoned");
        select_control_target(&args, &state)
    };
    let control_target = match control_target {
        Ok(Some(target)) => target,
        Ok(None) => {
            return ResponseWork {
                target: response_target,
                payload: ResponsePayload::Immediate {
                    rc: 4,
                    body: json!({
                        "ok": false,
                        "executed_by": Value::Null,
                        "error": {
                            "name": "org.cosmix.Mpris.NoPlayer",
                            "message": "no matching or active MPRIS player",
                        }
                    })
                    .to_string(),
                },
            };
        }
        Err(error) => {
            return ResponseWork {
                target: response_target,
                payload: ResponsePayload::Immediate {
                    rc: 10,
                    body: control_error_body(&error),
                },
            };
        }
    };
    let deadline = tokio::time::Instant::now() + CONTROL_JOB_BUDGET;
    let (reply, result) = oneshot::channel();
    if controls
        .try_send(ControlJob {
            target: control_target,
            action,
            deadline,
            reply,
        })
        .is_err()
    {
        return ResponseWork {
            target: response_target,
            payload: ResponsePayload::Immediate {
                rc: 5,
                body: busy_body("MPRIS control queue is full"),
            },
        };
    }
    ResponseWork {
        target: response_target,
        payload: ResponsePayload::Control { deadline, result },
    }
}

async fn execute_response<C: ResponseClient>(client: Arc<C>, work: ResponseWork) -> bool {
    let (rc, body) = match work.payload {
        ResponsePayload::Immediate { rc, body } => (rc, body),
        ResponsePayload::Control { deadline, result } => {
            match await_control_verdict(deadline, result).await {
                ControlVerdict::Completed(Ok(result)) => (
                    0,
                    json!({
                        "ok": true,
                        "result": result.result,
                        "executed_by": result.executed_by,
                    })
                    .to_string(),
                ),
                ControlVerdict::Completed(Err(error)) => (1, control_error_body(&error)),
                ControlVerdict::WorkerEnded => (
                    1,
                    json!({
                        "ok": false,
                        "executed_by": Value::Null,
                        "error": {
                            "name": "org.cosmix.Mpris.WorkerEnded",
                            "message": "MPRIS control worker ended before replying",
                        }
                    })
                    .to_string(),
                ),
                ControlVerdict::TimedOut => (
                    1,
                    json!({
                        "ok": false,
                        "executed_by": Value::Null,
                        "error": {
                            "name": "org.cosmix.Mpris.Timeout",
                            "message": "MPRIS control response timed out",
                        }
                    })
                    .to_string(),
                ),
            }
        }
    };
    respond(&*client, &work.target, rc, &body).await
}

fn busy_body(message: &str) -> String {
    json!({
        "ok": false,
        "executed_by": Value::Null,
        "error": {
            "name": "org.cosmix.Mpris.Busy",
            "message": message,
        }
    })
    .to_string()
}

enum ControlVerdict {
    Completed(std::result::Result<crate::mpris::ControlResult, crate::mpris::ControlError>),
    WorkerEnded,
    TimedOut,
}

async fn await_control_verdict(
    deadline: tokio::time::Instant,
    result: oneshot::Receiver<
        std::result::Result<crate::mpris::ControlResult, crate::mpris::ControlError>,
    >,
) -> ControlVerdict {
    match tokio::time::timeout_at(control_waiter_deadline(deadline), result).await {
        Ok(Ok(result)) => ControlVerdict::Completed(result),
        Ok(Err(_)) => ControlVerdict::WorkerEnded,
        Err(_) => ControlVerdict::TimedOut,
    }
}

fn control_waiter_deadline(job_deadline: tokio::time::Instant) -> tokio::time::Instant {
    job_deadline + CONTROL_RESPONSE_GRACE
}

fn parse_control_action(
    suffix: &str,
    command: &str,
    args: &Value,
) -> std::result::Result<ControlAction, crate::mpris::ControlError> {
    match suffix {
        "play" => Ok(ControlAction::Play),
        "pause" => Ok(ControlAction::Pause),
        "playpause" => Ok(ControlAction::PlayPause),
        "next" => Ok(ControlAction::Next),
        "previous" => Ok(ControlAction::Previous),
        "stop" => Ok(ControlAction::Stop),
        "seek" => args
            .get("offset_us")
            .and_then(Value::as_i64)
            .map(|offset_us| ControlAction::Seek { offset_us })
            .ok_or_else(|| invalid_args("mpris.player.seek requires integer args.offset_us")),
        "set_volume" => args
            .get("volume")
            .and_then(Value::as_f64)
            .filter(|volume| volume.is_finite() && *volume >= 0.0)
            .map(|volume| ControlAction::SetVolume { volume })
            .ok_or_else(|| {
                invalid_args("mpris.player.set_volume requires finite non-negative args.volume")
            }),
        _ => Err(crate::mpris::ControlError {
            name: "org.freedesktop.DBus.Error.UnknownMethod".into(),
            message: format!("unknown mpris control verb: {command}"),
            executed_by: None,
        }),
    }
}

fn invalid_args(message: &str) -> crate::mpris::ControlError {
    crate::mpris::ControlError {
        name: "org.freedesktop.DBus.Error.InvalidArgs".into(),
        message: message.into(),
        executed_by: None,
    }
}

fn control_error_body(error: &crate::mpris::ControlError) -> String {
    json!({
        "ok": false,
        "executed_by": error.executed_by,
        "error": {"name": error.name, "message": error.message},
    })
    .to_string()
}

fn select_control_target(
    args: &Value,
    state: &CitizenState,
) -> std::result::Result<Option<ControlTarget>, crate::mpris::ControlError> {
    let snapshot = state.model.snapshot();
    let selected = match args.get("player") {
        Some(Value::String(name)) => snapshot
            .players
            .values()
            .find(|player| player.name == *name || player.key == *name),
        Some(_) => {
            return Err(crate::mpris::ControlError {
                name: "org.freedesktop.DBus.Error.InvalidArgs".into(),
                message: "args.player must be a string".into(),
                executed_by: None,
            });
        }
        None => snapshot
            .active
            .as_ref()
            .and_then(|key| snapshot.players.get(key)),
    };
    Ok(selected.map(|player| ControlTarget {
        name: player.name.clone(),
        owner: player.owner.clone(),
        owner_epoch: player.owner_epoch,
        generation: state.generation,
    }))
}

async fn respond<C: ResponseClient>(
    client: &C,
    target: &ResponseTarget,
    rc: u8,
    body: &str,
) -> bool {
    match tokio::time::timeout(PUBLISH_TIMEOUT, client.send_response(target, rc, body)).await {
        Ok(Ok(())) => true,
        Ok(Err(error)) => {
            eprintln!("cosmix-mprisd: Bus response failed; reconnecting: {error}");
            false
        }
        Err(_) => {
            eprintln!("cosmix-mprisd: Bus response timed out; reconnecting");
            false
        }
    }
}

fn dispatch_read(command: &IncomingCommand, state: &Arc<Mutex<CitizenState>>) -> (u8, String) {
    if let Some(suffix) = command.command.strip_prefix("mpris.props.") {
        let state = state.lock().expect("MPRIS state poisoned");
        if suffix == "watch" {
            return (
                0,
                json!({
                    "topic": TOPIC_PROPS_CHANGED,
                    "domain_topics": [
                        TOPIC_PLAYER_APPEARED,
                        TOPIC_PLAYER_VANISHED,
                        TOPIC_ACTIVE_CHANGED,
                    ],
                    "event_seq": state.event_seq,
                    "event_sequence": "daemon_session_monotonic",
                    "loss_signal": "gap_and_lost_count",
                    "bootstrap": "subscribe on this connection, then read mpris.props.get",
                })
                .to_string(),
            );
        }
        let props = MprisProps::new(
            state.model.snapshot(),
            state.event_seq,
            state.publisher_loss.load(Ordering::Acquire),
            state.controls_dropped.load(Ordering::Acquire),
            monotonic_us(),
        );
        let args = resolve_args(command);
        let response = cosmix_props_core::bus::dispatch_props(&props, suffix, args.as_ref(), true);
        return (response.rc.clamp(0, 255) as u8, response.body);
    }
    match command.command.as_str() {
        "mpris.ping" => (
            0,
            json!({"pong": true, "service": BUS_SERVICE, "schema": "mpris.v1"}).to_string(),
        ),
        "mpris.info" => {
            let state = state.lock().expect("MPRIS state poisoned");
            let snapshot = state.model.snapshot();
            let build = cosmix_buildinfo::build_info!();
            let active = snapshot
                .active
                .as_ref()
                .and_then(|key| snapshot.players.get(key))
                .map(|player| player.name.clone());
            (
                0,
                json!({
                    "name": BUS_SERVICE,
                    "schema": "mpris.v1",
                    "props_level": "L2",
                    "binary": build.pkg,
                    "version": build.version,
                    "git_sha": build.git_sha,
                    "git_dirty": build.git_dirty,
                    "build_time": build.build_time,
                    "players": snapshot.players.values().map(|player| player.name.clone()).collect::<Vec<_>>(),
                    "active": active,
                    "event_seq": state.event_seq,
                    "publisher_loss": state.publisher_loss.load(Ordering::Acquire),
                })
                .to_string(),
            )
        }
        _ => (
            10,
            json!({"error": format!("unknown mpris verb: {}", command.command)}).to_string(),
        ),
    }
}

fn publication_for(event: &MprisEvent, event_seq: u64, generation: u64) -> Publication {
    let (topic, class, event_name, data) = match event {
        MprisEvent::PlayerAppeared { player } => (
            TOPIC_PLAYER_APPEARED,
            PublicationClass::Event,
            "player.appeared",
            lifecycle_player_data(player),
        ),
        MprisEvent::PlayerVanished { player } => (
            TOPIC_PLAYER_VANISHED,
            PublicationClass::Event,
            "player.vanished",
            lifecycle_player_data(player),
        ),
        MprisEvent::ActiveChanged { old, new } => (
            TOPIC_ACTIVE_CHANGED,
            PublicationClass::Event,
            "active.changed",
            json!({"old": old, "new": new}),
        ),
    };
    let mut message = cosmix_bus::bus::BusMessage::new();
    message.set("command", topic);
    message.body = json!({
        "event": event_name,
        "event_seq": event_seq,
        "data": data,
    })
    .to_string();
    Publication {
        topic,
        class,
        event_seq,
        generation,
        message,
        gap_loss: None,
    }
}

fn owner_publication(
    change: &OwnerChange,
    appeared: bool,
    player: Option<&PlayerSnapshot>,
    event_seq: u64,
    generation: u64,
) -> Publication {
    let (topic, event, owner) = if appeared {
        (TOPIC_PLAYER_APPEARED, "player.appeared", &change.new_owner)
    } else {
        (TOPIC_PLAYER_VANISHED, "player.vanished", &change.old_owner)
    };
    let mut message = cosmix_bus::bus::BusMessage::new();
    message.set("command", topic);
    message.body = json!({
        "event": event,
        "event_seq": event_seq,
        "data": {
            "name": change.name,
            "key": player_key(&change.name),
            "owner": owner,
            "player": player.map(player_json),
        },
    })
    .to_string();
    Publication {
        topic,
        class: PublicationClass::Event,
        event_seq,
        generation,
        message,
        gap_loss: None,
    }
}

fn gap_publication(loss: PendingLoss, generation: u64) -> Publication {
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
        generation,
        message,
        gap_loss: Some(loss),
    }
}

fn player_json(player: &PlayerSnapshot) -> Value {
    json!({
        "key": player.key,
        "name": player.name,
        "owner": player.owner,
        "identity": player.identity,
        "desktop_entry": player.desktop_entry,
        "owner_epoch": player.owner_epoch,
        "unresponsive": player.unresponsive,
        "stale": player.stale,
        "playback_status": player.playback_status.as_str(),
        "metadata": {
            "title": player.metadata.title,
            "artists": player.metadata.artists,
            "album": player.metadata.album,
            "length_us": player.metadata.length_us,
            "art_url": player.metadata.art_url,
        },
        "computed_position_us": player.computed_position_us(monotonic_us()),
        "volume": player.volume,
        "can_play": player.can_play,
        "can_pause": player.can_pause,
        "can_go_next": player.can_go_next,
        "can_go_previous": player.can_go_previous,
        "can_seek": player.can_seek,
        "can_control": player.can_control,
    })
}

fn lifecycle_player_data(player: &PlayerSnapshot) -> Value {
    json!({
        "name": player.name,
        "key": player.key,
        "owner": player.owner,
        "player": player_json(player),
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
    use crate::core::{PlaybackStatus, PlayerMetadata, player_key};

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

    struct CapturingResponseClient {
        stall: bool,
        stalled: tokio::sync::Notify,
        started: AtomicU64,
        active: AtomicU64,
        max_active: AtomicU64,
        replies: Mutex<Vec<(u8, String)>>,
    }

    impl CapturingResponseClient {
        fn new(stall: bool) -> Self {
            Self {
                stall,
                stalled: tokio::sync::Notify::new(),
                started: AtomicU64::new(0),
                active: AtomicU64::new(0),
                max_active: AtomicU64::new(0),
                replies: Mutex::new(Vec::new()),
            }
        }
    }

    impl ResponseClient for CapturingResponseClient {
        async fn send_response<'a>(
            &'a self,
            _target: &'a ResponseTarget,
            rc: u8,
            body: &'a str,
        ) -> Result<()> {
            self.started.fetch_add(1, Ordering::AcqRel);
            let active = self.active.fetch_add(1, Ordering::AcqRel) + 1;
            self.max_active.fetch_max(active, Ordering::AcqRel);
            if self.stall {
                self.stalled.notified().await;
            }
            self.active.fetch_sub(1, Ordering::AcqRel);
            self.replies.lock().unwrap().push((rc, body.to_string()));
            Ok(())
        }
    }

    struct SequencedEpoch {
        values: Mutex<VecDeque<u64>>,
        last: AtomicU64,
    }

    impl EpochSource for SequencedEpoch {
        fn current(&self) -> u64 {
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

    fn response_target(id: usize) -> ResponseTarget {
        ResponseTarget {
            from: "alpha".into(),
            command: "mpris.player.play".into(),
            id: Some(id.to_string()),
        }
    }

    fn player(name: &str, status: PlaybackStatus, title: &str) -> PlayerSnapshot {
        PlayerSnapshot {
            key: player_key(name),
            name: name.into(),
            owner: ":1.20".into(),
            owner_epoch: 2,
            unresponsive: false,
            stale: false,
            identity: "Alpha".into(),
            desktop_entry: Some("alpha".into()),
            playback_status: status,
            metadata: PlayerMetadata {
                title: Some(title.into()),
                artists: vec!["Artist".into()],
                album: None,
                length_us: Some(5_000_000),
                art_url: None,
            },
            position_us: 0,
            position_observed_at_us: monotonic_us(),
            rate: 1.0,
            volume: 0.5,
            can_play: true,
            can_pause: true,
            can_go_next: true,
            can_go_previous: true,
            can_seek: true,
            can_control: true,
        }
    }

    fn state_with(player: PlayerSnapshot) -> Arc<Mutex<CitizenState>> {
        let mut model = MediaModel::default();
        model.replace_players(BTreeMap::from([(player.key.clone(), player)]));
        Arc::new(Mutex::new(CitizenState {
            model,
            generation: 1,
            ..CitizenState::default()
        }))
    }

    fn assert_gap(publication: &Publication, lost_count: u64) {
        assert_eq!(publication.topic, TOPIC_PROPS_CHANGED);
        assert_eq!(publication.message.get("gap"), Some("true"));
        assert_eq!(publication.message.get("cause"), Some("publisher.loss"));
        let body: Value = serde_json::from_str(&publication.message.body).unwrap();
        assert_eq!(body["lost_count"], lost_count);
        assert_eq!(body["cause"], "publisher.loss");
    }

    #[test]
    fn read_verbs_expose_empty_players_and_active_none() {
        let state = Arc::new(Mutex::new(CitizenState::default()));
        let (rc, info) = dispatch_read(&command("mpris.info"), &state);
        assert_eq!(rc, 0);
        let info: Value = serde_json::from_str(&info).unwrap();
        assert_eq!(info["players"], json!([]));
        assert!(info["active"].is_null());
        let (rc, props) = dispatch_read(&command("mpris.props.get"), &state);
        assert_eq!(rc, 0);
        let props: Value = serde_json::from_str(&props).unwrap();
        assert_eq!(props["players"]["list"], json!([]));
    }

    #[test]
    fn info_and_props_expose_cumulative_publisher_loss() {
        let state = Arc::new(Mutex::new(CitizenState::default()));
        {
            let state = state.lock().unwrap();
            state.publisher_loss.store(4, Ordering::Release);
            state.controls_dropped.store(7, Ordering::Release);
        }
        let (_, info) = dispatch_read(&command("mpris.info"), &state);
        assert_eq!(
            serde_json::from_str::<Value>(&info).unwrap()["publisher_loss"],
            4
        );
        let (_, props) = dispatch_read(&command("mpris.props.get"), &state);
        assert_eq!(
            serde_json::from_str::<Value>(&props).unwrap()["lifecycle"]["publisher_loss"],
            4
        );
        assert_eq!(
            serde_json::from_str::<Value>(&props).unwrap()["controls"]["dropped"],
            7
        );
    }

    #[tokio::test]
    async fn metadata_update_reduces_to_player_keyed_props_changed() {
        let before = player(
            "org.mpris.MediaPlayer2.alpha",
            PlaybackStatus::Playing,
            "Before",
        );
        let mut after = before.clone();
        after.metadata.title = Some("After".into());
        let state = state_with(before);
        let (tx, rx) = mpsc::channel(4);
        let (ingress, _wakes) = PublicationIngress::channel();
        let owners = Arc::new(Mutex::new(BTreeMap::new()));
        let reducer = tokio::spawn(apply_snapshots(
            rx,
            Arc::clone(&state),
            ingress.clone(),
            owners,
        ));
        tx.send(TrackerUpdate::Snapshot {
            generation: 1,
            scan_revision: 2,
            players: BTreeMap::from([(after.key.clone(), after.clone())]),
            cause: "mpris.signal",
            owner_changes: Vec::new(),
            seeked: Vec::new(),
            playing: Vec::new(),
            adapter_loss: 0,
        })
        .await
        .unwrap();
        drop(tx);
        reducer.await.unwrap().unwrap();
        let publication = ingress.take_next(1).unwrap();
        assert_eq!(publication.topic, TOPIC_PROPS_CHANGED);
        assert_eq!(
            publication.class,
            PublicationClass::State(format!("players.by_id.{}.title", after.key))
        );
        assert!(publication.message.body.contains("After"));
    }

    #[tokio::test]
    async fn four_metadata_fields_publish_without_self_coalescing_or_gap() {
        let before = player(
            "org.mpris.MediaPlayer2.alpha",
            PlaybackStatus::Playing,
            "Before",
        );
        let mut after = before.clone();
        after.metadata.title = Some("After".into());
        after.metadata.artists = vec!["One".into(), "Two".into()];
        after.metadata.album = Some("Record".into());
        after.metadata.length_us = Some(9_000_000);
        let state = state_with(before);
        let (tx, rx) = mpsc::channel(1);
        let (ingress, _wakes) = PublicationIngress::channel();
        let owners = Arc::new(Mutex::new(BTreeMap::new()));
        let reducer = tokio::spawn(apply_snapshots(rx, state, ingress.clone(), owners));
        tx.send(TrackerUpdate::Snapshot {
            generation: 1,
            scan_revision: 2,
            players: BTreeMap::from([(after.key.clone(), after.clone())]),
            cause: "mpris.signal",
            owner_changes: Vec::new(),
            seeked: Vec::new(),
            playing: Vec::new(),
            adapter_loss: 0,
        })
        .await
        .unwrap();
        drop(tx);
        reducer.await.unwrap().unwrap();

        let mut paths = BTreeSet::new();
        while let Some(publication) = ingress.take_next(1) {
            assert_ne!(publication.class, PublicationClass::Gap);
            let body: Value = serde_json::from_str(&publication.message.body).unwrap();
            paths.insert(body["path"].as_str().unwrap().to_string());
        }
        let base = format!("players.by_id.{}", after.key);
        assert_eq!(
            paths,
            BTreeSet::from([
                format!("{base}.album"),
                format!("{base}.artists"),
                format!("{base}.length_us"),
                format!("{base}.title"),
                "players.scan_revision".to_string(),
            ])
        );
        assert_eq!(ingress.dropped_updates(), 0);
    }

    #[tokio::test]
    async fn startup_appeared_payload_uses_the_owner_edge_schema() {
        let appeared_player = player(
            "org.mpris.MediaPlayer2.alpha",
            PlaybackStatus::Paused,
            "Song",
        );
        let state = Arc::new(Mutex::new(CitizenState::default()));
        let (tx, rx) = mpsc::channel(1);
        let (ingress, _wakes) = PublicationIngress::channel();
        let owners = Arc::new(Mutex::new(BTreeMap::new()));
        let reducer = tokio::spawn(apply_snapshots(rx, state, ingress.clone(), owners));
        tx.send(TrackerUpdate::Snapshot {
            generation: 1,
            scan_revision: 1,
            players: BTreeMap::from([(appeared_player.key.clone(), appeared_player.clone())]),
            cause: "mpris.resync",
            owner_changes: Vec::new(),
            seeked: Vec::new(),
            playing: Vec::new(),
            adapter_loss: 0,
        })
        .await
        .unwrap();
        drop(tx);
        reducer.await.unwrap().unwrap();
        let publication = ingress.take_next(1).unwrap();
        let body: Value = serde_json::from_str(&publication.message.body).unwrap();
        assert_eq!(body["data"]["name"], appeared_player.name);
        assert_eq!(body["data"]["key"], appeared_player.key);
        assert_eq!(body["data"]["owner"], appeared_player.owner);
        assert_eq!(body["data"]["player"]["owner"], appeared_player.owner);
    }

    #[tokio::test]
    async fn partial_scan_publishes_properties_without_premature_lifecycle_or_active_change() {
        let partial_player = player(
            "org.mpris.MediaPlayer2.beta",
            PlaybackStatus::Playing,
            "Song",
        );
        let state = Arc::new(Mutex::new(CitizenState::default()));
        let (tx, rx) = mpsc::channel(1);
        let (ingress, _wakes) = PublicationIngress::channel();
        let owners = Arc::new(Mutex::new(BTreeMap::new()));
        let reducer = tokio::spawn(apply_snapshots(
            rx,
            Arc::clone(&state),
            ingress.clone(),
            owners,
        ));
        tx.send(TrackerUpdate::PartialSnapshot {
            generation: 1,
            scan_revision: 1,
            players: BTreeMap::from([(partial_player.key.clone(), partial_player)]),
            cause: "mpris.resync",
        })
        .await
        .unwrap();
        drop(tx);
        reducer.await.unwrap().unwrap();

        let snapshot = state.lock().unwrap().model.snapshot().clone();
        assert_eq!(snapshot.active, None);
        assert_eq!(snapshot.scan_revision, 1);
        assert!(!snapshot.scan_complete);
        while let Some(publication) = ingress.take_next(1) {
            assert_ne!(publication.class, PublicationClass::Event);
            assert_ne!(publication.topic, TOPIC_ACTIVE_CHANGED);
        }
    }

    #[tokio::test]
    async fn multi_player_startup_emits_each_appeared_once_after_complete_scan() {
        let alpha = player(
            "org.mpris.MediaPlayer2.alpha",
            PlaybackStatus::Paused,
            "Alpha",
        );
        let beta = player(
            "org.mpris.MediaPlayer2.beta",
            PlaybackStatus::Paused,
            "Beta",
        );
        let state = Arc::new(Mutex::new(CitizenState::default()));
        let (tx, rx) = mpsc::channel(4);
        let (ingress, _wakes) = PublicationIngress::channel();
        let owners = Arc::new(Mutex::new(BTreeMap::new()));
        let reducer = tokio::spawn(apply_snapshots(
            rx,
            Arc::clone(&state),
            ingress.clone(),
            owners,
        ));

        tx.send(TrackerUpdate::PartialSnapshot {
            generation: 1,
            scan_revision: 1,
            players: BTreeMap::from([(alpha.key.clone(), alpha.clone())]),
            cause: "mpris.resync",
        })
        .await
        .unwrap();
        while state.lock().unwrap().model.snapshot().players.len() != 1 {
            tokio::task::yield_now().await;
        }
        while let Some(publication) = ingress.take_next(1) {
            assert_ne!(publication.topic, TOPIC_PLAYER_APPEARED);
        }

        tx.send(TrackerUpdate::PartialSnapshot {
            generation: 1,
            scan_revision: 1,
            players: BTreeMap::from([
                (alpha.key.clone(), alpha.clone()),
                (beta.key.clone(), beta.clone()),
            ]),
            cause: "mpris.resync",
        })
        .await
        .unwrap();
        while state.lock().unwrap().model.snapshot().players.len() != 2 {
            tokio::task::yield_now().await;
        }
        while let Some(publication) = ingress.take_next(1) {
            assert_ne!(publication.topic, TOPIC_PLAYER_APPEARED);
        }

        tx.send(TrackerUpdate::Snapshot {
            generation: 1,
            scan_revision: 1,
            players: BTreeMap::from([(alpha.key.clone(), alpha), (beta.key.clone(), beta)]),
            cause: "mpris.resync",
            owner_changes: Vec::new(),
            seeked: Vec::new(),
            playing: Vec::new(),
            adapter_loss: 0,
        })
        .await
        .unwrap();
        drop(tx);
        reducer.await.unwrap().unwrap();

        let mut appeared = Vec::new();
        while let Some(publication) = ingress.take_next(1) {
            if publication.topic == TOPIC_PLAYER_APPEARED {
                let body: Value = serde_json::from_str(&publication.message.body).unwrap();
                appeared.push(body["data"]["name"].as_str().unwrap().to_string());
            }
        }
        appeared.sort();
        assert_eq!(
            appeared,
            [
                "org.mpris.MediaPlayer2.alpha".to_string(),
                "org.mpris.MediaPlayer2.beta".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn active_vanish_mid_scan_publishes_old_to_fallback_edge() {
        let alpha = player(
            "org.mpris.MediaPlayer2.alpha",
            PlaybackStatus::Paused,
            "Alpha",
        );
        let beta = player(
            "org.mpris.MediaPlayer2.beta",
            PlaybackStatus::Paused,
            "Beta",
        );
        let mut model = MediaModel::default();
        model.replace_players(BTreeMap::from([
            (alpha.key.clone(), alpha.clone()),
            (beta.key.clone(), beta.clone()),
        ]));
        let state = Arc::new(Mutex::new(CitizenState {
            model,
            generation: 1,
            ..CitizenState::default()
        }));
        let (tx, rx) = mpsc::channel(2);
        let (ingress, _wakes) = PublicationIngress::channel();
        let owners = Arc::new(Mutex::new(BTreeMap::new()));
        let reducer = tokio::spawn(apply_snapshots(
            rx,
            Arc::clone(&state),
            ingress.clone(),
            owners,
        ));

        tx.send(TrackerUpdate::PartialSnapshot {
            generation: 1,
            scan_revision: 2,
            players: BTreeMap::from([(beta.key.clone(), beta.clone())]),
            cause: "mpris.signal",
        })
        .await
        .unwrap();
        while state.lock().unwrap().model.snapshot().scan_complete {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            state.lock().unwrap().model.snapshot().active,
            Some(alpha.key.clone())
        );
        while ingress.take_next(1).is_some() {}

        tx.send(TrackerUpdate::Snapshot {
            generation: 1,
            scan_revision: 2,
            players: BTreeMap::from([(beta.key.clone(), beta.clone())]),
            cause: "mpris.signal",
            owner_changes: Vec::new(),
            seeked: Vec::new(),
            playing: Vec::new(),
            adapter_loss: 0,
        })
        .await
        .unwrap();
        drop(tx);
        reducer.await.unwrap().unwrap();

        let mut edge = None;
        while let Some(publication) = ingress.take_next(1) {
            if publication.topic == TOPIC_ACTIVE_CHANGED {
                let body: Value = serde_json::from_str(&publication.message.body).unwrap();
                edge = Some((
                    body["data"]["old"].as_str().unwrap().to_string(),
                    body["data"]["new"].as_str().unwrap().to_string(),
                ));
            }
        }
        assert_eq!(edge, Some((alpha.key, beta.key)));
    }

    #[tokio::test]
    async fn active_owner_turnover_publishes_fallback_before_playing_replacement() {
        let mut alpha = player(
            "org.mpris.MediaPlayer2.alpha",
            PlaybackStatus::Paused,
            "Alpha",
        );
        let mut beta = player(
            "org.mpris.MediaPlayer2.beta",
            PlaybackStatus::Paused,
            "Beta",
        );
        beta.owner = ":1.21".into();
        beta.owner_epoch = 3;
        let mut model = MediaModel::default();
        model.replace_players(BTreeMap::from([
            (alpha.key.clone(), alpha.clone()),
            (beta.key.clone(), beta.clone()),
        ]));
        beta.playback_status = PlaybackStatus::Playing;
        model.replace_players(BTreeMap::from([
            (alpha.key.clone(), alpha.clone()),
            (beta.key.clone(), beta.clone()),
        ]));
        beta.playback_status = PlaybackStatus::Paused;
        alpha.playback_status = PlaybackStatus::Playing;
        model.replace_players(BTreeMap::from([
            (alpha.key.clone(), alpha.clone()),
            (beta.key.clone(), beta.clone()),
        ]));
        let old_owner = alpha.owner.clone();
        let state = Arc::new(Mutex::new(CitizenState {
            model,
            generation: 1,
            ..CitizenState::default()
        }));
        alpha.owner = ":1.99".into();
        alpha.owner_epoch += 1;

        let (tx, rx) = mpsc::channel(1);
        let (ingress, _wakes) = PublicationIngress::channel();
        let owners = Arc::new(Mutex::new(BTreeMap::new()));
        let reducer = tokio::spawn(apply_snapshots(rx, state, ingress.clone(), owners));
        tx.send(TrackerUpdate::Snapshot {
            generation: 1,
            scan_revision: 4,
            players: BTreeMap::from([
                (alpha.key.clone(), alpha.clone()),
                (beta.key.clone(), beta.clone()),
            ]),
            cause: "mpris.signal",
            owner_changes: vec![OwnerChange {
                name: alpha.name.clone(),
                old_owner,
                new_owner: alpha.owner.clone(),
            }],
            seeked: Vec::new(),
            playing: Vec::new(),
            adapter_loss: 0,
        })
        .await
        .unwrap();
        drop(tx);
        reducer.await.unwrap().unwrap();

        let mut lifecycle = Vec::new();
        while let Some(publication) = ingress.take_next(1) {
            if matches!(
                publication.topic,
                TOPIC_PLAYER_VANISHED | TOPIC_PLAYER_APPEARED | TOPIC_ACTIVE_CHANGED
            ) {
                lifecycle.push((publication.topic, publication.message.body));
            }
        }
        assert_eq!(
            lifecycle
                .iter()
                .map(|(topic, _)| *topic)
                .collect::<Vec<_>>(),
            [
                TOPIC_PLAYER_VANISHED,
                TOPIC_ACTIVE_CHANGED,
                TOPIC_PLAYER_APPEARED,
                TOPIC_ACTIVE_CHANGED,
            ]
        );
        let fallback: Value = serde_json::from_str(&lifecycle[1].1).unwrap();
        let replacement: Value = serde_json::from_str(&lifecycle[3].1).unwrap();
        assert_eq!(fallback["data"]["old"], alpha.key);
        assert_eq!(fallback["data"]["new"], beta.key);
        assert_eq!(replacement["data"]["old"], beta.key);
        assert_eq!(replacement["data"]["new"], alpha.key);
        assert_eq!(ingress.dropped_updates(), 0);
    }

    #[test]
    fn malformed_player_selector_is_invalid_args_not_active_fallback() {
        let state = state_with(player(
            "org.mpris.MediaPlayer2.alpha",
            PlaybackStatus::Playing,
            "Song",
        ));
        let state = state.lock().unwrap();
        let error = select_control_target(&json!({"player": 123}), &state).unwrap_err();
        assert_eq!(error.name, "org.freedesktop.DBus.Error.InvalidArgs");
    }

    #[test]
    fn malformed_seek_and_volume_use_structured_invalid_args() {
        for (suffix, command, args) in [
            ("seek", "mpris.player.seek", json!({"offset_us": 1.5})),
            (
                "set_volume",
                "mpris.player.set_volume",
                json!({"volume": -0.1}),
            ),
        ] {
            let error = parse_control_action(suffix, command, &args).unwrap_err();
            assert_eq!(error.name, "org.freedesktop.DBus.Error.InvalidArgs");
            let body: Value = serde_json::from_str(&control_error_body(&error)).unwrap();
            assert_eq!(body["ok"], false);
            assert_eq!(
                body["error"]["name"],
                "org.freedesktop.DBus.Error.InvalidArgs"
            );
        }
    }

    #[tokio::test]
    async fn queued_expiry_verdict_wins_during_response_grace() {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(10);
        let (reply, result) = oneshot::channel();
        let expiry = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let _ = reply.send(Err(crate::mpris::ControlError {
                name: "org.cosmix.Mpris.Expired".into(),
                message: "MPRIS control deadline passed before dispatch".into(),
                executed_by: None,
            }));
        });
        let ControlVerdict::Completed(Err(error)) = await_control_verdict(deadline, result).await
        else {
            panic!("worker expiry verdict must beat the response waiter");
        };
        assert_eq!(error.name, "org.cosmix.Mpris.Expired");
        expiry.await.unwrap();
    }

    #[tokio::test]
    async fn invalid_control_flood_never_exceeds_response_task_cap() {
        let client = Arc::new(CapturingResponseClient::new(true));
        let dropped = Arc::new(AtomicU64::new(0));
        let mut responses = ResponseSupervisor::new(client, Arc::clone(&dropped));
        for id in 0..1000 {
            if responses.has_regular_capacity() {
                responses.admit(ResponseWork {
                    target: response_target(id),
                    payload: ResponsePayload::Immediate {
                        rc: 10,
                        body: control_error_body(&invalid_args("invalid control")),
                    },
                });
            } else if responses.can_receive() {
                responses.respond_overflow(response_target(id));
            } else {
                responses.drop_overflow();
            }
        }

        assert_eq!(responses.tasks.len(), RESPONSE_TASK_CAPACITY);
        assert_eq!(responses.overflow_count, 1);
        assert_eq!(dropped.load(Ordering::Acquire), 936);
    }

    #[tokio::test]
    async fn command_flood_drains_unbounded_intake_and_recovers_after_slot_frees() {
        let client = Arc::new(CapturingResponseClient::new(true));
        let state = Arc::new(Mutex::new(CitizenState::default()));
        let dropped = Arc::clone(&state.lock().unwrap().controls_dropped);
        let (incoming_tx, incoming_rx) = mpsc::unbounded_channel();
        let (control_tx, _control_rx) = mpsc::channel(1);
        let (_fault_tx, mut fault_rx) = mpsc::channel(1);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let broker_client = Arc::clone(&client);
        let broker_state = Arc::clone(&state);
        let broker = tokio::spawn(async move {
            serve_intake(
                broker_client,
                incoming_rx,
                &broker_state,
                &control_tx,
                &mut fault_rx,
                shutdown_rx,
                CONTROL_DRAIN_BUDGET,
            )
            .await
        });

        for id in 0..1000 {
            let mut incoming = command("mpris.ping");
            incoming.id = Some(id.to_string());
            incoming_tx.send(incoming).unwrap();
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            while client.started.load(Ordering::Acquire) != RESPONSE_TASK_CAPACITY as u64
                || dropped.load(Ordering::Acquire) != 936
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("broker must drain the unbounded intake while responses are stalled");
        assert_eq!(client.active.load(Ordering::Acquire), 64);
        assert_eq!(client.max_active.load(Ordering::Acquire), 64);

        client.stalled.notify_one();
        tokio::time::timeout(Duration::from_secs(2), async {
            while client.active.load(Ordering::Acquire) != 63 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("one stalled response must finish");

        let mut recovered = command("mpris.ping");
        recovered.id = Some("recovered".into());
        incoming_tx.send(recovered).unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while client.started.load(Ordering::Acquire) != 65 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("intake must process a new command after a response slot frees");
        assert_eq!(dropped.load(Ordering::Acquire), 936);
        assert_eq!(client.active.load(Ordering::Acquire), 64);
        assert_eq!(client.max_active.load(Ordering::Acquire), 64);

        client.stalled.notify_waiters();
        tokio::time::timeout(Duration::from_secs(2), async {
            while client.active.load(Ordering::Acquire) != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("all stalled responses must be releasable");
        shutdown_tx.send(true).unwrap();
        let exit = broker.await.unwrap();
        assert_eq!(exit, ServeBusExit::Shutdown);
        assert!(incoming_tx.send(command("mpris.ping")).is_err());
    }

    #[test]
    fn control_drop_limiter_is_episode_bounded_and_counter_saturates() {
        let start = Instant::now();
        let dropped = Arc::new(AtomicU64::new(0));
        let client = Arc::new(CapturingResponseClient::new(false));
        let mut responses = ResponseSupervisor::new(client, Arc::clone(&dropped));
        responses.record_drop_at(start);
        responses.record_drop_at(start + Duration::from_secs(59));
        responses.record_drop_at(start + Duration::from_secs(60));
        responses.admission_succeeded_at(start + Duration::from_secs(61));

        assert_eq!(dropped.load(Ordering::Acquire), 3);
        assert_eq!(
            responses.drop_logs,
            [
                "cosmix-mprisd: control intake loss episode started: response capacity exhausted; dropping commands without reply",
                "cosmix-mprisd: control intake loss episode continuing: 3 dropped over 60.000 s",
                "cosmix-mprisd: control intake loss episode ended: 3 dropped over 61.000 s",
            ]
        );

        let saturated = Arc::new(AtomicU64::new(u64::MAX));
        let client = Arc::new(CapturingResponseClient::new(false));
        let mut responses = ResponseSupervisor::new(client, Arc::clone(&saturated));
        responses.record_drop_at(start);
        assert_eq!(saturated.load(Ordering::Acquire), u64::MAX);
    }

    #[tokio::test]
    async fn shutdown_final_sweep_caps_batch_and_logs_unknown_tail_once() {
        let dropped = Arc::new(AtomicU64::new(0));
        let client = Arc::new(CapturingResponseClient::new(false));
        let mut responses = ResponseSupervisor::new(client, Arc::clone(&dropped));
        let (incoming_tx, mut incoming_rx) = mpsc::unbounded_channel();
        for _ in 0..=SHUTDOWN_INTAKE_SWEEP_CAP {
            incoming_tx.send(command("mpris.ping")).unwrap();
        }

        assert!(
            responses
                .drain_dropping_intake(
                    &mut incoming_rx,
                    tokio::time::Instant::now() + Duration::from_secs(1),
                )
                .await
        );
        responses.report_unknown_intake_tail();
        assert_eq!(
            dropped.load(Ordering::Acquire),
            SHUTDOWN_INTAKE_SWEEP_CAP as u64
        );
        assert_eq!(incoming_rx.len(), 1);
        assert_eq!(
            responses.drop_logs,
            [
                "cosmix-mprisd: control intake loss episode started: response capacity exhausted; dropping commands without reply",
                "cosmix-mprisd: shutdown intake sweep bound reached; remaining queued command count is unknown; dropping receiver",
            ]
        );
    }

    #[tokio::test]
    async fn shutdown_drain_replies_to_every_admitted_control() {
        const JOBS: usize = 12;
        let client = Arc::new(CapturingResponseClient::new(false));
        let mut responses =
            ResponseSupervisor::new(Arc::clone(&client), Arc::new(AtomicU64::new(0)));
        let mut verdicts = Vec::new();
        for id in 0..JOBS {
            let (reply, result) = oneshot::channel();
            responses.admit(ResponseWork {
                target: response_target(id),
                payload: ResponsePayload::Control {
                    deadline: tokio::time::Instant::now() + Duration::from_secs(1),
                    result,
                },
            });
            verdicts.push((id, reply));
        }
        for (id, reply) in verdicts {
            let verdict = if id % 2 == 0 {
                Ok(crate::mpris::ControlResult {
                    result: None,
                    executed_by: ":1.20".into(),
                })
            } else {
                Err(crate::mpris::ControlError {
                    name: "org.cosmix.Mpris.Expired".into(),
                    message: "MPRIS control deadline passed before dispatch".into(),
                    executed_by: None,
                })
            };
            reply.send(verdict).unwrap();
        }

        assert!(
            responses
                .drain(tokio::time::Instant::now() + Duration::from_secs(1))
                .await
        );
        let replies = client.replies.lock().unwrap();
        assert_eq!(replies.len(), JOBS);
        assert_eq!(replies.iter().filter(|(rc, _)| *rc == 0).count(), JOBS / 2);
        assert_eq!(
            replies
                .iter()
                .filter(|(_, body)| body.contains("org.cosmix.Mpris.Expired"))
                .count(),
            JOBS / 2
        );
    }

    #[tokio::test]
    async fn shutdown_drain_drops_continuing_flood_and_delivers_admitted_reply() {
        const FLOOD: usize = 1000;
        let client = Arc::new(CapturingResponseClient::new(true));
        let state = Arc::new(Mutex::new(CitizenState::default()));
        let dropped = Arc::clone(&state.lock().unwrap().controls_dropped);
        let (incoming_tx, incoming_rx) = mpsc::unbounded_channel();
        let (control_tx, _control_rx) = mpsc::channel(1);
        let (_fault_tx, mut fault_rx) = mpsc::channel(1);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let broker_client = Arc::clone(&client);
        let broker_state = Arc::clone(&state);
        let broker = tokio::spawn(async move {
            serve_intake(
                broker_client,
                incoming_rx,
                &broker_state,
                &control_tx,
                &mut fault_rx,
                shutdown_rx,
                CONTROL_DRAIN_BUDGET,
            )
            .await
        });

        incoming_tx.send(command("mpris.ping")).unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while client.started.load(Ordering::Acquire) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the pre-shutdown command must be admitted");

        shutdown_tx.send(true).unwrap();
        for id in 0..FLOOD {
            let mut incoming = command("mpris.ping");
            incoming.id = Some(format!("shutdown-{id}"));
            incoming_tx.send(incoming).unwrap();
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            while dropped.load(Ordering::Acquire) != FLOOD as u64 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("shutdown drain must keep consuming and counting the flood");

        client.stalled.notify_one();
        let exit = tokio::time::timeout(Duration::from_secs(2), broker)
            .await
            .expect("broker shutdown must finish after the admitted reply")
            .unwrap();
        assert_eq!(exit, ServeBusExit::Shutdown);
        assert!(incoming_tx.send(command("mpris.ping")).is_err());
        assert_eq!(dropped.load(Ordering::Acquire), FLOOD as u64);
        assert_eq!(client.started.load(Ordering::Acquire), 1);
        assert_eq!(client.replies.lock().unwrap().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_deadline_drops_receiver_under_continuous_refill() {
        const TEST_DRAIN_BUDGET: Duration = Duration::from_millis(10);
        let client = Arc::new(CapturingResponseClient::new(true));
        let state = Arc::new(Mutex::new(CitizenState::default()));
        let (incoming_tx, incoming_rx) = mpsc::unbounded_channel();
        let (control_tx, _control_rx) = mpsc::channel(1);
        let (_fault_tx, mut fault_rx) = mpsc::channel(1);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let broker_client = Arc::clone(&client);
        let broker_state = Arc::clone(&state);
        let broker = tokio::spawn(async move {
            serve_intake(
                broker_client,
                incoming_rx,
                &broker_state,
                &control_tx,
                &mut fault_rx,
                shutdown_rx,
                TEST_DRAIN_BUDGET,
            )
            .await
        });

        incoming_tx.send(command("mpris.ping")).unwrap();
        while client.started.load(Ordering::Acquire) != 1 {
            tokio::task::yield_now().await;
        }
        let drain_started = tokio::time::Instant::now();
        shutdown_tx.send(true).unwrap();
        tokio::task::yield_now().await;

        let produced = Arc::new(AtomicU64::new(0));
        let producer_count = Arc::clone(&produced);
        let producer = tokio::task::spawn_blocking(move || {
            let fallback = std::time::Instant::now() + Duration::from_secs(1);
            let mut sent = 0_u64;
            while std::time::Instant::now() < fallback {
                if incoming_tx.send(command("mpris.ping")).is_err() {
                    return true;
                }
                sent = sent.saturating_add(1);
                producer_count.store(sent, Ordering::Release);
                if sent.is_multiple_of(256) {
                    std::thread::yield_now();
                }
            }
            false
        });
        while produced.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }

        tokio::time::advance(TEST_DRAIN_BUDGET).await;
        assert_eq!(broker.await.unwrap(), ServeBusExit::Shutdown);
        assert!(
            tokio::time::Instant::now().saturating_duration_since(drain_started)
                <= TEST_DRAIN_BUDGET
        );
        assert!(
            producer.await.unwrap(),
            "producer must observe the bounded drain dropping its receiver"
        );
    }

    #[test]
    fn control_waiter_deadline_stays_inside_bus_client_outer_bound() {
        let admitted = tokio::time::Instant::now();
        let job_deadline = admitted + CONTROL_JOB_BUDGET;
        let waiter_deadline = control_waiter_deadline(job_deadline);
        assert_eq!(waiter_deadline - admitted, Duration::from_secs(52));
        assert!(waiter_deadline < admitted + Duration::from_secs(60));
    }

    #[tokio::test]
    async fn transient_owner_edges_reduce_to_ordered_fifo_events() {
        let name = "org.mpris.MediaPlayer2.transient";
        let state = Arc::new(Mutex::new(CitizenState::default()));
        let (tx, rx) = mpsc::channel(2);
        let (ingress, _wakes) = PublicationIngress::channel();
        let owners = Arc::new(Mutex::new(BTreeMap::new()));
        let reducer = tokio::spawn(apply_snapshots(rx, state, ingress.clone(), owners));
        tx.send(TrackerUpdate::Snapshot {
            generation: 1,
            scan_revision: 1,
            players: BTreeMap::new(),
            cause: "mpris.signal",
            owner_changes: vec![
                OwnerChange {
                    name: name.into(),
                    old_owner: String::new(),
                    new_owner: ":1.30".into(),
                },
                OwnerChange {
                    name: name.into(),
                    old_owner: ":1.30".into(),
                    new_owner: String::new(),
                },
            ],
            seeked: Vec::new(),
            playing: Vec::new(),
            adapter_loss: 0,
        })
        .await
        .unwrap();
        drop(tx);
        reducer.await.unwrap().unwrap();
        let appeared = ingress.take_next(1).unwrap();
        let vanished = ingress.take_next(1).unwrap();
        assert_eq!(appeared.topic, TOPIC_PLAYER_APPEARED);
        assert_eq!(vanished.topic, TOPIC_PLAYER_VANISHED);
        assert!(appeared.message.body.contains(name));
        assert!(vanished.message.body.contains(name));
    }

    #[tokio::test]
    async fn adapter_overflow_is_reported_with_the_publisher_gap_contract() {
        let (ingress, _wakes) = PublicationIngress::channel();
        let state = Arc::new(Mutex::new(CitizenState {
            publisher_loss: Arc::clone(&ingress.dropped_updates),
            ..CitizenState::default()
        }));
        let (tx, rx) = mpsc::channel(2);
        let owners = Arc::new(Mutex::new(BTreeMap::new()));
        let reducer = tokio::spawn(apply_snapshots(rx, state, ingress.clone(), owners));
        tx.send(TrackerUpdate::Snapshot {
            generation: 1,
            scan_revision: 1,
            players: BTreeMap::new(),
            cause: "mpris.signal",
            owner_changes: Vec::new(),
            seeked: Vec::new(),
            playing: Vec::new(),
            adapter_loss: 3,
        })
        .await
        .unwrap();
        drop(tx);
        reducer.await.unwrap().unwrap();
        // The adapter's three lost observations plus the scan-revision state
        // frame discarded when the pending gap supersedes the whole backlog.
        assert_gap(&ingress.take_next(1).unwrap(), 4);
        assert_eq!(ingress.dropped_updates(), 4);
    }

    #[test]
    fn latest_wins_and_fifo_overflow_use_gap_convention() {
        let (ingress, _wakes) = PublicationIngress::channel();
        for seq in 1..=2 {
            ingress.enqueue(Publication {
                topic: TOPIC_ACTIVE_CHANGED,
                class: PublicationClass::State("active".into()),
                event_seq: seq,
                generation: 1,
                message: cosmix_bus::bus::BusMessage::new(),
                gap_loss: None,
            });
        }
        assert_eq!(ingress.dropped_updates(), 1);
        assert_gap(&ingress.take_next(1).unwrap(), 2);

        let (ingress, _wakes) = PublicationIngress::channel();
        for seq in 1..=(EVENT_QUEUE_CAPACITY as u64 + 1) {
            ingress.enqueue(Publication {
                topic: TOPIC_PLAYER_APPEARED,
                class: PublicationClass::Event,
                event_seq: seq,
                generation: 1,
                message: cosmix_bus::bus::BusMessage::new(),
                gap_loss: None,
            });
        }
        assert_eq!(ingress.pending_counts(), (0, EVENT_QUEUE_CAPACITY));
        assert_gap(
            &ingress.take_next(1).unwrap(),
            EVENT_QUEUE_CAPACITY as u64 + 1,
        );
    }

    #[test]
    fn loss_limiter_logs_once_then_at_most_each_minute() {
        let (ingress, _wakes) = PublicationIngress::channel();
        let start = Instant::now();
        ingress.record_loss_at(1, 1, format_args!("first"), start);
        ingress.record_loss_at(1, 2, format_args!("early"), start + Duration::from_secs(59));
        ingress.record_loss_at(
            1,
            3,
            format_args!("minute"),
            start + Duration::from_secs(60),
        );
        assert_eq!(
            ingress.loss_logs(),
            [
                "cosmix-mprisd: publisher loss episode started: first",
                "cosmix-mprisd: publisher loss episode continuing: 3 lost over 60.000 s",
            ]
        );
        ingress.publication_succeeded_at(start + Duration::from_secs(61));
        assert_eq!(
            ingress.loss_logs().last().unwrap(),
            "cosmix-mprisd: publisher loss episode ended: 3 lost over 61.000 s"
        );
    }

    #[tokio::test]
    async fn publisher_checks_generation_at_single_send_await() {
        let (ingress, wakes) = PublicationIngress::channel();
        ingress.enqueue(Publication {
            topic: TOPIC_ACTIVE_CHANGED,
            class: PublicationClass::State("active".into()),
            event_seq: 1,
            generation: 1,
            message: cosmix_bus::bus::BusMessage::new(),
            gap_loss: None,
        });
        let client = Arc::new(ScriptedClient::default());
        let (client_tx, client_rx) = watch::channel(Some(Arc::clone(&client)));
        let (fault_tx, _fault_rx) = mpsc::channel(1);
        let epoch = Arc::new(SequencedEpoch {
            values: Mutex::new(VecDeque::from([1, 2])),
            last: AtomicU64::new(0),
        });
        let publisher = tokio::spawn(run_publisher(
            ingress.clone(),
            wakes,
            client_rx,
            epoch,
            fault_tx,
        ));
        while client.sends.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
        let sent = client.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].1.contains("gap: true"));
        drop(sent);
        assert_eq!(ingress.dropped_updates(), 1);
        drop(client_tx);
        publisher.abort();
    }
}
