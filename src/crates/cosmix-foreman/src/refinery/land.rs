use super::*;

pub(super) fn land_one(
    ledger: &Ledger,
    task: &Task,
    opts: &RefineOptions,
    fleet_policy: &crate::config::FleetPolicy,
    push_delivery: Option<&PushDelivery>,
) -> Result<LandingReport> {
    #[cfg(test)]
    FAIL_NEXT_LANDING_UNANNOTATED.with(|fail| -> Result<()> {
        anyhow::ensure!(
            !fail.replace(false),
            "injected unannotated landing-path failure"
        );
        Ok(())
    })?;
    // This is the run that produced the branch now moving through later
    // gates. Legacy/manual tasks may not have one, in which case the gate
    // record remains task-scoped and no attribution is invented.
    let implementation_run = latest_implementation_run_for_landing(
        ledger,
        task.id,
        "reading landing implementation run",
    )?;
    let branch = task
        .branch
        .clone()
        .context("landable task without branch")?;
    let profile = opts
        .profiles
        .iter()
        .find(|profile| profile.name == task.verifier_profile)
        .cloned()
        .map(Ok)
        .unwrap_or_else(|| verify::lookup_profile(&task.verifier_profile))
        .map_err(infrastructure)?;
    let profile_name = profile.name.clone();
    let bounce = |detail: String, reason: FindingReason| {
        let mut report = bounced_report(task, detail, reason);
        report.branch.clone_from(&branch);
        report.profile.clone_from(&profile_name);
        report
    };
    let landed = |verified_profile: &str| LandingReport {
        task_id: task.id,
        branch: branch.clone(),
        profile: verified_profile.to_string(),
        landed: true,
        task_status: "landed",
        detail: String::new(),
        // Unused: a landed report never files a generic bounce finding.
        // BranchContract is an arbitrary placeholder, not a claim about it.
        reason: FindingReason::BranchContract,
        finding_recorded: false,
        ladder_charged: false,
    };

    // Defense in depth behind the ledger's write-time validation, plus the
    // laundering guards: the branch must be a real LOCAL branch (not
    // "origin/main", not a raw sha) and not the integration branch itself.
    if !crate::ledger::valid_branch_name(&branch) {
        return Ok(bounce(
            format!("refusing branch name {branch:?} as git argv"),
            FindingReason::BranchContract,
        ));
    }
    if branch == opts.integration {
        return Ok(bounce(
            format!(
                "branch is the integration branch {:?} — completing a task with the \
                 integration branch lands nothing",
                opts.integration
            ),
            FindingReason::BranchContract,
        ));
    }
    let (code, _, stderr) = git_status(
        &opts.repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )?;
    match code {
        Some(0) => {}
        // --verify --quiet exits 1 for "no such ref" — the task's problem.
        Some(1) => {
            return Ok(bounce(
                format!(
                    "no local branch named {branch:?} in {}",
                    opts.repo.display()
                ),
                FindingReason::BranchContract,
            ));
        }
        _ => {
            return Err(infrastructure_message(format!(
                "git rev-parse failed: {stderr}"
            )));
        }
    }

    // Snapshot the integration head FIRST and rebase onto that exact SHA —
    // rebasing onto the name while integration moves would produce a tip
    // whose CAS base is newer than its actual ancestor, and the CAS would
    // then silently discard the concurrent commit.
    let base = git(
        &opts.repo,
        &["rev-parse", &format!("refs/heads/{}", opts.integration)],
    )?
    .trim()
    .to_string();

    // The done -> landing transition made this task unclaimable before we
    // arrived here. Reuse its registered worktree so tier 1 consumes the
    // exact private target the implementer and runner warmed. Legacy/manual
    // tasks without a task worktree retain the detached fallback.
    let wt = match TempWorktree::add_or_reuse_task(
        &opts.repo,
        task.id,
        &branch,
        task.worktree.as_deref(),
    ) {
        Ok(worktree) => worktree,
        Err(error) => {
            if let Some(dirty) = error.downcast_ref::<DirtyTaskWorktree>() {
                return Ok(bounce(dirty.to_string(), FindingReason::BranchContract));
            }
            return Err(error);
        }
    };
    let (code, _, stderr) = rebase_for_landing(&wt.path, &base)?;
    if code != Some(0) {
        // A conflict is the task's problem; anything else (disk full, git
        // identity, corruption) is infrastructure and stops the queue.
        let conflicted = stderr.contains("CONFLICT")
            || stderr.contains("could not apply")
            || stderr.contains("Merge conflict");
        if git_status(&wt.path, &["rebase", "--abort"])?.0 != Some(0) {
            let cleanup_path = wt.path.clone();
            return Err(infrastructure_message(format!(
                "rebase failed AND rebase --abort failed; legacy-checkout cleanup will be \
                 attempted at {}: {stderr}",
                cleanup_path.display()
            )));
        }
        if !conflicted {
            return Err(infrastructure_message(format!(
                "git rebase failed for a non-conflict reason: {stderr}"
            )));
        }
        return Ok(bounce(
            format!(
                "rebase onto {} conflicted; resolve on the branch and complete the \
                 task again: {stderr}",
                opts.integration
            ),
            FindingReason::RebaseConflict,
        ));
    }
    let versioning = create_landing_version_commit(&wt.path, &base, task)?;
    for discarded in &versioning.discarded {
        landing_ledger_write("recording discarded agent version bump", || {
            ledger.file_finding_reasoned(
                Some(task.id),
                "info",
                "agent package-version edit discarded",
                discarded,
                "refinery",
                FindingReason::VersionBumpDiscarded,
            )
        })?;
    }
    if opts.echo && !versioning.bumped.is_empty() {
        println!(
            "task {}: refinery landing commit bumped {}",
            task.id,
            versioning.bumped.join(", ")
        );
    }
    let tip = git(&wt.path, &["rev-parse", "HEAD"])?.trim().to_string();
    // Tree identity, not commit identity: an --allow-empty commit advances
    // the sha while landing nothing.
    let tip_tree = git(&wt.path, &["rev-parse", "HEAD^{tree}"])?;
    let base_tree = git(&opts.repo, &["rev-parse", &format!("{base}^{{tree}}")])?;
    if tip == base || tip_tree == base_tree {
        // One alias: a crash between a past ff-merge and its ledger write
        // makes genuinely-landed work look like a no-op here. That window is
        // healed by recover_landings() before the queue runs; a task reaching
        // this bounce was never recorded as entering a landing.
        return Ok(bounce(
            "landing would not change the integration tree (empty, already-merged, \
             or content-free branch) — nothing to land"
                .into(),
            FindingReason::BranchContract,
        ));
    }

    // SELF-MODIFICATION IS ALWAYS REVIEWED, --review or not: the policy gate
    // deliberately lets fleet agents edit the foreman crate (their backlog
    // IS foreman work), and the stated backstop for that freedom is the
    // merge-authority review further below — a backstop that only holds if
    // the code binds it. A diff-listing failure fails closed into the
    // mandatory path.
    let touches_foreman = git_status(
        &wt.path,
        &["diff", "--name-only", &format!("{base}..{tip}")],
    )
    .map(|(code, out, _)| {
        code != Some(0) || out.lines().any(|l| l.contains("crates/cosmix-foreman/"))
    })
    .unwrap_or(true);
    let needs_review = opts.review || touches_foreman;
    let reuses_approved_review =
        needs_review && recorded_approved_review(ledger, task, &base, &tip)?;
    // Resolve the full route before spending verifier time. This validates
    // strict config (fixed override, two-arm flag, model names) up front and
    // snapshots the implementer-derived choice before review runs are added.
    let review_specs = if needs_review && !reuses_approved_review {
        crate::review::reviewers_for_task(ledger, task, fleet_policy)
            .map_err(infrastructure)?
            .into_iter()
            .map(|reviewer| {
                Ok(ReviewSpec {
                    reviewer,
                    model: crate::review::model_for(fleet_policy, reviewer)?,
                })
            })
            .collect::<Result<Vec<_>>>()?
    } else {
        Vec::new()
    };
    validate_review_lanes(
        &review_specs,
        opts.lane_policy.as_ref(),
        crate::manifest::credential_in_environment,
    )?;

    // Governor preflight: BEFORE tier-1 verification (the expensive part),
    // check whether the review's reservation could currently be admitted —
    // measured overnight 2026-08-18: without this check here, a refused
    // reservation surfaced only after tier-1 cargo had already run, and the
    // landing->done retry oscillated every refine tick, burning minutes of
    // cargo on a hold that was never going to fit. Non-binding: a plain
    // headroom read (spend + reserved + request <= ceiling), no hold taken —
    // the real reserve() further below (at review time, once tier-1 is
    // green) stays the authoritative gate; reserving here too would
    // double-hold. A failed read (ledger hiccup) is treated as "can't
    // confirm it fits" — skip safely and retry next tick rather than spend
    // cargo minutes to find out.
    if needs_review && !reuses_approved_review {
        let governor = crate::governor::Governor::from_policy(&opts.db, fleet_policy);
        // Lane-aware sum: two-arm review holds two token reserves but only
        // Claude's dollar reserve; Codex has no reportable dollar cost.
        let metered_arms = review_specs
            .iter()
            .filter(|spec| spec.reviewer.meters_dollars())
            .count();
        let request_usd = fleet_policy.reserve_usd.value * metered_arms as f64;
        let request_tokens = fleet_policy
            .reserve_tokens
            .value
            .saturating_mul(review_specs.len() as u64);
        let fits = landing_ledger_write("checking merge-review headroom", || {
            governor.check_headroom_dimensions(
                ledger,
                metered_arms > 0,
                request_usd,
                request_tokens,
            )
        })
        .unwrap_or(false);
        if !fits {
            return Err(GovernorNoHeadroom.into());
        }
    }

    // Task-owned profile: landing through a weaker profile than the spec
    // demanded is the exact gaming the profile column exists to stop. The
    // verified tip is recorded with the report — it is the recovery evidence
    // if a crash lands the merge without the ledger write.
    // The done -> landing claim above is already committed. Keep this
    // explicit runtime fence beside the subprocess boundary so a future
    // refactor cannot quietly carry an unchecked transaction into cargo.
    ledger
        .ensure_autocommit("running the refinery verifier")
        .map_err(infrastructure)?;
    let gate = verify::GateRequest::local(
        task.id,
        task.attempt,
        verify::GateIdentity::RefineryTier,
        opts.tier,
        &wt.path,
        &profile,
        Some(&opts.subdir),
        &task.crates,
        fleet_policy,
    )
    .and_then(|request| verify::GateRunner::run_gate(&verify::LOCAL_GATE_RUNNER, &request));
    let report = match gate {
        Ok(report) => report,
        Err(error) => match error.downcast::<verify::GateDirectoryFailure>() {
            Ok(failure) if verifier_directory_error_is_infrastructure(&failure.0) => {
                return Err(infrastructure(failure.0));
            }
            Ok(failure) => {
                return Ok(bounce(
                    format!(
                        "verifier dir for profile {:?} could not be resolved (escapes the \
                         worktree, or the directory is missing on this branch): {:#}",
                        task.verifier_profile, failure.0
                    ),
                    FindingReason::VerifierRed,
                ));
            }
            Err(error) => return Err(infrastructure(error)),
        },
    };
    ledger
        .ensure_autocommit("recording the refinery verifier outcome")
        .map_err(infrastructure)?;
    let record = serde_json::json!({ "tip": tip, "base": base, "report": report });
    landing_ledger_write("recording landing verification", || {
        if let Some(run_id) = implementation_run {
            ledger.record_run_verification(
                task.id,
                run_id,
                opts.tier as i64,
                report.pass,
                &record.to_string(),
            )
        } else {
            ledger.record_verification(task.id, opts.tier as i64, report.pass, &record.to_string())
        }
    })?;
    landing_ledger_write("recording sccache bypass finding", || {
        verify::file_sccache_bypass_findings(ledger, task.id, &report, "refinery").map(|_| 0)
    })?;
    if !report.pass {
        return Ok(bounce(
            format!(
                "tier-{} ({}) red after rebase:\n{}",
                opts.tier,
                report.profile,
                report.failure_digest()
            ),
            FindingReason::VerifierRed,
        ));
    }

    // Operator policy hook: PUBLIC repositories cannot rely on local,
    // unversioned git hooks being present in the fleet clone. Run the
    // configured gate against the exact rebased throwaway tree after the
    // normal tier is green and before merge authority. It is argv-exec only
    // (no shell), bounded like a tier-1 step, and every configured-gate
    // failure is a task bounce — including malformed configuration or an
    // executable that cannot be spawned. Unset remains deliberately open.
    match verify::run_landing_gate_with_manifest(
        &wt.path,
        fleet_policy,
        opts.landing_gate.as_ref(),
        opts.project_root.is_some(),
    ) {
        Ok(None) => {}
        Ok(Some(step)) if step.pass => {
            if let Some(body) = step.sccache_bypass_digest("landing-gate") {
                landing_ledger_write("recording landing-gate sccache bypass finding", || {
                    ledger
                        .file_sccache_bypass_findings(
                            task.id,
                            std::slice::from_ref(&body),
                            "refinery",
                        )
                        .map(|_| 0)
                })?;
            }
        }
        Ok(Some(step)) => {
            let incident = step
                .sccache_incident
                .as_ref()
                .map(|incident| incident.render())
                .unwrap_or_default();
            return Ok(bounce(
                format!(
                    "landing gate red:\n$ {} (exit {:?})\n{}\n{}",
                    step.command,
                    step.exit_code,
                    step.tail.trim(),
                    incident.trim()
                ),
                FindingReason::VerifierRed,
            ));
        }
        Err(e) => {
            return Ok(bounce(
                format!("landing gate could not run (fail closed): {e:#}"),
                FindingReason::InfraRefusal,
            ));
        }
    }

    // Merge authority judges the actual diff. Every arm is governed and
    // accounted as its own run; all reservations are acquired before either
    // session starts, so two-arm review cannot degrade into one-arm review
    // after the first reviewer has already spent. Any failure or REJECT is a
    // recorded, fail-closed rejection.
    if needs_review && !reuses_approved_review {
        let review_context = LandingReviewContext {
            ledger,
            task,
            opts,
            fleet_policy,
            worktree: &wt.path,
            base: &base,
            tip: &tip,
            touches_foreman,
            profile: &profile,
        };
        let reviews = run_landing_reviews(&review_context, &review_specs)?;
        let (review_runs, review_findings) = review_ledger_records(&reviews);
        let record = crate::review::verification_record(&base, &tip, &reviews).to_string();
        let (_, finding_ids) = landing_ledger_write(
            "recording atomic merge-review verdict and typed findings",
            || {
                ledger.record_review_verification(
                    task.id,
                    implementation_run,
                    reviews.approve,
                    &record,
                    &review_runs,
                    &review_findings,
                )
            },
        )?;
        if !reviews.approve {
            let reason = reviews
                .rejection_reason()
                .context("a rejected review batch omitted its disposition reason")?;
            let mut rejected = bounce(
                format!(
                    "merge-authority review rejected the landing:\n{}",
                    reviews.report()
                ),
                reason,
            );
            rejected.finding_recorded = !finding_ids.is_empty();
            return Ok(rejected);
        }
    }

    // The task worktree is the verified object. Re-check its ordinary Git
    // state at the final authority boundary so a stray tracked/untracked
    // write, branch move, or reviewer side effect after verification cannot
    // be smuggled through the integration CAS. target/ remains ignored and
    // may keep changing: it is the deliberately reused build cache.
    let final_tip = git(&wt.path, &["rev-parse", "HEAD"])?;
    let mut allowed_targets = report
        .target_dir
        .as_deref()
        .map(PathBuf::from)
        .into_iter()
        .collect::<Vec<_>>();
    // An empty profile has no Cargo step and therefore no report target, but
    // the implementation session still inherited this verifier-directory
    // pin and may legitimately have warmed it. Keep the cache without making
    // "Cargo happened to run in the gate" a condition of worktree reuse.
    allowed_targets.push(crate::target_dir::pinned_target_dir(
        &wt.path,
        profile.cwd.as_deref().or(Some(opts.subdir.as_str())),
    )?);
    if fleet_policy.landing_gate.value.is_some() {
        allowed_targets.push(crate::target_dir::pinned_target_dir(&wt.path, None)?);
    }
    let final_dirty = worktree_dirt_except_targets(&wt.path, &allowed_targets)?;
    if final_tip.trim() != tip || !final_dirty.is_empty() {
        return Ok(bounce(
            format!(
                "task worktree changed after landing verification; expected tip {tip}, got {}\n{}",
                final_tip.trim(),
                final_dirty.join("\n")
            ),
            FindingReason::BranchContract,
        ));
    }

    // Land the verified sha with an atomic compare-and-swap on the REF
    // itself: update-ref names refs/heads/<integration> explicitly and
    // requires the old value, so neither a concurrent checkout (merge would
    // land into whatever HEAD names) nor a moved integration head can
    // corrupt the landing. The working tree is synced afterwards only if the
    // repo is still on integration and still clean — a stale-but-correct
    // worktree beats destroying anyone's in-flight state.
    let push_intents = if push_delivery.is_some() {
        let (_, update, delete) = journal_then_advance_integration(
            ledger,
            task.id,
            task.attempt,
            &opts.integration,
            &tip,
            || advance_integration_ref(opts, &base, &tip),
        )?;
        Some((update, delete))
    } else {
        advance_integration_ref(opts, &base, &tip)?;
        None
    };
    // The landing IS the CAS above — from here nothing may fail it (an error
    // after the ref advanced would restore the task to done and desync git
    // from the ledger). Checkout sync is best effort AND non-destructive:
    // a two-tree `read-tree -um base tip` fast-forwards the index/worktree
    // content (HEAD already names the tip via the moved ref), refuses to
    // clobber local changes, and touches no refs at all. A repo that
    // drifted off the integration branch is left alone with a warning.
    let synced = git(&opts.repo, &["branch", "--show-current"])
        .map(|cur| cur.trim() == opts.integration)
        .unwrap_or(false)
        && git_status(&opts.repo, &["read-tree", "-um", &base, &tip])
            .map(|(code, _, _)| code == Some(0))
            .unwrap_or(false);
    if !synced {
        eprintln!(
            "foreman: {} advanced to {tip} but the checkout was not synced \
             (off-branch or local changes) — `git reset --keep {tip}` there \
             when ready",
            opts.integration
        );
    }
    if let (Some(delivery), Some((update, delete))) = (push_delivery, push_intents.as_ref()) {
        deliver_remote_pushes(ledger, &opts.repo, delivery, update, delete);
    }
    if let Some(run_id) = implementation_run
        && let Err(e) = landing_ledger_write("recording landed implementation quality", || {
            ledger.set_run_quality(run_id, "landed")
        })
    {
        // The ref advance above is the landing commit point. A ledger write
        // failure after it may be reported, but must never pretend the
        // already-landed task bounced or retry the merge.
        eprintln!("foreman: recording landed quality for run {run_id} failed: {e:#}");
    }
    Ok(landed(&report.profile))
}

