use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use cosmix_skills::{
    EvalScore, IndexdClient, LlmClient, SkillDocument, StaleChunk, TaskOutcome, TaskTranscript,
    check_graduation, detect_domain_cwd, evaluate_task, extract_skill, format_skills_for_prompt,
    refine_skill, retrieve_skills_domain,
};

#[derive(Parser)]
#[command(
    name = "cosmix-skills-cli",
    about = "Test the Hermes skill learning loop"
)]
struct Cli {
    /// LLM backend name from llm.toml `[backends]` (default: top-level `default`)
    #[arg(long)]
    backend: Option<String>,

    /// Project domain filter (default: auto-detect from $PWD via CLAUDE.md)
    #[arg(long)]
    domain: Option<String>,

    /// Search all domains (ignore domain filter)
    #[arg(long)]
    all_domains: bool,

    /// indexd Unix socket path (default from indexd.toml `[service].socket_path`)
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

    /// Show database stats: vector counts by source, DB size, search latency
    Stats {
        /// Run N search benchmarks to measure latency (default: 5)
        #[arg(short = 'n', default_value = "5")]
        bench_runs: usize,
    },

    /// Create a sample transcript JSON for testing
    SampleTranscript,

    /// Sweep all skills checking graduation eligibility; emit markdown report to stdout.
    /// Used by CMM graduation_sweep job (30m tier).
    GraduateAll,

    /// Query indexd for stale chunks in 3 buckets; emit markdown report to stdout.
    /// Used by CMM staleness_report job (60m tier).
    StalenessReport {
        /// Only report chunks of this source (empty = all)
        #[arg(long, default_value = "")]
        source: String,
    },

    /// Delete indexd chunks for one or more absolute file paths (stale-path cleanup).
    /// Used when files have been moved or removed from disk and their stored chunks
    /// need to be purged. Operates on all source types (doc, journal, memory).
    DeletePaths {
        /// Absolute file paths to purge from indexd
        paths: Vec<String>,
    },

    /// Index a workspace's _doc/, _journal/, _memory/, and _notes.md files into indexd.
    /// Uses `git ls-files` so .gitignored content is skipped. Idempotent — deletes
    /// existing chunks for each file before re-indexing.
    BootstrapIndex {
        /// Workspace root (must be a git repo)
        #[arg(long)]
        path: String,

        /// Filter to a substring of the workspace-relative path (incremental re-index).
        /// Example: `--filter _spec/` only touches files under _spec/.
        #[arg(long)]
        filter: Option<String>,

        /// Skip delete-before-store (faster, but may create duplicates)
        #[arg(long)]
        skip_delete: bool,
    },

    /// Knowledge base health dashboard: skill confidence tiers, top skills,
    /// stale chunks, and overall index stats.
    Dashboard,

    /// Check which _doc/ files reference code that has changed since the doc was written.
    /// Reports stale documentation that may need updating.
    DocFreshness {
        /// Workspace root (must be a git repo). Default: current directory.
        #[arg(long, default_value = ".")]
        path: String,
    },

    /// Validate CLAUDE.md against the actual workspace: missing crates, unlisted crates,
    /// broken external deps, missing _doc/ refs, cosmix-lib-ui module structure.
    /// Used by CMM claude_md_audit job (1440m tier).
    ClaudeMdAudit {
        /// Workspace root (must contain CLAUDE.md). Default: current directory.
        #[arg(long, default_value = ".")]
        path: String,
    },

    /// Analyze skill usage patterns: trigger overlap, tool overlap, merge candidates.
    /// Used by CMM skill_composition job (60m tier).
    SkillComposition,

    /// Validate MEMORY.md entries: frontmatter, broken file references, stale entries.
    /// Reads from ~/.claude/projects/ memory directory. Used by CMM memory_hygiene job (1440m tier).
    MemoryHygiene {
        /// Maximum age in days before flagging a memory as stale (default: 30).
        #[arg(long, default_value = "30")]
        stale_days: u32,
    },

    /// Report crate directories with high git activity but no/few documentation references.
    /// Scans git log for recently-modified crates and cross-references with _doc/ entries.
    /// Used by CMM doc_coverage job (1440m tier).
    DocCoverage {
        /// Workspace root (must be a git repo). Default: current directory.
        #[arg(long, default_value = ".")]
        path: String,
        /// Days to look back in git history (default: 30).
        #[arg(long, default_value = "30")]
        days: u32,
    },

    /// Index all configured workspaces (from `domains.map` — defaults to
    /// ~/.cosmix, ~/.ns, ~/.mc; users override via configd) into the single
    /// indexd instance. Delegates to bootstrap-index for each workspace with a .git dir.
    BootstrapAllWorkspaces {
        /// Skip delete-before-store (faster, but may create duplicates)
        #[arg(long)]
        skip_delete: bool,
    },

    /// Detect contradictions between skills and _doc/ files within the same domain.
    /// Extracts key claims (crate names, patterns, deprecated terms) and cross-references
    /// for conflicting assertions. Used by CMM contradiction_check job (1440m tier).
    ContradictionCheck {
        /// Workspace root (must be a git repo). Default: current directory.
        #[arg(long, default_value = ".")]
        path: String,
    },

