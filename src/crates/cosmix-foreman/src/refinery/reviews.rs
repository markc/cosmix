use super::*;

#[derive(Debug)]
pub(super) struct ReviewSpec {
    pub(super) reviewer: crate::executor::AgentKind,
    pub(super) model: String,
}

pub(super) fn validate_review_lanes(
    specs: &[ReviewSpec],
    policy: Option<&crate::manifest::ProjectLanePolicy>,
    env: impl Fn(&str) -> bool,
) -> Result<()> {
    let Some(policy) = policy else {
        return Ok(());
    };
    for spec in specs {
        policy
            .check_lane(spec.reviewer, &env)
            .map_err(anyhow::Error::msg)
            .with_context(|| {
                format!(
                    "merge-review lane {} is refused by the project manifest",
                    spec.reviewer.as_str()
                )
            })
            .map_err(policy_denied)?;
    }
    Ok(())
}

#[derive(Debug)]
pub(super) struct ReservedReview {
    reviewer: crate::executor::AgentKind,
    model: String,
    reservation: i64,
}

#[derive(Debug)]
pub(super) struct PreparedReview {
    reviewer: crate::executor::AgentKind,
    model: String,
    reservation: i64,
    run_id: i64,
}

pub(super) struct LandingReviewContext<'a> {
    pub(super) ledger: &'a Ledger,
    pub(super) task: &'a Task,
    pub(super) opts: &'a RefineOptions,
    pub(super) fleet_policy: &'a crate::config::FleetPolicy,
    pub(super) worktree: &'a Path,
    pub(super) base: &'a str,
    pub(super) tip: &'a str,
    pub(super) touches_foreman: bool,
    pub(super) profile: &'a crate::verify::Profile,
}
/// Reuse only an exact, shell-recorded green review for this task generation
/// and rebased commit pair. This is the idempotency fence for a crash after the
/// atomic review batch commits but before the Git compare-and-swap lands it.
pub(super) fn recorded_approved_review(
    ledger: &Ledger,
    task: &Task,
    base: &str,
    tip: &str,
) -> Result<bool> {
    for report in ledger.review_verification_reports_for_attempt(task.id, task.attempt)? {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&report) else {
            continue;
        };
        if !matches!(
            value.get("kind").and_then(serde_json::Value::as_str),
            Some("review" | "two-arm-review")
        ) || value.get("base").and_then(serde_json::Value::as_str) != Some(base)
            || value.get("tip").and_then(serde_json::Value::as_str) != Some(tip)
        {
            continue;
        }
        return Ok(value
            .get("approve")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false));
    }
    Ok(false)
}

/// Make rebased commit identities replayable across a retry. Git otherwise
/// stamps rewritten commits with the current committer time, defeating the
/// exact base/tip review evidence fence after a pre-CAS interruption.
pub(super) fn rebase_for_landing(
    worktree: &Path,
    base: &str,
) -> Result<(Option<i32>, String, String)> {
    git_status(
        worktree,
        &["rebase", "--committer-date-is-author-date", base],
    )
}