fn advance_integration_ref(opts: &RefineOptions, base: &str, tip: &str) -> Result<String> {
    git(
        &opts.repo,
        &[
            "update-ref",
            "-m",
            "foreman refinery landing",
            &format!("refs/heads/{}", opts.integration),
            tip,
            base,
        ],
    )
    .with_context(|| {
        format!(
            "atomically advancing {} {base} -> {tip} (integration moved since \
             this landing was prepared?)",
            opts.integration
        )
    })
}

pub(super) fn deliver_remote_pushes(
    ledger: &Ledger,
    repo: &Path,
    delivery: &PushDelivery,
    update: &PushIntent,
    delete: &PushIntent,
) {
    if deliver_update_push(ledger, repo, delivery, update)
        == Some(crate::remote_git::RemoteOutcome::Succeeded)
        && let Err(error) = deliver_delete_push(ledger, repo, delivery, delete)
    {
        eprintln!(
            "foreman: refusing remote task-branch deletion for task {}: {error:#}",
            delete.task_id
        );
    }
}

pub(super) fn deliver_update_push(
    ledger: &Ledger,
    repo: &Path,
    delivery: &PushDelivery,
    update: &PushIntent,
) -> Option<crate::remote_git::RemoteOutcome> {
    if update.kind != crate::ledger::PushIntentKind::Update {
        eprintln!(
            "foreman: refusing to deliver non-update push journal {} through the update path",
            update.id
        );
        return None;
    }
    let runner = crate::remote_git::RemoteGitRunner::new(
        REMOTE_PUSH_DEADLINE,
        crate::remote_git::DEFAULT_OUTPUT_LIMIT,
    )
    .expect("fixed remote push runner policy is valid");
    let run = runner.run_with_credentials(
        repo,
        [
            OsStr::new("push"),
            OsStr::new("--porcelain"),
            OsStr::new("--"),
            OsStr::new(&delivery.remote),
            OsStr::new(&update.refspec),
        ],
        &delivery.credentials,
    );
    if let Err(error) = record_update_push_run(ledger, update, &run) {
        eprintln!(
            "foreman: remote update journal {} remains unknown after {:?}: {error:#}",
            update.id, run.outcome
        );
    } else if run.outcome != crate::remote_git::RemoteOutcome::Succeeded {
        eprintln!(
            "foreman: remote integration update for task {} is {}: {}",
            update.task_id,
            run.outcome.as_str(),
            remote_push_detail(&run)
        );
    }
    Some(run.outcome)
}

