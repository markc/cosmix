use super::*;

/// Retry a refinery ledger operation only when SQLite reports transient lock
/// contention. Every statement remains atomic: a busy transition either did
/// not happen and is safe to retry, or returned success and is not repeated.
/// Other failures retain their original error and fail immediately.
pub(crate) fn landing_ledger_write<T>(
    operation: &str,
    write: impl FnMut() -> Result<T>,
) -> Result<T> {
    ledger_write_with_busy_retry(operation, write).map_err(infrastructure)
}

pub(super) fn latest_implementation_run_for_landing(
    ledger: &Ledger,
    task_id: i64,
    operation: &str,
) -> Result<Option<i64>> {
    landing_ledger_write(operation, || ledger.latest_implementation_run(task_id))
}

/// Apply the recovery rule without performing any network I/O itself.
/// Definitive failures are first durably claimed as `unknown`, then handed to
/// the supplied replayer. Ambiguous outcomes are report-only because the
/// remote may already hold the ref. The guarded claim is single-winner across
/// concurrent recovery processes.
///
/// The remote-push slice supplies the bounded Git executor. Keeping that
/// dependency injected lets this journal-only slice prove the policy while
/// remaining incapable of contacting a remote.
pub fn recover_push_intents(
    ledger: &Ledger,
    mut replay_failed: impl FnMut(&PushIntent) -> Result<(PushIntentOutcome, String)>,
    mut report_unknown: impl FnMut(&PushIntent),
) -> Result<PushRecoveryReport> {
    let mut report = PushRecoveryReport::default();
    for intent in ledger.outstanding_push_intents()? {
        match intent.outcome {
            PushIntentOutcome::Failed => {
                let claimed =
                    landing_ledger_write("claiming failed push intent for replay", || {
                        ledger.claim_failed_push_for_replay(intent.id)
                    })?;
                if !claimed {
                    continue;
                }
                ledger
                    .ensure_autocommit("replaying a durably claimed push intent")
                    .map_err(infrastructure)?;
                let mut claimed_intent = intent;
                claimed_intent.outcome = PushIntentOutcome::Unknown;
                claimed_intent.detail = PUSH_REPLAY_CLAIM_DETAIL.into();
                let (outcome, detail) = replay_failed(&claimed_intent)?;
                landing_ledger_write("recording replayed push intent outcome", || {
                    ledger.record_push_outcome(claimed_intent.id, outcome, &detail)
                })?;
                report.replayed_failed += 1;
            }
            PushIntentOutcome::Unknown => {
                report_unknown(&intent);
                report.reported_unknown += 1;
            }
            PushIntentOutcome::Succeeded => {
                // The outstanding query excludes terminal successes. Retain
                // the arm so an enum extension cannot silently become replay.
            }
        }
    }
    Ok(report)
}

/// Commit the landing disposition and its bounce handoff as one ledger
/// transaction, then wake dispatch. A failed finding insert therefore leaves
/// the task in `landing`, and a successful wake can only observe a committed
/// bounce with its finding already present.
#[allow(clippy::too_many_arguments)]
pub(super) fn finish_landing_and_maybe_wake(
    ledger: &Ledger,
    task_id: i64,
    to: &str,
    implementation_run: Option<i64>,
    report: &LandingReport,
    infra_threshold: i64,
    infra_park_threshold: i64,
    branch_contract_limit: i64,
    now: chrono::DateTime<chrono::Utc>,
    fire_wake: impl FnOnce(),
) -> Result<LandingDisposition> {
    let title = format!("refinery bounce: {}", report.branch);
    let finding = (!report.landed && !report.finding_recorded)
        .then_some((title.as_str(), report.detail.as_str()));
    let result = landing_ledger_write("finishing landing transition", || {
        ledger.finish_landing_classified_with_infra(
            task_id,
            to,
            implementation_run,
            (!report.landed).then_some(report.reason),
            matches!(
                report.reason,
                FindingReason::InfraRefusal | FindingReason::PolicyDenied
            )
            .then_some(report.detail.as_str()),
            infra_threshold,
            infra_park_threshold,
            branch_contract_limit,
            finding,
            now,
        )
    })?;
    if result.moved && !report.landed {
        // Best-effort ABP wake — see wake.rs. A refinery bounce makes
        // quality bounces dispatchable immediately; infrastructure bounces
        // remain hidden until their short backoff expires.
        fire_wake();
    }
    Ok(result)
}

