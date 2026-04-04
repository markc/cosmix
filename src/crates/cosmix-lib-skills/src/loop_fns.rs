use anyhow::{Context, Result};
use chrono::Utc;
use tracing::info;

use crate::indexd_client::IndexdClient;
use crate::llm_client::LlmClient;
use crate::prompts::{self, fill_template};
use crate::types::{EvalScore, SkillDocument, TaskOutcome, TaskTranscript};

/// Evaluate whether a completed task is worth learning from.
/// Returns None if trivial, Some(score) if worth extracting.
pub async fn evaluate_task(
    llm: &LlmClient,
    transcript: &TaskTranscript,
) -> Result<Option<EvalScore>> {
    let tools_used: Vec<&str> = transcript.tool_calls.iter().map(|t| t.name.as_str()).collect();
    let tools_str = tools_used.join(", ");

    let final_output = truncate(&transcript.final_output, 2000);

    let user_prompt = fill_template(
        prompts::EVAL_USER_TEMPLATE,
        &[
            ("task_description", &transcript.task_description),
            ("final_output", &final_output),
            ("tools_used", &tools_str),
            ("duration_ms", &transcript.duration_ms.to_string()),
            ("token_count", &transcript.token_count.to_string()),
        ],
    );

    let response = llm
        .complete(prompts::EVAL_SYSTEM, &user_prompt)
        .await
        .context("LLM evaluation call")?;

    let score: EvalScore = parse_json_response(&response).context("parsing eval score")?;

    info!(
        success = score.success,
        novelty = score.novelty,
        "task evaluation complete"
    );

    if score.worth_extracting() {
        Ok(Some(score))
    } else {
        Ok(None)
    }
}

/// Extract a new skill from a successful, non-trivial task.
pub async fn extract_skill(
    llm: &LlmClient,
    transcript: &TaskTranscript,
    _eval: &EvalScore,
) -> Result<SkillDocument> {
    let tool_calls_str = transcript
        .tool_calls
        .iter()
        .enumerate()
        .map(|(i, tc)| format!("{}. {}({})", i + 1, tc.name, truncate(&tc.input, 200)))
        .collect::<Vec<_>>()
        .join("\n");

    let final_output = truncate(&transcript.final_output, 2000);

    let user_prompt = fill_template(
        prompts::EXTRACT_USER_TEMPLATE,
        &[
            ("task_description", &transcript.task_description),
            ("tool_calls", &tool_calls_str),
            ("final_output", &final_output),
        ],
    );

    let response = llm
        .complete(prompts::EXTRACT_SYSTEM, &user_prompt)
        .await
        .context("LLM extraction call")?;

    let raw: ExtractedSkillRaw = parse_json_response(&response).context("parsing extracted skill")?;

    let now = Utc::now().format("%Y-%m-%d").to_string();
    let skill = SkillDocument {
        name: raw.name,
        version: 1,
        domain: crate::detect_domain_cwd(),
        trigger: raw.trigger,
        approach: raw.approach,
        tools_required: raw.tools_required,
        failure_modes: raw.failure_modes,
        confidence: 0.5,
        use_count: 1,
        success_count: 1,
        last_used: Some(now.clone()),
        created: now.clone(),
        updated: now,
        graduated: false,
        superseded_by: None,
    };

    info!(name = %skill.name, "skill extracted");
    Ok(skill)
}

/// Retrieve relevant skills from indexd for a given task description.
/// Automatically filters to the current project domain (detected from $PWD).
/// Uses min_confidence from config if not overridden.
pub async fn retrieve_skills(
    indexd: &mut IndexdClient,
    task_description: &str,
    max_skills: usize,
) -> Result<Vec<(i64, SkillDocument)>> {
    let domain = crate::detect_domain_cwd();
    retrieve_skills_domain(indexd, task_description, max_skills, Some(&domain)).await
}

/// Retrieve skills with explicit domain filter (None = all domains).
pub async fn retrieve_skills_domain(
    indexd: &mut IndexdClient,
    task_description: &str,
    max_skills: usize,
    domain: Option<&str>,
) -> Result<Vec<(i64, SkillDocument)>> {
    let min_confidence = cosmix_config::store::load()
        .map(|c| c.skills.min_confidence)
        .unwrap_or(0.3) as f32;
    let results = indexd.search_skills_domain(task_description, max_skills, domain).await?;
    Ok(results
        .into_iter()
        .filter(|(_, doc, _)| doc.confidence >= min_confidence)
        .map(|(id, doc, _distance)| (id, doc))
        .collect())
}

