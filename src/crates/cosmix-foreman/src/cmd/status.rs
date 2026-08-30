use super::*;

#[derive(serde::Serialize)]
struct TaskBudgetStatus {
    task_id: i64,
    budget_usd: f64,
    budget_charged_usd: f64,
    budget_remaining_usd: f64,
}

fn task_budget_statuses(ledger: &Ledger) -> Result<Vec<TaskBudgetStatus>> {
    let mut statuses = Vec::new();
    for task in ledger.tasks(None, true)? {
        if let Some(budget) = ledger.task_budget_remainder(task.id)? {
            statuses.push(TaskBudgetStatus {
                task_id: task.id,
                budget_usd: budget.limit_usd,
                budget_charged_usd: budget.charged_usd,
                budget_remaining_usd: budget.remaining_usd,
            });
        }
    }
    Ok(statuses)
}

pub(super) fn run(context: Context, command: Cmd) -> Result<()> {
    let Context {
        manifest,
        resolved_db,
        db,
        db_create: _,
        fleet_policy,
    } = context;
    match command {
        Cmd::Status { json } => {
            let ledger = open_ledger(&resolved_db, manifest.as_ref())?;
            if json {
                // The Mix/script surface: one JSON object, same content the
                // MCP build_status serves (criterion 1: the scheduler's
                // state is structured data).
                let delivery_void = ledger.delivery_void_fraction()?;
                let quality_void = ledger.quality_void_fraction()?;
                let mut out = serde_json::json!({
                    "tasks": ledger
                        .status_counts()?
                        .into_iter()
                        .collect::<std::collections::BTreeMap<_, _>>(),
                    "operator_driven": ledger.operator_driven_statuses()?,
                    "task_budgets": task_budget_statuses(&ledger)?,
                    "recent_runs": ledger.recent_runs(10)?,
                    "total_spend_usd": ledger.total_spend_usd()?,
                    "total_spend_usd_delivery_void": delivery_void,
                    "runs": {
                        "total": delivery_void.contributing_runs,
                        "unknown_delivery": delivery_void.unknown_runs,
                        "delivery_void_fraction": delivery_void.fraction,
                        "unknown_quality": quality_void.unknown_runs,
                        "quality_void_fraction": quality_void.fraction,
                    },
                });
                match Governor::from_policy(&db, &fleet_policy).status(&ledger) {
                    Ok(g) => {
                        out["governor"] = serde_json::json!({
                            "stopped": g.stopped,
                            "spend_today_usd": g.spend_today_usd,
                            "output_tokens_today": g.output_tokens_today,
                            "delivery_void": g.delivery_void_fraction,
                            "reserved_usd": g.reserved_usd,
                            "reserved_tokens": g.reserved_tokens,
                            "daily_budget_usd": g.daily_budget_usd,
                            "daily_output_tokens": g.daily_output_tokens,
                        });
                    }
                    // Same contract as MCP build_status: a broken governor is
                    // reported, never silently omitted — consumers must not
                    // read "no ceiling data" as "no ceiling problem".
                    Err(e) => out["governor_error"] = serde_json::json!(format!("{e:#}")),
                }

                // Unit health is advisory, same contract as the governor
                // block above: a check that can't run must not take the
                // rest of `status --json` down with it. Note the gate is
                // `has_report`, not `has_issues` — the machine surface
                // carries every ACTIVE sweep too, not only the ones that
                // predate the deploy, because a consumer that has to infer
                // activity from an omitted key is the same blind spot.
                let health = unit_health::check_fleet_health(unit_health::current_binary_mtime());
                if health.has_report() {
                    out["unit_health"] = serde_json::to_value(&health)?;
                }

                println!("{}", serde_json::to_string_pretty(&out)?);
                return Ok(());
            }
            println!("tasks:");
            for (status, n) in ledger.status_counts()? {
                println!("  {status:<10} {n}");
            }
            let operator_driven = ledger.operator_driven_statuses()?;
            if !operator_driven.is_empty() {
                println!("operator-driven:");
                for reservation in operator_driven {
                    println!(
                        "  task {}{}",
                        reservation.task_id,
                        if reservation.reservation_explained {
                            ""
                        } else {
                            " [UNEXPLAINED]"
                        }
                    );
                }
            }
            let task_budgets = task_budget_statuses(&ledger)?;
            if !task_budgets.is_empty() {
                println!("task budgets:");
                for budget in task_budgets {
                    println!(
                        "  task {} budget ${:.4}, charged ${:.4}, remainder ${:.4}",
                        budget.task_id,
                        budget.budget_usd,
                        budget.budget_charged_usd,
                        budget.budget_remaining_usd,
                    );
                }
            }
            println!("recent runs:");
            for run in ledger.recent_runs(10)? {
                // `in=` is the folded input total; the breakdown behind it is
                // what separates a cheap cache re-read from fresh ingestion,
                // and only appears for lanes that report it.
                let breakdown = match (
                    run.fresh_input_tokens,
                    run.cache_read_input_tokens,
                    run.cache_creation_input_tokens,
                ) {
                    (None, None, None) => String::new(),
                    (fresh, read, write) => {
                        let f =
                            |v: Option<i64>| v.map(|v| v.to_string()).unwrap_or_else(|| "?".into());
                        format!(" [fresh={} read={} write={}]", f(fresh), f(read), f(write))
                    }
                };
                println!(
                    "  run {} task {} {} role={} delivery={} quality={} in={}{} out={} cost={} {}ms",
                    run.id,
                    run.task_id,
                    run.agent,
                    run.role,
                    run.delivery,
                    run.quality,
                    run.tokens_in,
                    breakdown,
                    run.tokens_out,
                    run.cost_usd
                        .map(|c| format!("${c:.4}"))
                        .unwrap_or_else(|| "?".into()),
                    run.duration_ms.unwrap_or(0),
                );
                if let Some(err) = &run.error {
                    let first = err.lines().next().unwrap_or("");
                    println!("      error: {first}");
                }
            }
            // Only claude reports real dollars; codex reports none and GLM's
            // are stripped as fiction — this is a floor, not the whole bill.
            let delivery_void = ledger.delivery_void_fraction()?;
            println!(
                "total spend (cost-reporting runs): ${:.4} ({:.1}% delivery void; {}/{})",
                ledger.total_spend_usd()?,
                delivery_void.fraction * 100.0,
                delivery_void.unknown_runs,
                delivery_void.contributing_runs,
            );
            // The fleet view is a sweep surface too — a crashed hold must
            // not wait for a claim to be released.
            if let Ok(g) = Governor::from_policy(&db, &fleet_policy).status(&ledger)
                && (g.reserved_usd > 0.0 || g.reserved_tokens > 0)
            {
                println!(
                    "reserved: ${:.2} / {} tokens (live holds)",
                    g.reserved_usd, g.reserved_tokens
                );
            }
            // Advisory, same as the governor block: prints nothing for a
            // healthy fleet, one line per failed/stale-deploy unit otherwise.
            let health = unit_health::check_fleet_health(unit_health::current_binary_mtime());
            for line in unit_health::render_text(&health) {
                println!("{line}");
            }
        }
        _ => unreachable!("status command router mismatch"),
    }
    Ok(())
}