/// Deliver the immutable delete intent only after proving that it is the
/// journal row for this task's own recorded branch. The supplied intent is
/// not authority: callers can hold stale or forged values, while the task
/// row and durable journal are the two records the refinery owns.
pub(super) fn deliver_delete_push(
    ledger: &Ledger,
    repo: &Path,
    delivery: &PushDelivery,
    supplied: &PushIntent,
) -> Result<crate::remote_git::RemoteOutcome> {
    let delete = authoritative_delete_intent(ledger, supplied)?;
    let runner = crate::remote_git::RemoteGitRunner::new(
        REMOTE_PUSH_DEADLINE,
        crate::remote_git::DEFAULT_OUTPUT_LIMIT,
    )
    .expect("fixed remote push runner policy is valid");
    let run = runner.run_with_credentials(
        repo,
        [
            OsStr::new("push"),
            OsStr::new("--porcelain"),
            OsStr::new("--"),
            OsStr::new(&delivery.remote),
            OsStr::new(&delete.refspec),
        ],
        &delivery.credentials,
    );
    if let Err(error) = record_delete_push_run(ledger, &delete, &run) {
        eprintln!(
            "foreman: remote delete journal {} remains unknown after {:?}: {error:#}",
            delete.id, run.outcome
        );
    } else if run.outcome != crate::remote_git::RemoteOutcome::Succeeded {
        eprintln!(
            "foreman: remote task-branch deletion for task {} is {}: {}",
            delete.task_id,
            run.outcome.as_str(),
            remote_push_detail(&run)
        );
    }
    Ok(run.outcome)
}