/// Format retrieved skills as a system prompt section.
pub fn format_skills_for_prompt(skills: &[(i64, SkillDocument)]) -> String {
    if skills.is_empty() {
        return String::new();
    }

    let mut section = String::from("\n## Relevant Skills from Prior Experience\n\n");
    for (i, (_id, skill)) in skills.iter().enumerate() {
        section.push_str(&format!(
            "### Skill {}: {} (confidence: {:.0}%)\n",
            i + 1,
            skill.name,
            skill.confidence * 100.0,
        ));
        section.push_str(&format!("**When:** {}\n", skill.trigger));
        section.push_str(&format!("**Approach:**\n{}\n", skill.approach));
        if !skill.failure_modes.is_empty() {
            section.push_str("**Watch out for:**\n");
            for mode in &skill.failure_modes {
                section.push_str(&format!("- {mode}\n"));
            }
        }
        section.push('\n');
    }
    section
}

/// After using a skill, report the outcome to refine it.
/// Uses supersede-don't-delete: creates a new chunk for the refined skill and marks
/// the old chunk as superseded. Returns (new_chunk_id, refined_skill).
pub async fn refine_skill(
    llm: &LlmClient,
    indexd: &mut IndexdClient,
    skill_id: i64,
    existing: &SkillDocument,
    outcome: &TaskOutcome,
) -> Result<(i64, SkillDocument)> {
    let existing_json = serde_json::to_string_pretty(existing)?;

    let user_prompt = fill_template(
        prompts::REFINE_USER_TEMPLATE,
        &[
            ("existing_skill", &existing_json),
            ("outcome", &outcome.notes),
            ("success", &outcome.success.to_string()),
            ("notes", &outcome.notes),
        ],
    );

    let response = llm
        .complete(prompts::REFINE_SYSTEM, &user_prompt)
        .await
        .context("LLM refinement call")?;

    let raw: RefinedSkillRaw = parse_json_response(&response).context("parsing refined skill")?;

    let now = Utc::now().format("%Y-%m-%d").to_string();
    let updated = SkillDocument {
        name: raw.name.unwrap_or_else(|| existing.name.clone()),
        version: existing.version + 1,
        domain: existing.domain.clone(),
        trigger: raw.trigger.unwrap_or_else(|| existing.trigger.clone()),
        approach: raw.approach.unwrap_or_else(|| existing.approach.clone()),
        tools_required: raw
            .tools_required
            .unwrap_or_else(|| existing.tools_required.clone()),
        failure_modes: raw
            .failure_modes
            .unwrap_or_else(|| existing.failure_modes.clone()),
        confidence: raw.confidence.unwrap_or(existing.confidence),
        use_count: existing.use_count + 1,
        success_count: existing.success_count + if outcome.success { 1 } else { 0 },
        last_used: Some(now.clone()),
        created: existing.created.clone(),
        updated: now,
        graduated: existing.graduated,
        superseded_by: None,
    };

    let new_id = indexd.supersede_skill(skill_id, existing, &updated).await?;

    info!(
        name = %updated.name,
        version = updated.version,
        confidence = updated.confidence,
        old_id = skill_id,
        new_id = new_id,
        "skill refined (superseded)"
    );

    Ok((new_id, updated))
}