    /// Index Rust doc comments (//! and ///) from .rs files into indexd.
    /// Each contiguous doc comment block becomes one chunk with file path and item
    /// name metadata. Source type: "rust-doc". Also indexes .mix scripts if found.
    IndexSource {
        /// Workspace root (must be a git repo). Default: current directory.
        #[arg(long, default_value = ".")]
        path: String,
        /// Only index files matching this substring (e.g. "cosmix-maild" for incremental).
        #[arg(long)]
        filter: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    let skills_cfg = cosmix_config::store::load_service::<cosmix_config::SkillsSettings>("skills")
        .unwrap_or_default();
    let indexd_cfg = cosmix_config::store::load_service::<cosmix_config::IndexdSettings>("indexd")
        .unwrap_or_default();

    // Use skills-specific backend override if set, else CLI arg, else [llm].default
    let backend = cli.backend.or_else(|| {
        let b = &skills_cfg.llm_backend;
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
        socket: cli.socket.unwrap_or(indexd_cfg.service.socket_path),
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
        Command::Stats { bench_runs } => cmd_stats(&resolved, *bench_runs).await,
        Command::SampleTranscript => cmd_sample_transcript(),
        Command::GraduateAll => cmd_graduate_all(&resolved).await,
        Command::StalenessReport { source } => cmd_staleness_report(&resolved, source).await,
        Command::DeletePaths { paths } => cmd_delete_paths(&resolved, paths).await,
        Command::BootstrapIndex {
            path,
            filter,
            skip_delete,
        } => cmd_bootstrap_index(&resolved, path, filter.as_deref(), *skip_delete).await,
        Command::Dashboard => cmd_dashboard(&resolved).await,
        Command::DocFreshness { path } => cmd_doc_freshness(path).await,
        Command::ClaudeMdAudit { path } => cmd_claude_md_audit(path).await,
        Command::SkillComposition => cmd_skill_composition(&resolved).await,
        Command::MemoryHygiene { stale_days } => cmd_memory_hygiene(*stale_days).await,
        Command::DocCoverage { path, days } => cmd_doc_coverage(path, *days).await,
        Command::BootstrapAllWorkspaces { skip_delete } => {
            cmd_bootstrap_all_workspaces(&resolved, *skip_delete).await
        }
        Command::ContradictionCheck { path } => cmd_contradiction_check(&resolved, path).await,
        Command::IndexSource { path, filter } => {
            cmd_index_source(&resolved, path, filter.as_deref()).await
        }
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
            if doc.domain.is_empty() {
                "general"
            } else {
                &doc.domain
            },
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

    println!(
        "Skills ({total} total, showing {}-{}):",
        offset + 1,
        offset + skills.len()
    );
    for (id, doc) in &skills {
        println!(
            "  [{id}] {} v{} [{}] — confidence: {:.0}%, used: {}, success: {}",
            doc.name,
            doc.version,
            if doc.domain.is_empty() {
                "general"
            } else {
                &doc.domain
            },
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

    let (new_id, updated) = refine_skill(&llm, &mut indexd, id, &existing, &outcome).await?;
    println!(
        "Refined: {} v{} (old chunk {id} superseded by {new_id})",
        updated.name, updated.version
    );
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

async fn cmd_stats(cli: &ResolvedConfig, bench_runs: usize) -> Result<()> {
    use std::time::Instant;

    let mut indexd = IndexdClient::connect(Some(&cli.socket)).await?;
    let stats = indexd.stats().await?;

    println!("=== Vector Database Stats ===");
    println!();

    // DB size
    let size_kb = stats.db_size_bytes as f64 / 1024.0;
    let size_mb = size_kb / 1024.0;
    if size_mb >= 1.0 {
        println!("  DB size:        {size_mb:.1} MB");
    } else {
        println!("  DB size:        {size_kb:.0} KB");
    }

    println!("  Total vectors:  {}", stats.total_vectors);
    println!(
        "  Model loaded:   {}",
        if stats.model_loaded { "yes" } else { "no" }
    );
    println!();

    // Per-source breakdown
    if !stats.by_source.is_empty() {
        println!("  By source:");
        for sc in &stats.by_source {
            println!("    {:<12} {}", sc.source, sc.count);
        }
        println!();
    }

    // Domain breakdown for skills
    let (skills, _total) = indexd.list_skills(1000, 0).await?;
    if !skills.is_empty() {
        let mut domain_counts: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        for (_id, doc) in &skills {
            let d = if doc.domain.is_empty() {
                "general"
            } else {
                &doc.domain
            };
            *domain_counts.entry(d.to_string()).or_default() += 1;
        }
        println!("  Skills by domain:");
        for (domain, count) in &domain_counts {
            println!("    {:<20} {}", domain, count);
        }
        println!();
    }

    // Search latency benchmark
    if stats.total_vectors > 0 && bench_runs > 0 {
        println!("  Search latency ({bench_runs} runs):");

        let queries = [
            "implement a streaming protocol parser",
            "debug email delivery issues",
            "add pagination to REST API",
            "configure WireGuard mesh networking",
            "refactor database schema migration",
        ];

        let mut latencies = Vec::new();
        for i in 0..bench_runs {
            let query = queries[i % queries.len()];
            let start = Instant::now();
            let _ = indexd.search_skills(query, 5).await?;
            let elapsed = start.elapsed();
            latencies.push(elapsed.as_millis() as u64);
        }

        latencies.sort();
        let p50 = latencies[latencies.len() / 2];
        let p95 = latencies[(latencies.len() as f64 * 0.95) as usize];
        let min = latencies[0];
        let max = latencies[latencies.len() - 1];
        let avg: u64 = latencies.iter().sum::<u64>() / latencies.len() as u64;

        println!("    min: {min}ms  avg: {avg}ms  p50: {p50}ms  p95: {p95}ms  max: {max}ms");

        // Note about first-run model loading
        if !stats.model_loaded {
            println!("    (first search loads embedding model — subsequent runs faster)");
        }
    }

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

async fn cmd_dashboard(cli: &ResolvedConfig) -> Result<()> {
    let mut indexd = IndexdClient::connect(Some(&cli.socket)).await?;

    // Stats
    let stats = indexd.stats().await?;
    println!("=== Knowledge Base Dashboard ===\n");
    let size_mb = stats.db_size_bytes as f64 / (1024.0 * 1024.0);
    println!(
        "  Total indexed:  {} chunks ({:.1} MB)",
        stats.total_vectors, size_mb
    );
    if !stats.by_source.is_empty() {
        let parts: Vec<String> = stats
            .by_source
            .iter()
            .map(|s| format!("{}: {}", s.source, s.count))
            .collect();
        println!("  By source:      {}", parts.join(", "));
    }
    println!();

    // Skills by confidence tier
    let (skills, _total) = indexd.list_skills(1000, 0).await?;
    let mut new = Vec::new();
    let mut developing = Vec::new();
    let mut proven = Vec::new();
    let mut graduate_candidate = Vec::new();
    let mut graduated = Vec::new();
    let mut superseded = 0u32;

    for (id, doc) in &skills {
        if doc.superseded_by.is_some() {
            superseded += 1;
            continue;
        }
        if let Some(ref d) = cli.domain
            && !doc.domain.is_empty()
            && &doc.domain != d
        {
            continue;
        }
        let entry = (*id, doc);
        if doc.graduated {
            graduated.push(entry);
        }
        match doc.confidence {
            c if c >= 0.9 => graduate_candidate.push(entry),
            c if c >= 0.7 => proven.push(entry),
            c if c >= 0.3 => developing.push(entry),
            _ => new.push(entry),
        }
    }

    println!("  Skills by confidence tier:");
    println!("    New (0.0–0.3):          {}", new.len());
    println!("    Developing (0.3–0.7):   {}", developing.len());
    println!("    Proven (0.7–0.9):       {}", proven.len());
    println!(
        "    Graduate candidate (0.9+): {}",
        graduate_candidate.len()
    );
    println!("    Already graduated:      {}", graduated.len());
    println!("    Superseded:             {superseded}");
    println!();

    // Top 5 most-used
    let mut by_use: Vec<(i64, &SkillDocument)> = skills
        .iter()
        .filter(|(_, d)| d.superseded_by.is_none())
        .map(|(id, d)| (*id, d))
        .collect();
    by_use.sort_by_key(|e| std::cmp::Reverse(e.1.use_count));

    println!("  Top 5 most-used skills:");
    for (id, doc) in by_use.iter().take(5) {
        println!(
            "    [{id}] {} — {:.0}% conf, {} uses, {} successes",
            doc.name,
            doc.confidence * 100.0,
            doc.use_count,
            doc.success_count
        );
    }
    println!();

    // Never used (use_count=0, older than 7 days)
    let never_used: Vec<_> = skills
        .iter()
        .filter(|(_, d)| d.superseded_by.is_none() && d.use_count == 0)
        .collect();
    if !never_used.is_empty() {
        println!("  Never-used skills ({}):", never_used.len());
        for (id, doc) in never_used.iter().take(10) {
            println!(
                "    [{id}] {} [{}] — created {}",
                doc.name, doc.domain, doc.created
            );
        }
        println!();
    }

    // Stale chunks
    let stale = indexd.stale(None, 90, 3, 180, 50).await?;
    println!("  Stale chunks:");
    println!(
        "    Never retrieved (>90d):    {}",
        stale.never_retrieved_old.len()
    );
    println!(
        "    Low value (>3 retrievals, feedback<=0): {}",
        stale.low_value.len()
    );
    println!(
        "    Long dormant (>180d):      {}",
        stale.long_dormant.len()
    );

    Ok(())
}

/// Check which _doc/ files reference code that has changed since the doc was written.
async fn cmd_doc_freshness(path: &str) -> Result<()> {
    use std::process::Command as Cmd;

    let expanded = if let Some(rest) = path.strip_prefix("~/") {
        format!("{}/{}", std::env::var("HOME").unwrap_or_default(), rest)
    } else if path == "~" {
        std::env::var("HOME").unwrap_or_default()
    } else if path == "." {
        std::env::current_dir()?.to_string_lossy().to_string()
    } else {
        path.to_string()
    };
    let workspace = std::path::Path::new(&expanded);

    // Find all _doc/*.md files tracked by git (recursive for _spec/ and other subfolders)
    let output = Cmd::new("git")
        .arg("-C")
        .arg(workspace)
        .args([
            "ls-files",
            "--",
            "src/_doc/*.md",
            "src/_doc/**/*.md",
            "*/_doc/*.md",
            "*/_doc/**/*.md",
            "_doc/*.md",
            "_doc/**/*.md",
        ])
        .output()
        .context("running git ls-files")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let doc_files: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();

    if doc_files.is_empty() {
        println!("No _doc/ files found.");
        return Ok(());
    }

    println!("## Document Freshness Report\n");
    println!("Scanned {} doc files.\n", doc_files.len());

    let mut stale_docs = Vec::new();
    let mut fresh_docs = 0u32;
    let mut no_refs = 0u32;

    for doc_rel in &doc_files {
        let doc_abs = workspace.join(doc_rel);
        let content = match std::fs::read_to_string(&doc_abs) {
            Ok(c) => c,
            Err(_) => continue,
        };

        // Get the doc's last commit date
        let doc_date_output = Cmd::new("git")
            .arg("-C")
            .arg(workspace)
            .args(["log", "-1", "--format=%aI", "--", doc_rel])
            .output()?;
        let doc_date = String::from_utf8_lossy(&doc_date_output.stdout)
            .trim()
            .to_string();
        if doc_date.is_empty() {
            continue;
        }
        let doc_date_short = doc_date.get(..10).unwrap_or(&doc_date);

        // Extract referenced source paths by scanning for src/crates/ references
        let mut refs = std::collections::HashSet::new();
        for line in content.lines() {
            let mut rest = line;
            while let Some(pos) = rest.find("src/crates/") {
                let start = pos;
                // Walk forward to end of path-like chars
                let path_slice = &rest[start..];
                let end = path_slice
                    .find(|c: char| {
                        !c.is_alphanumeric() && c != '/' && c != '-' && c != '_' && c != '.'
                    })
                    .unwrap_or(path_slice.len());
                let ref_path = &path_slice[..end];
                // Only include if it looks meaningful (has at least crate name)
                if ref_path.len() > "src/crates/x".len() {
                    refs.insert(ref_path.to_string());
                }
                rest = &rest[start + end..];
            }
        }

        if refs.is_empty() {
            no_refs += 1;
            continue;
        }

        // Check if any referenced file/directory was modified after the doc
        let mut changed_refs = Vec::new();
        for ref_path in &refs {
            let check = Cmd::new("git")
                .arg("-C")
                .arg(workspace)
                .args([
                    "log",
                    "--oneline",
                    &format!("--since={doc_date}"),
                    "--",
                    ref_path,
                ])
                .output()?;
            let log_out = String::from_utf8_lossy(&check.stdout);
            let commit_count = log_out.lines().filter(|l| !l.is_empty()).count();
            if commit_count > 0 {
                changed_refs.push((ref_path.clone(), commit_count));
            }
        }

        if changed_refs.is_empty() {
            fresh_docs += 1;
        } else {
            stale_docs.push((
                doc_rel.to_string(),
                doc_date_short.to_string(),
                changed_refs,
            ));
        }
    }

    // Sort by most stale first (most changed refs)
    stale_docs.sort_by(|a, b| {
        let a_total: usize = a.2.iter().map(|(_, c)| c).sum();
        let b_total: usize = b.2.iter().map(|(_, c)| c).sum();
        b_total.cmp(&a_total)
    });

    println!("## Stale docs: {}\n", stale_docs.len());
    for (doc, date, refs) in &stale_docs {
        println!("- `{}` (last updated {})", doc, date);
        for (ref_path, commits) in refs {
            println!("  - `{}` changed {} time(s) since", ref_path, commits);
        }
    }

    println!("\n## Summary\n");
    println!("- Fresh (refs unchanged): {fresh_docs}");
    println!("- Stale (refs changed): {}", stale_docs.len());
    println!("- No code refs: {no_refs}");

    Ok(())
}

/// CMM graduation sweep: iterate all skills, call check_graduation on each,
/// emit a markdown report to stdout. Silent-ok if no skills graduate.
async fn cmd_graduate_all(cli: &ResolvedConfig) -> Result<()> {
    let mut indexd = IndexdClient::connect(Some(&cli.socket)).await?;
    let (skills, total) = indexd.list_skills(1000, 0).await?;

    println!("## Summary\n");
    println!("Scanned {total} skill entries.\n");

    let cfg = cosmix_config::store::load_service::<cosmix_config::SkillsSettings>("skills")
        .unwrap_or_default();
    println!(
        "Thresholds: confidence >= {:.2}, use_count >= {}, success_count >= {}\n",
        cfg.graduation_confidence, cfg.graduation_min_uses, cfg.graduation_min_successes
    );

    let mut graduated_now = Vec::new();
    let mut already_graduated = Vec::new();
    let mut below_threshold = Vec::new();
    let mut superseded = 0;

    for (id, doc) in &skills {
        if doc.superseded_by.is_some() {
            superseded += 1;
            continue;
        }
        if doc.graduated {
            already_graduated.push((*id, doc.clone()));
            continue;
        }
        match check_graduation(&mut indexd, *id, doc).await {
            Ok(true) => graduated_now.push((*id, doc.clone())),
            Ok(false) => below_threshold.push((*id, doc.clone())),
            Err(e) => eprintln!("WARN: graduation check failed for skill {id}: {e}"),
        }
    }

    println!("## Graduated this run: {}\n", graduated_now.len());
    for (id, doc) in &graduated_now {
        println!(
            "- [{id}] **{}** ({}) — confidence {:.0}%, {} uses, {} successes",
            doc.name,
            doc.domain,
            doc.confidence * 100.0,
            doc.use_count,
            doc.success_count
        );
    }

    println!("\n## Already graduated: {}\n", already_graduated.len());
    for (id, doc) in &already_graduated {
        println!("- [{id}] {} ({})", doc.name, doc.domain);
    }

    println!("\n## Below threshold: {}\n", below_threshold.len());
    for (id, doc) in &below_threshold {
        let need_conf = (cfg.graduation_confidence as f32 - doc.confidence).max(0.0);
        let need_uses = cfg.graduation_min_uses.saturating_sub(doc.use_count);
        let need_succ = cfg
            .graduation_min_successes
            .saturating_sub(doc.success_count);
        println!(
            "- [{id}] {} ({}) — gap: conf+{:.2}, uses+{}, successes+{}",
            doc.name, doc.domain, need_conf, need_uses, need_succ
        );
    }

    if superseded > 0 {
        println!("\n_{superseded} superseded entries skipped._");
    }

    Ok(())
}

/// CMM staleness report: query indexd's stale buckets, emit a markdown report.
async fn cmd_staleness_report(cli: &ResolvedConfig, source: &str) -> Result<()> {
    let mut indexd = IndexdClient::connect(Some(&cli.socket)).await?;
    let src = if source.is_empty() {
        None
    } else {
        Some(source)
    };
    let resp = indexd.stale(src, 90, 3, 180, 50).await?;

    println!("## Summary\n");
    println!("Total chunks in index: {}\n", resp.total_chunks);
    if let Some(s) = src {
        println!("Filtered to source: `{s}`\n");
    }

    let emit_bucket = |title: &str, subtitle: &str, chunks: &[StaleChunk]| {
        println!("## {}: {}\n", title, chunks.len());
        println!("_{}_\n", subtitle);
        for c in chunks {
            let name = c
                .filename
                .clone()
                .or_else(|| c.path.clone())
                .unwrap_or_else(|| format!("chunk #{}", c.id));
            println!(
                "- [{}] `{}` ({}) — retrieved {}×, feedback {}, created {}, last retrieved {}",
                c.id,
                name,
                c.source,
                c.retrieval_count,
                c.feedback_score,
                c.created,
                c.last_retrieved.as_deref().unwrap_or("never"),
            );
        }
        println!();
    };

    emit_bucket(
        "Never retrieved & old (>90d)",
        "Candidates for archival — were indexed long ago but have never been useful.",
        &resp.never_retrieved_old,
    );
    emit_bucket(
        "Low-value (retrieved >3× with feedback ≤0)",
        "Candidates for reshaping — they surface in searches but agents never upvote them.",
        &resp.low_value,
    );
    emit_bucket(
        "Long-dormant (not retrieved in 180d)",
        "Candidates for refresh — were useful once, may be out of date now.",
        &resp.long_dormant,
    );

    Ok(())
}

/// Split markdown content on `## ` (level-2) headings. Returns (section_title, body) pairs.
/// First section uses filename (no .md) as title. Sections with body < 50 chars are dropped.
fn split_markdown_sections(content: &str, filename: &str) -> Vec<(String, String)> {
    let default_title = filename.strip_suffix(".md").unwrap_or(filename).to_string();
    let mut sections = Vec::new();
    let mut current_title = default_title;
    let mut current_body = String::new();
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("## ") {
            if current_body.trim().len() >= 50 {
                sections.push((current_title.clone(), current_body.trim().to_string()));
            }
            current_title = rest.to_string();
            current_body.clear();
        } else {
            current_body.push_str(line);
            current_body.push('\n');
        }
    }
    if current_body.trim().len() >= 50 {
        sections.push((current_title, current_body.trim().to_string()));
    }
    sections
}

/// Classify a workspace-relative path into a source type.
/// Returns (source_name, needs_date, is_memory) or None if not indexable.
fn classify_path(rel_path: &str) -> Option<(&'static str, bool, bool)> {
    if rel_path.ends_with("_notes.md") || rel_path == "_notes.md" {
        // Treat _notes.md as doc (workspace-level design truth)
        Some(("doc", false, false))
    } else if rel_path.contains("_doc/") && rel_path.ends_with(".md") {
        Some(("doc", false, false))
    } else if rel_path.contains("_journal/") && rel_path.ends_with(".md") {
        Some(("journal", true, false))
    } else if rel_path.contains("_memory/") && rel_path.ends_with(".md") {
        Some(("memory", true, true))
    } else {
        None
    }
}

/// One tracked markdown file enumerated by `enumerate_tracked_content`:
/// (absolute path, repo-relative path, source-type tag, needs-date-prefix
/// check, is-_memory-tree flag). Tuple kept rather than promoted to a
/// struct because every consumer destructures it inline.
type TrackedFile = (std::path::PathBuf, String, &'static str, bool, bool);

/// Enumerate tracked markdown files via git ls-files, filtered by classify_path.
fn enumerate_tracked_content(workspace: &std::path::Path) -> Result<Vec<TrackedFile>> {
    use std::process::Command;
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(["ls-files", "--", "*.md"])
        .output()
        .context("running git ls-files")?;
    if !output.status.success() {
        anyhow::bail!(
            "git ls-files failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut results = Vec::new();
    for rel in stdout.lines() {
        if let Some((source, needs_date, is_memory)) = classify_path(rel) {
            let abs = workspace.join(rel);
            if abs.is_file() {
                results.push((abs, rel.to_string(), source, needs_date, is_memory));
            }
        }
    }
    Ok(results)
}

/// Delete existing chunks for a given file path (idempotent re-index support).
async fn delete_chunks_for_path(
    indexd: &mut IndexdClient,
    source: &str,
    filepath: &str,
) -> Result<usize> {
    // Search for chunks matching this path via metadata filter (uses Contains since path is in JSON)
    let req = serde_json::json!({
        "action": "list",
        "source": source,
        "limit": 10000,
        "offset": 0,
    });
    let resp = indexd.raw_request(&req).await?;
    let items = resp
        .get("items")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut ids_to_delete = Vec::new();
    for item in items {
        let meta = item.get("metadata").and_then(|m| m.as_str()).unwrap_or("");
        if meta.contains(filepath)
            && let Some(id) = item.get("id").and_then(|v| v.as_i64())
        {
            ids_to_delete.push(id);
        }
    }
    if ids_to_delete.is_empty() {
        return Ok(0);
    }
    let del_req = serde_json::json!({
        "action": "delete",
        "ids": ids_to_delete,
    });
    let _ = indexd.raw_request(&del_req).await?;
    Ok(ids_to_delete.len())
}

async fn cmd_delete_paths(cli: &ResolvedConfig, paths: &[String]) -> Result<()> {
    if paths.is_empty() {
        println!("No paths provided.");
        return Ok(());
    }
    let mut indexd = IndexdClient::connect(Some(&cli.socket)).await?;
    let sources = ["doc", "journal", "memory"];
    let mut total_deleted = 0usize;
    for path in paths {
        let mut per_path = 0usize;
        for source in &sources {
            match delete_chunks_for_path(&mut indexd, source, path).await {
                Ok(n) => per_path += n,
                Err(e) => eprintln!("  {path} [{source}]: {e}"),
            }
        }
        println!("{per_path:>4}  {path}");
        total_deleted += per_path;
    }
    println!("\nTotal chunks deleted: {total_deleted}");
    Ok(())
}

async fn cmd_bootstrap_index(
    cli: &ResolvedConfig,
    path: &str,
    filter: Option<&str>,
    skip_delete: bool,
) -> Result<()> {
    let expanded = if let Some(rest) = path.strip_prefix("~/") {
        format!("{}/{}", std::env::var("HOME").unwrap_or_default(), rest)
    } else if path == "~" {
        std::env::var("HOME").unwrap_or_default()
    } else {
        path.to_string()
    };
    // Canonicalize so relative paths (e.g. `.`) resolve to an absolute form
    // before detect_domain consults [domains.map] — otherwise domain falls
    // through to "general" for anything not starting with an absolute prefix.
    let workspace_buf = std::fs::canonicalize(&expanded)
        .with_context(|| format!("canonicalizing workspace path: {expanded}"))?;
    let workspace = workspace_buf.as_path();
    if !workspace.exists() {
        anyhow::bail!("workspace does not exist: {}", workspace.display());
    }
    // Verify it's a git repo
    if !workspace.join(".git").exists() {
        anyhow::bail!(
            "not a git repo (no .git directory): {}",
            workspace.display()
        );
    }

    let domain = cosmix_skills::detect_domain(workspace);
    println!("Workspace:  {}", workspace.display());
    println!("Domain:     {domain}");

    let mut indexd = IndexdClient::connect(Some(&cli.socket)).await?;

    let all_files = enumerate_tracked_content(workspace)?;
    let files: Vec<_> = if let Some(filter) = filter {
        all_files
            .into_iter()
            .filter(|(_, rel, _, _, _)| rel.contains(filter))
            .collect()
    } else {
        all_files
    };
    if let Some(filter) = filter {
        eprintln!("Files to index (filter \"{filter}\"): {}\n", files.len());
    } else {
        eprintln!("Files to index: {}\n", files.len());
    }

    let mut stored = 0u32;
    let mut deduped = 0u32;
    let mut deleted = 0usize;
    let mut errors = Vec::new();

    // Collect all sections across all files grouped by source, batch stores.
    // Batching = fewer indexd round-trips + batch embedding (efficient).
    #[derive(Clone)]
    struct Pending {
        source: &'static str,
        text: String,
        metadata: String,
        file_label: String,
    }
    let mut pending: Vec<Pending> = Vec::new();

    for (abs_path, rel_path, source, needs_date, is_memory) in &files {
        let filename = abs_path.file_name().and_then(|f| f.to_str()).unwrap_or("?");
        let path_str = abs_path.to_string_lossy().to_string();

        let content = match std::fs::read_to_string(abs_path) {
            Ok(c) => c,
            Err(e) => {
                errors.push(format!("{rel_path}: read failed: {e}"));
                continue;
            }
        };

        let sections = split_markdown_sections(&content, filename);
        if sections.is_empty() {
            eprintln!("  skip (no sections ≥50 chars): {rel_path}");
            continue;
        }

        // Delete old chunks for this path before re-inserting
        if !skip_delete {
            match delete_chunks_for_path(&mut indexd, source, &path_str).await {
                Ok(n) => deleted += n,
                Err(e) => errors.push(format!("{rel_path}: delete failed: {e}")),
            }
        }

        let date = if *needs_date {
            filename.get(..10).unwrap_or("1970-01-01").to_string()
        } else {
            chrono::Local::now().format("%Y-%m-%d").to_string()
        };

        for (title, body) in &sections {
            let mut meta = serde_json::json!({
                "path": path_str,
                "filename": filename,
                "section": title,
                "domain": &domain,
                "date": date,
            });
            if *is_memory {
                meta["generator"] = serde_json::json!("bootstrap-index");
                meta["tier"] = serde_json::json!("manual");
            }
            pending.push(Pending {
                source: match *source {
                    "doc" => "doc",
                    "journal" => "journal",
                    "memory" => "memory",
                    _ => "doc",
                },
                text: body.clone(),
                metadata: meta.to_string(),
                file_label: format!("{rel_path}/{title}"),
            });
        }
        eprintln!(
            "  [{source:>7}] {rel_path} → {} sections queued",
            sections.len()
        );
    }

    // Flush batches — group by source (indexd store takes single source per batch).
    // NOTE: batch size kept at 1 because candle's nomic-embed-text pads every batch
    // element to the longest sequence's token count. A single long section in a batch
    // of 16 can blow memory past 10GB. Batch=1 avoids that entirely; throughput is
    // bounded by sequential model forward passes.
    const BATCH_SIZE: usize = 1;
    // Truncate sections to a reasonable token budget (~8KB ≈ 2000 tokens of text).
    const MAX_CHARS_PER_SECTION: usize = 8000;
    for p in pending.iter_mut() {
        if p.text.len() > MAX_CHARS_PER_SECTION {
            p.text.truncate(MAX_CHARS_PER_SECTION);
            p.text.push_str("\n\n[...truncated]");
        }
    }
    let total = pending.len();
    eprintln!(
        "\nEmbedding {total} sections (batch={BATCH_SIZE}, max {MAX_CHARS_PER_SECTION} chars)...\n"
    );

    use std::collections::HashMap;
    let mut by_source: HashMap<&'static str, Vec<Pending>> = HashMap::new();
    for p in pending {
        by_source.entry(p.source).or_default().push(p);
    }

    let mut done = 0usize;
    for (source, items) in by_source.iter() {
        for batch in items.chunks(BATCH_SIZE) {
            let texts: Vec<&str> = batch.iter().map(|p| p.text.as_str()).collect();
            let metadata: Vec<&str> = batch.iter().map(|p| p.metadata.as_str()).collect();
            let req = serde_json::json!({
                "action": "store",
                "texts": texts,
                "source": source,
                "metadata": metadata,
            });
            match indexd.raw_request(&req).await {
                Ok(resp) => {
                    let s = resp.get("stored").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                    let d = resp.get("duplicates").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                    stored += s;
                    deduped += d;
                }
                Err(e) => {
                    for p in batch {
                        errors.push(format!("{}: {e}", p.file_label));
                    }
                }
            }
            done += batch.len();
            eprintln!("  progress: {done}/{total} ({source})");
        }
    }

    println!("\n## Summary");
    println!("  stored:   {stored} new sections");
    println!("  deduped:  {deduped} exact duplicates (content_hash match)");
    println!("  deleted:  {deleted} pre-existing chunks");
    if !errors.is_empty() {
        println!("  errors:   {}", errors.len());
        for e in errors.iter().take(10) {
            println!("    - {e}");
        }
    }

    Ok(())
}

// ── Phase 4: Self-Modification commands ─────────────────────────────────

fn expand_path(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        format!("{}/{}", std::env::var("HOME").unwrap_or_default(), rest)
    } else if path == "~" {
        std::env::var("HOME").unwrap_or_default()
    } else if path == "." {
        std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| ".".into())
    } else {
        path.to_string()
    }
}

/// Validate CLAUDE.md against the actual workspace.
async fn cmd_claude_md_audit(path: &str) -> Result<()> {
    use std::collections::HashSet;
    use std::path::Path;

    let workspace = expand_path(path);
    let ws = Path::new(&workspace);
    let claude_md_path = ws.join("CLAUDE.md");

    let content = std::fs::read_to_string(&claude_md_path).context("reading CLAUDE.md")?;

    println!("## CLAUDE.md Audit Report\n");
    println!("Workspace: `{workspace}`\n");

    let mut issues = 0u32;

    // ── 1. Parse crate tables ──
    // Extract crate names from markdown table rows: | `cosmix-xxx` | ... |
    let mut listed_libs = HashSet::new();
    let mut listed_apps = HashSet::new();
    let mut listed_daemons = HashSet::new();

    #[derive(Clone, Copy, PartialEq)]
    enum Section {
        None,
        Libs,
        Apps,
        Daemons,
    }
    let mut current_section = Section::None;

    for line in content.lines() {
        if line.contains("### Libraries") {
            current_section = Section::Libs;
            continue;
        }
        if line.contains("### GUI Apps") {
            current_section = Section::Apps;
            continue;
        }
        if line.contains("### Daemons") {
            current_section = Section::Daemons;
            continue;
        }
        if line.starts_with("### ") || line.starts_with("## ") {
            current_section = Section::None;
            continue;
        }
        if current_section == Section::None {
            continue;
        }

        // Extract crate name from first table column: | `cosmix-xxx` |
        if let Some(rest) = line.strip_prefix('|') {
            let cell = rest.split('|').next().unwrap_or("").trim();
            // Strip backticks and bold markers
            let name = cell.trim_matches('`').trim_matches('*').trim();
            if name.starts_with("cosmix-") {
                match current_section {
                    Section::Libs => {
                        listed_libs.insert(name.to_string());
                    }
                    Section::Apps => {
                        listed_apps.insert(name.to_string());
                    }
                    Section::Daemons => {
                        listed_daemons.insert(name.to_string());
                    }
                    Section::None => {}
                }
            }
        }
    }

    let all_listed: HashSet<_> = listed_libs
        .iter()
        .chain(listed_apps.iter())
        .chain(listed_daemons.iter())
        .cloned()
        .collect();

    // ── 2. Scan actual crates ──
    let crates_dir = ws.join("src/crates");
    let mut actual_crates = HashSet::new();
    if crates_dir.is_dir() {
        for entry in std::fs::read_dir(&crates_dir)? {
            let entry = entry?;
            if entry.path().is_dir()
                && let Some(name) = entry.file_name().to_str()
                && name.starts_with("cosmix-")
            {
                actual_crates.insert(name.to_string());
            }
        }
    }

    // ── 3. Compare ──
    let listed_missing: Vec<_> = all_listed.difference(&actual_crates).collect();
    let unlisted: Vec<_> = actual_crates.difference(&all_listed).collect();

    println!("## Crate table accuracy\n");
    println!(
        "Listed in CLAUDE.md: {} (libs: {}, apps: {}, daemons: {})",
        all_listed.len(),
        listed_libs.len(),
        listed_apps.len(),
        listed_daemons.len()
    );
    println!("Actual in src/crates/: {}\n", actual_crates.len());

    if listed_missing.is_empty() {
        println!("Listed but missing from workspace: _none_\n");
    } else {
        println!(
            "### Listed but missing from workspace: {}\n",
            listed_missing.len()
        );
        for name in &listed_missing {
            println!("- `{name}` — listed in CLAUDE.md but `src/crates/{name}/` does not exist");
            issues += 1;
        }
        println!();
    }

    if unlisted.is_empty() {
        println!("Exist but unlisted in CLAUDE.md: _none_\n");
    } else {
        println!("### Exist but unlisted in CLAUDE.md: {}\n", unlisted.len());
        for name in &unlisted {
            println!("- `{name}` — exists at `src/crates/{name}/` but not in any CLAUDE.md table");
            issues += 1;
        }
        println!();
    }

    // ── 4. External path dependencies ──
    println!("## External path dependencies\n");
    // Since the 2026-08-30 monorepo, cosmix-lib-mix is a sibling crate in
    // the one workspace: `$COSMIX_SRC/crates/cosmix-lib-mix`.
    let mix_lib_path = cosmix_config::cosmix_src()
        .join("crates/cosmix-lib-mix")
        .to_string_lossy()
        .into_owned();
    let ext_deps = [("cosmix-lib-mix", mix_lib_path)];
    for (name, dep_path) in &ext_deps {
        if Path::new(dep_path).is_dir() {
            println!("- `{name}` at `{dep_path}` — OK");
        } else {
            println!("- `{name}` at `{dep_path}` — **MISSING**");
            issues += 1;
        }
    }
    println!();

    // ── 5. _doc/ file references ──
    println!("## Document references\n");
    let mut doc_refs_ok = 0u32;
    let mut doc_refs_missing = Vec::new();
    for line in content.lines() {
        // Find backtick-quoted paths containing _doc/
        let mut rest = line;
        while let Some(start) = rest.find('`') {
            let after = &rest[start + 1..];
            if let Some(end) = after.find('`') {
                let quoted = &after[..end];
                if quoted.contains("_doc/") && quoted.ends_with(".md") {
                    let check_path = ws.join(quoted);
                    if check_path.is_file() {
                        doc_refs_ok += 1;
                    } else {
                        doc_refs_missing.push(quoted.to_string());
                    }
                }
                rest = &after[end + 1..];
            } else {
                break;
            }
        }
    }
    println!("- Valid _doc/ references: {doc_refs_ok}");
    if doc_refs_missing.is_empty() {
        println!("- Missing _doc/ references: _none_");
    } else {
        println!("- Missing _doc/ references: {}", doc_refs_missing.len());
        for r in &doc_refs_missing {
            println!("  - `{r}` — referenced in CLAUDE.md but file does not exist");
            issues += 1;
        }
    }
    println!();

    // ── 6. cosmix-lib-ui module structure ──
    println!("## cosmix-lib-ui module structure\n");
    let ui_src = ws.join("src/crates/cosmix-lib-ui/src");
    let expected_modules: &[(&str, bool)] = &[
        ("theme.rs", false),
        ("icons.rs", false),
        ("app_init.rs", false),
        ("primitives.rs", false),
        ("menu", true), // directory
        ("components", true),
        ("dx_components", true),
    ];
    for (name, is_dir) in expected_modules {
        let p = ui_src.join(name);
        let exists = if *is_dir { p.is_dir() } else { p.is_file() };
        if exists {
            println!("- `{name}` — OK");
        } else {
            println!("- `{name}` — **MISSING**");
            issues += 1;
        }
    }
    println!();

    // ── Summary ──
    println!("## Summary\n");
    if issues == 0 {
        println!("CLAUDE.md is consistent with the workspace. No issues found.");
    } else {
        println!("**{issues} issue(s) detected.** Review flagged items and update CLAUDE.md.");
    }

    Ok(())
}

/// Analyze skill usage patterns: trigger overlap, tool overlap, merge candidates.
async fn cmd_skill_composition(cli: &ResolvedConfig) -> Result<()> {
    use std::collections::{HashMap, HashSet};

    let mut indexd = IndexdClient::connect(Some(&cli.socket)).await?;
    let (skills, _total) = indexd.list_skills(1000, 0).await?;

    // Filter to active (non-superseded) skills
    let active: Vec<_> = skills
        .iter()
        .filter(|(_, doc)| doc.superseded_by.is_none())
        .collect();

    println!("## Skill Composition Analysis\n");
    println!("Active skills: {}\n", active.len());

    if active.len() < 2 {
        println!("Too few active skills for composition analysis. Need at least 2.");
        return Ok(());
    }

    // ── Inventory by domain ──
    let mut by_domain: HashMap<&str, u32> = HashMap::new();
    for (_, doc) in &active {
        *by_domain.entry(doc.domain.as_str()).or_default() += 1;
    }
    println!("### Inventory by domain\n");
    for (domain, count) in &by_domain {
        println!("- `{domain}`: {count} skills");
    }
    println!();

    // ── Trigger similarity (Jaccard on words) ──
    fn word_set(text: &str) -> HashSet<String> {
        text.to_lowercase()
            .split_whitespace()
            .filter(|w| w.len() > 2) // skip tiny words
            .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_string())
            .filter(|w| !w.is_empty())
            .collect()
    }

    fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
        if a.is_empty() && b.is_empty() {
            return 0.0;
        }
        let intersection = a.intersection(b).count() as f64;
        let union = a.union(b).count() as f64;
        intersection / union
    }

    let trigger_sets: Vec<_> = active
        .iter()
        .map(|(id, doc)| (*id, &doc.name, word_set(&doc.trigger)))
        .collect();

    let mut overlapping_triggers = Vec::new();
    for i in 0..trigger_sets.len() {
        for j in (i + 1)..trigger_sets.len() {
            let coeff = jaccard(&trigger_sets[i].2, &trigger_sets[j].2);
            if coeff > 0.3 {
                overlapping_triggers.push((
                    trigger_sets[i].0,
                    trigger_sets[i].1.as_str(),
                    trigger_sets[j].0,
                    trigger_sets[j].1.as_str(),
                    coeff,
                ));
            }
        }
    }

    println!("### Overlapping triggers (Jaccard > 0.3)\n");
    if overlapping_triggers.is_empty() {
        println!("_No overlapping triggers found._\n");
    } else {
        for (id_a, name_a, id_b, name_b, coeff) in &overlapping_triggers {
            println!(
                "- [{id_a}] `{name_a}` ↔ [{id_b}] `{name_b}` — similarity {:.0}%",
                coeff * 100.0
            );
        }
        println!();
    }

    // ── Tool overlap ──
    let tool_sets: Vec<_> = active
        .iter()
        .map(|(id, doc)| {
            let tools: HashSet<_> = doc.tools_required.iter().cloned().collect();
            (*id, &doc.name, tools)
        })
        .collect();

    let mut tool_overlaps = Vec::new();
    for i in 0..tool_sets.len() {
        for j in (i + 1)..tool_sets.len() {
            if tool_sets[i].2.is_empty() || tool_sets[j].2.is_empty() {
                continue;
            }
            let shared = tool_sets[i].2.intersection(&tool_sets[j].2).count();
            let max_size = tool_sets[i].2.len().max(tool_sets[j].2.len());
            let overlap = shared as f64 / max_size as f64;
            if overlap > 0.5 {
                tool_overlaps.push((
                    tool_sets[i].0,
                    tool_sets[i].1.as_str(),
                    tool_sets[j].0,
                    tool_sets[j].1.as_str(),
                    shared,
                    overlap,
                ));
            }
        }
    }

    println!("### Tool overlap (>50% shared tools)\n");
    if tool_overlaps.is_empty() {
        println!("_No significant tool overlap found._\n");
    } else {
        for (id_a, name_a, id_b, name_b, shared, overlap) in &tool_overlaps {
            println!(
                "- [{id_a}] `{name_a}` ↔ [{id_b}] `{name_b}` — {shared} shared tools ({:.0}%)",
                overlap * 100.0
            );
        }
        println!();
    }

    // ── Merge candidates (overlapping triggers + same domain) ──
    let mut merge_candidates = Vec::new();
    for (id_a, name_a, id_b, name_b, coeff) in &overlapping_triggers {
        let doc_a = active.iter().find(|(id, _)| id == id_a).map(|(_, d)| d);
        let doc_b = active.iter().find(|(id, _)| id == id_b).map(|(_, d)| d);
        if let (Some(a), Some(b)) = (doc_a, doc_b)
            && a.domain == b.domain
        {
            merge_candidates.push((*id_a, *name_a, *id_b, *name_b, *coeff, a.domain.as_str()));
        }
    }

    println!("### Merge candidates\n");
    if merge_candidates.is_empty() {
        println!("_No merge candidates found._\n");
    } else {
        for (id_a, name_a, id_b, name_b, coeff, domain) in &merge_candidates {
            println!(
                "- [{id_a}] `{name_a}` + [{id_b}] `{name_b}` (domain: `{domain}`, trigger similarity: {:.0}%)",
                coeff * 100.0
            );
        }
        println!();
    }

    // ── Co-retrieval patterns (placeholder) ──
    println!("### Co-retrieval patterns\n");
    println!("_Not yet implemented. Requires retrieval session logging in indexd._");
    println!(
        "_Will activate when indexd tracks which skills are retrieved together per session._\n"
    );

    // ── Summary ──
    println!("## Summary\n");
    println!("- Active skills: {}", active.len());
    println!(
        "- Overlapping trigger pairs: {}",
        overlapping_triggers.len()
    );
    println!("- Tool overlap pairs: {}", tool_overlaps.len());
    println!("- Merge candidates: {}", merge_candidates.len());

    Ok(())
}

/// Validate MEMORY.md entries: frontmatter, broken file references, stale entries.
async fn cmd_memory_hygiene(stale_days: u32) -> Result<()> {
    use std::path::Path;
    use std::time::{Duration, SystemTime};

    let home = std::env::var("HOME").unwrap_or_default();
    // Derive memory dir from workspace path using Claude Code's convention:
    // /home/cosmix/.cosmix → -home-cosmix--cosmix (strip /, replace / and . with -, prepend -)
    let workspace = cosmix_config::cosmix_src().to_string_lossy().into_owned();
    let ws_path = workspace.strip_prefix('/').unwrap_or(&workspace);
    let project_key = format!("-{}", ws_path.replace(['/', '.'], "-"));
    let memory_dir = format!("{home}/.claude/projects/{project_key}/memory");
    let mem_path = Path::new(&memory_dir);

    if !mem_path.is_dir() {
        println!("## Memory Hygiene Report\n");
        println!("Memory directory not found: `{memory_dir}`");
        return Ok(());
    }

    let index_path = mem_path.join("MEMORY.md");
    let index_content = std::fs::read_to_string(&index_path).context("reading MEMORY.md")?;

    println!("## Memory Hygiene Report\n");
    println!("Memory dir: `{memory_dir}`\n");

    // ── Parse MEMORY.md links ──
    let mut linked_files = Vec::new();
    for line in index_content.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with("- [") {
            continue;
        }
        // Extract filename from [title](filename.md)
        if let Some(paren_start) = trimmed.find("](") {
            let after = &trimmed[paren_start + 2..];
            if let Some(paren_end) = after.find(')') {
                let filename = &after[..paren_end];
                linked_files.push(filename.to_string());
            }
        }
    }

    println!("Entries in MEMORY.md: {}\n", linked_files.len());

    let now = SystemTime::now();
    let stale_threshold = Duration::from_secs(stale_days as u64 * 86400);

    let mut valid = Vec::new();
    let mut broken_links = Vec::new();
    let mut invalid_frontmatter = Vec::new();
    let mut broken_refs = Vec::new();
    let mut stale_entries = Vec::new();

    for filename in &linked_files {
        let file_path = mem_path.join(filename);

        // Check existence
        if !file_path.is_file() {
            broken_links.push(filename.clone());
            continue;
        }

        let content = match std::fs::read_to_string(&file_path) {
            Ok(c) => c,
            Err(_) => {
                broken_links.push(filename.clone());
                continue;
            }
        };

        // Parse frontmatter
        let mut fm_valid = true;
        let mut fm_issues = Vec::new();
        if let Some(after_open) = content.strip_prefix("---\n") {
            if let Some(end) = after_open.find("\n---") {
                let fm_block = &after_open[..end];
                let mut has_name = false;
                let mut has_desc = false;
                let mut has_type = false;
                let mut type_valid = false;
                for fm_line in fm_block.lines() {
                    if let Some((key, val)) = fm_line.split_once(": ") {
                        let key = key.trim();
                        let val = val.trim();
                        match key {
                            "name" => {
                                has_name = !val.is_empty();
                            }
                            "description" => {
                                has_desc = !val.is_empty();
                            }
                            "type" => {
                                has_type = true;
                                type_valid =
                                    matches!(val, "project" | "feedback" | "reference" | "user");
                            }
                            _ => {}
                        }
                    }
                }
                if !has_name {
                    fm_issues.push("missing `name`");
                    fm_valid = false;
                }
                if !has_desc {
                    fm_issues.push("missing `description`");
                    fm_valid = false;
                }
                if !has_type {
                    fm_issues.push("missing `type`");
                    fm_valid = false;
                }
                if has_type && !type_valid {
                    fm_issues.push("invalid `type` value");
                    fm_valid = false;
                }
            } else {
                fm_issues.push("unclosed frontmatter (no closing ---)");
                fm_valid = false;
            }
        } else {
            fm_issues.push("no frontmatter found");
            fm_valid = false;
        }

        if !fm_valid {
            invalid_frontmatter.push((filename.clone(), fm_issues));
        }

        // Check referenced paths in body
        let mut file_broken_refs = Vec::new();
        let ws_root = Path::new(&workspace);
        for line in content.lines() {
            let mut rest = line;
            while let Some(start) = rest.find('`') {
                let after = &rest[start + 1..];
                if let Some(end) = after.find('`') {
                    let quoted = &after[..end];
                    // Check src/ paths relative to workspace
                    if quoted.starts_with("src/") && (quoted.contains('/') && quoted.len() > 5) {
                        let check = ws_root.join(quoted);
                        if !check.exists() {
                            file_broken_refs.push(quoted.to_string());
                        }
                    }
                    // Check ~/ absolute paths
                    if let Some(rest_path) = quoted.strip_prefix("~/") {
                        let expanded = format!("{home}/{rest_path}");
                        if !Path::new(&expanded).exists() {
                            file_broken_refs.push(quoted.to_string());
                        }
                    }
                    rest = &after[end + 1..];
                } else {
                    break;
                }
            }
        }
        if !file_broken_refs.is_empty() {
            broken_refs.push((filename.clone(), file_broken_refs));
        }

        // Check staleness by mtime
        if let Ok(meta) = std::fs::metadata(&file_path)
            && let Ok(modified) = meta.modified()
            && let Ok(age) = now.duration_since(modified)
            && age > stale_threshold
        {
            let days = age.as_secs() / 86400;
            stale_entries.push((filename.clone(), days));
        }

        if fm_valid {
            valid.push(filename.clone());
        }
    }

    // ── Detect orphan files ──
    let mut orphans = Vec::new();
    if let Ok(entries) = std::fs::read_dir(mem_path) {
        let linked_set: std::collections::HashSet<_> = linked_files.iter().cloned().collect();
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str()
                && name.ends_with(".md")
                && name != "MEMORY.md"
                && !linked_set.contains(name)
            {
                orphans.push(name.to_string());
            }
        }
    }

    // ── Output ──
    println!("### Valid entries: {}\n", valid.len());
    for f in &valid {
        println!("- `{f}` — OK");
    }
    println!();

    println!("### Broken links: {}\n", broken_links.len());
    if broken_links.is_empty() {
        println!("_None._\n");
    } else {
        for f in &broken_links {
            println!("- `{f}` — linked in MEMORY.md but file does not exist");
        }
        println!();
    }

    println!("### Invalid frontmatter: {}\n", invalid_frontmatter.len());
    if invalid_frontmatter.is_empty() {
        println!("_None._\n");
    } else {
        for (f, issues) in &invalid_frontmatter {
            println!("- `{f}` — {}", issues.join(", "));
        }
        println!();
    }

    println!("### Broken references: {}\n", broken_refs.len());
    if broken_refs.is_empty() {
        println!("_None._\n");
    } else {
        for (f, refs) in &broken_refs {
            println!("- `{f}`:");
            for r in refs {
                println!("  - `{r}` — path does not exist");
            }
        }
        println!();
    }

    println!(
        "### Stale entries (>{stale_days}d since modified): {}\n",
        stale_entries.len()
    );
    if stale_entries.is_empty() {
        println!("_None._\n");
    } else {
        for (f, days) in &stale_entries {
            println!("- `{f}` — last modified {days} days ago");
        }
        println!();
    }

    println!(
        "### Orphan files (in memory dir but not in MEMORY.md): {}\n",
        orphans.len()
    );
    if orphans.is_empty() {
        println!("_None._\n");
    } else {
        for f in &orphans {
            println!("- `{f}`");
        }
        println!();
    }

    // ── Summary ──
    let total_issues = broken_links.len()
        + invalid_frontmatter.len()
        + broken_refs.len()
        + stale_entries.len()
        + orphans.len();
    println!("## Summary\n");
    if total_issues == 0 {
        println!(
            "All {0} memory entries are valid. No issues found.",
            linked_files.len()
        );
    } else {
        println!(
            "**{total_issues} issue(s) across {} entries.** Review flagged items.",
            linked_files.len()
        );
    }
    println!("\n_No auto-deletion performed. Review flagged entries manually._");

    Ok(())
}

/// Report crate directories with high git activity but few documentation references.
async fn cmd_doc_coverage(path: &str, days: u32) -> Result<()> {
    use std::collections::HashMap;
    use std::process::Command as Cmd;

    let workspace = expand_path(path);
    let ws = std::path::Path::new(&workspace);

    // Get all files changed in src/crates/ in the last N days
    let output = Cmd::new("git")
        .arg("-C")
        .arg(ws)
        .args([
            "log",
            &format!("--since={days} days ago"),
            "--name-only",
            "--format=",
            "--",
            "src/crates/",
        ])
        .output()
        .context("running git log")?;
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Count commits per crate
    let mut crate_commits: HashMap<String, u32> = HashMap::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Extract crate name: src/crates/cosmix-foo/... → cosmix-foo
        if let Some(rest) = line.strip_prefix("src/crates/")
            && let Some(crate_name) = rest.split('/').next()
            && crate_name.starts_with("cosmix-")
        {
            *crate_commits.entry(crate_name.to_string()).or_default() += 1;
        }
    }

    if crate_commits.is_empty() {
        println!("## Documentation Coverage Report\n");
        println!("No crate changes found in the last {days} days.");
        return Ok(());
    }

    // Find all _doc/ files (recursive for _spec/ and other subfolders)
    let doc_output = Cmd::new("git")
        .arg("-C")
        .arg(ws)
        .args([
            "ls-files",
            "--",
            "src/_doc/*.md",
            "src/_doc/**/*.md",
            "*/_doc/*.md",
            "*/_doc/**/*.md",
            "_doc/*.md",
            "_doc/**/*.md",
        ])
        .output()
        .context("running git ls-files for docs")?;
    let doc_stdout = String::from_utf8_lossy(&doc_output.stdout);
    let doc_files: Vec<&str> = doc_stdout.lines().filter(|l| !l.is_empty()).collect();

    // Read each doc and check which crates it references
    let mut crate_docs: HashMap<String, Vec<String>> = HashMap::new();
    for doc_rel in &doc_files {
        let doc_abs = ws.join(doc_rel);
        let content = match std::fs::read_to_string(&doc_abs) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let filename = doc_abs
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or(doc_rel)
            .to_string();

        for crate_name in crate_commits.keys() {
            if content.contains(crate_name) {
                crate_docs
                    .entry(crate_name.clone())
                    .or_default()
                    .push(filename.clone());
            }
        }
    }

    // Sort by commits descending
    let mut sorted: Vec<_> = crate_commits.iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(a.1));

    println!("## Documentation Coverage Report\n");
    println!(
        "Scanned {} crates with commits in last {days} days.\n",
        sorted.len()
    );

    let mut under_doc = Vec::new();
    let mut well_doc = Vec::new();

    for (name, commits) in &sorted {
        let docs = crate_docs.get(*name);
        let doc_count = docs.map(|d| d.len()).unwrap_or(0);
        if doc_count <= 1 {
            under_doc.push((*name, *commits, docs));
        } else {
            well_doc.push((*name, *commits, docs));
        }
    }

    println!("### Under-documented (0-1 doc refs, sorted by activity)\n");
    if under_doc.is_empty() {
        println!("_None — all active crates have documentation._\n");
    } else {
        for (name, commits, docs) in &under_doc {
            let doc_count = docs.map(|d| d.len()).unwrap_or(0);
            println!("- `{name}` — {commits} file changes, {doc_count} doc reference(s)");
            if let Some(ds) = docs {
                for d in *ds {
                    println!("  - {d}");
                }
            }
        }
        println!();
    }

    println!("### Well-documented (2+ doc refs)\n");
    if well_doc.is_empty() {
        println!("_None._\n");
    } else {
        for (name, commits, docs) in &well_doc {
            let doc_count = docs.map(|d| d.len()).unwrap_or(0);
            println!("- `{name}` — {commits} file changes, {doc_count} doc reference(s)");
            if let Some(ds) = docs {
                for d in ds.iter().take(5) {
                    println!("  - {d}");
                }
                if ds.len() > 5 {
                    println!("  - ...and {} more", ds.len() - 5);
                }
            }
        }
        println!();
    }

    println!("## Summary\n");
    println!("- Crates with activity: {}", sorted.len());
    println!("- Under-documented (0-1 refs): {}", under_doc.len());
    println!("- Well-documented (2+ refs): {}", well_doc.len());

    Ok(())
}

/// Index all known workspaces into the single indexd instance.
async fn cmd_bootstrap_all_workspaces(cli: &ResolvedConfig, skip_delete: bool) -> Result<()> {
    let home = std::env::var("HOME").context("HOME not set")?;

    // Derive workspace list from domains.toml `map` — deduplicate paths
    let domains = cosmix_config::store::load_service::<cosmix_config::DomainsSettings>("domains")
        .unwrap_or_default();
    let mut seen = std::collections::BTreeSet::new();
    let mut workspaces = Vec::new();
    for prefix in domains.map.keys() {
        let expanded = if let Some(rest) = prefix.strip_prefix("~/") {
            format!("{home}/{rest}")
        } else {
            prefix.clone()
        };
        if seen.insert(expanded.clone()) {
            workspaces.push(expanded);
        }
    }

    let mut indexed = 0u32;
    let mut skipped = 0u32;

    for ws in &workspaces {
        let ws_path = std::path::Path::new(ws);
        if !ws_path.join(".git").exists() {
            eprintln!("skip {ws} (no .git)");
            skipped += 1;
            continue;
        }
        eprintln!("\n=== Indexing docs: {ws} ===\n");
        if let Err(e) = cmd_bootstrap_index(cli, ws, None, skip_delete).await {
            eprintln!("ERROR indexing docs for {ws}: {e}");
        } else {
            indexed += 1;
        }
        // Also index source doc comments and Mix scripts
        eprintln!("\n=== Indexing source: {ws} ===\n");
        if let Err(e) = cmd_index_source(cli, ws, None).await {
            eprintln!("ERROR indexing source for {ws}: {e}");
        }
    }

    println!("\n## Cross-Workspace Index Summary\n");
    println!("- Indexed: {indexed} workspaces (docs + source)");
    println!("- Skipped: {skipped} (no .git directory)");

    Ok(())
}

/// Detect contradictions between skills and _doc/ files within the same domain.
///
/// Strategy:
/// 1. Extract "claims" from each skill: crate names mentioned, tools referenced,
///    deprecated terms (e.g. "use X", "don't use Y", "replaced by Z").
/// 2. Cross-reference skills for conflicting claims (one says "use X", another says
///    "X is deprecated" or "replaced by Y").
/// 3. Scan _doc/ files for references to deprecated patterns mentioned in skills.
async fn cmd_contradiction_check(cli: &ResolvedConfig, path: &str) -> Result<()> {
    use std::collections::{HashMap, HashSet};
    use std::process::Command as Cmd;

    let expanded = if let Some(rest) = path.strip_prefix("~/") {
        format!("{}/{}", std::env::var("HOME").unwrap_or_default(), rest)
    } else if path == "~" {
        std::env::var("HOME").unwrap_or_default()
    } else if path == "." {
        std::env::current_dir()?.to_string_lossy().to_string()
    } else {
        path.to_string()
    };
    let workspace = std::path::Path::new(&expanded);

    println!("## Contradiction Check Report\n");

    // ── Load all active skills ──
    let mut indexd = IndexdClient::connect(Some(&cli.socket)).await?;
    let (skills, _total) = indexd.list_skills(1000, 0).await?;
    let active: Vec<_> = skills
        .iter()
        .filter(|(_, doc)| doc.superseded_by.is_none())
        .collect();

    println!("Active skills scanned: {}\n", active.len());

    // ── Claim extraction ──
    // A "claim" is a (subject, assertion) pair extracted from skill text.
    // Subjects: crate names, tool names, pattern names.
    // Assertions: "use", "deprecated", "replaced", "removed", "avoid", "never".

    #[derive(Debug)]
    struct Claim {
        skill_id: i64,
        skill_name: String,
        subject: String,
        assertion: ClaimType,
        context: String,
    }

    #[derive(Debug, PartialEq)]
    enum ClaimType {
        Use,                // "use X", "X is required"
        Deprecated,         // "X is deprecated", "don't use X", "avoid X", "never X"
        ReplacedBy(String), // "X replaced by Y", "use Y instead of X"
    }

    let deprecation_patterns = [
        "deprecated",
        "don't use",
        "do not use",
        "avoid",
        "never use",
        "removed",
        "replaced by",
        "instead of",
        "no longer",
    ];
    let use_patterns = ["use ", "require", "need ", "must use"];

    fn extract_crate_refs(text: &str) -> HashSet<String> {
        let mut refs = HashSet::new();
        // Match cosmix-* and cosmix_* crate/module references
        for word in text.split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_') {
            let w = word.to_lowercase();
            if w.starts_with("cosmix-") || w.starts_with("cosmix_") {
                refs.insert(w);
            }
        }
        refs
    }