pub(super) fn review_ledger_records(
    reviews: &crate::review::ReviewBatch,
) -> (
    Vec<crate::ledger::ReviewRunRecord>,
    Vec<crate::ledger::ReviewFindingInsert>,
) {
    let runs = reviews
        .arms
        .iter()
        .map(|arm| crate::ledger::ReviewRunRecord {
            run_id: arm.run_id,
            approve: arm.outcome.approve,
            delivered: arm.outcome.delivery == "delivered",
        })
        .collect();
    let findings = reviews
        .arms
        .iter()
        .flat_map(|arm| {
            let filed_by = format!("merge-review:{}", arm.reviewer.as_str());
            arm.outcome
                .findings
                .iter()
                .map(move |finding| crate::ledger::ReviewFindingInsert {
                    run_id: arm.run_id,
                    severity: finding.severity.as_db_str().to_string(),
                    file: finding.file.clone(),
                    line: finding.line,
                    title: finding.title.clone(),
                    body: finding.body.clone(),
                    filed_by: filed_by.clone(),
                })
        })
        .collect();
    (runs, findings)
}
pub(super) fn run_landing_reviews(
    context: &LandingReviewContext<'_>,
    specs: &[ReviewSpec],
) -> Result<crate::review::ReviewBatch> {
    anyhow::ensure!(
        !specs.is_empty(),
        "review required but no reviewer was routed"
    );
    let ledger = context.ledger;
    let task = context.task;
    let worktree = context.worktree;
    let base = context.base;
    let tip = context.tip;
    let touches_foreman = context.touches_foreman;
    let governor = crate::governor::Governor::from_policy(&context.opts.db, context.fleet_policy);

    // Hold every arm before starting one. A race after the non-binding
    // preflight may still make a hold fail; in that case none of the review
    // sessions runs and every acquired hold is released.
    let mut reserved: Vec<ReservedReview> = Vec::with_capacity(specs.len());
    for (index, spec) in specs.iter().enumerate() {
        let budget = crate::review::budget_for(context.fleet_policy, spec.reviewer);
        let reservation = match ledger_write_with_busy_retry("reserving merge review", || {
            governor.reserve(
                ledger,
                &format!(
                    "merge-review-{}-{index}@{}",
                    spec.reviewer.as_str(),
                    std::process::id()
                ),
                Some(task.id),
                &budget,
                spec.reviewer,
            )
        }) {
            Ok(reservation) => reservation,
            Err(error) => {
                for arm in reserved {
                    let _ = landing_ledger_write("releasing merge-review reservation", || {
                        governor.release(ledger, arm.reservation)
                    });
                }
                // A genuine governor refusal (the ceiling race lost against
                // the non-binding preflight above) is the same capacity case
                // as GovernorNoHeadroom, not an infrastructure failure — the
                // caller (refine()) already knows how to turn that into a
                // quiet skip-and-continue. Any other reserve() failure (a
                // ledger/IO error) stays a genuine infra error and must still
                // stop the queue.
                if error
                    .downcast_ref::<crate::governor::GovernorReservationRefused>()
                    .is_some()
                {
                    return Err(GovernorNoHeadroom.into());
                }
                return Err(infrastructure(error));
            }
        };
        reserved.push(ReservedReview {
            reviewer: spec.reviewer,
            model: spec.model.clone(),
            reservation,
        });
    }

    // Every governed session gets its own run row. If bookkeeping cannot be
    // established for the whole batch, no reviewer starts.
    let mut prepared = Vec::with_capacity(reserved.len());
    let mut reserved = reserved.into_iter();
    while let Some(arm) = reserved.next() {
        match landing_ledger_write("starting merge-review run", || {
            ledger.start_review_run(task.id, arm.reviewer, Some(&arm.model))
        }) {
            Ok(run_id) => prepared.push(PreparedReview {
                reviewer: arm.reviewer,
                model: arm.model,
                reservation: arm.reservation,
                run_id,
            }),
            Err(error) => {
                let reason = format!("review batch could not start: {error:#}");
                let _ = landing_ledger_write("releasing merge-review reservation", || {
                    governor.release(ledger, arm.reservation)
                });
                for pending in reserved {
                    let _ = landing_ledger_write("releasing merge-review reservation", || {
                        governor.release(ledger, pending.reservation)
                    });
                }
                abort_prepared_reviews(ledger, &governor, prepared, &reason);
                return Err(error);
            }
        }
    }

    let mut outcomes = Vec::with_capacity(prepared.len());
    let mut prepared = prepared.into_iter();
    while let Some(arm) = prepared.next() {
        let started = Instant::now();
        // A prior review of THIS task by THIS reviewer kind resumes its own
        // thread — a re-review after a fix asks the same session to re-judge
        // its own findings rather than re-reading the whole diff. Narrowed
        // to the same model too: a routed model change is a fresh context,
        // same rationale as the implementer's same-rung check in runner.rs.
        //
        // Both registered task worktrees and the deterministic legacy review
        // checkout reuse the same cwd on every sweep, so every supported task
        // class can continue its per-(task, arm, model) reviewer thread.
        let resume_session_ref =
            match landing_ledger_write("loading prior merge-review session", || {
                ledger.last_run_ref(task.id, "review", Some(arm.reviewer.as_str()), arm.run_id)
            }) {
                Ok(prior) => prior
                    .filter(|prior| prior.model.as_deref() == Some(arm.model.as_str()))
                    .and_then(|prior| prior.session_ref),
                Err(error) => {
                    let reason = format!("review resume lookup failed: {error:#}");
                    abort_prepared_reviews(
                        ledger,
                        &governor,
                        std::iter::once(arm).chain(prepared),
                        &reason,
                    );
                    return Err(error);
                }
            };
        let outcome = crate::review::review_landing(
            ledger,
            arm.run_id,
            worktree,
            task,
            crate::review::ReviewConfig {
                base,
                tip,
                touches_foreman,
                reviewer: arm.reviewer,
                model: &arm.model,
                claude_bin: &context.fleet_policy.claude_bin.value,
                codex_bin: &context.fleet_policy.codex_bin.value,
                sibling_repos: context.fleet_policy.sibling_repos.value.as_deref(),
                reserve_usd: context.fleet_policy.reserve_usd.value,
                reserve_tokens: context.fleet_policy.reserve_tokens.value,
                stall_secs: crate::review::stall_secs_for(context.fleet_policy, arm.reviewer),
                verify_subdir: &context.opts.subdir,
                profile: context.profile,
                project_pack: &context.opts.project_pack,
                resume_session_ref: resume_session_ref.as_deref(),
            },
        )
        .unwrap_or_else(|error| crate::review::ReviewOutcome {
            approve: false,
            verdict: None,
            report: format!(
                "{} review could not complete (fail-closed reject):\n{error:#}",
                arm.reviewer.as_str()
            ),
            usage: Default::default(),
            delivery: "harness_error",
            findings: Vec::new(),
            files_inspected: Vec::new(),
            // The typed failure preserves the requested id only until a
            // not-found result is durably classified. After retirement it
            // carries None, so this terminal write cannot resurrect the
            // known-dead thread if fresh fallback setup later fails.
            session_ref: error.session_ref.clone(),
            usage_observed: false,
            output_observed: false,
            resume_failure: None,
        });

        let mut usage = outcome.usage.clone();
        if !arm.reviewer.meters_dollars() {
            usage.cost_usd = None;
        }
        let run_outcome = crate::executor::RunOutcome {
            stop: crate::executor::StopReason::Done,
            result: Some(if outcome.approve {
                "approved".into()
            } else {
                "rejected".into()
            }),
            error: None,
            usage,
            // Persisted so a LATER re-review of this same (task, arm) can
            // resume this exact thread — see `resume_session_ref` above.
            session_ref: outcome.session_ref.clone(),
            terminal_session_ref: None,
            usage_observed: outcome.usage_observed,
            output_observed: outcome.output_observed,
            resume_failure: None,
        };
        let duration_ms = i64::try_from(started.elapsed().as_millis())
            .unwrap_or(i64::MAX)
            .max(1);
        let finish = landing_ledger_write("finishing merge-review run", || {
            ledger.finish_run_as(
                arm.run_id,
                &run_outcome,
                duration_ms,
                Some(outcome.delivery),
            )
        });
        // Actuals first, then release: there is no window where neither the
        // hold nor the spend counts against the ceiling.
        if let Err(error) = finish {
            // Keep this arm's reservation: its actuals did not land, so
            // releasing would leave neither spend nor hold in the ceiling.
            // The normal stale-reservation sweep recovers it conservatively.
            let reason = format!("review run accounting failed: {error:#}");
            abort_prepared_reviews(ledger, &governor, prepared, &reason);
            return Err(error);
        }
        if let Err(error) = landing_ledger_write("releasing merge-review reservation", || {
            governor.release(ledger, arm.reservation)
        }) {
            eprintln!(
                "foreman: releasing {} review reservation failed: {error:#}",
                arm.reviewer.as_str()
            );
        }
        outcomes.push(crate::review::ReviewArmOutcome {
            reviewer: arm.reviewer,
            model: arm.model,
            run_id: arm.run_id,
            outcome,
        });
    }

    Ok(crate::review::merge_review_outcomes(outcomes))
}

pub(super) fn abort_prepared_reviews(
    ledger: &Ledger,
    governor: &crate::governor::Governor,
    reviews: impl IntoIterator<Item = PreparedReview>,
    reason: &str,
) {
    let outcome = crate::executor::RunOutcome {
        stop: crate::executor::StopReason::Error,
        result: None,
        error: Some(reason.to_string()),
        usage: Default::default(),
        session_ref: None,
        terminal_session_ref: None,
        usage_observed: false,
        output_observed: false,
        resume_failure: None,
    };
    for review in reviews {
        let _ = landing_ledger_write("aborting merge-review run", || {
            ledger.finish_run_as(review.run_id, &outcome, 0, Some("harness_error"))
        });
        let _ = landing_ledger_write("releasing merge-review reservation", || {
            governor.release(ledger, review.reservation)
        });
    }
}