/// Check if a skill qualifies for graduation to CLAUDE.md.
/// Returns Ok(true) if the skill was graduated, Ok(false) if not eligible.
pub async fn check_graduation(
    indexd: &mut IndexdClient,
    skill_id: i64,
    skill: &SkillDocument,
) -> Result<bool> {
    if skill.graduated {
        return Ok(false);
    }

    let config = cosmix_config::store::load().unwrap_or_default();
    let threshold_conf = config.skills.graduation_confidence as f32;
    let threshold_uses = config.skills.graduation_min_uses;
    let threshold_successes = config.skills.graduation_min_successes;

    if skill.confidence < threshold_conf
        || skill.use_count < threshold_uses
        || skill.success_count < threshold_successes
    {
        return Ok(false);
    }

    // Find CLAUDE.md for the skill's domain
    let claude_md_path = find_claude_md_for_domain(&skill.domain)?;

    // Format the skill as a CLAUDE.md rule
    let rule = format_graduation_rule(skill);

    // Read current CLAUDE.md and append to Graduated Skills section
    let content = std::fs::read_to_string(&claude_md_path)
        .with_context(|| format!("reading {}", claude_md_path.display()))?;

    let section_marker = "## Graduated Skills (auto-generated)";
    let updated = if content.contains(section_marker) {
        // Append under existing section
        content.replacen(section_marker, &format!("{section_marker}\n\n{rule}"), 1)
    } else {
        // Create section at end of file
        format!("{content}\n\n{section_marker}\n\n{rule}")
    };

    std::fs::write(&claude_md_path, updated)
        .with_context(|| format!("writing {}", claude_md_path.display()))?;

    // Mark skill as graduated in indexd
    let mut graduated_skill = skill.clone();
    graduated_skill.graduated = true;
    indexd.update_skill(skill_id, &graduated_skill).await?;

    info!(
        name = %skill.name,
        confidence = skill.confidence,
        uses = skill.use_count,
        "skill graduated to CLAUDE.md"
    );

    Ok(true)
}

fn format_graduation_rule(skill: &SkillDocument) -> String {
    let mut rule = format!("### {}\n\n", skill.name);
    rule.push_str(&format!("**When:** {}\n\n", skill.trigger));
    rule.push_str(&format!("**Approach:** {}\n", skill.approach));
    if !skill.failure_modes.is_empty() {
        rule.push_str("\n**Watch out for:**\n");
        for mode in &skill.failure_modes {
            rule.push_str(&format!("- {mode}\n"));
        }
    }
    rule.push_str(&format!(
        "\n_Graduated from skill learning loop — confidence {:.0}%, {} uses, {} successes._\n",
        skill.confidence * 100.0,
        skill.use_count,
        skill.success_count,
    ));
    rule
}

fn find_claude_md_for_domain(domain: &str) -> Result<std::path::PathBuf> {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home".into());
    // Domain maps back to directory: "cosmix" -> ~/.cosmix, "ns" -> ~/.ns, "gh/foo" -> ~/.gh/foo
    let dir = if domain == "general" {
        std::path::PathBuf::from(&home)
    } else {
        let segments: Vec<&str> = domain.split('/').collect();
        let mut path = std::path::PathBuf::from(&home);
        for (i, seg) in segments.iter().enumerate() {
            if i == 0 {
                path.push(format!(".{seg}"));
            } else {
                path.push(seg);
            }
        }
        path
    };

    let claude_md = dir.join("CLAUDE.md");
    if claude_md.exists() {
        Ok(claude_md)
    } else {
        anyhow::bail!("CLAUDE.md not found for domain '{}' at {}", domain, claude_md.display())
    }
}

// --- Helpers ---

/// Parse JSON from LLM response, stripping markdown fences if present.
fn parse_json_response<T: serde::de::DeserializeOwned>(response: &str) -> Result<T> {
    let trimmed = response.trim();

    // Strip markdown code fences if the LLM wrapped the JSON
    let json_str = if trimmed.starts_with("```") {
        let inner = trimmed
            .strip_prefix("```json")
            .or_else(|| trimmed.strip_prefix("```"))
            .unwrap_or(trimmed);
        inner
            .strip_suffix("```")
            .unwrap_or(inner)
            .trim()
    } else {
        trimmed
    };

    serde_json::from_str(json_str).with_context(|| {
        format!(
            "failed to parse JSON from LLM response: {}",
            &json_str[..json_str.len().min(200)]
        )
    })
}

fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...[truncated]", &s[..max_len])
    }
}

/// Raw shape returned by the extraction prompt.
#[derive(serde::Deserialize)]
struct ExtractedSkillRaw {
    name: String,
    trigger: String,
    approach: String,
    tools_required: Vec<String>,
    failure_modes: Vec<String>,
}

/// Raw shape returned by the refinement prompt (all fields optional for partial updates).
#[derive(serde::Deserialize)]
struct RefinedSkillRaw {
    name: Option<String>,
    trigger: Option<String>,
    approach: Option<String>,
    tools_required: Option<Vec<String>>,
    failure_modes: Option<Vec<String>>,
    confidence: Option<f32>,
}
