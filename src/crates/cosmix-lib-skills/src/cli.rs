use anyhow::Result;
use clap::{Parser, Subcommand};
use cosmix_skills::{
    evaluate_task, extract_skill, format_skills_for_prompt, refine_skill,
    retrieve_skills_domain, detect_domain_cwd,
    EvalScore, IndexdClient, LlmClient, TaskOutcome, TaskTranscript,
};

#[derive(Parser)]
#[command(name = "cosmix-skills-cli", about = "Test the Hermes skill learning loop")]
struct Cli {
    /// LLM backend name from settings.toml [llm.backends] (default: [llm].default)
    #[arg(long)]
    backend: Option<String>,

    /// Project domain filter (default: auto-detect from $PWD via CLAUDE.md)
    #[arg(long)]
    domain: Option<String>,

    /// Search all domains (ignore domain filter)
    #[arg(long)]
    all_domains: bool,

    /// indexd Unix socket path (default from settings.toml [embed])
    #[arg(long)]
    socket: Option<String>,

    #[command(subcommand)]
    command: Command,
}

struct ResolvedConfig {
    backend: Option<String>,
    domain: Option<String>,
    socket: String,
}

#[derive(Subcommand)]
enum Command {
    /// Evaluate a task transcript from a JSON file
    Evaluate {
        /// Path to transcript JSON file
        path: String,
    },

    /// Extract a skill from a task transcript
    Extract {
        /// Path to transcript JSON file
        path: String,
    },

    /// Full loop: evaluate + extract + store
    Learn {
        /// Path to transcript JSON file
        path: String,
    },

    /// Search for skills relevant to a task description
    Search {
        /// Natural language task description
        query: String,

        /// Max results
        #[arg(short = 'n', default_value = "5")]
        limit: usize,
    },

    /// List all stored skills
    List {
        /// Max results
        #[arg(short = 'n', default_value = "20")]
        limit: usize,

        /// Offset for pagination
        #[arg(long, default_value = "0")]
        offset: usize,
    },

    /// Report an outcome and refine a skill
    Refine {
        /// Skill ID in indexd
        id: i64,

        /// Was the skill application successful?
        #[arg(long)]
        success: bool,

        /// Notes about the outcome
        #[arg(long, default_value = "")]
        notes: String,
    },

    /// Delete a skill by ID
    Delete {
        /// Skill ID
        id: i64,
    },

    /// Format skills for a task (shows what would be injected into a prompt)
    Format {
        /// Natural language task description
        query: String,

        /// Max skills to retrieve
        #[arg(short = 'n', default_value = "3")]
        limit: usize,
    },

    /// Create a sample transcript JSON for testing
    SampleTranscript,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    let cfg = cosmix_config::store::load().unwrap_or_default();

    // Use skills-specific backend override if set, else CLI arg, else [llm].default
    let backend = cli.backend.or_else(|| {
        let b = &cfg.skills.llm_backend;
        if b.is_empty() { None } else { Some(b.clone()) }
    });
    let domain = if cli.all_domains {
        None
    } else {
        Some(cli.domain.unwrap_or_else(detect_domain_cwd))
    };
    let resolved = ResolvedConfig {
        backend,
        domain,
        socket: cli.socket.unwrap_or(cfg.embed.socket_path),
    };

    if let Some(d) = &resolved.domain {
        eprintln!("Domain: {d}");
    } else {
        eprintln!("Domain: (all)");
    }

    match &cli.command {
        Command::Evaluate { path } => cmd_evaluate(&resolved, path).await,
        Command::Extract { path } => cmd_extract(&resolved, path).await,
        Command::Learn { path } => cmd_learn(&resolved, path).await,
        Command::Search { query, limit } => cmd_search(&resolved, query, *limit).await,
        Command::List { limit, offset } => cmd_list(&resolved, *limit, *offset).await,
        Command::Refine { id, success, notes } => cmd_refine(&resolved, *id, *success, notes).await,
        Command::Delete { id } => cmd_delete(&resolved, *id).await,
        Command::Format { query, limit } => cmd_format(&resolved, query, *limit).await,
        Command::SampleTranscript => cmd_sample_transcript(),
    }
}

fn load_transcript(path: &str) -> Result<TaskTranscript> {
    let content = std::fs::read_to_string(path)?;
    let transcript: TaskTranscript = serde_json::from_str(&content)?;
    Ok(transcript)
}

async fn cmd_evaluate(cli: &ResolvedConfig, path: &str) -> Result<()> {
    let transcript = load_transcript(path)?;
    let llm = LlmClient::from_config(cli.backend.as_deref())?;

    println!("Evaluating task: {}", transcript.task_description);
    match evaluate_task(&llm, &transcript).await? {
        Some(score) => {
            println!("Worth extracting!");
            println!(
                "  Success: {}/5  Novelty: {}/5",
                score.success, score.novelty
            );
            println!("  Reasoning: {}", score.reasoning);
        }
        None => {
            println!("Task is too routine to extract a skill from.");
        }
    }
    Ok(())
}

async fn cmd_extract(cli: &ResolvedConfig, path: &str) -> Result<()> {
    let transcript = load_transcript(path)?;
    let llm = LlmClient::from_config(cli.backend.as_deref())?;

    println!("Extracting skill from: {}", transcript.task_description);
    let eval = EvalScore {
        success: 5,
        novelty: 5,
        reasoning: "manual extraction".into(),
    };
    let skill = extract_skill(&llm, &transcript, &eval).await?;
    println!("{}", serde_json::to_string_pretty(&skill)?);
    Ok(())
}

