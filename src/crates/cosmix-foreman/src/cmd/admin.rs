use super::*;

pub(super) fn run(context: Context, command: Cmd) -> Result<()> {
    let Context {
        manifest,
        resolved_db,
        db,
        db_create: _,
        fleet_policy,
    } = context;
    match command {
        Cmd::Config(ConfigCmd::Show { json }) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&fleet_policy.json())?);
            } else {
                for line in fleet_policy.render_text() {
                    println!("{line}");
                }
            }
        }
        Cmd::Init => {
            open_ledger(&resolved_db, manifest.as_ref())?;
            println!("ledger ready at {}", db.display());
        }
        Cmd::Finding {
            task,
            severity,
            title,
            body,
        } => {
            let ledger = open_ledger(&resolved_db, manifest.as_ref())?;
            let id = ledger.file_finding_reasoned(
                task,
                &severity,
                &title,
                &body,
                "cli",
                FindingReason::Operator,
            )?;
            println!("finding {id} filed");
        }
        Cmd::Mcp => {
            let ledger = open_ledger(&resolved_db, manifest.as_ref())?;
            cosmix_foreman::mcp::serve_with_ledger(
                db,
                ledger,
                manifest
                    .as_ref()
                    .map(|project| project.profiles.clone())
                    .unwrap_or_default(),
                manifest.as_ref().and_then(|project| project.subdir.clone()),
                manifest.as_ref().and_then(|project| project.lane_policy()),
                manifest
                    .as_ref()
                    .map(cosmix_foreman::mcp::McpProjectWorkspace::from),
            )?;
        }
        Cmd::Refine {
            repo,
            integration,
            subdir,
            tier,
            review,
        } => {
            let ledger = open_ledger(&resolved_db, manifest.as_ref())?;
            let repo = resolve_project_repo_arg(repo, manifest.as_ref(), "--repo")?
                .context("--repo (or an active --project manifest's `repo`) is required")?;
            let repo = repo
                .canonicalize()
                .with_context(|| format!("repo {}", repo.display()))?;
            let integration = resolve_project_integration_arg(integration, manifest.as_ref())?;
            let subdir = subdir
                .or_else(|| manifest.as_ref().and_then(|m| m.subdir.clone()))
                .unwrap_or_else(|| ".".to_string());
            let tier = tier
                .or_else(|| manifest.as_ref().map(|m| m.landing_tier))
                .unwrap_or(1);
            let review = review
                .or_else(|| manifest.as_ref().map(|m| m.landing_review))
                .unwrap_or(false);
            anyhow::ensure!(tier <= 2, "--tier must be 0, 1, or 2");
            let reports = refinery::refine(
                &ledger,
                &RefineOptions {
                    repo,
                    project_root: manifest.as_ref().map(|m| m.root.clone()),
                    integration,
                    subdir,
                    tier,
                    review,
                    db: db.clone(),
                    echo: true,
                    fleet_policy: Some(fleet_policy.clone()),
                    profiles: manifest
                        .as_ref()
                        .map(|m| m.profiles.clone())
                        .unwrap_or_default(),
                    project_pack: manifest
                        .as_ref()
                        .map(|m| m.instruction_pack.clone())
                        .unwrap_or_default(),
                    landing_gate: manifest.as_ref().and_then(|m| m.landing_gate.clone()),
                    lane_policy: manifest.as_ref().and_then(|m| m.lane_policy()),
                },
            )?;
            let landed = reports.iter().filter(|r| r.landed).count();
            let total = reports.len();
            println!("{landed}/{total} landed");
            // A task failing its pre-land verification (e.g., clippy failures) is
            // a task outcome, not a harness fault. The harness worked correctly
            // — it ran the verification and reported the result. Exit 0.
            //
            // Genuine harness faults (unable to open repo, verification
            // invocation failed, ledger error) already bail out earlier with
            // anyhow::Error, which exits non-zero via the ? operator.
        }
        Cmd::Governor(cmd) => {
            let governor = Governor::from_policy(&db, &fleet_policy);
            match cmd {
                GovernorCmd::Status => {
                    let ledger = open_ledger(&resolved_db, manifest.as_ref())?;
                    let s = governor.status(&ledger)?;
                    println!(
                        "kill switch: {}\ndaily budget: ${:.2} (source: {})\nspend today: ${:.4} / ${:.2} (+ ${:.2} reserved; {:.1}% delivery void)\noutput tokens today: {} / {} (+ {} reserved; {:.1}% delivery void)",
                        if s.stopped { "THROWN" } else { "clear" },
                        fleet_policy.daily_budget_usd.value,
                        fleet_policy.daily_budget_usd.source,
                        s.spend_today_usd,
                        s.daily_budget_usd,
                        s.reserved_usd,
                        s.delivery_void_fraction.fraction * 100.0,
                        s.output_tokens_today,
                        s.daily_output_tokens,
                        s.reserved_tokens,
                        s.delivery_void_fraction.fraction * 100.0,
                    );
                }
                GovernorCmd::Stop { reason } => {
                    governor.stop(&reason)?;
                    println!("kill switch thrown; claims will refuse");
                }
                GovernorCmd::Resume => {
                    governor.resume()?;
                    println!("kill switch cleared");
                }
            }
        }
        _ => unreachable!("admin command router mismatch"),
    }
    Ok(())
}
