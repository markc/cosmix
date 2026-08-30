//! Runs one task through one driver: claim, spawn, stream events into the
//! ledger, enforce runner-side budgets (the caps the vendor CLI has no flag
//! for), and disposition the task from the outcome. The verdict here is only
//! "the agent finished" — the verifier pipeline (Phase 1) is what decides
//! whether the work is any good.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::clock::{RunClock, SystemClock};
use crate::executor::{
    AgentEvent, Budget, Executor, ResumeFailure, RunOutcome, StopReason, Workspace,
};
use crate::ledger::{
    CLAIM_HEARTBEAT_SECS, ClaimToken, FindingReason, Ledger, ledger_cleanup_write_with_busy_retry,
    ledger_run_event_write_with_busy_retry, ledger_write_with_busy_retry,
    sqlite_busy_retries_exhausted,
};
use crate::lowering::{build_prompt, build_retry_turn};
use crate::wake;

const EXACT_PRE_MODEL_NOT_FOUND: &str = "exact_pre_model_session_not_found";

#[cfg(test)]
std::thread_local! {
    static FAIL_AFTER_FALLBACK_JOURNAL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn fail_after_fallback_journal_for_test() {
    FAIL_AFTER_FALLBACK_JOURNAL.with(|fail| fail.set(true));
}

pub struct RunOptions {
    pub workdir: PathBuf,
    pub budget: Budget,
    pub model: Option<String>,
    /// Launch-time resume candidate selected before the run reservation.
    /// The runner checks it against the prior ledger row and passes it only
    /// through `Executor::resume`; drivers themselves retain no implicit id.
    pub resume_session: Option<String>,
    /// Max seconds with no event before the session is declared stalled and
    /// killed. Distinct from the wall-clock cap in `budget`.
    pub stall_secs: u64,
    /// Print events to stdout as they arrive.
    pub echo: bool,
    /// Gate `done` on the task's tier-0 verifier (the anti-gaming rule:
    /// every path to done runs the spec-owned profile). Off is an explicit
    /// operator override, not a default.
    pub verify: bool,
    /// Branch the work lands on, recorded after the claim (claiming resets
    /// the workspace fields precisely so stale branches cannot survive into
    /// a new attempt).
    pub branch: Option<String>,
    /// Buildable workspace subdirectory the tier-0 verifier runs in,
    /// relative to the workdir (for cos: "src" — the repo root has no
    /// Cargo.toml, so verifying there is red no matter how good the work
    /// is). The agent still gets the whole workdir; only verify moves.
    pub verify_subdir: Option<String>,
    /// The explicit operator-run path may claim tasks reserved from
    /// unattended dispatch. Dispatch and MCP claiming leave this false.
    /// Defaults to false: the reservation bypass is an opt-in, not the
    /// baseline, so a caller that forgets to set it stays fail-closed.
    pub allow_operator_driven: bool,
    /// Base of record: the integration commit this attempt's branch was
    /// replayed onto during worktree provisioning. Recorded as the run's
    /// first event so the trail answers "which tree was this attempt
    /// actually testing?" — `events.run_id` is NOT NULL, so the run is the
    /// only place a per-attempt fact like this can live.
    pub rebased_onto: Option<String>,
    /// Operator-authored profiles from the active project manifest.
    pub profiles: Vec<crate::verify::Profile>,
    /// The target project's manifest-supplied instruction pack, spliced into
    /// the prompt by `lowering::build_prompt`. Empty when no `--project`
    /// manifest is in play, in which case no project section is rendered.
    pub project_pack: String,
}

impl Default for RunOptions {
    fn default() -> Self {
        RunOptions {
            workdir: PathBuf::from("."),
            budget: Budget::default(),
            model: None,
            resume_session: None,
            stall_secs: 600,
            echo: false,
            verify: true,
            branch: None,
            verify_subdir: None,
            allow_operator_driven: false,
            rebased_onto: None,
            profiles: Vec::new(),
            project_pack: String::new(),
        }
    }
}

pub struct RunReport {
    pub run_id: i64,
    pub outcome: RunOutcome,
    pub task_status: &'static str,
    pub duration_ms: i64,
}

/// Mint the one fresh, run-scoped nonce that lowering uses to fence findings.
/// An earlier agent cannot forge a nonce derived from OS-seeded hash state and
/// time after its output was recorded. Lowering deliberately takes this value
/// as a parameter and never mints a second one.
fn mint_nonce(run_id: i64) -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let mut first = RandomState::new().build_hasher();
    first.write_i64(run_id);
    first.write_u128(nanos);
    let a = first.finish();
    let mut second = RandomState::new().build_hasher();
    second.write_u64(a);
    second.write_i64(run_id);
    let b = second.finish();
    format!("{run_id:x}.{a:016x}{b:016x}")
}

pub fn run_task(
    ledger: &Ledger,
    task_id: i64,
    executor: &dyn Executor,
    opts: &RunOptions,
) -> Result<RunReport> {
    let clock = SystemClock::new();
    run_task_with_clock_and_policy(
        ledger,
        task_id,
        executor,
        opts,
        &clock,
        &crate::config::FleetPolicy::defaults(),
    )
}

/// CLI path: the verifier uses the same immutable policy snapshot that
/// governed and routed the run.
pub fn run_task_with_policy(
    ledger: &Ledger,
    task_id: i64,
    executor: &dyn Executor,
    opts: &RunOptions,
    policy: &crate::config::FleetPolicy,
) -> Result<RunReport> {
    let clock = SystemClock::new();
    run_task_with_clock_and_policy(ledger, task_id, executor, opts, &clock, policy)
}

/// Clock-injected full runner path used by golden-stream replay. All
/// runner-owned time decisions and ledger timestamps flow through `clock`;
/// production enters through [`run_task`] with [`SystemClock`].
pub fn run_task_with_clock(
    ledger: &Ledger,
    task_id: i64,
    executor: &dyn Executor,
    opts: &RunOptions,
    clock: &dyn RunClock,
) -> Result<RunReport> {
    run_task_with_clock_and_policy(
        ledger,
        task_id,
        executor,
        opts,
        clock,
        &crate::config::FleetPolicy::defaults(),
    )
}