pub(super) fn recorded_review_has_delivered_reject(value: &serde_json::Value) -> bool {
    value
        .get("arms")
        .and_then(|arms| arms.as_array())
        .is_some_and(|arms| {
            arms.iter().any(|arm| {
                arm.get("delivery").and_then(|delivery| delivery.as_str()) == Some("delivered")
                    && arm.get("verdict").and_then(|verdict| verdict.as_str()) == Some("REJECT")
            })
        })
}

/// Tasks stranded in 'landing' by a crash: the recorded verified tip decides
/// — already an ancestor of integration means the merge happened (landed);
/// otherwise the landing never completed (back to done for a retry).
pub(super) fn recover_landings(ledger: &Ledger, opts: &RefineOptions) -> Result<()> {
    for task in ledger.landing_tasks()? {
        if let Some(branch) = task.branch.as_deref() {
            recover_interrupted_task_rebase(&opts.repo, task.id, branch, task.worktree.as_deref())
                .with_context(|| {
                    format!(
                        "task {}: recovering the task worktree after an interrupted landing",
                        task.id
                    )
                })?;
        }
        // Only a GREEN recorded verification can vouch for a landing; its
        // tip being an ancestor of integration means the merge happened.
        // Only rows stamped with this task attempt are eligible; rows from
        // older attempts (and pre-migration rows whose attempt is NULL) are
        // not evidence for this landing. The eligible rows come in two
        // shapes (tier gate: {tip, report:
        // {pass}}, review: {tip, approve}); unrelated rows (manual
        // `foreman verify --task`) carry no tip and are skipped. The NEWEST
        // tip-bearing row DECIDES: green → the merge may have happened
        // (ancestor check below); red (a rejection or red gate) → this
        // landing did not complete, and falling back to an OLDER green row
        // would resurrect a previous attempt's landing as this one's.
        let mut evidence = None;
        for report in ledger.verification_reports_for_attempt(task.id, task.attempt, 10)? {
            let v: serde_json::Value = serde_json::from_str(&report).with_context(|| {
                format!(
                    "task {}: malformed verification-report JSON while recovering an \
                     interrupted landing — cannot safely determine whether the merge \
                     happened; the newest-tip-row rule requires reading rows in \
                     order and this one could not be read",
                    task.id
                )
            })?;
            let tip = v.get("tip").and_then(|t| t.as_str()).map(String::from);
            if let Some(tip) = tip {
                let base = v.get("base").and_then(|b| b.as_str()).map(String::from);
                let tier_pass = v
                    .get("report")
                    .and_then(|r| r.get("pass"))
                    .and_then(|p| p.as_bool());
                let review_approve = v.get("approve").and_then(|a| a.as_bool());
                let green = tier_pass == Some(true) || review_approve == Some(true);
                let red_reason = if tier_pass == Some(false) {
                    FindingReason::VerifierRed
                } else if review_approve == Some(false) && recorded_review_has_delivered_reject(&v)
                {
                    FindingReason::ReviewRejected
                } else {
                    FindingReason::InfraRefusal
                };
                evidence = Some((tip, base, green, red_reason));
                break;
            }
            // Valid JSON without a tip (for example a manual verification)
            // is not landing evidence; keep looking at older rows.
        }
        // A RED newest row (gate or review rejection recorded, crash before
        // the transition) recovers to BOUNCED — sending it back to done
        // would hand a stochastic review another roll of the dice.
        if let Some((_, _, false, reason)) = &evidence {
            let implementation_run = latest_implementation_run_for_landing(
                ledger,
                task.id,
                "reading recovered implementation run",
            )?;
            let threshold = if *reason == FindingReason::InfraRefusal {
                crate::ledger::infra_refusal_finding_threshold()?
            } else {
                1
            };
            let park_threshold = if *reason == FindingReason::InfraRefusal {
                crate::ledger::infra_refusal_park_threshold()?
            } else {
                1
            };
            let disposition = landing_ledger_write("recovering red landing", || {
                ledger.finish_landing_classified_with_infra(
                    task.id,
                    "bounced",
                    implementation_run,
                    Some(*reason),
                    (*reason == FindingReason::InfraRefusal)
                        .then_some("recorded landing verdict was not delivered"),
                    threshold,
                    park_threshold,
                    crate::ledger::DEFAULT_BRANCH_CONTRACT_LIMIT,
                    Some((
                        "refinery recovered a recorded red landing",
                        "The previous refinery process stopped after recording a red landing verdict but before disposition. The task was bounced through the classified recovery path; inspect the recorded verification report before retrying.",
                    )),
                    chrono::Utc::now(),
                )
            })?;
            if disposition.moved {
                eprintln!(
                    "foreman: recovered task {} from interrupted landing -> bounced \
                     (a red verdict was recorded before the crash)",
                    task.id
                );
            }
            continue;
        }
        let tip = evidence.as_ref().map(|(tip, _, _, _)| tip);
        let landed = match tip {
            Some(tip) => {
                let (code, _, stderr) = git_status(
                    &opts.repo,
                    &[
                        "merge-base",
                        "--is-ancestor",
                        tip.as_str(),
                        &opts.integration,
                    ],
                )?;
                match code {
                    Some(0) => true,
                    // 1 = "not an ancestor"; anything else is git failing,
                    // not the question being answered.
                    Some(1) => false,
                    _ => anyhow::bail!("merge-base failed during landing recovery: {stderr}"),
                }
            }
            None => false,
        };
        let to = if landed { "landed" } else { "done" };
        if landed {
            // A real crash immediately after update-ref leaves HEAD naming
            // the new integration tip while the index/worktree still reflect
            // the recorded base. Heal that exact two-tree fast-forward before
            // the normal clean-tree preflight runs. read-tree refuses to
            // overwrite unrelated local edits.
            if let (Some(tip), Some(base)) = (
                evidence.as_ref().map(|(tip, _, _, _)| tip.as_str()),
                evidence
                    .as_ref()
                    .and_then(|(_, base, _, _)| base.as_deref()),
            ) {
                let on_integration = git(&opts.repo, &["branch", "--show-current"])
                    .is_ok_and(|current| current.trim() == opts.integration);
                if on_integration
                    && !matches!(
                        git_status(&opts.repo, &["read-tree", "-um", base, tip]),
                        Ok((Some(0), _, _))
                    )
                {
                    eprintln!(
                        "foreman: recovered integration ref at {tip}, but could not sync the checkout from {base}; clean it before the refinery can continue"
                    );
                }
            }
        }
        if landing_ledger_write("recovering interrupted landing", || {
            ledger.transition_if(task.id, "landing", to)
        })? {
            eprintln!(
                "foreman: recovered task {} from interrupted landing -> {to}",
                task.id
            );
            if landed {
                reclaim_landed_scratch(ledger, opts, task.id);
                if let Some(branch) = task.branch.as_deref() {
                    prune_landed_branch(opts, branch);
                }
            }
        }
    }
    Ok(())
}

