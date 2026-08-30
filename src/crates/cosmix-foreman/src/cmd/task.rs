use super::*;

pub(super) fn run(context: Context, command: TaskCmd) -> Result<()> {
    let Context {
        manifest,
        resolved_db,
        db: _,
        db_create: _,
        fleet_policy,
    } = context;
    let ledger = open_ledger(&resolved_db, manifest.as_ref())?;
    task_cmd(&ledger, command, manifest.as_ref(), &fleet_policy)
}

fn validate_task_budget(budget: f64, fleet_policy: &FleetPolicy) -> Result<()> {
    anyhow::ensure!(
        budget.is_finite() && budget > 0.0,
        "task --budget must be a finite positive value, got {budget}"
    );
    anyhow::ensure!(
        fleet_policy.daily_budget_usd.value == 0.0 || budget <= fleet_policy.daily_budget_usd.value,
        "task --budget ${budget:.4} exceeds the resolved daily_budget_usd \
         ceiling ${:.4} (source: {})",
        fleet_policy.daily_budget_usd.value,
        fleet_policy.daily_budget_usd.source
    );
    // Only rungs at or above `start_rung` can ever run this task, so a metering
    // rung below the entry is not admission — it is a task that dispatch will
    // refuse rung by rung and park without a single run.
    let start = fleet_policy.start_rung.value;
    anyhow::ensure!(
        fleet_policy
            .ladder
            .value
            .iter()
            .skip(start)
            .any(|rung| rung.agent.meters_dollars()),
        "task --budget requires a dollar-metering ladder rung reachable from start_rung {start}; \
         reachable rungs [{}] contain only lanes that cannot meter dollars",
        fleet_policy
            .ladder
            .value
            .iter()
            .skip(start)
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    );
    Ok(())
}