fn run_task_with_clock_and_policy(
    ledger: &Ledger,
    task_id: i64,
    executor: &dyn Executor,
    opts: &RunOptions,
    clock: &dyn RunClock,
    policy: &crate::config::FleetPolicy,
) -> Result<RunReport> {
    // Anything refusable must fail HERE — after the claim it would burn an
    // attempt and strand the task claimed without an agent ever starting.
    executor.check_budget(&opts.budget)?;
    if let Some(b) = &opts.branch {
        anyhow::ensure!(
            crate::ledger::valid_branch_name(b),
            "invalid branch name {b:?}"
        );
    }
    let this_pid = std::process::id();
    let claimant = format!("{}@{this_pid}", executor.kind().as_str());
    let claimed_at = clock.wall_now().to_rfc3339();
    let (task, run_id) = ledger_write_with_busy_retry("claiming task and starting run", || {
        ledger.start_attempt_at(
            task_id,
            &claimant,
            // Read directly from the OS, not parsed back out of `claimant`
            // above — this is the one call site `Ledger::reap_dead_claims`
            // trusts to name the real claim holder.
            Some(this_pid as i64),
            opts.workdir.to_str(),
            opts.branch.as_deref(),
            executor.kind().as_str(),
            opts.model.as_deref(),
            opts.budget.max_budget_usd,
            &claimed_at,
            opts.allow_operator_driven,
        )
    })?;
    let started = clock.monotonic();

    // Whatever usage streamed before a mid-run failure is real spend, and
    // `drive`'s own accumulator dies with the error — hold it out here so the
    // Error outcome (and the run row `finish_run` writes from it) reports it
    // instead of zeros.
    let mut streamed: Option<crate::executor::Usage> = None;
    let mut claim_released = false;
    let run_result = (|| -> Result<RunReport> {
        // Resolved INSIDE the closure for the same reason as every ledger
        // write below: the claim is already committed by the time this runs,
        // so an unknown or removed verifier profile is a post-claim
        // run-ending failure. Resolved above the closure its `?` returned
        // straight out of the function, leaving the task claimed and
        // `running` until the six-hour lease expired — and, while the
        // supervisor's pid stayed alive, the reaper deliberately never
        // touches it, so the strand outlived the run. It cannot be resolved
        // BEFORE the claim either: the profile name is a column of the task
        // this call claims, and reading it unclaimed would be a different
        // (raceable) task's answer.
        let profile = opts
            .profiles
            .iter()
            .find(|profile| profile.name == task.verifier_profile)
            .cloned()
            .map(Ok)
            .unwrap_or_else(|| crate::verify::lookup_profile(&task.verifier_profile))?;
        let nonce = mint_nonce(run_id);
        let prompt = build_prompt(ledger, &task, &nonce, &opts.project_pack, &profile);
        let retry_turn = build_retry_turn(ledger, &task, &nonce);
        let resume_ref = if task.attempt > 1 && executor.capabilities().resume {
            ledger
                .last_run_ref(task_id, "implement", None, run_id)?
                .filter(|prior| {
                    prior.agent == executor.kind().as_str()
                        && prior.model.as_deref() == opts.model.as_deref()
                })
                .and_then(|prior| prior.session_ref.map(|session| (prior.id, session)))
        } else {
            None
        };
        if let Some(configured) = opts.resume_session.as_deref() {
            let discovered = resume_ref
                .as_ref()
                .map(|(_, session)| session.as_str())
                .context("launch-time resume id has no matching prior same-rung run")?;
            anyhow::ensure!(
                discovered == configured,
                "launch-time resume id {configured:?} disagrees with prior same-rung id {discovered:?}"
            );
        }
        let ws = Workspace {
            dir: opts.workdir.clone(),
            verify_subdir: profile.workspace_subdir(opts.verify_subdir.as_deref()),
        };
        // Shared by the rebase bookend, a discarded resume, its fallback
        // marker and the fresh process. Event sequence numbers never restart
        // inside one run row.
        let mut seq = 0_i64;
        // Seq 0, ahead of the agent stream (which starts at 1): the base of
        // record for this attempt. Without it "the gate was green on task N"
        // does not say WHICH tree was green — the whole reason the branch is
        // replayed onto the integration head during provisioning. This write
        // must fail INSIDE the closure, same as every other ledger write in
        // this run: a failure here still ends the run, and the outer match
        // below is the only thing that releases the claim on the way out. A
        // failure recorded before entering this closure would return early
        // with the task still claimed and `running` — the exact phantom-claim
        // gap task 94 closed (a real occurrence: run 425 died at exactly this
        // write and stranded task 70 for 31 hours, while run 535's write
        // failure inside `drive` below released task 82's claim correctly).
        let rebase_failure = if let Some(base) = &opts.rebased_onto {
            let payload = serde_json::json!({ "base": base, "branch": opts.branch }).to_string();
            let event_at = clock.wall_now().to_rfc3339();
            ledger_run_event_write_with_busy_retry("recording rebased task base", || {
                ledger.record_event_at(run_id, 0, "rebase", &payload, &event_at)
            })
            .err()
        } else {
            None
        };
        let drive_once = |turn: &str,
                          resume: Option<&str>,
                          budget: &Budget,
                          streamed: &mut Option<crate::executor::Usage>,
                          seq: &mut i64|
         -> Result<(RunOutcome, Option<&'static str>)> {
            match drive(
                ledger,
                task_id,
                ClaimToken {
                    owner: &claimant,
                    generation: task.attempt,
                },
                run_id,
                executor,
                turn,
                resume,
                &ws,
                budget,
                opts,
                streamed,
                seq,
                clock,
            ) {
                Ok(outcome) => {
                    let delivery =
                        (outcome.stop == StopReason::Interrupted).then_some("harness_error");
                    Ok((outcome, delivery))
                }
                Err(err) if sqlite_busy_retries_exhausted(&err) => Err(err),
                Err(err) => {
                    let mut outcome = errored_outcome(&err, streamed.clone());
                    // `record_run_resume_intent` already made the requested
                    // thread durable. Preserve it through the terminal write
                    // when the harness failed before obtaining any stronger
                    // session identity from the stream.
                    if streamed.is_none() {
                        outcome.session_ref = resume.map(str::to_owned);
                    }
                    Ok((outcome, Some("harness_error")))
                }
            }
        };
        let (mut outcome, mut delivery_override) = if let Some(err) = rebase_failure {
            if sqlite_busy_retries_exhausted(&err) {
                return Err(err);
            }
            (
                errored_outcome(&err, streamed.clone()),
                Some("harness_error"),
            )
        } else {
            if let Some((_, requested)) = &resume_ref {
                ledger_write_with_busy_retry("recording implementer resume intent", || {
                    ledger.record_run_resume_intent(run_id, requested)
                })?;
            }
            let before = resume_ref
                .as_ref()
                .and_then(|_| crate::executor::workspace_fingerprint(&ws.dir));
            let first = drive_once(
                if resume_ref.is_some() {
                    &retry_turn
                } else {
                    &prompt
                },
                resume_ref.as_ref().map(|(_, session)| session.as_str()),
                &opts.budget,
                &mut streamed,
                &mut seq,
            )?;
            match &resume_ref {
                Some((prior_run_id, requested))
                    if first
                        .0
                        .resume_failure
                        .is_some_and(ResumeFailure::permits_fresh_fallback)
                        && !first.0.output_observed
                        && !first.0.usage_observed
                        && streamed.is_none()
                        && before.is_some()
                        && before == crate::executor::workspace_fingerprint(&ws.dir) =>
                {
                    let cause = first.0.resume_failure.expect("guarded above");
                    let spend_evidence = (cause == ResumeFailure::SessionNotFound)
                        .then_some(EXACT_PRE_MODEL_NOT_FOUND);
                    let elapsed = clock.monotonic().saturating_sub(started);
                    if let Some(fallback_budget) = remaining_runner_budget(
                        &opts.budget,
                        &first.0.usage,
                        elapsed,
                        spend_evidence,
                    ) {
                        seq += 1;
                        let payload = serde_json::json!({
                            "requested_session_ref": requested,
                            "cause": cause.as_str(),
                            "first_process_usage": first.0.usage,
                            "elapsed_ms": elapsed.as_millis(),
                            "spend_evidence": spend_evidence,
                        })
                        .to_string();
                        let event_at = clock.wall_now().to_rfc3339();
                        ledger_run_event_write_with_busy_retry(
                            "recording resume fallback",
                            || {
                                ledger.record_resume_fallback_and_retire_current_at(
                                    run_id, seq, &payload, requested, &event_at,
                                )
                            },
                        )?;
                        #[cfg(test)]
                        FAIL_AFTER_FALLBACK_JOURNAL.with(|fail| -> Result<()> {
                            anyhow::ensure!(
                                !fail.replace(false),
                                "injected crash after resume fallback journal"
                            );
                            Ok(())
                        })?;
                        ledger_write_with_busy_retry("retiring dead resume session", || {
                            ledger.mark_run_session_dead(*prior_run_id, requested)
                        })?;
                        if opts.echo {
                            eprintln!(
                                "foreman: {} for session {requested}; starting fresh",
                                cause.as_str()
                            );
                        }
                        drive_once(&prompt, None, &fallback_budget, &mut streamed, &mut seq)?
                    } else {
                        first
                    }
                }
                _ => first,
            }
        };
        if !executor.kind().meters_dollars() {
            // The claude CLI prices the remapped tier at Anthropic rates; for
            // Z.ai traffic that number is fiction — do not let it pollute the
            // spend ledger. Same predicate the governor reserves against, so
            // the two cannot drift apart again.
            outcome.usage.cost_usd = None;
        }

        let duration_ms = i64::try_from(clock.monotonic().saturating_sub(started).as_millis())
            .context("run duration exceeds the ledger's i64 millisecond range")?;
        // A killed background helper is not itself proof that work was
        // abandoned: Claude Code also tears down harmless wait/poll helpers
        // after agents that committed and landed successfully. Where the
        // branch contract applies, its dirty tree is the delivery arbiter.
        let background_signal = agent_abandoned_background(&outcome);
        let branch_dirt = opts
            .branch
            .as_ref()
            .and_then(|_| worktree_dirt(&opts.workdir));
        let abandoned_background =
            background_signal && (opts.branch.is_none() || branch_dirt.is_some());
        if background_signal && !abandoned_background && outcome.result.is_some() {
            // The stream signal was real, but the committed branch is the
            // stronger outcome evidence. Restore the successful result the
            // parser held while surfacing the task bookend to this runner.
            outcome.stop = StopReason::Done;
            outcome.error = None;
            delivery_override = None;
        }
        if abandoned_background {
            let finished_at = clock.wall_now().to_rfc3339();
            let evidence = outcome
                .error
                .as_deref()
                .unwrap_or(crate::driver::claude::AGENT_ABANDONED_BACKGROUND);
            // Release the claim first, atomically with the bounded counter and
            // single finding. If later run-row bookkeeping is interrupted,
            // the task is still recoverable rather than stranded as running.
            let (_, parked) =
                ledger_write_with_busy_retry("disposing abandoned-background run", || {
                    ledger.finish_abandoned_background_at(
                        task_id,
                        ClaimToken {
                            owner: &claimant,
                            generation: task.attempt,
                        },
                        evidence,
                        &finished_at,
                    )
                })?;
            claim_released = true;
            ledger_write_with_busy_retry("finishing abandoned-background run", || {
                ledger.finish_run_as(run_id, &outcome, duration_ms, Some("harness_error"))
            })?;
            ledger_write_with_busy_retry("recording abandoned-background quality", || {
                ledger.set_run_quality(run_id, crate::driver::claude::AGENT_ABANDONED_BACKGROUND)
            })?;
            let task_status = if parked { "parked" } else { "queued" };
            if opts.echo {
                eprintln!(
                    "foreman: background Bash was abandoned with uncommitted work — task {task_status}"
                );
            }
            wake::fire(wake::WAKE_VERB);
            return Ok(RunReport {
                run_id,
                outcome,
                task_status,
                duration_ms,
            });
        }
        ledger_write_with_busy_retry("finishing run", || {
            ledger.finish_run_as(run_id, &outcome, duration_ms, delivery_override)
        })?;
        let mut task_status = match outcome.stop {
            StopReason::Done => "done",
            StopReason::BudgetCeiling => "bounced",
            StopReason::Interrupted => "bounced",
            StopReason::Error => "failed",
        };
        let mut disposition_reason = match outcome.stop {
            StopReason::Error | StopReason::Interrupted => Some(FindingReason::InfraRefusal),
            _ => None,
        };
        // The branch contract is otherwise prompt-only: an agent that finishes
        // without committing would pass tier-0 on its dirty tree, go done, and
        // die much later in the refinery with a misleading "content-free
        // landing" bounce. Fail fast here with the honest cause instead.
        if task_status == "done"
            && let Some(expected) = &opts.branch
        {
            if let Some(dirt) = branch_dirt.as_deref() {
                task_status = "bounced";
                disposition_reason = Some(FindingReason::BranchContract);
                ledger_write_with_busy_retry("recording branch-contract quality", || {
                    ledger.set_run_quality(run_id, "branch_contract_failed")
                })?;
                ledger_write_with_busy_retry("recording branch-contract finding", || {
                    ledger.file_finding_reasoned(
                        Some(task_id),
                        "major",
                        "agent left uncommitted work",
                        &format!(
                            "the branch contract requires all work committed; \
                         `git status --porcelain` in the task worktree:\n{dirt}"
                        ),
                        "runner",
                        FindingReason::BranchContract,
                    )
                })?;
                if opts.echo {
                    eprintln!("foreman: uncommitted work in the task worktree — bouncing:\n{dirt}");
                }
            } else if let Some(actual) = current_branch(&opts.workdir)
                && actual != *expected
            {
                // A clean tree on the WRONG branch is the other contract break:
                // the commits exist, but the refinery lands `expected` — which
                // is still at base — and would report a misleading content-free
                // landing much later.
                task_status = "bounced";
                disposition_reason = Some(FindingReason::BranchContract);
                ledger_write_with_busy_retry("recording branch-contract quality", || {
                    ledger.set_run_quality(run_id, "branch_contract_failed")
                })?;
                ledger_write_with_busy_retry("recording branch-contract finding", || {
                    ledger.file_finding_reasoned(
                        Some(task_id),
                        "major",
                        "agent left the task branch",
                        &format!(
                            "work was committed on `{actual}` but the refinery lands \
                         `{expected}`; the task branch never moved"
                        ),
                        "runner",
                        FindingReason::BranchContract,
                    )
                })?;
                if opts.echo {
                    eprintln!(
                        "foreman: worktree is on `{actual}`, not the task branch \
                     `{expected}` — bouncing"
                    );
                }
            }
        }
        // The agent finishing is not the work passing: every path to done runs
        // the task's spec-owned tier-0 profile. A verifier that cannot RUN
        // (missing binary, bad profile) fails the task with the cause recorded —
        // propagating would strand the task claimed-and-running forever.
        if task_status == "done" && opts.verify {
            // The agent stream may have ended close to its current deadline.
            // Refresh before the potentially two-hour verifier so slice B
            // cannot expire healthy work while the runner is still gating it.
            let heartbeat_at = clock.wall_now().to_rfc3339();
            ledger_write_with_busy_retry("renewing claim before verification", || {
                ledger
                    .renew_claim_at(
                        task_id,
                        ClaimToken {
                            owner: &claimant,
                            generation: task.attempt,
                        },
                        &heartbeat_at,
                    )
                    .map(|_| ())
            })?;
            // The verify dir escaping the worktree is laundering, not weather:
            // a committed symlink at the subdir path would point tier-0 at
            // unrelated green code while the contract checks (which inspect
            // the real tree) stay clean. Refused loudly, like a verifier that
            // cannot run.
            let gate = crate::verify::GateRequest::local(
                task_id,
                task.attempt,
                crate::verify::GateIdentity::RunnerCompletion,
                0,
                &opts.workdir,
                &profile,
                opts.verify_subdir.as_deref(),
                &task.crates,
                policy,
            )
            .and_then(|request| {
                crate::verify::GateRunner::run_gate(&crate::verify::LOCAL_GATE_RUNNER, &request)
            });
            match gate {
                Ok(report) => {
                    let report_json = serde_json::to_string(&report)?;
                    ledger_write_with_busy_retry("recording run verification", || {
                        ledger.record_run_verification(
                            task_id,
                            run_id,
                            0,
                            report.pass,
                            &report_json,
                        )
                    })?;
                    let sccache_bypasses = report.sccache_bypass_digests();
                    ledger_write_with_busy_retry("recording sccache bypass finding", || {
                        ledger
                            .file_sccache_bypass_findings_claimed(
                                task_id,
                                ClaimToken {
                                    owner: &claimant,
                                    generation: task.attempt,
                                },
                                &sccache_bypasses,
                                "runner",
                            )
                            .map(|_| 0)
                    })?;
                    if report.pass {
                        if opts.echo {
                            println!("foreman: tier-0 green (profile: {})", report.profile);
                        }
                    } else {
                        task_status = "bounced";
                        disposition_reason = Some(FindingReason::VerifierRed);
                        ledger_write_with_busy_retry("recording tier-0 failure finding", || {
                            ledger.file_finding_reasoned(
                                Some(task_id),
                                "major",
                                "tier-0 red after agent run",
                                &report.failure_digest(),
                                "runner",
                                FindingReason::VerifierRed,
                            )
                        })?;
                        if opts.echo {
                            eprintln!(
                                "foreman: tier-0 verifier ({}) red — bouncing:\n{}",
                                report.profile,
                                report.failure_digest()
                            );
                        }
                    }
                }
                Err(engine) => {
                    task_status = "failed";
                    disposition_reason = Some(FindingReason::InfraRefusal);
                    ledger_write_with_busy_retry(
                        "recording verifier infrastructure finding",
                        || {
                            ledger.file_finding_reasoned(
                                Some(task_id),
                                "major",
                                "tier-0 verifier could not run",
                                &format!("{engine:#}"),
                                "runner",
                                FindingReason::InfraRefusal,
                            )
                        },
                    )?;
                    if opts.echo {
                        eprintln!("foreman: verifier could not run: {engine:#}");
                    }
                }
            }
        }
        // Guarded by claimant: if an operator requeued this task mid-run and
        // another agent claimed it, this refuses rather than clobbering them.
        let finished_at = clock.wall_now().to_rfc3339();
        let infra_detail = (disposition_reason == Some(FindingReason::InfraRefusal)).then(|| {
            outcome
                .error
                .as_deref()
                .unwrap_or("vendor or harness failure")
        });
        let infra_threshold = if disposition_reason == Some(FindingReason::InfraRefusal) {
            crate::ledger::infra_refusal_finding_threshold()?
        } else {
            1
        };
        let infra_park_threshold = if disposition_reason == Some(FindingReason::InfraRefusal) {
            crate::ledger::infra_refusal_park_threshold()?
        } else {
            1
        };
        let disposition = ledger_write_with_busy_retry("completing task", || {
            ledger.finish_task_classified_at(
                task_id,
                ClaimToken {
                    owner: &claimant,
                    generation: task.attempt,
                },
                run_id,
                task_status,
                disposition_reason,
                infra_detail,
                infra_threshold,
                infra_park_threshold,
                i64::from(policy.branch_contract_limit.value),
                &finished_at,
            )
        })?;
        // The claim is gone as of that commit. Anything below which fails
        // must NOT try to release it again: the outer arm's release is
        // generation-guarded and would report a false "still claimed,
        // needs an operator" for a task that is already dispositioned.
        claim_released = true;
        let task_status = disposition.status.as_db_str();
        if opts.echo {
            println!(
                "[disposition attempt={} status={} ladder_charge={} reason={}]",
                task.attempt,
                task_status,
                i64::from(disposition.charged),
                disposition_reason
                    .map(|reason| reason.as_db_str())
                    .unwrap_or("none")
            );
        }
        // Best-effort ABP wake — see wake.rs. `done` can unblock dependents;
        // `bounced`/`failed` re-enter the dispatchable set themselves. Either
        // way the supervisor may have new work before its backstop timer fires.
        wake::fire(wake::WAKE_VERB);
        Ok(RunReport {
            run_id,
            outcome,
            task_status,
            duration_ms,
        })
    })();

    match run_result {
        Err(error) => {
            // ANY error escaping the closure ends this run, so any error
            // escaping it must also dispose of the claim — including the
            // ones raised while REPORTING an outcome that was otherwise
            // decided (a finding write, a verification record, the run-row
            // finish). Those are ordinary `?` escapes, not SQLite weather,
            // and this arm used to match only busy-exhausted errors: every
            // other failure fell through to the pass-through arm below and
            // left the task claimed and `running` with nothing behind it —
            // the same phantom claim in a different costume, recoverable
            // only by the six-hour reaper.
            //
            // Preserve any usage already observed, mark the run as
            // infrastructure, and return the task to dispatch without moving
            // its ladder position: a harness failure is not an agent verdict.
            // Dispatch's existing infrastructure-error arm records the
            // refusal and keeps the sweep moving. A run whose disposition
            // already committed (`claim_released`) skips the release below
            // and only reports.
            let mut outcome = errored_outcome(&error, streamed);
            if !executor.kind().meters_dollars() {
                outcome.usage.cost_usd = None;
            }
            let duration_ms = i64::try_from(clock.monotonic().saturating_sub(started).as_millis())
                .context("run duration exceeds the ledger's i64 millisecond range")?;
            let finished_at = clock.wall_now().to_rfc3339();
            // Release the claim FIRST, and on the larger cleanup budget. Of
            // the two cleanup writes only this one is unreconstructable: a
            // run row left without its completion is the same bookkeeping a
            // crashed foreman already produces (and `update_run_usage`
            // checkpointed the spend during the stream, so the governor's day
            // is not lost), whereas a task left claimed and `running` is
            // stranded — `note_infra_refusal` matches only unclaimed tasks,
            // so no later sweep recovers it.
            let released = if claim_released {
                Ok(())
            } else {
                ledger_cleanup_write_with_busy_retry(
                    "releasing task after infrastructure failure",
                    || {
                        ledger.finish_infrastructure_failure_at(
                            task_id,
                            ClaimToken {
                                owner: &claimant,
                                generation: task.attempt,
                            },
                            &finished_at,
                        )
                    },
                )
            };
            // Both writes are attempted unconditionally: `?` on the first
            // would let one still-blocked write silently skip the other, and
            // the whole point of this arm is that the run is disposed of.
            let recorded = ledger_cleanup_write_with_busy_retry(
                "recording run infrastructure failure",
                || ledger.finish_run_as(run_id, &outcome, duration_ms, Some("harness_error")),
            );
            if let Err(bookkeeping) = &recorded {
                eprintln!(
                    "foreman: run {run_id} could not be recorded as an infrastructure \
                     failure and stays open in the ledger: {bookkeeping:#}"
                );
            }
            if let Err(stranded) = &released {
                // Nothing further can be written, so say exactly what is stuck
                // and the one command that clears it. `requeue --force` is the
                // recovery: plain requeue refuses a running task.
                eprintln!(
                    "foreman: task {task_id} is still claimed by `{claimant}` and could not be \
                     released — the release write failed too (a ledger still locked through \
                     the cleanup budget, or a claim that changed hands mid-run): {stranded:#}"
                );
                eprintln!(
                    "foreman: task {task_id} needs an operator — run \
                     `foreman task requeue {task_id} --force` once the ledger unlocks. Its \
                     ladder position was NOT charged."
                );
            }
            wake::fire(wake::WAKE_VERB);
            Err(error)
        }
        // A run that returned a report already dispositioned itself through
        // the closure — the claim is gone and the outcome is recorded.
        Ok(report) => Ok(report),
    }
}