fn authoritative_delete_intent(ledger: &Ledger, supplied: &PushIntent) -> Result<PushIntent> {
    anyhow::ensure!(
        supplied.kind == crate::ledger::PushIntentKind::Delete,
        "refusing remote branch deletion through a non-delete journal row"
    );
    let task = ledger.task(supplied.task_id)?.with_context(|| {
        format!(
            "refusing remote branch deletion for missing task {}",
            supplied.task_id
        )
    })?;
    let branch = task
        .branch
        .context("refusing remote branch deletion for a task without a recorded branch")?;
    let expected_refspec = format!(":refs/heads/{branch}");
    anyhow::ensure!(
        supplied.refspec == expected_refspec,
        "refusing caller-supplied remote branch deletion {:?}; task {} owns {:?}",
        supplied.refspec,
        supplied.task_id,
        expected_refspec
    );

    let durable = ledger
        .push_intents_for_attempt(supplied.task_id, supplied.attempt)?
        .into_iter()
        .find(|intent| intent.id == supplied.id)
        .context("refusing remote branch deletion without its durable journal row")?;
    anyhow::ensure!(
        durable.kind == crate::ledger::PushIntentKind::Delete
            && durable.task_id == supplied.task_id
            && durable.attempt == supplied.attempt
            && durable.refspec == expected_refspec
            && durable.verified_tip == supplied.verified_tip,
        "refusing remote branch deletion because the supplied intent does not match the durable delete row"
    );
    Ok(durable)
}

