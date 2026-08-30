use super::launch_support::{sweep_stale_configs, write_new};
use super::*;

pub(super) fn run(context: Context, command: Cmd) -> Result<()> {
    let Context {
        manifest,
        resolved_db: _,
        db,
        db_create,
        fleet_policy,
    } = context;
    match command {
        Cmd::Mayor {
            question,
            model,
            full_tools,
        } => {
            mayor(
                &db,
                db_create,
                question,
                model,
                full_tools,
                &fleet_policy,
                manifest.as_ref(),
            )?;
        }
        _ => unreachable!("mayor command router mismatch"),
    }
    Ok(())
}

/// The mayor: one conversational front-end for the fleet — a claude
/// session with this foreman's MCP server attached, so "what's blocked?"
/// and "why did task 41 bounce?" are questions, not queries. Interactive
/// by default; a question makes it a one-shot. The session inherits the
/// terminal (this is the one foreman surface built for a human).
fn mayor(
    db: &std::path::Path,
    db_create: cosmix_foreman::state::DbCreateMode,
    question: Vec<String>,
    model: Option<String>,
    full_tools: bool,
    fleet_policy: &FleetPolicy,
    project: Option<&ProjectManifest>,
) -> Result<()> {
    let db_abs = db.canonicalize().unwrap_or_else(|_| db.to_path_buf());
    let foreman = std::env::current_exe().context("resolving the foreman binary path")?;
    let mut mcp_args = vec![
        "--db".to_string(),
        db_abs.to_string_lossy().into_owned(),
        "--db-create".to_string(),
        db_create.as_cli_value().to_string(),
    ];
    if let Some(project) = project {
        mcp_args.push("--project".to_string());
        mcp_args.push(project.path.to_string_lossy().into_owned());
    }
    mcp_args.push("mcp".to_string());
    let mcp_config = serde_json::json!({
        "mcpServers": {
            "foreman": {
                "command": foreman.to_string_lossy(),
                "args": mcp_args,
            }
        }
    });
    // Unique per invocation (concurrent mayors must not swap configs) and
    // named under the policy gate's protected prefix — an agent must not be
    // able to redirect the operator's next mayor session to a malicious
    // MCP binary. Scope-guarded removal covers every exit path.
    let ledger_dir = db_abs.parent().context("ledger path has no parent")?;
    sweep_stale_configs(ledger_dir);
    let config_path = ledger_dir.join(format!("policy-settings-mayor-{}.json", std::process::id()));
    write_new(&config_path, &serde_json::to_string_pretty(&mcp_config)?)?;
    struct RemoveOnDrop(PathBuf);
    impl Drop for RemoveOnDrop {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let config_guard = RemoveOnDrop(config_path.clone());
    let mut system_prompt = "You are the foreman mayor: the conversational front-end for a \
        build-orchestration fleet. Use the foreman MCP tools to answer questions about \
        tasks, runs, spend, findings, and the governor, and to file findings the \
        operator dictates. Report ledger facts as they are; do not claim or complete \
        tasks unless explicitly asked."
        .to_string();
    if let Some(project) = project
        && !project.instruction_pack.is_empty()
    {
        system_prompt.push_str("\n\nProject context:\n");
        system_prompt.push_str(&project.instruction_pack);
    }
    let mut cmd = std::process::Command::new(&fleet_policy.claude_bin.value);
    // The mayor's claude spawns this foreman's MCP server; neither must
    // inherit verify-lane delegation or recursion depth from a
    // verifier-step ancestor.
    cmd.env_remove(cosmix_foreman::verify::LANE_HELD_ENV);
    cmd.env_remove(cosmix_foreman::verify::DEPTH_ENV);
    // In -p mode nobody can approve a prompt, so the mayor's tools are
    // pre-allowed — but only the READ/REPORT set by default; the mutating
    // fleet tools (claim/complete/bounce) are an explicit --full-tools
    // opt-in, because "report-only" in a prompt is advice, not enforcement.
    // allowedTools is ADDITIVE over inherited settings, so the mutators are
    // also explicitly DISALLOWED (an explicit deny outranks any inherited
    // allow).
    cmd.arg("--mcp-config")
        .arg(&config_path)
        .arg("--append-system-prompt")
        .arg(&system_prompt);
    if full_tools {
        cmd.arg("--allowedTools").arg("mcp__foreman");
    } else {
        cmd.arg("--allowedTools")
            .arg("mcp__foreman__build_status,mcp__foreman__task_show,mcp__foreman__finding_file")
            .arg("--disallowedTools")
            .arg(
                "mcp__foreman__task_claim,mcp__foreman__task_complete,\
                 mcp__foreman__task_heartbeat,mcp__foreman__task_bounce,\
                 mcp__foreman__task_next",
            );
    }
    if let Some(model) = model {
        cmd.arg("--model").arg(model);
    }
    if !question.is_empty() {
        cmd.arg("-p").arg(question.join(" "));
    }
    let status = cmd
        .status()
        .context("launching the mayor's claude session")?;
    // process::exit skips Drop — clean up explicitly first.
    drop(config_guard);
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}