fn agent_abandoned_background(outcome: &RunOutcome) -> bool {
    outcome
        .error
        .as_deref()
        .is_some_and(|error| error.starts_with(crate::driver::claude::AGENT_ABANDONED_BACKGROUND))
}

fn remaining_runner_budget(
    original: &Budget,
    spent: &crate::executor::Usage,
    elapsed: Duration,
    spend_evidence: Option<&str>,
) -> Option<Budget> {
    let mut remaining = original.clone();
    remaining.max_output_tokens = original
        .max_output_tokens
        .map(|limit| limit.saturating_sub(spent.output_tokens));
    if original.max_output_tokens.is_some() && remaining.max_output_tokens == Some(0) {
        return None;
    }
    remaining.max_budget_usd = match original.max_budget_usd {
        None => None,
        Some(limit) => match spent.cost_usd {
            Some(cost) if cost < limit => Some(limit - cost),
            Some(_) => return None,
            None if spend_evidence == Some(EXACT_PRE_MODEL_NOT_FOUND) => Some(limit),
            None => return None,
        },
    };
    let elapsed_secs = elapsed
        .as_secs()
        .saturating_add(u64::from(elapsed.subsec_nanos() != 0));
    remaining.max_wall_secs = match original.max_wall_secs {
        None => None,
        Some(limit) if elapsed_secs < limit => Some(limit - elapsed_secs),
        Some(_) => return None,
    };
    Some(remaining)
}