pub(super) fn record_update_push_run(
    ledger: &Ledger,
    update: &PushIntent,
    run: &crate::remote_git::RemoteGitRun,
) -> Result<bool> {
    let outcome = match run.outcome {
        crate::remote_git::RemoteOutcome::Succeeded => PushIntentOutcome::Succeeded,
        crate::remote_git::RemoteOutcome::Failed => PushIntentOutcome::Failed,
        crate::remote_git::RemoteOutcome::Unknown => PushIntentOutcome::Unknown,
    };
    let detail = remote_push_detail(run);
    ledger_write_with_busy_retry("recording remote update push outcome", || {
        ledger.record_push_outcome(update.id, outcome, &detail)
    })
}

pub(super) fn record_delete_push_run(
    ledger: &Ledger,
    delete: &PushIntent,
    run: &crate::remote_git::RemoteGitRun,
) -> Result<bool> {
    anyhow::ensure!(
        delete.kind == crate::ledger::PushIntentKind::Delete,
        "remote deletion outcome requires a delete journal row"
    );
    let outcome = match run.outcome {
        crate::remote_git::RemoteOutcome::Succeeded => PushIntentOutcome::Succeeded,
        crate::remote_git::RemoteOutcome::Failed => PushIntentOutcome::Failed,
        crate::remote_git::RemoteOutcome::Unknown => PushIntentOutcome::Unknown,
    };
    let detail = remote_push_detail(run);
    ledger_write_with_busy_retry("recording remote delete push outcome", || {
        ledger.record_push_outcome(delete.id, outcome, &detail)
    })
}