    let mut claims: Vec<Claim> = Vec::new();

    for (id, doc) in &active {
        let full_text = format!(
            "{} {} {}",
            doc.trigger,
            doc.approach,
            doc.failure_modes.join(" ")
        );
        let full_lower = full_text.to_lowercase();

        // Extract crate references and classify assertions
        let crate_refs = extract_crate_refs(&full_text);

        for crate_name in &crate_refs {
            // Check if any line containing this crate has a deprecation signal
            for line in full_text.lines() {
                let line_lower = line.to_lowercase();
                if !line_lower.contains(crate_name) {
                    continue;
                }

                let is_deprecated = deprecation_patterns.iter().any(|p| line_lower.contains(p));
                let is_use = use_patterns.iter().any(|p| line_lower.contains(p));

                // Check for "replaced by" pattern
                if let Some(pos) = line_lower.find("replaced by") {
                    let rest = &line[pos + 12..];
                    let replacement: String = rest
                        .split_whitespace()
                        .next()
                        .unwrap_or("")
                        .trim_matches(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
                        .to_lowercase();
                    if !replacement.is_empty() {
                        claims.push(Claim {
                            skill_id: *id,
                            skill_name: doc.name.clone(),
                            subject: crate_name.clone(),
                            assertion: ClaimType::ReplacedBy(replacement),
                            context: line.trim().to_string(),
                        });
                        continue;
                    }
                }

                if is_deprecated {
                    claims.push(Claim {
                        skill_id: *id,
                        skill_name: doc.name.clone(),
                        subject: crate_name.clone(),
                        assertion: ClaimType::Deprecated,
                        context: line.trim().to_string(),
                    });
                } else if is_use {
                    claims.push(Claim {
                        skill_id: *id,
                        skill_name: doc.name.clone(),
                        subject: crate_name.clone(),
                        assertion: ClaimType::Use,
                        context: line.trim().to_string(),
                    });
                }
            }
        }

        // Also check for technology-level deprecation signals (lua, docker, etc.)
        let known_deprecated = ["lua", "docker", "actix", "warp", "libcosmic"];
        for term in &known_deprecated {
            if full_lower.contains(term) {
                // Check if it's being recommended (contradiction) vs warned against (fine)
                for line in full_text.lines() {
                    let line_lower = line.to_lowercase();
                    if !line_lower.contains(term) {
                        continue;
                    }
                    let is_recommending = use_patterns.iter().any(|p| line_lower.contains(p))
                        && !deprecation_patterns.iter().any(|p| line_lower.contains(p));
                    if is_recommending {
                        claims.push(Claim {
                            skill_id: *id,
                            skill_name: doc.name.clone(),
                            subject: term.to_string(),
                            assertion: ClaimType::Use,
                            context: line.trim().to_string(),
                        });
                    }
                }
            }
        }
    }

    // ── Cross-reference claims for contradictions ──
    // A contradiction: same subject has both Use and Deprecated/ReplacedBy claims.
    let mut by_subject: HashMap<&str, Vec<&Claim>> = HashMap::new();
    for claim in &claims {
        by_subject.entry(&claim.subject).or_default().push(claim);
    }

    let mut contradictions = Vec::new();
    for (subject, subject_claims) in &by_subject {
        let has_use = subject_claims.iter().any(|c| c.assertion == ClaimType::Use);
        let has_deprecated = subject_claims.iter().any(|c| {
            matches!(
                c.assertion,
                ClaimType::Deprecated | ClaimType::ReplacedBy(_)
            )
        });
        if has_use && has_deprecated {
            contradictions.push((subject.to_string(), subject_claims.clone()));
        }
    }

    println!("### Skill-to-Skill Contradictions\n");
    if contradictions.is_empty() {
        println!("_No contradictions found between skills._\n");
    } else {
        for (subject, subject_claims) in &contradictions {
            println!("**`{subject}`** — conflicting claims:\n");
            for claim in subject_claims {
                let assertion_str = match &claim.assertion {
                    ClaimType::Use => "USE",
                    ClaimType::Deprecated => "DEPRECATED",
                    ClaimType::ReplacedBy(r) => &format!("REPLACED BY {r}"),
                };
                println!(
                    "  - [{assertion_str}] skill [{id}] `{name}`: {ctx}",
                    id = claim.skill_id,
                    name = claim.skill_name,
                    ctx = claim.context
                );
            }
            println!();
        }
    }

    // ── Scan _doc/ for references to deprecated subjects ──
    let deprecated_subjects: HashSet<&str> = by_subject
        .iter()
        .filter(|(_, claims)| {
            claims.iter().any(|c| {
                matches!(
                    c.assertion,
                    ClaimType::Deprecated | ClaimType::ReplacedBy(_)
                )
            })
        })
        .map(|(subj, _)| *subj)
        .collect();

    println!("### Doc-to-Skill Contradictions\n");

    if deprecated_subjects.is_empty() {
        println!("_No deprecated subjects found in skills to check against docs._\n");
    } else {
        // Find all _doc/*.md files (recursive for _spec/ and other subfolders)
        let output = Cmd::new("git")
            .arg("-C")
            .arg(workspace)
            .args([
                "ls-files",
                "--",
                "src/_doc/*.md",
                "src/_doc/**/*.md",
                "*/_doc/*.md",
                "*/_doc/**/*.md",
                "_doc/*.md",
                "_doc/**/*.md",
            ])
            .output()
            .context("running git ls-files")?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let doc_files: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();

        let mut doc_contradictions = Vec::new();

        for doc_rel in &doc_files {
            let doc_abs = workspace.join(doc_rel);
            let content = match std::fs::read_to_string(&doc_abs) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let content_lower = content.to_lowercase();

            for subj in &deprecated_subjects {
                // Check if doc recommends/uses the deprecated subject (not just mentions it)
                if !content_lower.contains(*subj) {
                    continue;
                }

                for (line_num, line) in content.lines().enumerate() {
                    let line_lower = line.to_lowercase();
                    if !line_lower.contains(*subj) {
                        continue;
                    }
                    // Skip lines that themselves say deprecated/removed
                    if deprecation_patterns.iter().any(|p| line_lower.contains(p)) {
                        continue;
                    }
                    // Only flag if the line looks like a recommendation or usage
                    let looks_like_use = use_patterns.iter().any(|p| line_lower.contains(p))
                        || line_lower.contains("import")
                        || line_lower.contains("depend");
                    if looks_like_use {
                        doc_contradictions.push((
                            doc_rel.to_string(),
                            line_num + 1,
                            subj.to_string(),
                            line.trim().to_string(),
                        ));
                    }
                }
            }
        }

        if doc_contradictions.is_empty() {
            println!("_No docs recommend deprecated subjects._\n");
        } else {
            for (doc, line_num, subj, ctx) in &doc_contradictions {
                println!("- `{doc}`:{line_num} — references deprecated `{subj}`: {ctx}");
            }
            println!();
        }
    }

    // ── Provenance check: skills referencing files that no longer exist ──
    println!("### Stale Provenance\n");
    let mut stale_provenance = Vec::new();
    for (id, doc) in &active {
        if let Some(ref source_file) = doc.source_file {
            let full_path = workspace.join(source_file);
            if !full_path.exists() {
                stale_provenance.push((*id, doc.name.as_str(), source_file.as_str()));
            }
        }
    }

    if stale_provenance.is_empty() {
        println!("_All skill source files still exist (or no provenance recorded)._\n");
    } else {
        for (id, name, file) in &stale_provenance {
            println!("- [{id}] `{name}` — source file `{file}` no longer exists");
        }
        println!();
    }

    // ── Summary ──
    println!("## Summary\n");
    println!("- Skills scanned: {}", active.len());
    println!("- Claims extracted: {}", claims.len());
    println!("- Skill contradictions: {}", contradictions.len());
    println!(
        "- Deprecated subjects tracked: {}",
        deprecated_subjects.len()
    );
    println!("- Stale provenance: {}", stale_provenance.len());

    Ok(())
}

/// A doc comment block extracted from a .rs file.
struct DocBlock {
    /// "module" for //! or "item" for ///
    kind: &'static str,
    /// The doc comment text (stripped of /// or //! prefixes)
    text: String,
    /// The item signature following a /// block (first non-comment, non-attribute line)
    item_sig: Option<String>,
    /// Line number where the block starts (1-based)
    line_start: usize,
}

/// Parse a .rs file and extract all doc comment blocks.
fn extract_doc_blocks(source: &str) -> Vec<DocBlock> {
    let mut blocks = Vec::new();
    let lines: Vec<&str> = source.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let trimmed = lines[i].trim();

        // //! module doc comment
        if trimmed.starts_with("//!") {
            let start = i;
            let mut text = String::new();
            while i < lines.len() && lines[i].trim().starts_with("//!") {
                let line = lines[i].trim();
                let content = line
                    .strip_prefix("//! ")
                    .or_else(|| line.strip_prefix("//!"))
                    .unwrap_or("");
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(content);
                i += 1;
            }
            if !text.trim().is_empty() {
                blocks.push(DocBlock {
                    kind: "module",
                    text,
                    item_sig: None,
                    line_start: start + 1,
                });
            }
            continue;
        }

        // /// item doc comment
        if trimmed.starts_with("///") && !trimmed.starts_with("////") {
            let start = i;
            let mut text = String::new();
            while i < lines.len() {
                let t = lines[i].trim();
                if t.starts_with("///") && !t.starts_with("////") {
                    let content = t
                        .strip_prefix("/// ")
                        .or_else(|| t.strip_prefix("///"))
                        .unwrap_or("");
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(content);
                    i += 1;
                } else {
                    break;
                }
            }

            // Skip attribute lines (#[...]) to find the item signature
            let mut item_sig = None;
            let mut j = i;
            while j < lines.len() {
                let t = lines[j].trim();
                if t.starts_with("#[") || t.is_empty() {
                    j += 1;
                } else {
                    // Capture the signature line (pub fn ..., pub struct ..., etc.)
                    let sig = t.to_string();
                    // Trim to just the signature (up to the opening brace or end)
                    let sig_clean = sig.split('{').next().unwrap_or(&sig).trim().to_string();
                    item_sig = Some(sig_clean);
                    break;
                }
            }

            if !text.trim().is_empty() {
                blocks.push(DocBlock {
                    kind: "item",
                    text,
                    item_sig,
                    line_start: start + 1,
                });
            }
            continue;
        }

        i += 1;
    }

