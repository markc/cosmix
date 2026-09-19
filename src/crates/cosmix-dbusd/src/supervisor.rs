//! Adapter supervision: one tokio task per adapter, panic and error
//! containment, exponential backoff, runtime restart/enable/disable.
//!
//! Each adapter gets a lifecycle task that owns the restart loop for
//! that adapter alone. The run itself is a grandchild task observed via
//! its JoinHandle: `Err(e)` with `e.is_panic()` (a panicking run) and
//! `Ok(Err(_))` (a failing run) both record a failure, move the adapter
//! to `backoff` and relaunch after the schedule; `Ok(Ok(_))` (a run
//! that returned when it should serve forever) is treated the same way.
//! A failing adapter never leaves its own lifecycle task, so it can
//! never take the daemon or a sibling adapter down.
//!
//! Two limits of the language bound this containment: a panic inside
//! `Drop` during a panic unwind aborts the whole process, and a run
//! that never yields cannot be preempted — it is reported `stuck`, its
//! names may stay held until the process restarts, and its lifecycle
//! keeps answering commands.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use anyhow::Result;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::adapter::{AdapterCtx, AdapterSpec, BusIdentity, SessionBus};
use crate::backoff::Backoff;
use crate::state::{AdapterEvent, AdapterStateKind, AdapterStatus, RegistryState};

/// Depth of one adapter's control channel. The only writers are the
/// `dbusd.adapter.*` verbs — human pace — so eight pending commands is
/// already a storm; a full channel refuses with "busy" rather than
/// queueing behind a stuck adapter.
const CONTROL_CAPACITY: usize = 8;

/// Bounded event backlog. State changes are rare; a full channel drops
/// with a logged line (props.get is the bootstrap truth, events are the
/// deltas) rather than blocking supervision.
const EVENT_CAPACITY: usize = 64;

/// How long a stopping run gets to observe its stop signal, unwind and
/// drop its connections before the task is aborted.
pub const GRACEFUL_STOP: Duration = Duration::from_secs(5);

/// How long an aborted run gets to land the cancellation (abort is
/// cooperative — it only takes effect at a yield point). A run that
/// exceeds even this never yields: it is reported `stuck` and detached.
pub const ABORT_STOP: Duration = Duration::from_secs(1);

/// Whether [`stop_run`] actually ended the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunStop {
    /// The run returned (gracefully or on abort) and dropped what it
    /// owned.
    Stopped,
    /// The run never yielded: its task is detached and leaked, and its
    /// Bus service / D-Bus names may still be held until the process
    /// restarts.
    Stuck,
}

/// One supervisor command, as issued by the `dbusd.adapter.*` verbs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleCmd {
    /// Stop the current run and relaunch now (resets backoff).
    Restart,
    /// Stop the current run and park in `disabled` (releases names).
    Disable,
    /// Launch a disabled adapter (resets backoff).
    Enable,
}

/// Everything `serve()` needs after starting supervision: the control
/// surface, the event stream for publication, and the lifecycle tasks
/// themselves (a lifecycle task that ends before daemon shutdown is a
/// daemon-level bug and must take the daemon down for repair, not be
/// silently ignored).
pub struct StartedSupervisor {
    pub handle: SupervisorHandle,
    pub events: mpsc::Receiver<AdapterEvent>,
    pub lifecycles: Vec<(String, JoinHandle<()>)>,
    /// Holds an events sender for the supervisor's whole lifetime. With
    /// zero adapters (J1 ships none) no lifecycle task holds one, and
    /// without this the channel would close when `start` returns — the
    /// publisher's `recv()` would end and the daemon would exit into a
    /// systemd crash-loop. Underscore-prefixed *binding*, not `_`: it
    /// must live as long as the struct, not drop on construction.
    pub _events_keepalive: mpsc::Sender<AdapterEvent>,
}

/// Control surface over the running supervision. Cheap to clone; the
/// citizen dispatch and the event publisher each hold one.
#[derive(Clone)]
pub struct SupervisorHandle {
    control: BTreeMap<String, mpsc::Sender<LifecycleCmd>>,
    registry: Arc<Mutex<RegistryState>>,
}

impl SupervisorHandle {
    /// All adapter statuses, ordered by name.
    pub fn statuses(&self) -> Vec<AdapterStatus> {
        lock_registry(&self.registry)
            .adapters
            .values()
            .cloned()
            .collect()
    }

    pub fn status(&self, name: &str) -> Option<AdapterStatus> {
        lock_registry(&self.registry).adapters.get(name).cloned()
    }

    /// The per-daemon-session event sequence counter: what the last
    /// stamped event carried. Surfaced on `dbusd.info` / props.watch so
    /// a subscriber can tell whether it has seen every event.
    pub fn event_seq(&self) -> u64 {
        lock_registry(&self.registry).next_event_seq
    }