/// The outcome for a run whose stream died mid-flight (a ledger write that
/// failed, a driver that could not be waited on). Usage streamed before the
/// failure is spend that was really incurred: reporting `Default` here would
/// hand `finish_run` zeros and undercount the governor's day for work already
/// paid for.
fn errored_outcome(err: &anyhow::Error, streamed: Option<crate::executor::Usage>) -> RunOutcome {
    RunOutcome {
        stop: StopReason::Error,
        result: None,
        error: Some(format!("{err:#}")),
        usage: streamed.clone().unwrap_or_default(),
        session_ref: None,
        terminal_session_ref: None,
        usage_observed: streamed.is_some(),
        output_observed: false,
        resume_failure: None,
    }
}

fn renew_claim_if_due(
    ledger: &Ledger,
    task_id: i64,
    claim: ClaimToken<'_>,
    clock: &dyn RunClock,
    heartbeat_due: &mut Duration,
) -> Result<bool> {
    let heartbeat_now = clock.monotonic();
    if heartbeat_now < *heartbeat_due {
        return Ok(false);
    }
    let heartbeat_at = clock.wall_now().to_rfc3339();
    ledger_write_with_busy_retry("renewing live task claim", || {
        ledger
            .renew_claim_at(task_id, claim, &heartbeat_at)
            .map(|_| ())
    })?;
    *heartbeat_due = heartbeat_now + Duration::from_secs(CLAIM_HEARTBEAT_SECS);
    Ok(true)
}

// The clock is deliberately explicit rather than hidden in RunOptions: it is
// an execution authority, not user configuration, and replay must be unable
// to forget which clock governs this call.
#[allow(clippy::too_many_arguments)]
fn drive(
    ledger: &Ledger,
    task_id: i64,
    claim: ClaimToken<'_>,
    run_id: i64,
    executor: &dyn Executor,
    prompt: &str,
    resume_ref: Option<&str>,
    ws: &Workspace,
    budget: &Budget,
    opts: &RunOptions,
    // Last `Usage` event seen, owned by the caller so a mid-stream error
    // (which loses everything else in here) still surfaces the spend.
    last_usage: &mut Option<crate::executor::Usage>,
    seq: &mut i64,
    clock: &dyn RunClock,
) -> Result<RunOutcome> {
    let mut session = match resume_ref {
        Some(session_ref) => executor
            .resume(session_ref, prompt, ws, budget)
            .with_context(|| {
                format!(
                    "resuming {} session {session_ref}",
                    executor.kind().as_str()
                )
            })?,
        None => executor
            .start(prompt, ws, budget)
            .with_context(|| format!("starting {} session", executor.kind().as_str()))?,
    };
    let started = clock.monotonic();
    let mut budget_killed = false;
    // Once the session is killed, drain briefly and stop — an escaped
    // descendant can hold the pipe open (and even keep emitting lines)
    // long after the agent itself is dead.
    const DRAIN: Duration = Duration::from_secs(15);
    let mut killed_at: Option<Duration> = None;

    let stall = Duration::from_secs(opts.stall_secs);
    let mut last_event = clock.monotonic();
    let heartbeat_every = Duration::from_secs(CLAIM_HEARTBEAT_SECS);
    let mut heartbeat_due = clock.monotonic() + heartbeat_every;

    loop {
        renew_claim_if_due(ledger, task_id, claim, clock, &mut heartbeat_due)?;
        if let Some(t) = killed_at
            && clock.monotonic().saturating_sub(t) >= DRAIN
        {
            break;
        }
        if !budget_killed
            && budget.max_wall_secs.is_some_and(|cap| {
                clock.monotonic().saturating_sub(started) >= Duration::from_secs(cap)
            })
        {
            budget_killed = true;
            killed_at.get_or_insert_with(|| clock.monotonic());
            if opts.echo {
                eprintln!("foreman: budget ceiling (wall clock); killing session");
            }
            session.interrupt();
        }
        // Wake early enough to enforce the wall-clock cap even on a quiet
        // stream; the stall clock (time since the last line) governs hangs.
        let mut wait = stall
            .saturating_sub(clock.monotonic().saturating_sub(last_event))
            .max(Duration::from_millis(50));
        if let Some(cap) = budget.max_wall_secs {
            let remaining = Duration::from_secs(cap)
                .saturating_sub(clock.monotonic().saturating_sub(started))
                + Duration::from_secs(1);
            wait = wait.min(remaining);
        }
        if let Some(t) = killed_at {
            wait = wait.min(
                DRAIN
                    .saturating_sub(clock.monotonic().saturating_sub(t))
                    .max(Duration::from_millis(50)),
            );
        }
        // A quiet vendor stream must wake for the lease independently of
        // stall and wall-clock budgets. This is the local claimant's actual
        // liveness signal; agent progress events are only stream content.
        wait = wait.min(
            heartbeat_due
                .saturating_sub(clock.monotonic())
                .max(Duration::from_millis(50)),
        );
        let batch = match session.next_batch(wait) {
            Ok(Some(batch)) => batch,
            Ok(None) => break,
            Err(_timeout) => {
                clock.timeout_elapsed(wait);
                if killed_at.is_some() || clock.monotonic().saturating_sub(last_event) < stall {
                    continue;
                }
                if opts.echo {
                    eprintln!(
                        "foreman: no event for {}s; killing stalled session",
                        opts.stall_secs
                    );
                }
                killed_at = Some(clock.monotonic());
                session.interrupt();
                continue;
            }
        };
        clock.line_arrived();
        last_event = clock.monotonic();
        for mut ev in batch {
            if matches!(ev, AgentEvent::Heartbeat) {
                continue;
            }
            if !executor.kind().meters_dollars()
                && let AgentEvent::Usage { usage } = &mut ev
            {
                // Anthropic-priced Z.ai cost is fiction in the event ledger too,
                // not just in the run totals.
                usage.cost_usd = None;
            }
            if let AgentEvent::Usage { usage } = &ev {
                *last_usage = Some(usage.clone());
                // Checkpoint into the run row itself, not just the events
                // table: a hard kill of foreman mid-stream (SIGTERM with no
                // signal handler installed skips every Drop guard) must not
                // leave the row's usage NULL when real usage was already seen.
                // Fallible on purpose — a non-transient ledger failure still
                // aborts the run rather than carrying on writing nowhere. A
                // transient lock gets the shared bounded retry; exhaustion is
                // classified by the caller as infrastructure and never feeds
                // the task's escalation ladder.
                ledger_write_with_busy_retry("checkpointing run usage", || {
                    ledger.update_run_usage(run_id, usage)
                })?;
            }
            *seq += 1;
            let kind = match &ev {
                AgentEvent::Started { .. } => "started",
                AgentEvent::Text { .. } => "text",
                AgentEvent::ToolUse { .. } => "tool_use",
                AgentEvent::ToolResult { .. } => "tool_result",
                AgentEvent::Usage { .. } => "usage",
                AgentEvent::Heartbeat => unreachable!("heartbeats are filtered above"),
                AgentEvent::Raw { .. } => "raw",
            };
            let event_at = clock.wall_now().to_rfc3339();
            let payload = serde_json::to_string(&ev)?;
            ledger_run_event_write_with_busy_retry("appending run event", || {
                ledger.record_event_at(run_id, *seq, kind, &payload, &event_at)
            })?;
            if opts.echo {
                echo_event(&ev);
            }

            let (over_tokens, over_spend) = match &ev {
                AgentEvent::Usage { usage } => (
                    budget
                        .max_output_tokens
                        .is_some_and(|cap| usage.output_tokens > cap),
                    // Backstop behind claude's native flag; the main cap for any
                    // driver that reports cost without enforcing it.
                    budget
                        .max_budget_usd
                        .is_some_and(|cap| usage.cost_usd.is_some_and(|c| c > cap)),
                ),
                _ => (false, false),
            };
            if !budget_killed && (over_tokens || over_spend) {
                budget_killed = true;
                killed_at.get_or_insert_with(|| clock.monotonic());
                if opts.echo {
                    eprintln!(
                        "foreman: budget ceiling ({}); killing session",
                        if over_tokens {
                            "output tokens"
                        } else {
                            "spend"
                        }
                    );
                }
                session.interrupt();
                // Keep draining briefly: the parser may still hold usage worth
                // recording.
            }
        }
    }

    let mut outcome = session.wait_with_clock(clock)?;
    if budget_killed {
        // A violated cap is a ceiling even if the final turn completed — the
        // operator's limit outranks the agent's finish. (Stall/grace kills
        // after clean completion stay Done via the parsers instead.) The
        // parser's error text, if any, is kept for diagnosis.
        outcome.stop = StopReason::BudgetCeiling;
    }
    if outcome.usage == Default::default()
        && let Some(usage) = last_usage.clone()
    {
        // The abandoned-reader path loses the parser's accumulation; the
        // stream's last Usage event is better evidence than zeros.
        outcome.usage = usage;
    }
    outcome.usage_observed |= last_usage.is_some();
    Ok(outcome)
}