/// A landing is already durable before this runs, so scratch cleanup is
/// best-effort and loud just like branch pruning. It must never turn an
/// integrated task into a false bounce. Goes through
/// [`crate::scratch::reclaim_task_scratch_leased`], never a plain re-read, so
/// an operator requeue racing this exact moment cannot hand the worktree
/// back to a live run while cleanup is still deleting it.
pub(super) fn reclaim_landed_scratch(ledger: &Ledger, opts: &RefineOptions, task_id: i64) {
    let Some(fleet_dir) = opts.project_root.as_deref().or_else(|| opts.repo.parent()) else {
        eprintln!(
            "foreman: cannot resolve fleet root for task {task_id} scratch cleanup from repo {}",
            opts.repo.display()
        );
        return;
    };
    let report =
        crate::scratch::reclaim_task_scratch_leased(ledger, task_id, &opts.repo, fleet_dir, false);
    eprintln!("foreman: {}", report.summary(task_id, false));
    for refusal in report.skipped_paths {
        eprintln!("foreman: task {task_id} scratch SKIPPED: {refusal}");
    }
}

/// Remove a landed task name from both places agents could accidentally
/// resurrect it from. The integration CAS has already completed when this is
/// called, so cleanup is deliberately best-effort and loud: a network or ref
/// failure must not turn an already-landed commit into a false bounce.
pub(super) fn prune_landed_branch(opts: &RefineOptions, branch: &str) {
    if branch == "main" || branch == opts.integration {
        eprintln!("foreman: refusing to prune protected branch {branch:?} after landing");
        return;
    }

    let local_ref = format!("refs/heads/{branch}");
    match git_status(&opts.repo, &["update-ref", "-d", &local_ref]) {
        Ok((Some(0), _, _)) => eprintln!(
            "foreman: pruned landed branch {branch} from shared repo {}",
            opts.repo.display()
        ),
        Ok((code, _, stderr)) => eprintln!(
            "foreman: could not prune landed branch {branch} from shared repo {} ({code:?}): {}",
            opts.repo.display(),
            stderr.trim()
        ),
        Err(error) => eprintln!(
            "foreman: could not prune landed branch {branch} from shared repo {}: {error:#}",
            opts.repo.display()
        ),
    }
}