async fn cmd_learn(cli: &ResolvedConfig, path: &str) -> Result<()> {
    let transcript = load_transcript(path)?;
    let llm = LlmClient::from_config(cli.backend.as_deref())?;

    println!("=== Evaluate ===");
    let eval = match evaluate_task(&llm, &transcript).await? {
        Some(score) => {
            println!(
                "Worth learning! Success: {}/5  Novelty: {}/5",
                score.success, score.novelty
            );
            println!("Reasoning: {}", score.reasoning);
            score
        }
        None => {
            println!("Task too routine — nothing to learn.");
            return Ok(());
        }
    };

    println!("\n=== Extract ===");
    let skill = extract_skill(&llm, &transcript, &eval).await?;
    println!("Skill: {} (trigger: {})", skill.name, skill.trigger);

    println!("\n=== Store ===");
    let mut indexd = IndexdClient::connect(Some(&cli.socket)).await?;
    let id = indexd.store_skill(&skill).await?;
    println!("Stored as id={id}");

    println!("\n=== Verify ===");
    let results = indexd
        .search_skills(&transcript.task_description, 3)
        .await?;
    for (rid, doc, distance) in &results {
        println!("  [{rid}] {} (distance: {distance:.4})", doc.name);
    }

    Ok(())
}

async fn cmd_search(cli: &ResolvedConfig, query: &str, limit: usize) -> Result<()> {
    let mut indexd = IndexdClient::connect(Some(&cli.socket)).await?;
    let results = indexd
        .search_skills_domain(query, limit, cli.domain.as_deref())
        .await?;

    if results.is_empty() {
        println!("No skills found.");
        return Ok(());
    }

    for (id, doc, distance) in &results {
        println!(
            "[{id}] {} [{}] — confidence: {:.0}%, used: {} times, distance: {distance:.4}",
            doc.name,
            if doc.domain.is_empty() { "general" } else { &doc.domain },
            doc.confidence * 100.0,
            doc.use_count,
        );
        println!("     trigger: {}", doc.trigger);
    }
    Ok(())
}

async fn cmd_list(cli: &ResolvedConfig, limit: usize, offset: usize) -> Result<()> {
    let mut indexd = IndexdClient::connect(Some(&cli.socket)).await?;
    let (skills, total) = indexd.list_skills(limit, offset).await?;

    println!("Skills ({total} total, showing {}-{}):", offset + 1, offset + skills.len());
    for (id, doc) in &skills {
        println!(
            "  [{id}] {} v{} [{}] — confidence: {:.0}%, used: {}, success: {}",
            doc.name,
            doc.version,
            if doc.domain.is_empty() { "general" } else { &doc.domain },
            doc.confidence * 100.0,
            doc.use_count,
            doc.success_count,
        );
    }
    Ok(())
}

async fn cmd_refine(cli: &ResolvedConfig, id: i64, success: bool, notes: &str) -> Result<()> {
    let mut indexd = IndexdClient::connect(Some(&cli.socket)).await?;
    let llm = LlmClient::from_config(cli.backend.as_deref())?;

    // Fetch the existing skill
    let results = indexd.list_skills(100, 0).await?;
    let (_, existing) = results
        .0
        .into_iter()
        .find(|(sid, _)| *sid == id)
        .ok_or_else(|| anyhow::anyhow!("skill {id} not found"))?;

    let outcome = TaskOutcome {
        skill_id: id,
        success,
        notes: notes.to_string(),
        duration_ms: 0,
    };

    let updated = refine_skill(&llm, &mut indexd, id, &existing, &outcome).await?;
    println!("Refined: {} v{}", updated.name, updated.version);
    println!(
        "  confidence: {:.0}% → {:.0}%",
        existing.confidence * 100.0,
        updated.confidence * 100.0
    );
    Ok(())
}

async fn cmd_delete(cli: &ResolvedConfig, id: i64) -> Result<()> {
    let mut indexd = IndexdClient::connect(Some(&cli.socket)).await?;
    indexd.delete_skill(id).await?;
    println!("Deleted skill {id}");
    Ok(())
}

async fn cmd_format(cli: &ResolvedConfig, query: &str, limit: usize) -> Result<()> {
    let mut indexd = IndexdClient::connect(Some(&cli.socket)).await?;
    let skills = retrieve_skills_domain(&mut indexd, query, limit, cli.domain.as_deref()).await?;

    if skills.is_empty() {
        println!("No relevant skills found.");
        return Ok(());
    }

    let formatted = format_skills_for_prompt(&skills);
    println!("{formatted}");
    Ok(())
}

fn cmd_sample_transcript() -> Result<()> {
    use cosmix_skills::{Message, ToolCall};

    let sample = TaskTranscript {
        task_description: "Add pagination to the /api/users endpoint".into(),
        system_prompt: "You are a helpful coding assistant.".into(),
        messages: vec![
            Message {
                role: "user".into(),
                content: "Add pagination to the /api/users endpoint with limit/offset query params"
                    .into(),
            },
            Message {
                role: "assistant".into(),
                content: "I'll add pagination support with limit and offset parameters.".into(),
            },
        ],
        tool_calls: vec![
            ToolCall {
                name: "read_file".into(),
                input: "src/api/users.rs".into(),
                output: "fn list_users() -> Vec<User> { ... }".into(),
            },
            ToolCall {
                name: "edit_file".into(),
                input: "src/api/users.rs: add Query<Pagination> parameter".into(),
                output: "File updated successfully".into(),
            },
            ToolCall {
                name: "run_tests".into(),
                input: "cargo test api::users".into(),
                output: "3 tests passed".into(),
            },
        ],
        final_output: "Added pagination to /api/users with limit (default 20, max 100) and offset query parameters. Updated the SQL query to use LIMIT/OFFSET and added a total count header.".into(),
        duration_ms: 45000,
        token_count: 3200,
        success: true,
    };

    println!("{}", serde_json::to_string_pretty(&sample)?);
    Ok(())
}