fn echo_event(ev: &AgentEvent) {
    match ev {
        AgentEvent::Started { session_ref } => {
            println!("[session {}]", session_ref.as_deref().unwrap_or("?"));
        }
        AgentEvent::Text { text } => println!("{text}"),
        AgentEvent::ToolUse { name, detail } => {
            let mut d = detail.replace('\n', " ");
            if d.len() > 120 {
                let cut = d
                    .char_indices()
                    .map(|(i, _)| i)
                    .take_while(|&i| i <= 120)
                    .last()
                    .unwrap_or(0);
                d.truncate(cut);
                d.push('…');
            }
            println!("[tool {name}] {d}");
        }
        AgentEvent::Usage { usage } => {
            // `?` for a component the lane does not report — the folded `in=`
            // total alone cannot tell a cheap cache re-read from fresh
            // ingestion, and those price about 10x apart.
            let component = |v: Option<u64>| v.map(|v| v.to_string()).unwrap_or_else(|| "?".into());
            println!(
                "[usage in={} (fresh={} cache_read={} cache_write={}) out={} cost={}]",
                usage.input_tokens,
                component(usage.fresh_input_tokens),
                component(usage.cache_read_input_tokens),
                component(usage.cache_creation_input_tokens),
                usage.output_tokens,
                usage
                    .cost_usd
                    .map(|c| format!("${c:.4}"))
                    .unwrap_or_else(|| "?".into())
            );
        }
        AgentEvent::ToolResult { detail } => {
            println!("[tool result: {} bytes]", detail.len());
        }
        AgentEvent::Raw { line } => println!("[raw] {line}"),
        AgentEvent::Heartbeat => {}
    }
}

/// `git status --porcelain` in the task worktree; Some(listing) when dirty.
/// None on both "clean" and "git itself failed" — a broken worktree will
/// fail loudly in tier-0/refinery with its own cause, and bouncing a done
/// task over a git PROBE error would punish the agent for harness weather.
/// Resolve where tier-0 runs: the workdir itself, or a subdir CONTAINED in
/// it. Canonicalized on both sides so a committed symlink at the subdir
/// path (or a traversal like `../../green`) cannot point the verifier at
/// unrelated code that happens to be green — same invariant the refinery
/// enforces for its own subdir.
pub(crate) fn resolve_verify_dir(
    workdir: &std::path::Path,
    subdir: Option<&str>,
) -> Result<PathBuf> {
    let root = workdir
        .canonicalize()
        .with_context(|| format!("canonicalizing workdir {}", workdir.display()))?;
    let Some(sub) = subdir else {
        return Ok(root);
    };
    let dir = root
        .join(sub)
        .canonicalize()
        .with_context(|| format!("verify subdir {sub:?} in {}", root.display()))?;
    anyhow::ensure!(
        dir.starts_with(&root),
        "verify subdir {sub:?} resolves to {} — outside the task worktree {}",
        dir.display(),
        root.display()
    );
    Ok(dir)
}