    /// Issue a control command. Unknown names are refused (the refusal
    /// the `dbusd.adapter.*` verbs reply with), a full queue reports
    /// busy, a closed one means that adapter's supervision ended.
    pub fn control(&self, name: &str, cmd: LifecycleCmd) -> std::result::Result<(), String> {
        let sender = self
            .control
            .get(name)
            .ok_or_else(|| format!("unknown adapter: {name}"))?;
        match sender.try_send(cmd) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => {
                Err(format!("adapter {name} control queue is full; retry"))
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                Err(format!("adapter {name} supervision has ended"))
            }
        }
    }
}

/// Start supervision: one lifecycle task per adapter spec. `enabled`
/// selects which adapters launch (from config); disabled ones park in
/// `disabled` and can be enabled at runtime. `bus_identity` (when
/// given) is attached to every adapter context so adapters can register
/// their own Bus service; the session bus rides in every context.
pub fn start(
    specs: Vec<(AdapterSpec, bool)>,
    session_bus: SessionBus,
    shutdown: watch::Receiver<bool>,
    bus_identity: Option<BusIdentity>,
) -> StartedSupervisor {
    let registry = Arc::new(Mutex::new(RegistryState::default()));
    let (events_tx, events_rx) = mpsc::channel(EVENT_CAPACITY);
    let mut control = BTreeMap::new();
    let mut lifecycles = Vec::new();

    for (spec, enabled) in specs {
        let status = AdapterStatus {
            name: spec.name.clone(),
            service: spec.service.clone(),
            state: if enabled {
                AdapterStateKind::Starting
            } else {
                AdapterStateKind::Disabled
            },
            restarts: 0,
            last_error: None,
            since: SystemTime::now(),
        };
        lock_registry(&registry)
            .adapters
            .insert(spec.name.clone(), status);

        let (cmd_tx, cmd_rx) = mpsc::channel(CONTROL_CAPACITY);
        control.insert(spec.name.clone(), cmd_tx);
        let name = spec.name.clone();
        let join = tokio::spawn(adapter_lifecycle(
            spec,
            enabled,
            session_bus.clone(),
            cmd_rx,
            shutdown.clone(),
            Arc::clone(&registry),
            events_tx.clone(),
            bus_identity.clone(),
        ));
        lifecycles.push((name, join));
    }

    StartedSupervisor {
        handle: SupervisorHandle { control, registry },
        events: events_rx,
        lifecycles,
        _events_keepalive: events_tx,
    }
}

