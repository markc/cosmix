//! A scripted misbehaving adapter for the supervision tests.
//!
//! Compiled only under `cfg(test)`, so it cannot ride along in the
//! release binary or be reached from the Bus. Each `run` pops its next
//! action from a shared script; the script outlives restarts because
//! the supervisor re-creates the adapter from a factory that shares it.
//!
//! `NameLease` stands in for what a real adapter owns: its Bus service
//! registration and its zbus connection/names. The lease is a local of
//! `run`, so returning, failing, or panicking releases it — the same
//! drop-based ownership that withdraws a real adapter's names.

use std::collections::{BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow};

use crate::adapter::{Adapter, AdapterCtx, BoxRunFuture};

/// What this launch does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FaultAction {
    /// Panic inside the run task.
    Panic,
    /// Return `Err` immediately.
    Fail(String),
    /// Refuse to run without a session bus address in the context — the
    /// "no bus" adapter. With an address it dials it (zbus, `cosmix`
    /// builds) and fails unless the dial succeeds.
    NeedSessionBus,
    /// Serve until stopped, failing with a scripted expiry after `0`
    /// means immediate.
    RunFor(Duration),
    /// Serve until stopped; never fails.
    RunForever,
}

/// Shared script + observation points, so the test and every (re)launch
/// of the adapter see the same queues.
#[derive(Default)]
pub(crate) struct FaultScript {
    pub actions: Mutex<VecDeque<FaultAction>>,
    /// `tokio::time::Instant` of each launch — virtual time under the
    /// paused test clock, so backoff gaps are asserted exactly.
    pub launches: Mutex<Vec<tokio::time::Instant>>,
    /// Names currently held by live runs.
    pub held_names: Mutex<BTreeSet<String>>,
}

impl FaultScript {
    pub(crate) fn new(actions: Vec<FaultAction>) -> Arc<Self> {
        Arc::new(Self {
            actions: Mutex::new(actions.into_iter().collect()),
            launches: Mutex::new(Vec::new()),
            held_names: Mutex::new(BTreeSet::new()),
        })
    }

    pub(crate) fn launch_times(&self) -> Vec<Duration> {
        let launches = self.launches.lock().expect("fault script poisoned");
        let mut times = Vec::new();
        for window in launches.windows(2) {
            times.push(window[1] - window[0]);
        }
        times
    }

    pub(crate) fn launch_count(&self) -> usize {
        self.launches.lock().expect("fault script poisoned").len()
    }

    pub(crate) fn holds(&self, name: &str) -> bool {
        self.held_names
            .lock()
            .expect("fault script poisoned")
            .contains(name)
    }
}

pub(crate) struct FaultAdapter {
    pub name: &'static str,
    pub service: &'static str,
    pub script: Arc<FaultScript>,
}

impl Default for FaultAdapter {
    fn default() -> Self {
        Self {
            name: "fault",
            service: "fault",
            script: FaultScript::new(Vec::new()),
        }
    }
}

impl Adapter for FaultAdapter {
    fn name(&self) -> &'static str {
        self.name
    }

    fn bus_service(&self) -> &'static str {
        self.service
    }

    fn run(self: Box<Self>, mut ctx: AdapterCtx) -> BoxRunFuture {
        Box::pin(async move {
            let action = self
                .script
                .actions
                .lock()
                .expect("fault script poisoned")
                .pop_front()
                .unwrap_or(FaultAction::RunForever);
            self.script
                .launches
                .lock()
                .expect("fault script poisoned")
                .push(tokio::time::Instant::now());
            let _lease = NameLease::acquire(&self.script, self.name);
            match action {
                FaultAction::Panic => panic!("fault adapter {}: scripted panic", self.name),
                FaultAction::Fail(reason) => Err(anyhow!("scripted failure: {reason}")),
                FaultAction::NeedSessionBus => {
                    match ctx.session_bus().address() {
                        Err(reason) => Err(anyhow!("session bus unavailable: {reason}")),
                        #[cfg(feature = "cosmix")]
                        Ok(_) => {
                            // A real adapter would dial here and serve on
                            // success; the dial failure path is what J1
                            // proves — unreachable bus lands in backoff.
                            ctx.signal_ready();
                            let connection = ctx.connect_session_bus().await?;
                            drop(connection);
                            park_until_stopped(&mut ctx).await
                        }
                        #[cfg(not(feature = "cosmix"))]
                        Ok(address) => Err(anyhow!(
                            "session bus present ({address}) but this build cannot dial it"
                        )),
                    }
                }
                FaultAction::RunFor(lifetime) => {
                    ctx.signal_ready();
                    let stopped = wait_lifetime(&mut ctx, lifetime).await;
                    if stopped {
                        Ok(())
                    } else {
                        Err(anyhow!(
                            "scripted expiry after {} s",
                            lifetime.as_secs_f64()
                        ))
                    }
                }
                FaultAction::RunForever => {
                    ctx.signal_ready();
                    park_until_stopped(&mut ctx).await
                }
            }
        })
    }
}

async fn park_until_stopped(ctx: &mut AdapterCtx) -> Result<()> {
    let mut shutdown = ctx.shutdown().clone();
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                changed.map_err(|_| anyhow!("stop channel ended"))?;
                if *shutdown.borrow_and_update() {
                    return Ok(());
                }
            }
        }
    }
}

/// Serve for `lifetime`, then expire (a failure). Returns early with
/// `true` when stopped first.
async fn wait_lifetime(ctx: &mut AdapterCtx, lifetime: Duration) -> bool {
    let mut shutdown = ctx.shutdown().clone();
    tokio::select! {
        changed = shutdown.changed() => {
            changed.is_ok() && *shutdown.borrow_and_update()
        }
        _ = tokio::time::sleep(lifetime) => false,
    }
}

/// Stand-in for the Bus service + zbus names a real adapter owns:
/// acquired at run start, released by Drop — including on panic unwind.
struct NameLease {
    script: Arc<FaultScript>,
    name: &'static str,
}

impl NameLease {
    fn acquire(script: &Arc<FaultScript>, name: &'static str) -> Self {
        script
            .held_names
            .lock()
            .expect("fault script poisoned")
            .insert(name.to_string());
        Self {
            script: Arc::clone(script),
            name,
        }
    }
}

impl Drop for NameLease {
    fn drop(&mut self) {
        self.script
            .held_names
            .lock()
            .expect("fault script poisoned")
            .remove(self.name);
    }
}