fn task_cmd(
    ledger: &Ledger,
    cmd: TaskCmd,
    manifest: Option<&ProjectManifest>,
    fleet_policy: &FleetPolicy,
) -> Result<()> {
    match cmd {
        TaskCmd::Add {
            title,
            spec,
            spec_file,
            kind,
            risk,
            bump,
            deps,
            crates,
            verifier,
            operator_driven,
            reason,
            budget,
        } => {
            if let Some(budget) = budget {
                validate_task_budget(budget, fleet_policy)?;
            }
            let spec = match (spec, spec_file) {
                (Some(s), _) => s,
                (None, Some(path)) => std::fs::read_to_string(&path)
                    .with_context(|| format!("reading {}", path.display()))?,
                (None, None) => anyhow::bail!("provide --spec or --spec-file"),
            };
            let verifier = verifier
                .or_else(|| manifest.map(|m| m.verifier.clone()))
                .unwrap_or_else(|| "rust".to_string());
            resolve_profile(manifest, &verifier).context("bad --verifier profile")?;
            let operator_driven_reason = match (operator_driven, reason.as_deref()) {
                (true, Some(reason)) if !reason.trim().is_empty() => Some(reason),
                (true, _) => anyhow::bail!(
                    "--operator-driven requires a non-blank --reason explaining the reservation"
                ),
                (false, _) => None,
            };
            let id = ledger.add_task_scoped_with_budget_and_bump(
                &title,
                &spec,
                &kind,
                &risk,
                &deps,
                TaskControls {
                    verifier_profile: &verifier,
                    crates: &crates,
                    operator_driven_reason,
                },
                budget,
                bump.as_deref(),
            )?;
            println!("task {id} queued");
            // Best-effort ABP wake — see wake.rs. Never fails the add: a
            // missed wake only costs latency until the backstop timer runs.
            wake::fire(wake::WAKE_VERB);
        }
        TaskCmd::List { status, all, json } => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&ledger.tasks(status.as_deref(), all)?)?
                );
                return Ok(());
            }
            for t in ledger.tasks(status.as_deref(), all)? {
                println!(
                    "{:>4} {:<8} {:<6} attempt {} {}{}{}",
                    t.id,
                    t.status,
                    t.risk,
                    t.attempt,
                    t.title,
                    t.claimed_by.map(|c| format!(" [{c}]")).unwrap_or_default(),
                    if t.operator_driven {
                        " [operator-driven]"
                    } else {
                        ""
                    },
                );
            }
        }
        TaskCmd::Show { id } => {
            let t = ledger.task(id)?.with_context(|| format!("no task {id}"))?;
            let effective_bump = t.effective_version_bump()?;
            let bump_source = t.version_bump_source();
            let mut shown = serde_json::to_value(&t)?;
            shown["effective_bump"] = serde_json::json!(effective_bump.as_str());
            shown["bump_source"] = serde_json::json!(bump_source);
            shown["attempt_charges"] = serde_json::to_value(ledger.task_attempt_charges(id)?)?;
            if let Some(budget) = ledger.task_budget_remainder(id)? {
                shown["budget_charged_usd"] = serde_json::json!(budget.charged_usd);
                shown["budget_remaining_usd"] = serde_json::json!(budget.remaining_usd);
            } else {
                shown["budget_charged_usd"] = serde_json::Value::Null;
                shown["budget_remaining_usd"] = serde_json::Value::Null;
            }
            println!("{}", serde_json::to_string_pretty(&shown)?);
        }
        TaskCmd::Set {
            id,
            operator_driven,
            reason,
            verifier,
            budget,
            bump,
        } => {
            if operator_driven.is_none()
                && reason.is_none()
                && verifier.is_none()
                && budget.is_none()
                && bump.is_none()
            {
                anyhow::bail!(
                    "provide --operator-driven[=true|false], --verifier=<profile>, or \
                     --budget=<USD|clear>, or --bump=<patch|minor>"
                );
            }

            anyhow::ensure!(
                reason.is_none() || operator_driven.is_some(),
                "--reason is only valid with --operator-driven[=true|false]"
            );

            if let Some(operator_driven) = operator_driven {
                let reason = reason
                    .as_deref()
                    .filter(|reason| !reason.trim().is_empty())
                    .context(
                        "--operator-driven requires a non-blank --reason explaining the decision",
                    )?;
                let changed =
                    ledger_write_with_busy_retry("setting task operator-driven flag", || {
                        ledger.set_operator_driven(id, operator_driven, reason, "operator")
                    })?;
                if changed {
                    println!(
                        "task {id} operator-driven {}",
                        if operator_driven {
                            "enabled"
                        } else {
                            "disabled"
                        }
                    );
                } else {
                    println!(
                        "task {id} operator-driven unchanged ({})",
                        if operator_driven {
                            "enabled"
                        } else {
                            "disabled"
                        }
                    );
                }
                if changed && !operator_driven {
                    wake::fire(wake::WAKE_VERB);
                }
            }

            if let Some(profile) = verifier {
                let previous = ledger
                    .task(id)?
                    .with_context(|| format!("no task {id}"))?
                    .verifier_profile;
                let previous = resolve_profile(manifest, &previous)?.name;
                let canonical = resolve_profile(manifest, &profile)?.name;
                let canonical =
                    ledger_write_with_busy_retry("setting task verifier profile", || {
                        ledger.set_verifier_profile_resolved(id, &previous, &canonical)
                    })?;
                println!("task {id} verifier profile set to '{canonical}'");
            }

            if let Some(budget) = budget {
                let budget = match budget {
                    TaskBudgetUpdate::Set(usd) => {
                        validate_task_budget(usd, fleet_policy)?;
                        Some(usd)
                    }
                    TaskBudgetUpdate::Clear => None,
                };
                ledger_write_with_busy_retry("setting task budget", || {
                    ledger.set_task_budget(id, budget)
                })?;
                match budget {
                    Some(usd) => println!("task {id} budget set to ${usd:.4}"),
                    None => println!("task {id} budget cleared"),
                }
            }

            if let Some(bump) = bump {
                ledger_write_with_busy_retry("setting task version bump", || {
                    ledger.set_task_bump(id, &bump)
                })?;
                println!("task {id} version bump set to '{bump}'");
            }
        }
        TaskCmd::Requeue { id, force } => {
            ledger.requeue_task(id, force)?;
            println!("task {id} requeued");
            wake::fire(wake::WAKE_VERB);
        }
        TaskCmd::Land { id, repo } => {
            let repo = resolve_project_repo_arg(repo, manifest, "--repo")?
                .unwrap_or_else(|| PathBuf::from("."));
            let repo = repo
                .canonicalize()
                .with_context(|| format!("repo {}", repo.display()))?;
            ledger.land_task(id, &repo)?;
            println!("task {id} marked for landing");
            wake::fire(wake::WAKE_VERB);
        }
        TaskCmd::Retire { id, reason } => {
            ledger.retire_task(id, &reason)?;
            println!("task {id} retired: {reason}");
        }
    }
    Ok(())
}