/// The restart loop for one adapter. Runs until daemon shutdown (or its
/// control senders all dropping); never returns because the adapter
/// failed — failure is contained here as backoff + relaunch.
#[allow(clippy::too_many_arguments)]
async fn adapter_lifecycle(
    spec: AdapterSpec,
    mut enabled: bool,
    session_bus: SessionBus,
    mut control: mpsc::Receiver<LifecycleCmd>,
    mut daemon_shutdown: watch::Receiver<bool>,
    registry: Arc<Mutex<RegistryState>>,
    events: mpsc::Sender<AdapterEvent>,
    bus_identity: Option<BusIdentity>,
) {
    let mut backoff = Backoff::default();
    let mut launches: u64 = 0;
    // The last run could not be stopped (never yielded): park in
    // `stuck` rather than `disabled` until a command relaunches.
    let mut stuck = false;

    // A watch receiver cloned after the value flipped never sees a
    // change; check the current value once on entry.
    if *daemon_shutdown.borrow_and_update() {
        return;
    }

    loop {
        if !enabled {
            if stuck {
                transition(
                    &registry,
                    &events,
                    &spec.name,
                    AdapterStateKind::Stuck,
                    Some(Some(stuck_error())),
                    false,
                );
            } else {
                transition(
                    &registry,
                    &events,
                    &spec.name,
                    AdapterStateKind::Disabled,
                    None,
                    false,
                );
            }
            loop {
                tokio::select! {
                    biased;
                    changed = daemon_shutdown.changed() => {
                        if changed.is_err() || *daemon_shutdown.borrow_and_update() {
                            return;
                        }
                    }
                    cmd = control.recv() => match cmd {
                        None => return,
                        Some(LifecycleCmd::Disable) => {}
                        Some(LifecycleCmd::Restart | LifecycleCmd::Enable) => {
                            enabled = true;
                            stuck = false;
                            backoff.reset();
                            break;
                        }
                    },
                }
            }
            continue;
        }

        // Enabled: (re)launch. Every launch after the first is a restart.
        launches += 1;
        transition(
            &registry,
            &events,
            &spec.name,
            AdapterStateKind::Starting,
            None,
            launches > 1,
        );

        let adapter = (spec.factory)();
        // The ready signal hops through a monitor task, never straight
        // into this lifecycle: a run task may only wake the MONITOR.
        // A run that never yields strands whatever it wakes on its own
        // worker (tokio's LIFO slot is unstealable while that worker
        // spins), so the lifecycle must not be a direct wake target of
        // its own run — this way commands and shutdown (woken from
        // their senders' contexts) keep reaching the lifecycle even
        // then, and a wedged run simply never reaches `running`.
        let (ready_tx, ready_rx) = oneshot::channel();
        let (ready_in_tx, mut ready_in) = mpsc::channel(1);
        tokio::spawn(async move {
            if ready_rx.await.is_ok() {
                let _ = ready_in_tx.send(()).await;
            }
        });
        // This launch's own stop signal: flipped on daemon shutdown,
        // disable and restart alike, so one select in the adapter covers
        // every stop path.
        let (launch_stop_tx, launch_stop) = watch::channel(false);
        let mut ctx = AdapterCtx::new(session_bus.clone(), launch_stop);
        ctx.set_ready(ready_tx);
        if let Some(identity) = bus_identity.as_ref() {
            let mut identity = identity.clone();
            identity.service = spec.service.clone();
            ctx.set_bus_identity(identity);
        }

        let started_at = Instant::now();
        let mut launch = tokio::spawn(adapter.run(ctx));

        'launch: loop {
            tokio::select! {
                biased;
                changed = daemon_shutdown.changed() => {
                    if changed.is_err() || *daemon_shutdown.borrow_and_update() {
                        let _ = launch_stop_tx.send(true);
                        if stop_run(&mut launch).await == RunStop::Stuck {
                            mark_stuck(&registry, &events, &spec.name);
                        }
                        return;
                    }
                }
                cmd = control.recv() => match cmd {
                    None => {
                        let _ = launch_stop_tx.send(true);
                        if stop_run(&mut launch).await == RunStop::Stuck {
                            mark_stuck(&registry, &events, &spec.name);
                        }
                        return;
                    }
                    Some(LifecycleCmd::Enable) => {}
                    Some(LifecycleCmd::Restart) => {
                        let _ = launch_stop_tx.send(true);
                        if stop_run(&mut launch).await == RunStop::Stuck {
                            mark_stuck(&registry, &events, &spec.name);
                            // The wedged run is detached and leaked, its
                            // names possibly still held; relaunch anyway —
                            // the operator explicitly asked for a restart.
                        }
                        backoff.reset();
                        break 'launch;
                    }
                    Some(LifecycleCmd::Disable) => {
                        let _ = launch_stop_tx.send(true);
                        if stop_run(&mut launch).await == RunStop::Stuck {
                            stuck = true;
                        }
                        enabled = false;
                        break 'launch;
                    }
                },
                Some(()) = ready_in.recv() => {
                    transition(
                        &registry,
                        &events,
                        &spec.name,
                        AdapterStateKind::Running,
                        Some(None),
                        false,
                    );
                }
                result = &mut launch => {
                    let failure = classify_run(result);
                    backoff.record_failure_after_healthy(started_at.elapsed());
                    transition(
                        &registry,
                        &events,
                        &spec.name,
                        AdapterStateKind::Backoff,
                        Some(Some(failure)),
                        false,
                    );
                    let deadline = Instant::now() + backoff.next_delay();
                    loop {
                        if Instant::now() >= deadline {
                            break;
                        }
                        tokio::select! {
                            biased;
                            changed = daemon_shutdown.changed() => {
                                if changed.is_err() || *daemon_shutdown.borrow_and_update() {
                                    return;
                                }
                            }
                            cmd = control.recv() => match cmd {
                                None => return,
                                // Enable while waiting out backoff is an
                                // explicit ask to run: relaunch now (same
                                // as restart), not a silent no-op until
                                // the schedule expires.
                                Some(LifecycleCmd::Restart | LifecycleCmd::Enable) => {
                                    backoff.reset();
                                    break;
                                }
                                Some(LifecycleCmd::Disable) => {
                                    enabled = false;
                                    break;
                                }
                            },
                            _ = tokio::time::sleep_until(deadline) => break,
                        }
                    }
                    break 'launch;
                }
            }
        }
    }
}

/// Stop the current run: graceful window first (the run observed its
/// stop signal, returned, and dropped its Bus + zbus connections and
/// names), abort as the backstop for a run that will not stop. Abort is
/// cooperative — it only lands at a yield point — so the post-abort
/// wait is bounded too: a run that never yields is returned as
/// [`RunStop::Stuck`] and detached (dropping the handle leaks the
/// wedged task; its names go when the process does).
async fn stop_run(launch: &mut JoinHandle<Result<()>>) -> RunStop {
    if tokio::time::timeout(GRACEFUL_STOP, &mut *launch)
        .await
        .is_err()
    {
        launch.abort();
        if tokio::time::timeout(ABORT_STOP, &mut *launch)
            .await
            .is_err()
        {
            return RunStop::Stuck;
        }
    }
    RunStop::Stopped
}

