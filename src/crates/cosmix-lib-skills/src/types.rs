use serde::{Deserialize, Serialize};

/// A learned, reusable skill extracted from a successful task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillDocument {
    pub name: String,
    pub version: u32,
    /// Project domain derived from CLAUDE.md location (e.g. "cosmix", "ns", "gh/wg-admin").
    #[serde(default)]
    pub domain: String,
    /// When this skill should be applied (natural language trigger condition).
    pub trigger: String,
    /// Step-by-step approach for executing this skill.
    pub approach: String,
    pub tools_required: Vec<String>,
    pub failure_modes: Vec<String>,
    /// Confidence score 0.0–1.0, updated on each use.
    pub confidence: f32,
    pub use_count: u32,
    pub success_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used: Option<String>,
    pub created: String,
    pub updated: String,
    /// Whether this skill has been graduated to CLAUDE.md as a permanent rule.
    #[serde(default)]
    pub graduated: bool,
    /// If set, this skill version has been superseded by the chunk with this ID
    /// (supersede-don't-delete semantics — history preserved for rollback/audit).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<i64>,
}

impl SkillDocument {
    /// Render as readable markdown for embedding (the content field in indexd).
    pub fn to_markdown(&self) -> String {
        let mut md = format!("# {}\n\n", self.name);
        md.push_str(&format!("**Trigger:** {}\n\n", self.trigger));
        md.push_str(&format!("## Approach\n\n{}\n\n", self.approach));
        if !self.tools_required.is_empty() {
            md.push_str("## Tools Required\n\n");
            for tool in &self.tools_required {
                md.push_str(&format!("- {tool}\n"));
            }
            md.push('\n');
        }
        if !self.failure_modes.is_empty() {
            md.push_str("## Known Failure Modes\n\n");
            for mode in &self.failure_modes {
                md.push_str(&format!("- {mode}\n"));
            }
            md.push('\n');
        }
        md.push_str(&format!(
            "Confidence: {:.0}% | Used: {} times ({} successes)\n",
            self.confidence * 100.0,
            self.use_count,
            self.success_count,
        ));
        md
    }
}

/// A captured record of a completed agent task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskTranscript {
    pub task_description: String,
    pub system_prompt: String,
    pub messages: Vec<Message>,
    pub tool_calls: Vec<ToolCall>,
    pub final_output: String,
    pub duration_ms: u64,
    pub token_count: u32,
    pub success: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub name: String,
    pub input: String,
    pub output: String,
}

/// Evaluation result from the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalScore {
    /// 1–5: was the task completed successfully?
    pub success: u8,
    /// 1–5: was the approach non-trivial / worth learning?
    pub novelty: u8,
    pub reasoning: String,
}

impl EvalScore {
    /// Worth extracting a skill if both scores are >= 3.
    pub fn worth_extracting(&self) -> bool {
        self.success >= 3 && self.novelty >= 3
    }
}

/// Outcome reported after using a skill during a task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskOutcome {
    pub skill_id: i64,
    pub success: bool,
    pub notes: String,
    pub duration_ms: u64,
}