/// The worktree's current branch; None when git itself fails (same
/// harness-weather rationale as `worktree_dirt` — the probe must not
/// punish the agent, and a truly broken worktree fails loudly downstream).
fn current_branch(dir: &std::path::Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn worktree_dirt(dir: &std::path::Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let listing = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
    if listing.is_empty() {
        None
    } else {
        Some(listing)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::claude::ClaudeDriver;
    use crate::executor::{ExecutorCaps, Session, Usage};
    use std::process::Command;

    struct FixedRunClock {
        monotonic: Duration,
        wall: chrono::DateTime<chrono::Utc>,
    }

    impl RunClock for FixedRunClock {
        fn monotonic(&self) -> Duration {
            self.monotonic
        }

        fn wall_now(&self) -> chrono::DateTime<chrono::Utc> {
            self.wall
        }
    }

    #[test]
    fn live_local_runner_heartbeat_advances_the_lease_when_due() {
        let temp = tempfile::TempDir::new().unwrap();
        let ledger = Ledger::open(&temp.path().join("ledger.db")).unwrap();
        let id = ledger
            .add_task("long local run", "spec", "impl", "low", &[], "none")
            .unwrap();
        let claimed_at: chrono::DateTime<chrono::Utc> = "2026-08-30T00:00:00Z".parse().unwrap();
        let (task, _) = ledger
            .start_attempt_at(
                id,
                "claude@123",
                Some(123),
                None,
                None,
                "claude",
                None,
                None,
                &claimed_at.to_rfc3339(),
                true,
            )
            .unwrap();
        let initial = task.lease_until.clone().unwrap();
        let clock = FixedRunClock {
            monotonic: Duration::from_secs(CLAIM_HEARTBEAT_SECS + 1),
            wall: claimed_at
                + chrono::Duration::seconds(i64::try_from(CLAIM_HEARTBEAT_SECS + 1).unwrap()),
        };
        let mut due = Duration::from_secs(CLAIM_HEARTBEAT_SECS);

        assert!(
            renew_claim_if_due(
                &ledger,
                id,
                ClaimToken {
                    owner: "claude@123",
                    generation: task.attempt,
                },
                &clock,
                &mut due,
            )
            .unwrap()
        );
        let renewed = ledger.task(id).unwrap().unwrap().lease_until.unwrap();
        assert!(
            chrono::DateTime::parse_from_rfc3339(&renewed).unwrap()
                > chrono::DateTime::parse_from_rfc3339(&initial).unwrap()
        );
        assert_eq!(due, Duration::from_secs((CLAIM_HEARTBEAT_SECS * 2) + 1));
    }

    struct LockingExecutor {
        inner: ClaudeDriver,
        db: PathBuf,
        release_lock: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
        released: std::sync::Mutex<Option<std::sync::mpsc::Sender<()>>>,
        holder: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
    }

    impl LockingExecutor {
        fn new(inner: ClaudeDriver, db: PathBuf, hold: Duration) -> Self {
            Self {
                inner,
                db,
                release_lock: std::sync::Mutex::new(Some(Box::new(move || {
                    std::thread::sleep(hold)
                }))),
                released: std::sync::Mutex::new(None),
                holder: std::sync::Mutex::new(None),
            }
        }

        fn until_signalled(
            inner: ClaudeDriver,
            db: PathBuf,
            release: std::sync::mpsc::Receiver<()>,
        ) -> (Self, std::sync::mpsc::Receiver<()>) {
            let (released_tx, released_rx) = std::sync::mpsc::channel();
            (
                Self {
                    inner,
                    db,
                    release_lock: std::sync::Mutex::new(Some(Box::new(move || {
                        release.recv().unwrap()
                    }))),
                    released: std::sync::Mutex::new(Some(released_tx)),
                    holder: std::sync::Mutex::new(None),
                },
                released_rx,
            )
        }

        fn join_holder(&self) {
            let holder = self.holder.lock().unwrap().take().unwrap();
            holder.join().unwrap();
        }
    }

    impl Executor for LockingExecutor {
        fn kind(&self) -> crate::executor::AgentKind {
            self.inner.kind()
        }

        fn capabilities(&self) -> ExecutorCaps {
            self.inner.capabilities()
        }

        fn check_budget(&self, budget: &Budget) -> Result<()> {
            self.inner.check_budget(budget)
        }

        fn start(&self, prompt: &str, ws: &Workspace, budget: &Budget) -> Result<Session> {
            let db = self.db.clone();
            let release_lock = self.release_lock.lock().unwrap().take().unwrap();
            let released = self.released.lock().unwrap().take();
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            let holder = std::thread::spawn(move || {
                let conn = rusqlite::Connection::open(db).unwrap();
                conn.execute_batch("BEGIN IMMEDIATE").unwrap();
                ready_tx.send(()).unwrap();
                release_lock();
                conn.execute_batch("COMMIT").unwrap();
                if let Some(released) = released {
                    released.send(()).unwrap();
                }
            });
            ready_rx.recv().unwrap();
            *self.holder.lock().unwrap() = Some(holder);
            self.inner.start(prompt, ws, budget)
        }
    }

    fn fixture_driver(temp: &tempfile::TempDir) -> ClaudeDriver {
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/claude-ok.jsonl");
        let script = temp.path().join("fake-claude");
        crate::fixture::write_executable(
            &script,
            format!("#!/bin/sh\ncat '{}'\n", fixture.display()),
        );
        ClaudeDriver::new().with_program(script.to_str().unwrap())
    }

    /// A run that dies mid-stream still spent whatever it had streamed —
    /// the Error outcome must carry it so `finish_run` writes the real
    /// figure over the checkpoint rather than zeros.
    #[test]
    fn errored_outcome_carries_the_usage_seen_before_the_failure() {
        let seen = Usage {
            input_tokens: 42,
            fresh_input_tokens: None,
            output_tokens: 14,
            cost_usd: Some(0.0421),
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
        };
        let out = errored_outcome(&anyhow::anyhow!("ledger write failed"), Some(seen.clone()));
        assert_eq!(out.stop, StopReason::Error);
        assert_eq!(out.usage, seen);
        assert!(out.error.unwrap().contains("ledger write failed"));

        // Nothing streamed (a spawn that never produced an event) still
        // reports zeros — there is no spend to preserve.
        let none = errored_outcome(&anyhow::anyhow!("spawn failed"), None);
        assert_eq!(none.usage, Usage::default());
    }

    #[test]
    fn fresh_fallback_receives_only_the_residual_runner_budget() {
        let original = Budget {
            max_budget_usd: Some(5.0),
            max_output_tokens: Some(1_000),
            max_wall_secs: Some(60),
            ..Default::default()
        };
        let spent = Usage {
            output_tokens: 125,
            cost_usd: Some(1.25),
            ..Default::default()
        };

        let remaining =
            remaining_runner_budget(&original, &spent, Duration::from_millis(10_001), None)
                .unwrap();
        assert_eq!(remaining.max_output_tokens, Some(875));
        assert_eq!(remaining.max_budget_usd, Some(3.75));
        assert_eq!(remaining.max_wall_secs, Some(49));
    }

    #[test]
    fn runner_fallback_refuses_unknown_capped_spend_without_journalled_evidence() {
        let original = Budget {
            max_budget_usd: Some(5.0),
            max_output_tokens: Some(1_000),
            max_wall_secs: Some(60),
            ..Default::default()
        };
        let spent = Usage::default();

        assert!(remaining_runner_budget(&original, &spent, Duration::ZERO, None).is_none());
        let trusted = remaining_runner_budget(
            &original,
            &spent,
            Duration::ZERO,
            Some(EXACT_PRE_MODEL_NOT_FOUND),
        )
        .unwrap();
        assert_eq!(trusted.max_budget_usd, Some(5.0));
        assert_eq!(trusted.max_output_tokens, Some(1_000));
        assert_eq!(trusted.max_wall_secs, Some(60));
    }

    fn test_ledger() -> (tempfile::TempDir, Ledger) {
        let temp = tempfile::TempDir::new().unwrap();
        let db_path = temp.path().join("ledger.db");
        let ledger = Ledger::open(&db_path).unwrap();
        (temp, ledger)
    }

    /// Two mints for the SAME run id must differ — if they didn't, the
    /// nonce would be pure function of `run_id` and therefore guessable by
    /// anyone who knows a task is on its Nth attempt (visible elsewhere in
    /// the ledger). The random component is what makes it unpredictable
    /// before this specific run's prompt is built.
    #[test]
    fn mint_nonce_carries_entropy_beyond_the_run_id() {
        let a = mint_nonce(7);
        let b = mint_nonce(7);
        assert_ne!(a, b, "same run id minted twice must not collide");
    }

    /// The property the retry loop actually depends on: two attempts at
    /// the same task get two different nonces, so a marker minted for
    /// attempt N cannot be pre-empted by anything an agent wrote during
    /// attempt N-1 (it didn't exist yet).
    #[test]
    fn nonce_differs_between_two_runs_of_the_same_task() {
        let (_temp, ledger) = test_ledger();
        let task_id = ledger
            .add_task("t", "spec", "impl", "low", &[], "rust")
            .unwrap();
        let (_, run1) = ledger
            .start_attempt(task_id, "claude@1", None, None, "claude", None)
            .unwrap();
        // Release the claim so the task is retry-eligible again — a second
        // `start_attempt` on a still-running task is refused, same as the
        // real dispatcher requires the prior attempt to finish first.
        ledger.finish_task(task_id, "claude@1", "bounced").unwrap();
        let (_, run2) = ledger
            .start_attempt(task_id, "claude@1", None, None, "claude", None)
            .unwrap();
        assert_ne!(
            run1, run2,
            "sanity: start_attempt must mint a fresh run row"
        );
        assert_ne!(mint_nonce(run1), mint_nonce(run2));
    }

    #[test]
    fn same_rung_retry_resumes_the_prior_session_with_the_finding_as_the_turn() {
        let (temp, ledger) = test_ledger();
        let repo = temp.path().join("worktree");
        std::fs::create_dir(&repo).unwrap();
        for args in [
            vec!["init", "-q", "-b", "task/resume"],
            vec!["config", "user.name", "test"],
            vec!["config", "user.email", "test@example.com"],
        ] {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(&repo)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        std::fs::write(repo.join("base.txt"), "base\n").unwrap();
        assert!(
            Command::new("git")
                .args(["add", "."])
                .current_dir(&repo)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args(["commit", "-qm", "base"])
                .current_dir(&repo)
                .status()
                .unwrap()
                .success()
        );

        let task_id = ledger
            .add_task(
                "same-rung retry",
                "COLD SPEC MUST NOT BE REPEATED",
                "impl",
                "low",
                &[],
                "none",
            )
            .unwrap();
        let claimant = "seed";
        let (_, prior_run) = ledger
            .start_attempt(
                task_id,
                claimant,
                repo.to_str(),
                Some("task/resume"),
                "claude",
                None,
            )
            .unwrap();
        ledger
            .finish_run(
                prior_run,
                &RunOutcome {
                    stop: StopReason::Error,
                    result: None,
                    error: Some("review rejected".into()),
                    usage: Usage::default(),
                    session_ref: Some("thread-impl".into()),
                    terminal_session_ref: Some("thread-impl".into()),
                    usage_observed: false,
                    output_observed: true,
                    resume_failure: None,
                },
                1,
            )
            .unwrap();
        ledger.finish_task(task_id, claimant, "bounced").unwrap();
        ledger
            .file_finding_reasoned(
                Some(task_id),
                "major",
                "RETRY FINDING TITLE",
                "RETRY FINDING BODY",
                "review",
                FindingReason::ReviewRejected,
            )
            .unwrap();

        let argv_log = temp.path().join("argv.log");
        let script = temp.path().join("recording-claude");
        crate::fixture::write_executable(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\0' \"$@\" > '{argv_log}'\nprintf '%s\\n' \
                 '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"thread-impl\",\"model\":\"m\",\"cwd\":\"/tmp\"}}' \
                 '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"duration_ms\":1,\"num_turns\":1,\"result\":\"done\",\"session_id\":\"thread-impl\",\"usage\":{{\"input_tokens\":1,\"output_tokens\":1}},\"total_cost_usd\":0.0}}'\n",
                argv_log = argv_log.display(),
            ),
        );
        let driver = ClaudeDriver::new().with_program(script.to_str().unwrap());
        let opts = RunOptions {
            workdir: repo,
            branch: Some("task/resume".into()),
            resume_session: Some("thread-impl".into()),
            verify: false,
            stall_secs: 10,
            ..Default::default()
        };

        let report = run_task(&ledger, task_id, &driver, &opts).unwrap();
        assert_eq!(report.task_status, "done");
        let argv = std::fs::read(&argv_log)
            .unwrap()
            .split(|byte| *byte == 0)
            .filter(|arg| !arg.is_empty())
            .map(|arg| String::from_utf8(arg.to_vec()).unwrap())
            .collect::<Vec<_>>();
        let resume_at = argv.iter().position(|arg| arg == "--resume").unwrap();
        assert_eq!(argv[resume_at + 1], "thread-impl");
        let prompt_at = argv.iter().position(|arg| arg == "-p").unwrap();
        let turn = &argv[prompt_at + 1];
        assert!(turn.contains("RETRY FINDING TITLE"), "{turn}");
        assert!(turn.contains("RETRY FINDING BODY"), "{turn}");
        assert!(turn.contains("SECURITY:"), "{turn}");
        assert!(!turn.contains("COLD SPEC MUST NOT BE REPEATED"), "{turn}");
        assert!(!turn.contains("## Workspace"), "{turn}");

        let implementation_runs = ledger
            .recent_runs(10)
            .unwrap()
            .into_iter()
            .filter(|run| run.task_id == task_id && run.role == "implement")
            .collect::<Vec<_>>();
        assert_eq!(implementation_runs.len(), 2);
        assert!(
            implementation_runs
                .iter()
                .all(|run| run.session_ref.as_deref() == Some("thread-impl")),
            "both run rows must name the one continued implementer session: {implementation_runs:?}"
        );
    }

    #[test]
    fn pre_event_harness_failure_preserves_implementer_resume_intent() {
        let (temp, ledger) = test_ledger();
        let repo = temp.path().join("worktree");
        std::fs::create_dir(&repo).unwrap();
        for args in [
            ["init", "-q", "-b", "task/resume-crash"].as_slice(),
            ["config", "user.name", "test"].as_slice(),
            ["config", "user.email", "test@example.com"].as_slice(),
        ] {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(&repo)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        std::fs::write(repo.join("base.txt"), "base\n").unwrap();
        assert!(
            Command::new("git")
                .args(["add", "."])
                .current_dir(&repo)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args(["commit", "-qm", "base"])
                .current_dir(&repo)
                .status()
                .unwrap()
                .success()
        );

        let task_id = ledger
            .add_task("resume crash", "spec", "impl", "low", &[], "none")
            .unwrap();
        let (_, prior_run) = ledger
            .start_attempt(
                task_id,
                "seed",
                repo.to_str(),
                Some("task/resume-crash"),
                "claude",
                None,
            )
            .unwrap();
        ledger
            .finish_run(
                prior_run,
                &RunOutcome {
                    stop: StopReason::Error,
                    result: None,
                    error: Some("retry me".into()),
                    usage: Usage::default(),
                    session_ref: Some("thread-before-crash".into()),
                    terminal_session_ref: Some("thread-before-crash".into()),
                    usage_observed: false,
                    output_observed: true,
                    resume_failure: None,
                },
                1,
            )
            .unwrap();
        ledger.finish_task(task_id, "seed", "bounced").unwrap();

        // Spawn fails before a Session exists, hence before any Started or
        // usage event can make the requested id durable by another route.
        let missing = temp.path().join("missing-claude");
        let dying = ClaudeDriver::new().with_program(missing.to_str().unwrap());
        let failed = run_task(
            &ledger,
            task_id,
            &dying,
            &RunOptions {
                workdir: repo.clone(),
                branch: Some("task/resume-crash".into()),
                resume_session: Some("thread-before-crash".into()),
                verify: false,
                stall_secs: 10,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(failed.outcome.stop, StopReason::Error);
        assert_eq!(
            ledger
                .recent_runs(10)
                .unwrap()
                .into_iter()
                .find(|run| run.id == failed.run_id)
                .unwrap()
                .session_ref
                .as_deref(),
            Some("thread-before-crash")
        );

        ledger.requeue_task(task_id, true).unwrap();
        let argv_log = temp.path().join("resumed.argv");
        let script = temp.path().join("recording-claude");
        crate::fixture::write_executable(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\0' \"$@\" > '{argv_log}'\nprintf '%s\\n' \
                 '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"thread-before-crash\"}}' \
                 '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"done\",\"session_id\":\"thread-before-crash\",\"usage\":{{\"input_tokens\":1,\"output_tokens\":1}}}}'\n",
                argv_log = argv_log.display(),
            ),
        );
        let resumed = ClaudeDriver::new().with_program(script.to_str().unwrap());
        let report = run_task(
            &ledger,
            task_id,
            &resumed,
            &RunOptions {
                workdir: repo,
                branch: Some("task/resume-crash".into()),
                resume_session: Some("thread-before-crash".into()),
                verify: false,
                stall_secs: 10,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(report.task_status, "done");
        let argv = std::fs::read(&argv_log).unwrap();
        assert!(
            argv.windows(b"--resume\0thread-before-crash\0".len())
                .any(|window| window == b"--resume\0thread-before-crash\0"),
            "next same-rung attempt did not resume the journalled id: {:?}",
            String::from_utf8_lossy(&argv)
        );
    }

    #[test]
    fn launch_preconfigured_dead_session_falls_back_to_an_explicit_fresh_start() {
        use std::os::unix::fs::PermissionsExt;

        let (temp, ledger) = test_ledger();
        let repo = temp.path().join("worktree");
        std::fs::create_dir(&repo).unwrap();
        for args in [
            vec!["init", "-q", "-b", "task/resume"],
            vec!["config", "user.name", "test"],
            vec!["config", "user.email", "test@example.com"],
        ] {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(&repo)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        std::fs::write(repo.join("base.txt"), "base\n").unwrap();
        assert!(
            Command::new("git")
                .args(["add", "."])
                .current_dir(&repo)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args(["commit", "-qm", "base"])
                .current_dir(&repo)
                .status()
                .unwrap()
                .success()
        );

        let task_id = ledger
            .add_task(
                "dead implementer session",
                "spec",
                "impl",
                "low",
                &[],
                "none",
            )
            .unwrap();
        let (_, prior_run) = ledger
            .start_attempt(
                task_id,
                "seed",
                repo.to_str(),
                Some("task/resume"),
                "claude",
                None,
            )
            .unwrap();
        ledger
            .finish_run(
                prior_run,
                &RunOutcome {
                    stop: StopReason::Error,
                    result: None,
                    error: Some("old failure".into()),
                    usage: Usage::default(),
                    session_ref: Some("dead".into()),
                    terminal_session_ref: None,
                    usage_observed: false,
                    output_observed: false,
                    resume_failure: None,
                },
                1,
            )
            .unwrap();
        ledger.finish_task(task_id, "seed", "bounced").unwrap();

        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/claude-ok.jsonl");
        let script = temp.path().join("resume-then-fresh-claude");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nif [ -f '{used}' ]; then \
                 case \" $* \" in *\" --resume \"*) echo 'fallback resumed again' >&2; exit 9;; esac; \
                 cat '{fixture}'; else : > '{used}'; \
                 echo 'No conversation found with session ID: dead' >&2; exit 1; fi\n",
                used = temp.path().join("used").display(),
                fixture = fixture.display(),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let driver = ClaudeDriver::new().with_program(script.to_str().unwrap());
        let opts = RunOptions {
            workdir: repo,
            branch: Some("task/resume".into()),
            resume_session: Some("dead".into()),
            verify: false,
            stall_secs: 10,
            ..Default::default()
        };

        let report = run_task(&ledger, task_id, &driver, &opts).unwrap();
        assert_eq!(report.task_status, "done");
        assert!(
            ledger
                .run_event_kinds(report.run_id)
                .unwrap()
                .contains(&"resume_fallback".into())
        );
        assert_eq!(
            ledger
                .recent_runs(10)
                .unwrap()
                .into_iter()
                .find(|run| run.id == prior_run)
                .unwrap()
                .session_ref,
            None
        );
    }

    #[test]
    fn crash_after_fallback_journal_does_not_resume_the_dead_session_next_sweep() {
        let (temp, ledger) = test_ledger();
        let repo = temp.path().join("worktree");
        std::fs::create_dir(&repo).unwrap();
        for args in [
            ["init", "-q", "-b", "task/fallback-crash"].as_slice(),
            ["config", "user.name", "test"].as_slice(),
            ["config", "user.email", "test@example.com"].as_slice(),
        ] {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(&repo)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        std::fs::write(repo.join("base.txt"), "base\n").unwrap();
        assert!(
            Command::new("git")
                .args(["add", "."])
                .current_dir(&repo)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args(["commit", "-qm", "base"])
                .current_dir(&repo)
                .status()
                .unwrap()
                .success()
        );

        let task_id = ledger
            .add_task("fallback crash", "spec", "impl", "low", &[], "none")
            .unwrap();
        let (_, prior_run) = ledger
            .start_attempt(
                task_id,
                "seed",
                repo.to_str(),
                Some("task/fallback-crash"),
                "claude",
                None,
            )
            .unwrap();
        ledger
            .finish_run(
                prior_run,
                &RunOutcome {
                    stop: StopReason::Error,
                    result: None,
                    error: Some("retry me".into()),
                    usage: Usage::default(),
                    session_ref: Some("dead".into()),
                    terminal_session_ref: Some("dead".into()),
                    usage_observed: false,
                    output_observed: true,
                    resume_failure: None,
                },
                1,
            )
            .unwrap();
        ledger.finish_task(task_id, "seed", "bounced").unwrap();

        let not_found = temp.path().join("not-found-claude");
        crate::fixture::write_executable(
            &not_found,
            "#!/bin/sh\necho 'No conversation found with session ID: dead' >&2\nexit 1\n",
        );
        fail_after_fallback_journal_for_test();
        let error = match run_task(
            &ledger,
            task_id,
            &ClaudeDriver::new().with_program(not_found.to_str().unwrap()),
            &RunOptions {
                workdir: repo.clone(),
                branch: Some("task/fallback-crash".into()),
                resume_session: Some("dead".into()),
                verify: false,
                stall_secs: 10,
                ..Default::default()
            },
        ) {
            Ok(_) => panic!("the injected crash must escape the run"),
            Err(error) => error,
        };
        assert!(
            format!("{error:#}").contains("injected crash after resume fallback journal"),
            "{error:#}"
        );
        let crashed_run = ledger
            .recent_runs(10)
            .unwrap()
            .into_iter()
            .find(|run| run.task_id == task_id && run.id != prior_run)
            .unwrap();
        assert_eq!(crashed_run.session_ref, None);
        assert!(
            ledger
                .run_event_kinds(crashed_run.id)
                .unwrap()
                .contains(&"resume_fallback".into())
        );

        let argv_log = temp.path().join("next.argv");
        let fresh = temp.path().join("fresh-claude");
        crate::fixture::write_executable(
            &fresh,
            format!(
                "#!/bin/sh\nprintf '%s\\0' \"$@\" > '{argv_log}'\nprintf '%s\\n' \\
                 '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"fresh\"}}' \\
                 '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"done\",\"session_id\":\"fresh\",\"usage\":{{\"input_tokens\":1,\"output_tokens\":1}}}}'\n",
                argv_log = argv_log.display(),
            ),
        );
        let report = run_task(
            &ledger,
            task_id,
            &ClaudeDriver::new().with_program(fresh.to_str().unwrap()),
            &RunOptions {
                workdir: repo,
                branch: Some("task/fallback-crash".into()),
                verify: false,
                stall_secs: 10,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(report.task_status, "done");
        let argv = std::fs::read(&argv_log).unwrap();
        assert!(
            !argv
                .windows(b"--resume\0dead\0".len())
                .any(|window| window == b"--resume\0dead\0"),
            "next sweep resumed the retired session: {:?}",
            String::from_utf8_lossy(&argv)
        );
    }

    #[test]
    fn run_event_append_waits_out_ten_second_immediate_lock() {
        let (temp, ledger) = test_ledger();
        let db = temp.path().join("ledger.db");
        let task_id = ledger
            .add_task("busy run", "spec", "impl", "low", &[], "none")
            .unwrap();
        // Shorten each SQLite attempt so success depends on the shared
        // event-specific wall-clock budget spanning the holder's full ten
        // seconds. The ordinary retry budget is intentionally much shorter
        // with this 250 ms per-attempt timeout.
        ledger
            .set_busy_timeout_for_test(Duration::from_millis(250))
            .unwrap();
        let executor = LockingExecutor::new(fixture_driver(&temp), db, Duration::from_secs(10));
        let opts = RunOptions {
            workdir: temp.path().to_path_buf(),
            stall_secs: 10,
            verify: false,
            ..Default::default()
        };

        let report = run_task(&ledger, task_id, &executor, &opts).unwrap();
        executor.join_holder();

        assert_eq!(report.task_status, "done");
        assert_eq!(ledger.task(task_id).unwrap().unwrap().status, "done");
        assert!(ledger.run_event_count(report.run_id).unwrap() > 0);
    }

    /// Task 94: the seq-0 "rebase" event is written before `drive()` ever
    /// starts, and a real occurrence (run 425) died at exactly this write and
    /// stranded its task `running` for 31 hours — while an equivalent
    /// mid-stream write failure inside `drive()` (run 535) released its
    /// claim correctly. The asymmetry was structural: the rebase write sat
    /// outside the closure whose `Err` arm disposes of the run. This proves
    /// the fix — the write still fails, but the claim comes back clean.
    #[test]
    fn rebase_event_write_failure_still_releases_the_claim() {
        let (temp, ledger) = test_ledger();
        let task_id = ledger
            .add_task("rebase write failure", "spec", "impl", "low", &[], "none")
            .unwrap();
        crate::ledger::fail_next_run_event_write_for_test();
        let opts = RunOptions {
            workdir: temp.path().to_path_buf(),
            stall_secs: 10,
            verify: false,
            rebased_onto: Some("deadbeef".into()),
            ..Default::default()
        };

        let report = run_task(&ledger, task_id, &fixture_driver(&temp), &opts).unwrap();

        assert_eq!(report.task_status, "failed");
        let task = ledger.task(task_id).unwrap().unwrap();
        assert_eq!(task.status, "failed");
        assert!(
            task.claimed_by.is_none(),
            "the claim must be released even though the failure happened before drive() ran"
        );
        assert_eq!(
            task.ladder_failures, 0,
            "an infrastructure-classified failure must never charge the ladder"
        );
        // The retry-through-drive() path is unaffected: the injected
        // failure fires exactly once and self-resets, so a fresh attempt on
        // the now-unclaimed task runs the agent normally.
        let retry = run_task(&ledger, task_id, &fixture_driver(&temp), &opts).unwrap();
        assert_eq!(retry.task_status, "done");
    }

    /// Task 94, the other half of the audit: a run can also die while
    /// REPORTING an outcome it already reached — the disposition write, a
    /// finding write, a verification record. Those are ordinary `?` escapes
    /// from the runner's closure, not SQLite weather, and they used to fall
    /// through the outer match untouched, leaving the task claimed and
    /// `running` with the run over. The claim must come back for a
    /// reporting failure exactly as it does for a driver failure.
    #[test]
    fn outcome_reporting_write_failure_still_releases_the_claim() {
        let (temp, ledger) = test_ledger();
        let task_id = ledger
            .add_task("report write failure", "spec", "impl", "low", &[], "none")
            .unwrap();
        crate::ledger::fail_next_task_disposition_write_for_test();
        let opts = RunOptions {
            workdir: temp.path().to_path_buf(),
            stall_secs: 10,
            verify: false,
            ..Default::default()
        };

        let Err(error) = run_task(&ledger, task_id, &fixture_driver(&temp), &opts) else {
            panic!("the injected reporting write must surface as the run's error");
        };
        assert!(
            format!("{error:#}").contains("injected task disposition write failure"),
            "the original failure must not be swallowed by the cleanup: {error:#}"
        );

        let task = ledger.task(task_id).unwrap().unwrap();
        assert!(
            task.claimed_by.is_none(),
            "a run that died reporting its outcome must still release the claim"
        );
        assert_eq!(
            task.status, "queued",
            "a harness failure returns the task to dispatch"
        );
        assert_eq!(
            task.ladder_failures, 0,
            "the task did nothing wrong — reporting failed, not the work"
        );
        // And the recovery is an ordinary re-dispatch, not an operator.
        let retry = run_task(&ledger, task_id, &fixture_driver(&temp), &opts).unwrap();
        assert_eq!(retry.task_status, "done");
    }

    /// Task 94, the third row of the audit: resolving the task's verifier
    /// profile happens AFTER the claim commits, so a profile the binary no
    /// longer knows (renamed, removed, or a ledger written by a newer
    /// foreman) is a run-ending failure like any other and must release.
    /// Resolved above the runner's closure it returned straight out of the
    /// function with the task claimed and `running`, and the reaper could
    /// not recover it either: the supervisor process is still alive, and
    /// liveness — not lease age — is the reaping predicate.
    #[test]
    fn unknown_verifier_profile_still_releases_the_claim() {
        let (temp, ledger) = test_ledger();
        let task_id = ledger
            .add_task(
                "gone profile",
                "spec",
                "impl",
                "low",
                &[],
                "retired-profile",
            )
            .unwrap();
        let opts = RunOptions {
            workdir: temp.path().to_path_buf(),
            stall_secs: 10,
            verify: false,
            ..Default::default()
        };

        let Err(error) = run_task(&ledger, task_id, &fixture_driver(&temp), &opts) else {
            panic!("an unknown verifier profile must fail the run");
        };
        assert!(
            format!("{error:#}").contains("retired-profile"),
            "the naming cause must reach the caller: {error:#}"
        );

        let task = ledger.task(task_id).unwrap().unwrap();
        assert!(
            task.claimed_by.is_none(),
            "a run that ends resolving its profile must still release the claim"
        );
        assert_eq!(
            task.status, "queued",
            "a harness failure returns the task to dispatch"
        );
        assert_eq!(
            task.ladder_failures, 0,
            "the task did nothing wrong — its profile is missing from this binary"
        );
        // And once an operator points the task at a profile that exists,
        // ordinary re-dispatch is the whole recovery. Through the resolved
        // setter: the built-in wrapper canonicalises the PREVIOUS name too,
        // which is the retired one this task is stuck on.
        ledger
            .set_verifier_profile_resolved(task_id, "retired-profile", "none")
            .unwrap();
        let retry = run_task(&ledger, task_id, &fixture_driver(&temp), &opts).unwrap();
        assert_eq!(retry.task_status, "done");
    }

    #[test]
    fn runner_reports_parked_when_branch_contract_limit_is_reached() {
        let (temp, ledger) = test_ledger();
        let task_id = ledger
            .add_task("honest parked report", "spec", "impl", "low", &[], "none")
            .unwrap();
        for _ in 0..2 {
            let (task, run) = ledger
                .start_attempt(task_id, "seed", None, None, "claude", None)
                .unwrap();
            ledger
                .finish_task_classified(
                    task_id,
                    ClaimToken {
                        owner: "seed",
                        generation: task.attempt,
                    },
                    run,
                    "bounced",
                    Some(FindingReason::BranchContract),
                )
                .unwrap();
        }

        let repo = temp.path().join("worktree");
        std::fs::create_dir(&repo).unwrap();
        for args in [
            vec!["init", "-b", "task/report"],
            vec!["config", "user.name", "test"],
            vec!["config", "user.email", "test@example.com"],
        ] {
            let output = Command::new("git")
                .args(&args)
                .current_dir(&repo)
                .output()
                .unwrap();
            assert!(output.status.success(), "git {args:?} failed");
        }
        std::fs::write(repo.join("base.txt"), "base\n").unwrap();
        for args in [vec!["add", "."], vec!["commit", "-m", "base"]] {
            let output = Command::new("git")
                .args(&args)
                .current_dir(&repo)
                .output()
                .unwrap();
            assert!(output.status.success(), "git {args:?} failed");
        }
        std::fs::write(repo.join("uncommitted.txt"), "dirty\n").unwrap();
        let opts = RunOptions {
            workdir: repo,
            branch: Some("task/report".into()),
            verify: false,
            stall_secs: 10,
            ..Default::default()
        };
        let report = run_task(&ledger, task_id, &fixture_driver(&temp), &opts).unwrap();

        assert_eq!(report.task_status, "parked");
        assert_eq!(ledger.task(task_id).unwrap().unwrap().status, "parked");
        let payload: String = rusqlite::Connection::open(temp.path().join("ledger.db"))
            .unwrap()
            .query_row(
                "SELECT payload FROM events WHERE run_id = ?1 AND kind = 'disposition'",
                [report.run_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&payload).unwrap()["status"],
            "parked"
        );
    }

    /// The honest worst case: the lock is held for the WHOLE of `run_task`,
    /// so the run-path budget and BOTH cleanup writes exhaust — no test hook
    /// unblocks the ledger part-way. Nothing can be written to a database
    /// that never unlocks, so what is asserted here is what survives that:
    /// the error is marked as SQLite weather, the ladder is not charged, and
    /// the stranded claim is recoverable with the documented command.
    #[test]
    fn a_lock_outlasting_the_cleanup_budget_strands_nothing_on_the_ladder() {
        let (temp, ledger) = test_ledger();
        let db = temp.path().join("ledger.db");
        let task_id = ledger
            .add_task("wedged ledger", "spec", "impl", "low", &[], "none")
            .unwrap();
        ledger
            .set_busy_timeout_for_test(Duration::from_millis(1))
            .unwrap();
        // Released only AFTER `run_task` has returned, so the hold is longer
        // than both budgets by construction rather than by a sleep that a
        // loaded machine could outrun.
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (executor, _released_rx) =
            LockingExecutor::until_signalled(fixture_driver(&temp), db, release_rx);
        let opts = RunOptions {
            workdir: temp.path().to_path_buf(),
            stall_secs: 10,
            verify: false,
            ..Default::default()
        };

        let result = run_task(&ledger, task_id, &executor, &opts);
        release_tx.send(()).unwrap();
        executor.join_holder();

        let error = match result {
            Ok(_) => panic!("run unexpectedly survived a ledger that never unlocked"),
            Err(error) => error,
        };
        assert!(
            sqlite_busy_retries_exhausted(&error),
            "a wedged ledger must stay classified as infrastructure weather: {error:#}"
        );

        let task = ledger.task(task_id).unwrap().unwrap();
        assert_eq!(
            task.ladder_failures, 0,
            "a ledger that never unlocked is not the task's failure"
        );
        // The claim could not be released — the ledger was unwritable. That
        // is the residual this arm cannot design away; what it must not do
        // is charge the task or leave the operator without a lever.
        assert_eq!(task.status, "running");
        assert!(task.claimed_by.is_some());
        ledger.requeue_task(task_id, true).unwrap();
        let recovered = ledger.task(task_id).unwrap().unwrap();
        assert_eq!(recovered.status, "queued");
        assert_eq!(recovered.claimed_by, None);
        assert_eq!(
            recovered.ladder_failures, 0,
            "the documented recovery must not charge the ladder either"
        );
    }

    /// The recoverable case, and the one contention actually looks like: the
    /// lock outlasts the run-path budget but clears before the cleanup's.
    /// The hook is what makes "outlasts the run-path budget" exact rather
    /// than a sleep race — and it CANNOT fire for the cleanup budget, so the
    /// cleanup writes here are racing the real holder, not a rigged one. The
    /// wedged-forever case is covered above.
    #[test]
    fn exhausted_run_write_is_infrastructure_and_does_not_charge_ladder() {
        let (temp, ledger) = test_ledger();
        let db = temp.path().join("ledger.db");
        let task_id = ledger
            .add_task("blocked run", "spec", "impl", "low", &[], "none")
            .unwrap();
        ledger
            .set_busy_timeout_for_test(Duration::from_millis(1))
            .unwrap();
        // Release only when the shared helper reports actual exhaustion. This
        // makes the mechanism deterministic even when process startup timing
        // changes: the failed write sees the full retry budget, while the
        // infrastructure-cleanup writes can proceed immediately afterwards.
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (executor, released_rx) =
            LockingExecutor::until_signalled(fixture_driver(&temp), db, release_rx);
        let hook_tx = release_tx.clone();
        crate::ledger::set_busy_retries_exhausted_hook_for_test(move || {
            hook_tx.send(()).unwrap();
            released_rx.recv().unwrap();
        });
        let opts = RunOptions {
            workdir: temp.path().to_path_buf(),
            stall_secs: 10,
            verify: false,
            ..Default::default()
        };

        let result = run_task(&ledger, task_id, &executor, &opts);
        crate::ledger::clear_busy_retries_exhausted_hook_for_test();
        let _ = release_tx.send(());
        executor.join_holder();
        let error = match result {
            Ok(_) => panic!("run unexpectedly survived an exhausted SQLite retry budget"),
            Err(error) => error,
        };

        assert!(sqlite_busy_retries_exhausted(&error), "{error:#}");
        let task = ledger.task(task_id).unwrap().unwrap();
        assert_eq!(task.status, "queued");
        assert_eq!(task.claimed_by, None);
        assert_eq!(
            task.ladder_failures, 0,
            "SQLite infrastructure weather must not advance the task ladder"
        );
        let run = ledger.recent_runs(1).unwrap().remove(0);
        assert_eq!(run.task_id, task_id);
        assert_eq!(run.verdict.as_deref(), Some("error"));
        assert_eq!(run.delivery, "harness_error");
        assert!(
            run.error
                .as_deref()
                .is_some_and(|message| message.contains("bounded SQLite busy retries"))
        );
    }
}