/// Record a wedged run: state `stuck`, `last_error` saying what that
/// means for the names it may still hold.
fn mark_stuck(
    registry: &Arc<Mutex<RegistryState>>,
    events: &mpsc::Sender<AdapterEvent>,
    name: &str,
) {
    transition(
        registry,
        events,
        name,
        AdapterStateKind::Stuck,
        Some(Some(stuck_error())),
        false,
    );
}

fn stuck_error() -> String {
    "run ignored abort and never yielded; its Bus service and D-Bus \
     names may still be held until the process restarts"
        .to_string()
}

/// Lock the registry, recovering from poison. One holder's panic must
/// not cascade to every adapter and the Bus service: a poisoned mutex
/// means a panic mid-update, and the map is still structurally sound —
/// the next transition overwrites whatever the panicking writer left.
fn lock_registry(registry: &Arc<Mutex<RegistryState>>) -> std::sync::MutexGuard<'_, RegistryState> {
    registry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Render a finished run's JoinHandle result as the failure reason
/// stored in `last_error`. Every arm is a failure: the run contract is
/// "serve until stopped", so even `Ok` is an unexpected exit.
fn classify_run(result: std::result::Result<Result<()>, tokio::task::JoinError>) -> String {
    match result {
        Ok(Ok(())) => "run returned Ok unexpectedly".to_string(),
        Ok(Err(error)) => format!("{error:#}"),
        Err(error) if error.is_panic() => {
            let payload = error.into_panic();
            let message = payload
                .downcast_ref::<&str>()
                .map(|literal| (*literal).to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "non-string panic payload".to_string());
            format!("panicked: {message}")
        }
        Err(error) => format!("run task cancelled unexpectedly: {error}"),
    }
}

