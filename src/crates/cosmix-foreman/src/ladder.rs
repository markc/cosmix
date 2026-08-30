//! The escalation ladder: route a task to a model tier by configured entry
//! rung × failure history, not by availability or task risk. A task starts at
//! the fleet's entry rung, climbs after the configured number of
//! verifier/review charges, and PARKS (blocker finding, unclaimable until an
//! operator requeues) when the ladder is exhausted — a task the top tier
//! cannot land twice needs a human decision, not an infinite retry loop.
//!
//! [`crate::config::FleetPolicy`] resolves the ladder from strict-data fleet
//! config, with environment variables retained only as one-shot overrides.

use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::BTreeMap;

use crate::executor::AgentKind;
use crate::ledger::{Ledger, Task};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Rung {
    pub agent: AgentKind,
    pub model: Option<String>,
}

impl std::fmt::Display for Rung {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.model {
            Some(m) => write!(f, "{}:{m}", self.agent.as_str()),
            None => write!(f, "{}", self.agent.as_str()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Ladder {
    pub rungs: Vec<Rung>,
    /// First rung used by every task, independent of risk.
    pub start_rung: usize,
    /// Verifier-red/review-rejected charges tolerated per rung before climbing (≥ 1).
    pub patience: u32,
    /// Optional patience overrides keyed by full rung (`claude:sonnet`) or
    /// agent family (`glm`). Full-rung keys win.
    pub per_rung_patience: BTreeMap<String, u32>,
}

impl Default for Ladder {
    fn default() -> Self {
        Ladder {
            rungs: vec![
                Rung {
                    agent: AgentKind::Glm,
                    model: None,
                },
                Rung {
                    agent: AgentKind::Claude,
                    model: Some("sonnet".into()),
                },
                Rung {
                    agent: AgentKind::Claude,
                    model: Some("opus".into()),
                },
            ],
            start_rung: 0,
            patience: 2,
            per_rung_patience: BTreeMap::new(),
        }
    }
}

impl Ladder {
    /// The rung a task should run at, or `None` when the ladder is
    /// exhausted for it. `failures` is the task's once-per-attempt ladder
    /// charge counter (verifier-red plus review-rejected).
    /// `risk` is retained for API compatibility but does not affect routing;
    /// risk governs landing review policy instead.
    /// A hand-built `patience: 0` is treated as 1 rather than dividing by
    /// zero.
    pub fn rung_for(&self, _risk: &str, failures: i64) -> Option<&Rung> {
        self.rung_index_for(failures)
            .and_then(|index| self.rungs.get(index))
    }

    fn rung_index_for(&self, failures: i64) -> Option<usize> {
        let mut remaining = failures.max(0) as usize;
        for (index, rung) in self.rungs.iter().enumerate().skip(self.start_rung) {
            let patience = self.patience_for(rung) as usize;
            if remaining < patience {
                return Some(index);
            }
            remaining = remaining.saturating_sub(patience);
        }
        None
    }

    fn patience_for(&self, rung: &Rung) -> u32 {
        self.per_rung_patience
            .get(&rung.to_string())
            .or_else(|| self.per_rung_patience.get(rung.agent.as_str()))
            .copied()
            .unwrap_or(self.patience)
            .max(1)
    }
}

/// Route a task from its charged rung, skipping any exact lane/rung which a
/// previous pre-claim attempt proved unable to enforce. Those refusals are
/// findings, not quality charges, so they never alter `ladder_failures`.
pub fn rung_for_task<'a>(
    ledger: &Ledger,
    ladder: &'a Ladder,
    task: &Task,
) -> Result<Option<&'a Rung>> {
    let Some(start) = ladder.rung_index_for(task.ladder_failures) else {
        return Ok(None);
    };
    for rung in ladder.rungs.iter().skip(start) {
        if !ledger.task_rung_refused(task.id, &rung.to_string())? {
            return Ok(Some(rung));
        }
    }
    Ok(None)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParkCause {
    LadderExhausted,
    RungsRefused,
}

pub fn park_cause(ladder: &Ladder, task: &Task) -> ParkCause {
    if ladder.rung_index_for(task.ladder_failures).is_some() {
        ParkCause::RungsRefused
    } else {
        ParkCause::LadderExhausted
    }
}

/// Parse an `agent[:model],…` rung list (the FOREMAN_LADDER format).
/// Malformed pieces (empty rungs, empty models) are errors — a typo must
/// not become an empty `--model` argument.
pub fn parse_ladder(spec: &str) -> Result<Vec<Rung>> {
    let mut rungs = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        anyhow::ensure!(!part.is_empty(), "empty rung in ladder spec {spec:?}");
        let (agent, model) = match part.split_once(':') {
            Some((a, m)) => {
                anyhow::ensure!(!m.trim().is_empty(), "empty model in rung {part:?}");
                (a, Some(m.trim().to_string()))
            }
            None => (part, None),
        };
        rungs.push(Rung {
            agent: agent.parse().map_err(|e: String| anyhow::anyhow!(e))?,
            model,
        });
    }
    anyhow::ensure!(!rungs.is_empty(), "ladder has no rungs");
    Ok(rungs)
}

/// What the dispatcher decided for one planning pass.
#[derive(Debug)]
pub enum Dispatch {
    /// Run this task at this rung.
    Run { task: Box<Task>, rung: Rung },
    /// The named task was parked because quality exhausted the ladder or all
    /// remaining rungs were refused; a cause-specific finding was filed.
    Parked {
        task_id: i64,
        failures: i64,
        cause: ParkCause,
    },
    /// Nothing ready.
    Idle,
}

/// One planning pass: the decision plus every task parked in passing —
/// callers must surface parked ids, not swallow them into a clean exit.
#[derive(Debug)]
pub struct PlanOutcome {
    pub decision: Dispatch,
    pub parked: Vec<ParkedTask>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParkedTask {
    pub task_id: i64,
    pub failures: i64,
    pub cause: ParkCause,
}

/// Pick the next ready task (readiness, deps, and `kind` apply to pinned
/// tasks too) and route it. With `park_exhausted`, tasks whose ladder is
/// exhausted or whose remaining rungs are all refused are parked (guarded,
/// with a cause-specific blocker finding, skipped if a concurrent claim raced
/// us) as they are encountered; without it (dry runs) the pass is read-only.
/// `exclude` skips tasks already dispatched this invocation so a re-bounced
/// task cannot starve the queue.
pub fn plan(
    ledger: &Ledger,
    ladder: &Ladder,
    task_id: Option<i64>,
    kind: Option<&str>,
    park_exhausted: bool,
    exclude: &std::collections::HashSet<i64>,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<PlanOutcome> {
    let mut candidates = ledger.ready_tasks_at(kind, now)?;
    if let Some(id) = task_id {
        candidates.retain(|t| t.id == id);
        if candidates.is_empty() {
            let task = ledger.task(id)?.with_context(|| format!("no task {id}"))?;
            if task.operator_driven {
                anyhow::bail!("task {id} not ready: operator-driven");
            }
            anyhow::bail!(
                "task {id} is not dispatchable (status {}, kind {:?}, deps {:?} — \
                 must be an unclaimed queued/bounced/failed task with all deps done)",
                task.status,
                task.kind,
                task.deps
            );
        }
    }
    let mut parked = Vec::new();
    for task in candidates {
        if exclude.contains(&task.id) {
            continue;
        }
        match rung_for_task(ledger, ladder, &task)? {
            Some(rung) => {
                return Ok(PlanOutcome {
                    decision: Dispatch::Run {
                        rung: rung.clone(),
                        task: Box::new(task),
                    },
                    parked,
                });
            }
            None => {
                let failures = task.ladder_failures;
                let cause = park_cause(ladder, &task);
                if park_exhausted {
                    if crate::ledger::ledger_write_with_busy_retry(
                        "parking unroutable task",
                        || match cause {
                            ParkCause::LadderExhausted => {
                                ledger.park_task(task.id, failures, &task.risk)
                            }
                            ParkCause::RungsRefused => {
                                ledger.park_task_rungs_refused(task.id, failures, &task.risk)
                            }
                        },
                    )? {
                        parked.push(ParkedTask {
                            task_id: task.id,
                            failures,
                            cause,
                        });
                    } else if task_id.is_some() {
                        // Pinned and the guarded park lost a race: the task
                        // changed under us — say so instead of reporting a
                        // park that did not happen.
                        anyhow::bail!(
                            "task {} changed state while planning (claimed, requeued, \
                             or already parked) — re-run dispatch",
                            task.id
                        );
                    }
                }
                if task_id.is_some() {
                    return Ok(PlanOutcome {
                        decision: Dispatch::Parked {
                            task_id: task.id,
                            failures,
                            cause,
                        },
                        parked,
                    });
                }
            }
        }
    }
    Ok(PlanOutcome {
        decision: Dispatch::Idle,
        parked,
    })
}
