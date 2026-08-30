use super::launch::launch;
use super::*;

pub(super) fn run(context: Context, command: Cmd) -> Result<()> {
    let Context {
        manifest,
        resolved_db,
        db,
        db_create,
        fleet_policy,
    } = context;
    match command {
        Cmd::Dispatch {
            workdir,
            task,
            kind,
            max_tasks,
            max_wall_secs,
            branch_template,
            integration,
            subdir,
            policy,
            no_verify,
            dry_run,
            stall_secs,
        } => {
            let ledger = open_ledger(&resolved_db, manifest.as_ref())?;
            let workdir = resolve_project_repo_arg(workdir, manifest.as_ref(), "--workdir")?
                .unwrap_or_else(|| PathBuf::from("."));
            let branch_template =
                branch_template.or_else(|| manifest.as_ref().map(|m| m.branch_template.clone()));
            let integration = resolve_project_integration_arg(integration, manifest.as_ref())?;
            let subdir = subdir.or_else(|| manifest.as_ref().and_then(|m| m.subdir.clone()));
            let project_pack = manifest
                .as_ref()
                .map(|m| m.instruction_pack.clone())
                .unwrap_or_default();
            let worktree_template = manifest
                .as_ref()
                .map(|m| m.worktree_template.clone())
                .unwrap_or_else(|| "task-{id}".to_string());
            let profiles = manifest
                .as_ref()
                .map(|m| m.profiles.clone())
                .unwrap_or_default();
            let ladder = fleet_policy.ladder();
            // Resolved once, up front: a mistyped knob fails the sweep here
            // rather than silently defaulting mid-refusal.
            let infra_refusals_finding = cosmix_foreman::ledger::infra_refusal_finding_threshold()?;
            let infra_refusals_park = cosmix_foreman::ledger::infra_refusal_park_threshold()?;
            let mut dispatched: std::collections::HashSet<i64> = Default::default();
            // Sweep outcome counters (for the summary line)
            let mut ran: u32 = 0;
            let mut bounced: u32 = 0;
            let mut parked: u32 = 0;
            let mut rung_refusals: u32 = 0;
            // Harness fault flag: a genuine infrastructure error, not a task outcome
            let mut harness_fault = false;
            // One recorded wall time owns admission for the whole sweep. A
            // replay supplies this same input instead of re-reading its host.
            let sweep_now = chrono::Utc::now();
            // A dead dispatch supervisor leaves its claim `running` forever
            // with no agent behind it (task 94) — reap those before planning
            // so a freed task is visible to THIS sweep's `plan` calls below.
            // Read-only for dry runs, same rule as parking below.
            if !dry_run {
                // Liveness is the sweep's other host observation, handed in
                // beside `sweep_now` rather than read from inside the
                // ledger: both are inputs, and a replay supplies the
                // recorded answer for each instead of re-reading its own
                // host. Every reap writes the observation it acted on into
                // the finding it files.
                //
                // NOT wrapped in `ledger_write_with_busy_retry`: the sweep
                // retries internally, per candidate. Retrying it from out
                // here would re-run it from scratch, and claims reaped in
                // the abandoned pass are no longer candidates — the report
                // below would silently omit them.
                let sweep = ledger.reap_dead_claims_with(
                    &sweep_now.to_rfc3339(),
                    cosmix_foreman::procutil::owner_alive,
                )?;
                for reaped in &sweep.reaped {
                    let age = match reaped.claim_age_secs {
                        Some(age) => format!("held {age}s"),
                        None => "age unknown".to_string(),
                    };
                    println!(
                        "dispatch: task {} reaped a dead claim held by `{}` (pid {} gone, {}, \
                         {}s past its lease) — requeued, ladder position unchanged",
                        reaped.task_id, reaped.claimant, reaped.claim_pid, age, reaped.overdue_secs
                    );
                }
                // A claim proven dead that the sweep could not release is a
                // ledger write failure — the harness's fault, same rule as
                // every other ledger fault in this sweep: report it, keep
                // going, exit non-zero. It is NOT a reason to abort the
                // sweep (the claim is still expired and still dead next
                // time), but a dispatch that exits 0 here would be the
                // silently stranded `running` task all over again.
                for unreaped in &sweep.unreaped {
                    harness_fault = true;
                    eprintln!(
                        "dispatch: task {}'s dead claim (held by `{}`, pid {} gone) could not be \
                         reaped this sweep and stays claimed — harness fault: {:#}",
                        unreaped.task_id, unreaped.claimant, unreaped.claim_pid, unreaped.error
                    );
                }
            }
            while (dispatched.len() as u32) < max_tasks {
                // Dry runs are READ-ONLY: no parking, no findings.
                let outcome = cosmix_foreman::ladder::plan(
                    &ledger,
                    &ladder,
                    task,
                    kind.as_deref(),
                    !dry_run,
                    &dispatched,
                    sweep_now,
                )?;
                for parked_task in &outcome.parked {
                    println!(
                        "dispatch: task {} PARKED {} — see findings",
                        parked_task.task_id,
                        dispatch_park_cause(parked_task.cause, parked_task.failures)
                    );
                    parked += 1;
                }
                match outcome.decision {
                    cosmix_foreman::ladder::Dispatch::Run { task: t, rung } => {
                        let routed_rung = rung.to_string();
                        let branch = branch_template
                            .as_ref()
                            .map(|tpl| tpl.replace("{id}", &t.id.to_string()));
                        println!(
                            "dispatch: task {} (risk {}, failures {}, profile: {}) -> {rung}{}",
                            t.id,
                            t.risk,
                            t.ladder_failures,
                            resolve_profile(manifest.as_ref(), &t.verifier_profile)?.name,
                            branch
                                .as_deref()
                                .map(|b| format!(" on {b}"))
                                .unwrap_or_default(),
                        );
                        if dry_run {
                            break;
                        }
                        // One rung per task per invocation: a re-bounced task
                        // waits for the next dispatch rather than hogging
                        // --max-tasks and starving the queue.
                        dispatched.insert(t.id);
                        let spec = LaunchSpec {
                            task: t.id,
                            agent: rung.agent,
                            model: rung.model,
                            workdir: workdir.clone(),
                            budget: Budget {
                                max_wall_secs,
                                ..Default::default()
                            },
                            stall_secs,
                            permission_mode: None,
                            extra_args: Vec::new(),
                            branch,
                            integration: integration.clone(),
                            verify_subdir: subdir.clone(),
                            no_governor: false,
                            no_verify,
                            policy,
                            allow_operator_driven: false,
                            worktree_template: worktree_template.clone(),
                            profiles: profiles.clone(),
                            project_pack: project_pack.clone(),
                        };
                        match launch(
                            &db,
                            db_create,
                            &ledger,
                            spec,
                            &fleet_policy,
                            manifest.as_ref(),
                        ) {
                            Ok("parked") => {
                                // No agent ran and no rung was consumed. Let
                                // this sweep use its task slot on another
                                // candidate; the parked row is no longer
                                // returned by the planner.
                                dispatched.remove(&t.id);
                                parked += 1;
                            }
                            Ok(status) => {
                                ran += 1;
                                if status != "done" {
                                    bounced += 1;
                                }
                            }
                            // A rung that cannot start (missing vendor key or
                            // refused budget) must not abort the whole sweep.
                            // It is an infrastructure refusal with backoff,
                            // never a quality charge.
                            Err(err) => {
                                eprintln!("dispatch: task {} refused: {err:#}", t.id);
                                if err
                                    .downcast_ref::<cosmix_foreman::ledger::OperatorDrivenTask>()
                                    .is_some()
                                {
                                    // The operator set the flag after this
                                    // sweep planned. The atomic claim guard
                                    // won: no attempt was consumed and this
                                    // is ordinary readiness movement, not an
                                    // infrastructure failure.
                                } else if err.downcast_ref::<ProjectPolicyDenied>().is_some() {
                                    // `launch` already filed the typed blocker.
                                    // No attempt ran and neither quality nor
                                    // infrastructure counters move.
                                    ran += 1;
                                } else if err.downcast_ref::<driver::RungRefusal>().is_some() {
                                    ran += 1;
                                    rung_refusals += 1;
                                    if tolerate_rung_refusal_write(
                                        t.id,
                                        ledger_write_with_busy_retry(
                                            "recording routed rung refusal",
                                            || {
                                                ledger.file_rung_refusal(
                                                    t.id,
                                                    &routed_rung,
                                                    &format!("{err:#}"),
                                                )
                                            },
                                        ),
                                    ) {
                                        // The persisted refusal lets the next
                                        // planning pass advance this task to a
                                        // meter-capable rung immediately.
                                        dispatched.remove(&t.id);
                                    } else {
                                        harness_fault = true;
                                    }
                                } else {
                                    // Not the task's fault, so the ladder does
                                    // not move. Count and report it separately
                                    // so repeated harness failures cannot
                                    // livelock invisibly.
                                    ran += 1;
                                    harness_fault = true;
                                    eprintln!(
                                        "dispatch: infrastructure error — not counted against \
                                         task {}'s ladder position",
                                        t.id
                                    );
                                    match ledger_write_with_busy_retry(
                                        "recording dispatch infrastructure refusal",
                                        || {
                                            ledger.note_infra_refusal(
                                                t.id,
                                                &err,
                                                infra_refusals_finding,
                                                infra_refusals_park,
                                            )
                                        },
                                    ) {
                                        Ok(None) => eprintln!(
                                            "dispatch: task {} moved on mid-launch-failure — \
                                             needs an operator look",
                                            t.id
                                        ),
                                        Ok(Some(disposition)) => {
                                            // `dispatch_after` is later than this sweep's fixed
                                            // admission time, so removing the task from the
                                            // per-sweep set cannot retry it here. It only frees
                                            // this refused slot for ready work behind it.
                                            dispatched.remove(&t.id);
                                            if disposition.parked {
                                                parked += 1;
                                                eprintln!(
                                                    "dispatch: task {} PARKED after {} consecutive \
                                                     infrastructure refusals — see blocker finding",
                                                    t.id, disposition.count
                                                );
                                            } else {
                                                eprintln!(
                                                    "dispatch: task {} has {} consecutive \
                                                     infrastructure refusals",
                                                    t.id, disposition.count
                                                );
                                            }
                                        }
                                        // Same rule as the launch failure above:
                                        // a ledger hiccup here is the harness's
                                        // fault and must not abort the sweep for
                                        // every remaining task.
                                        Err(e) => {
                                            // A ledger error here is a harness fault — the sweep
                                            // ran correctly but we couldn't record the outcome.
                                            harness_fault = true;
                                            eprintln!(
                                                "dispatch: could not record task {}'s infrastructure \
                                                 refusal: {e:#}",
                                                t.id
                                            );
                                        }
                                    }
                                }
                            }
                        }
                        if task.is_some() {
                            break;
                        }
                    }
                    cosmix_foreman::ladder::Dispatch::Parked {
                        task_id,
                        failures,
                        cause,
                    } => {
                        let cause = dispatch_park_cause(cause, failures);
                        println!(
                            "dispatch: task {task_id} {} {cause} — needs a human (see findings)",
                            if dry_run { "WOULD PARK" } else { "PARKED" }
                        );
                        parked += 1;
                        break;
                    }
                    cosmix_foreman::ladder::Dispatch::Idle => {
                        println!("dispatch: no ready tasks");
                        break;
                    }
                }
            }
            let unexplained = ledger
                .unexplained_operator_driven_task_ids()?
                .into_iter()
                .collect::<std::collections::HashSet<_>>();
            let operator_driven = ledger
                .operator_driven_tasks(kind.as_deref())?
                .into_iter()
                .map(|task| {
                    if unexplained.contains(&task.id) {
                        format!("{} [UNEXPLAINED]", task.id)
                    } else {
                        task.id.to_string()
                    }
                })
                .collect::<Vec<_>>();
            if !operator_driven.is_empty() {
                println!(
                    "dispatch: queue summary — operator-driven: {}",
                    operator_driven.join(", ")
                );
            }
            // Sweep summary: what actually happened (the information the exit code
            // used to smear before this fix).
            println!(
                "dispatch: sweep complete — ran {}, bounced {}, parked {}, rung refusals {}",
                ran, bounced, parked, rung_refusals
            );
            // Exit non-zero only for genuine harness faults — not for routine
            // task outcomes like bounces, parks, or rung refusals.
            if harness_fault {
                std::process::exit(1);
            }
        }
        _ => unreachable!("dispatch command router mismatch"),
    }
    Ok(())
}