    blocks
}

/// Derive a crate name from a file path relative to workspace root.
/// e.g. "src/crates/cosmix-maild/src/smtp/mod.rs" → "cosmix-maild"
fn crate_name_from_path(rel_path: &str) -> Option<String> {
    // Pattern: src/crates/<crate-name>/...
    let parts: Vec<&str> = rel_path.split('/').collect();
    if parts.len() >= 3 && parts[0] == "src" && parts[1] == "crates" {
        Some(parts[2].to_string())
    } else {
        None
    }
}

/// Index Rust doc comments and Mix scripts from a workspace into indexd.
async fn cmd_index_source(cli: &ResolvedConfig, path: &str, filter: Option<&str>) -> Result<()> {
    use std::process::Command as Cmd;

    let expanded = if let Some(rest) = path.strip_prefix("~/") {
        format!("{}/{}", std::env::var("HOME").unwrap_or_default(), rest)
    } else if path == "~" {
        std::env::var("HOME").unwrap_or_default()
    } else if path == "." {
        std::env::current_dir()?.to_string_lossy().to_string()
    } else {
        path.to_string()
    };
    let workspace = std::path::Path::new(&expanded);

    let mut indexd = IndexdClient::connect(Some(&cli.socket)).await?;

    // ── Collect .rs files via git ──
    let output = Cmd::new("git")
        .arg("-C")
        .arg(workspace)
        .args(["ls-files", "--", "*.rs"])
        .output()
        .context("running git ls-files for .rs files")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut rs_files: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();

    // Apply filter if given (for incremental indexing)
    if let Some(f) = filter {
        rs_files.retain(|p| p.contains(f));
    }

    println!("## Source Index Report\n");
    println!("Scanning {} .rs files...\n", rs_files.len());

    let mut total_blocks = 0usize;
    let mut module_blocks = 0usize;
    let mut item_blocks = 0usize;
    let mut indexed_files = 0usize;
    let domain = cli.domain.as_deref().unwrap_or("cosmix");

    for rel_path in &rs_files {
        let abs_path = workspace.join(rel_path);
        let source = match std::fs::read_to_string(&abs_path) {
            Ok(s) => s,
            Err(_) => continue,
        };

        let blocks = extract_doc_blocks(&source);
        if blocks.is_empty() {
            continue;
        }

        let crate_name = crate_name_from_path(rel_path).unwrap_or_else(|| "unknown".to_string());

        indexed_files += 1;

        for block in &blocks {
            // Build the chunk content as readable markdown
            let content = if block.kind == "module" {
                format!(
                    "# Module: {}\n\nFile: `{}`\nCrate: `{}`\n\n{}\n",
                    rel_path, rel_path, crate_name, block.text
                )
            } else {
                let sig = block.item_sig.as_deref().unwrap_or("(unknown item)");
                format!(
                    "# {}\n\nFile: `{}:{}`\nCrate: `{}`\n\n{}\n\n```rust\n{}\n```\n",
                    sig, rel_path, block.line_start, crate_name, block.text, sig
                )
            };

            // Metadata for retrieval filtering
            let metadata = serde_json::json!({
                "file": rel_path,
                "crate": crate_name,
                "kind": block.kind,
                "line": block.line_start,
                "item_sig": block.item_sig,
                "domain": domain,
            });

            let req = serde_json::json!({
                "action": "store",
                "texts": [content],
                "source": "rust-doc",
                "metadata": [metadata.to_string()],
                "paths": [rel_path],
            });

            match indexd.raw_request(&req).await {
                Ok(_) => {
                    total_blocks += 1;
                    match block.kind {
                        "module" => module_blocks += 1,
                        _ => item_blocks += 1,
                    }
                }
                Err(e) => {
                    eprintln!("  WARN: failed to index block from {}: {e}", rel_path);
                }
            }
        }
    }

    // ── Mix scripts ──
    let mix_output = Cmd::new("git")
        .arg("-C")
        .arg(workspace)
        .args(["ls-files", "--", "*.mix", "*/_mixrc"])
        .output()
        .unwrap_or_else(|_| std::process::Output {
            status: std::process::ExitStatus::default(),
            stdout: Vec::new(),
            stderr: Vec::new(),
        });
    let mix_stdout = String::from_utf8_lossy(&mix_output.stdout);
    let mut mix_files: Vec<&str> = mix_stdout.lines().filter(|l| !l.is_empty()).collect();
    if let Some(f) = filter {
        mix_files.retain(|p| p.contains(f));
    }

    let mut mix_indexed = 0usize;
    for rel_path in &mix_files {
        let abs_path = workspace.join(rel_path);
        let content = match std::fs::read_to_string(&abs_path) {
            Ok(s) => s,
            Err(_) => continue,
        };

        // Index the entire Mix script as one chunk (they're typically short)
        let chunk_content = format!(
            "# Mix Script: {}\n\nFile: `{}`\n\n```mix\n{}\n```\n",
            rel_path, rel_path, content
        );

        let metadata = serde_json::json!({
            "file": rel_path,
            "kind": "mix-script",
            "domain": domain,
        });

        let req = serde_json::json!({
            "action": "store",
            "texts": [chunk_content],
            "source": "mix-script",
            "metadata": [metadata.to_string()],
            "paths": [rel_path],
        });

        match indexd.raw_request(&req).await {
            Ok(_) => mix_indexed += 1,
            Err(e) => eprintln!("  WARN: failed to index Mix script {}: {e}", rel_path),
        }
    }

    println!("### Rust Doc Comments\n");
    println!("- Files with doc comments: {indexed_files}");
    println!("- Module blocks (//!): {module_blocks}");
    println!("- Item blocks (///): {item_blocks}");
    println!("- Total chunks stored: {total_blocks}");
    println!();

    if !mix_files.is_empty() || mix_indexed > 0 {
        println!("### Mix Scripts\n");
        println!("- Scripts found: {}", mix_files.len());
        println!("- Indexed: {mix_indexed}");
        println!();
    }

    println!("### Summary\n");
    println!("- Total source chunks: {}", total_blocks + mix_indexed);

    Ok(())
}