fn remote_push_detail(run: &crate::remote_git::RemoteGitRun) -> String {
    const DETAIL_LIMIT: usize = 16 * 1024;
    let mut detail = format!(
        "termination={:?}; stdout_truncated={}; stderr_truncated={}",
        run.termination, run.stdout_truncated, run.stderr_truncated
    );
    if let Some(error) = &run.io_error {
        detail.push_str("; io_error=");
        detail.push_str(error);
    }
    detail.push_str("\nstdout:\n");
    detail.push_str(&String::from_utf8_lossy(&run.stdout));
    detail.push_str("\nstderr:\n");
    detail.push_str(&String::from_utf8_lossy(&run.stderr));
    if detail.len() > DETAIL_LIMIT {
        let mut boundary = DETAIL_LIMIT;
        while !detail.is_char_boundary(boundary) {
            boundary -= 1;
        }
        detail.truncate(boundary);
    }
    detail
}

/// Commit immutable delivery intent, leave the ledger transaction, and only
/// then invoke the local ref advance. Keeping the CAS behind this callable
/// boundary makes the ordering observable in tests and prevents a future
/// refactor from wrapping journal and ref movement in one apparent unit.
pub(super) fn journal_then_advance_integration<T>(
    ledger: &Ledger,
    task_id: i64,
    attempt: i64,
    integration: &str,
    verified_tip: &str,
    advance: impl FnOnce() -> Result<T>,
) -> Result<(T, PushIntent, PushIntent)> {
    let (update, delete) = landing_ledger_write("recording push intents before landing", || {
        ledger.record_push_intents_before_landing(task_id, attempt, integration, verified_tip)
    })?;
    ledger
        .ensure_autocommit("advancing the integration ref after push-intent journalling")
        .map_err(infrastructure)?;
    let advanced = advance()?;
    Ok((advanced, update, delete))
}