/// Apply a state change to the registry and hand the citizen layer an
/// event. `error`: `None` leaves `last_error` alone, `Some(e)` sets it
/// (`Some(None)` clears it — used when a run reaches healthy again).
/// `restart_bump` marks that this transition is a relaunch. Emits
/// unless nothing observable changed.
fn transition(
    registry: &Arc<Mutex<RegistryState>>,
    events: &mpsc::Sender<AdapterEvent>,
    name: &str,
    new_state: AdapterStateKind,
    error: Option<Option<String>>,
    restart_bump: bool,
) {
    let event = {
        let mut registry = lock_registry(registry);
        let Some(status) = registry.adapters.get_mut(name) else {
            return;
        };
        let previous = status.state;
        if previous == new_state && error.is_none() && !restart_bump {
            return;
        }
        status.state = new_state;
        status.since = SystemTime::now();
        if restart_bump {
            status.restarts = status.restarts.saturating_add(1);
        }
        if let Some(error) = error {
            status.last_error = error;
        }
        let status = status.clone();
        registry.next_event_seq = registry.next_event_seq.saturating_add(1);
        AdapterEvent {
            status,
            previous,
            seq: registry.next_event_seq,
        }
    };
    let seq = event.seq;
    if events.try_send(event).is_err() {
        eprintln!(
            "cosmix-dbusd: adapter state event dropped (backlog full): {name} -> {} (event_seq {})",
            new_state.as_str(),
            seq
        );
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::adapter::{AdapterFactory, AdapterSpec, SessionBus};
    use crate::fault::{FaultAction, FaultAdapter, FaultScript};
    use crate::state::AdapterStateKind;
    use anyhow::anyhow;

    const NO_BUS: &str = "DBUS_SESSION_BUS_ADDRESS is not set";

    fn spec(name: &'static str, script: std::sync::Arc<FaultScript>) -> AdapterSpec {
        let script_for_factory = Arc::clone(&script);
        let factory: AdapterFactory = Arc::new(move || {
            Box::new(FaultAdapter {
                name,
                service: name,
                script: Arc::clone(&script_for_factory),
            })
        });
        AdapterSpec {
            name: name.into(),
            service: name.into(),
            factory,
        }
    }

    fn two_adapters(
        a_actions: Vec<FaultAction>,
    ) -> (
        Vec<(AdapterSpec, bool)>,
        std::sync::Arc<FaultScript>,
        std::sync::Arc<FaultScript>,
    ) {
        // Adapter B is always healthy: the containment witness.
        let a = FaultScript::new(a_actions);
        let b = FaultScript::new(Vec::new());
        let specs = vec![
            (spec("a", Arc::clone(&a)), true),
            (spec("b", Arc::clone(&b)), true),
        ];
        (specs, a, b)
    }

    async fn wait_until(
        handle: &SupervisorHandle,
        name: &str,
        predicate: impl Fn(&AdapterStatus) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(600);
        loop {
            if let Some(status) = handle.status(name)
                && predicate(&status)
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for adapter '{name}' (now {:?})",
                handle
                    .status(name)
                    .map(|status| (status.state, status.restarts))
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn wait_running(handle: &SupervisorHandle, name: &str) {
        wait_until(handle, name, |status| {
            status.state == AdapterStateKind::Running
        })
        .await;
    }

    /// Signal shutdown and require every lifecycle task to stop cleanly
    /// (each run observes its stop signal, drops what it owns, returns).
    async fn shutdown_and_drain(started: StartedSupervisor, shutdown_tx: watch::Sender<bool>) {
        shutdown_tx.send(true).expect("send shutdown");
        for (name, join) in started.lifecycles {
            match tokio::time::timeout(Duration::from_secs(30), join).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => panic!("lifecycle '{name}' failed on shutdown: {error}"),
                Err(_) => panic!("lifecycle '{name}' did not stop"),
            }
        }
    }

    /// The panic-containment contract the supervisor relies on: this
    /// workspace's profile unwinds, so a panicking run task surfaces as
    /// a JoinError with `is_panic()` — never an abort of the process.
    #[tokio::test(start_paused = true)]
    async fn panicking_task_is_reported_as_panic_via_join_handle() {
        let joined: std::result::Result<(), tokio::task::JoinError> =
            tokio::spawn(async { panic!("boom") }).await;
        let error = joined.expect_err("panicking task must fail to join");
        assert!(error.is_panic());
        assert!(!error.is_cancelled());
    }

    /// THE containment test: adapter A panics; adapter B keeps running
    /// untouched (its run is still alive, holding its names, zero
    /// restarts), the supervision tasks survive, and A is restarted
    /// after backoff and runs healthy.
    #[tokio::test(start_paused = true)]
    async fn panic_in_one_adapter_leaves_the_other_running_and_restarts_it() {
        let (specs, a, b) = two_adapters(vec![FaultAction::Panic]);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let started = start(
            specs,
            SessionBus::Unavailable(NO_BUS.into()),
            shutdown_rx,
            None,
        );
        let handle = started.handle.clone();

        wait_until(&handle, "a", |status| {
            status.state == AdapterStateKind::Backoff
                && status.last_error.as_deref().is_some_and(|error| {
                    error.contains("panicked") && error.contains("scripted panic")
                })
        })
        .await;
        // B was running before, during and after A's panic, and was
        // never relaunched for it. If containment were broken — the
        // panic taking the supervision or B's task down — these fail.
        assert_eq!(handle.status("b").unwrap().state, AdapterStateKind::Running);
        assert_eq!(handle.status("b").unwrap().restarts, 0);
        assert!(b.holds("b"), "adapter B's run must still hold its names");
        assert!(
            !started
                .lifecycles
                .iter()
                .any(|(_, join)| join.is_finished()),
            "no lifecycle task may die from an adapter panic"
        );
        // A's panic released its names (drop on unwind) ...
        assert!(!a.holds("a"));

        // ... and backoff brings it back healthy (script exhausted ->
        // run forever). One restart so far, with the 1 s initial delay.
        wait_running(&handle, "a").await;
        assert_eq!(handle.status("a").unwrap().restarts, 1);
        assert_eq!(a.launch_times(), vec![Duration::from_secs(1)]);
        assert!(a.holds("a"));
        assert_eq!(handle.status("a").unwrap().last_error, None);

        shutdown_and_drain(started, shutdown_tx).await;
    }

    /// `Ok(Err(_))` — a failing (not panicking) run — is contained the
    /// same way, and the restarts follow the exponential schedule:
    /// 1 s, 2 s, 4 s between launches (paused-clock virtual time).
    #[tokio::test(start_paused = true)]
    async fn failing_adapter_restarts_on_the_exponential_schedule() {
        let (specs, a, b) = two_adapters(vec![
            FaultAction::Fail("one".into()),
            FaultAction::Fail("two".into()),
            FaultAction::Fail("three".into()),
        ]);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let started = start(
            specs,
            SessionBus::Unavailable(NO_BUS.into()),
            shutdown_rx,
            None,
        );
        let handle = started.handle.clone();

        wait_until(&handle, "a", |status| {
            status.state == AdapterStateKind::Running && status.restarts == 3
        })
        .await;
        assert_eq!(
            a.launch_times(),
            vec![
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
            ]
        );
        assert_eq!(
            handle.status("a").unwrap().last_error,
            None,
            "reaching running clears the last failure"
        );
        assert_eq!(handle.status("b").unwrap().restarts, 0);
        assert!(b.holds("b"));

        shutdown_and_drain(started, shutdown_tx).await;
    }

    /// No session bus (DBUS_SESSION_BUS_ADDRESS unset): the adapter
    /// that needs it lands in backoff with that reason and keeps
    /// retrying — the daemon (and its other adapter) stay up and the
    /// control surface stays answerable.
    #[tokio::test(start_paused = true)]
    async fn missing_session_bus_is_backoff_not_exit() {
        let (specs, a, _b) = two_adapters(vec![FaultAction::NeedSessionBus]);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let started = start(
            specs,
            SessionBus::Unavailable(NO_BUS.into()),
            shutdown_rx,
            None,
        );
        let handle = started.handle.clone();

        wait_until(&handle, "a", |status| {
            status.state == AdapterStateKind::Backoff
                && status
                    .last_error
                    .as_deref()
                    .is_some_and(|error| error.contains("DBUS_SESSION_BUS_ADDRESS"))
        })
        .await;
        assert_eq!(handle.status("b").unwrap().state, AdapterStateKind::Running);
        assert!(
            started
                .lifecycles
                .iter()
                .all(|(_, join)| !join.is_finished())
        );

        // The control surface still answers while a is stuck failing:
        // disable it (which also ends the test's backoff cycling).
        handle
            .control("a", LifecycleCmd::Disable)
            .expect("control while in backoff");
        wait_until(&handle, "a", |status| {
            status.state == AdapterStateKind::Disabled
        })
        .await;
        assert!(!a.holds("a"));
        assert_eq!(handle.status("b").unwrap().state, AdapterStateKind::Running);

        shutdown_and_drain(started, shutdown_tx).await;
    }

    /// A configured-but-unreachable session bus address fails at dial
    /// time inside the run; that is an ordinary adapter failure —
    /// backoff with the dial error, daemon up. Exercises the real zbus
    /// dial path from AdapterCtx.
    #[cfg(feature = "cosmix")]
    #[tokio::test(start_paused = true)]
    async fn unreachable_session_bus_is_backoff_not_exit() {
        let (specs, a, _b) = two_adapters(vec![FaultAction::NeedSessionBus]);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let started = start(
            specs,
            SessionBus::Address("unix:path=/nonexistent-cosmix-dbusd-test/bus".into()),
            shutdown_rx,
            None,
        );
        let handle = started.handle.clone();

        wait_until(&handle, "a", |status| {
            status.state == AdapterStateKind::Backoff
                && status
                    .last_error
                    .as_deref()
                    .is_some_and(|error| !error.contains("DBUS_SESSION_BUS_ADDRESS"))
        })
        .await;
        assert_eq!(handle.status("b").unwrap().state, AdapterStateKind::Running);
        assert!(a.launch_count() >= 1);

        handle
            .control("a", LifecycleCmd::Disable)
            .expect("control after dial failure");
        wait_until(&handle, "a", |status| {
            status.state == AdapterStateKind::Disabled
        })
        .await;

        shutdown_and_drain(started, shutdown_tx).await;
    }

    /// A consistently failing adapter stays contained through a
    /// backoff-escalation storm: sibling untouched, control answerable
    /// mid-storm, and it recovers when its script finally lets it run.
    #[tokio::test(start_paused = true)]
    async fn consistent_failures_escalate_and_stay_contained() {
        let (specs, a, b) = two_adapters(vec![
            FaultAction::RunFor(Duration::ZERO),
            FaultAction::RunFor(Duration::ZERO),
            FaultAction::RunFor(Duration::ZERO),
            FaultAction::RunFor(Duration::ZERO),
        ]);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let started = start(
            specs,
            SessionBus::Unavailable(NO_BUS.into()),
            shutdown_rx,
            None,
        );
        let handle = started.handle.clone();

        // Mid-storm: four failures in (restarts counts launches, so the
        // fourth failing launch shows restarts == 3), delays have
        // reached 8 s, and everything else is still healthy and
        // answerable.
        wait_until(&handle, "a", |status| {
            status.state == AdapterStateKind::Backoff && status.restarts == 3
        })
        .await;
        assert_eq!(
            a.launch_times(),
            vec![
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
            ]
        );
        assert_eq!(handle.status("b").unwrap().state, AdapterStateKind::Running);
        assert_eq!(handle.status("b").unwrap().restarts, 0);
        assert!(b.holds("b"));
        handle
            .control("b", LifecycleCmd::Disable)
            .expect("control answerable mid-storm");
        handle
            .control("b", LifecycleCmd::Enable)
            .expect("control answerable mid-storm");
        wait_running(&handle, "b").await;

        // Script exhausted -> a recovers once its backoff expires.
        wait_running(&handle, "a").await;
        assert!(a.holds("a"));

        shutdown_and_drain(started, shutdown_tx).await;
    }

    /// Disable stops the run and releases what it owns (a real adapter
    /// would withdraw its Bus service and D-Bus names here); enable
    /// starts it again as a counted restart.
    #[tokio::test(start_paused = true)]
    async fn disable_releases_names_and_enable_reacquires() {
        let (specs, a, b) = two_adapters(Vec::new());
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let mut started = start(
            specs,
            SessionBus::Unavailable(NO_BUS.into()),
            shutdown_rx,
            None,
        );
        let handle = started.handle.clone();
        wait_running(&handle, "a").await;
        assert!(a.holds("a"));

        handle.control("a", LifecycleCmd::Disable).expect("disable");
        wait_until(&handle, "a", |status| {
            status.state == AdapterStateKind::Disabled
        })
        .await;
        assert!(!a.holds("a"), "disable must release the adapter's names");
        assert!(b.holds("b"), "disable touches only its own adapter");

        handle.control("a", LifecycleCmd::Enable).expect("enable");
        wait_running(&handle, "a").await;
        assert!(a.holds("a"));
        assert_eq!(handle.status("a").unwrap().restarts, 1);

        // The disable produced a state-change event (what the citizen
        // publishes as dbusd.adapter.changed + props diffs).
        let mut saw_disabled = false;
        while let Ok(event) = started.events.try_recv() {
            if event.status.name == "a" && event.status.state == AdapterStateKind::Disabled {
                saw_disabled = true;
            }
        }
        assert!(saw_disabled, "disable must emit a state-change event");

        shutdown_and_drain(started, shutdown_tx).await;
    }

    /// The restart verb relaunches a healthy adapter immediately
    /// (graceful stop, backoff reset, counted restart).
    #[tokio::test(start_paused = true)]
    async fn restart_verb_relaunches_the_adapter() {
        let (specs, a, _b) = two_adapters(Vec::new());
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let started = start(
            specs,
            SessionBus::Unavailable(NO_BUS.into()),
            shutdown_rx,
            None,
        );
        let handle = started.handle.clone();
        wait_running(&handle, "a").await;

        handle.control("a", LifecycleCmd::Restart).expect("restart");
        wait_until(&handle, "a", |status| {
            status.state == AdapterStateKind::Running && status.restarts == 1
        })
        .await;
        assert!(a.holds("a"));

        shutdown_and_drain(started, shutdown_tx).await;
    }

    /// Control for an unknown adapter name is a refusal, not a panic —
    /// the same refusal the `dbusd.adapter.*` verbs reply with.
    #[tokio::test(start_paused = true)]
    async fn unknown_adapter_control_is_refused() {
        let (specs, _a, _b) = two_adapters(Vec::new());
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let started = start(
            specs,
            SessionBus::Unavailable(NO_BUS.into()),
            shutdown_rx,
            None,
        );
        let handle = started.handle.clone();

        for cmd in [
            LifecycleCmd::Restart,
            LifecycleCmd::Enable,
            LifecycleCmd::Disable,
        ] {
            assert_eq!(
                handle.control("nonsense", cmd),
                Err("unknown adapter: nonsense".to_string())
            );
        }

        shutdown_and_drain(started, shutdown_tx).await;
    }

    #[tokio::test(start_paused = true)]
    async fn classify_run_labels_each_failure_kind() {
        let panicked = tokio::spawn(async { panic!("kaboom") }).await;
        assert!(classify_run(panicked).contains("panicked: kaboom"));
        let failed = tokio::spawn(async { Err::<(), _>(anyhow!("nope")) }).await;
        assert_eq!(classify_run(failed), "nope");
        let exited = tokio::spawn(async { Ok::<_, anyhow::Error>(()) }).await;
        assert!(classify_run(exited).contains("returned Ok unexpectedly"));
    }

    /// F2: a run that never yields cannot be preempted — disable must
    /// still complete (the post-abort wait is bounded), the state must
    /// tell the truth (`stuck`, names possibly still held), and the
    /// lifecycle must keep answering commands. Real time and a
    /// multi-thread runtime: the wedged task busy-loops on one worker
    /// while everything else runs on another — a current-thread runtime
    /// (or a paused clock, which cannot advance past a busy loop) could
    /// never run this.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn never_yielding_run_is_reported_stuck_and_control_stays_answerable() {
        let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let a = FaultScript::new(vec![FaultAction::NeverYield(Arc::clone(&release))]);
        let b = FaultScript::new(Vec::new());
        let specs = vec![
            (spec("a", Arc::clone(&a)), true),
            (spec("b", Arc::clone(&b)), true),
        ];
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let started = start(
            specs,
            SessionBus::Unavailable(NO_BUS.into()),
            shutdown_rx,
            None,
        );
        let handle = started.handle.clone();
        // Do NOT wait for the supervision state here: the wedged run
        // cannot yield, so even its ready signal strands on the wedged
        // worker's LIFO slot — `running` may never land. Observe the
        // launch through the fault script instead (no scheduler
        // involvement). Commands still get through because they wake
        // the lifecycle from the sender's context, not the spinner's.
        let deadline = Instant::now() + Duration::from_secs(30);
        while !(a.launch_count() == 1 && a.holds("a")) {
            assert!(Instant::now() < deadline, "the wedged run never launched");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // Disable completes (bounded stop) and reports the honest state.
        handle
            .control("a", LifecycleCmd::Disable)
            .expect("disable a wedged run");
        wait_until(&handle, "a", |status| {
            status.state == AdapterStateKind::Stuck
                && status
                    .last_error
                    .as_deref()
                    .is_some_and(|error| error.contains("ignored abort"))
        })
        .await;
        assert!(
            a.holds("a"),
            "stuck means the names may still be held — that is the report"
        );
        assert_eq!(handle.status("b").unwrap().state, AdapterStateKind::Running);

        // The lifecycle keeps answering: enable relaunches (counted
        // restart) and the relaunched run — script exhausted — behaves.
        handle
            .control("a", LifecycleCmd::Enable)
            .expect("enable after stuck");
        wait_running(&handle, "a").await;
        assert_eq!(handle.status("a").unwrap().restarts, 1);

        // Test teardown: end the leaked busy-loop, then shut down.
        release.store(true, std::sync::atomic::Ordering::Release);
        shutdown_and_drain(started, shutdown_tx).await;
    }

    /// F3: a panic while holding the registry lock (poison) must not
    /// cascade — the control surface keeps reading, the lifecycles keep
    /// writing, siblings are untouched.
    #[tokio::test(start_paused = true)]
    async fn poisoned_registry_does_not_take_down_the_control_surface() {
        let (specs, _a, _b) = two_adapters(Vec::new());
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let started = start(
            specs,
            SessionBus::Unavailable(NO_BUS.into()),
            shutdown_rx,
            None,
        );
        let handle = started.handle.clone();
        wait_running(&handle, "a").await;

        // Poison the mutex: panic while holding the lock.
        let registry = Arc::clone(&handle.registry);
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = registry.lock().expect("take the lock to poison it");
            panic!("poison the dbusd registry");
        }));
        assert!(poisoned.is_err(), "the poisoning panic must be caught");

        // Readers recover across the poison:
        assert_eq!(handle.statuses().len(), 2);
        assert_eq!(handle.status("a").unwrap().state, AdapterStateKind::Running);
        assert!(handle.event_seq() > 0, "seq reads recover too");
        // And writers: a command still lands a transition.
        handle
            .control("a", LifecycleCmd::Disable)
            .expect("control across poison");
        wait_until(&handle, "a", |status| {
            status.state == AdapterStateKind::Disabled
        })
        .await;
        assert_eq!(handle.status("b").unwrap().state, AdapterStateKind::Running);

        shutdown_and_drain(started, shutdown_tx).await;
    }

    /// F9: enable while waiting out backoff relaunches now (same as
    /// restart), not a silent no-op until the schedule expires.
    #[tokio::test(start_paused = true)]
    async fn enable_during_backoff_relaunches_immediately() {
        let (specs, a, _b) = two_adapters(vec![FaultAction::Fail("once".into())]);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let started = start(
            specs,
            SessionBus::Unavailable(NO_BUS.into()),
            shutdown_rx,
            None,
        );
        let handle = started.handle.clone();

        wait_until(&handle, "a", |status| {
            status.state == AdapterStateKind::Backoff
        })
        .await;
        handle
            .control("a", LifecycleCmd::Enable)
            .expect("enable during backoff");
        wait_running(&handle, "a").await;
        assert_eq!(handle.status("a").unwrap().restarts, 1);
        // The relaunch came from the command, not the schedule: the gap
        // after the failed launch is well under the 1 s initial backoff.
        let gaps = a.launch_times();
        assert_eq!(gaps.len(), 1);
        assert!(
            gaps[0] < Duration::from_secs(1),
            "relaunched immediately, not after backoff: {:?}",
            gaps[0]
        );

        shutdown_and_drain(started, shutdown_tx).await;
    }

    /// F5: every state change carries a per-daemon-session monotonic
    /// seq, strictly increasing, and the handle's counter matches the
    /// last stamped event.
    #[tokio::test(start_paused = true)]
    async fn events_carry_a_strictly_monotonic_session_seq() {
        let (specs, _a, _b) = two_adapters(Vec::new());
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let mut started = start(
            specs,
            SessionBus::Unavailable(NO_BUS.into()),
            shutdown_rx,
            None,
        );
        let handle = started.handle.clone();
        wait_running(&handle, "a").await;
        handle.control("a", LifecycleCmd::Disable).expect("disable");
        wait_until(&handle, "a", |status| {
            status.state == AdapterStateKind::Disabled
        })
        .await;

        let mut seqs = Vec::new();
        while let Ok(event) = started.events.try_recv() {
            seqs.push(event.seq);
        }
        assert!(!seqs.is_empty(), "state changes must produce events");
        assert!(
            seqs.windows(2).all(|pair| pair[0] < pair[1]),
            "seq must strictly increase: {seqs:?}"
        );
        assert_eq!(
            *seqs.last().unwrap(),
            handle.event_seq(),
            "the counter matches the last stamped event"
        );

        shutdown_and_drain(started, shutdown_tx).await;
    }
}
