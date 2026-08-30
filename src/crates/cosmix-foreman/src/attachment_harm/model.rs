use std::path::PathBuf;

use serde::Serialize;

use crate::agent_sessions::{RunJoin, SessionPopulation};

#[derive(Debug, Clone)]
pub struct AnalysisOptions {
    pub claude_projects: PathBuf,
    pub ledger: Option<PathBuf>,
    pub limit: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct AnalysisReport {
    pub read_only: bool,
    pub ranking_key: &'static str,
    pub transcript_root: String,
    pub observation_start: Option<String>,
    pub observation_end: Option<String>,
    pub sessions_considered: usize,
    pub sessions_complete: usize,
    pub malformed_records: usize,
    pub oversized_records: usize,
    pub populations: Vec<PopulationReport>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PopulationReport {
    pub population: SessionPopulation,
    pub sessions_considered: usize,
    pub whole_file_attachments: usize,
    pub sliced_file_attachments_excluded: usize,
    pub file_attachments_with_unknown_extent_excluded: usize,
    pub whole_file_attachments_with_unknown_content_size: usize,
    pub compactions: usize,
    pub worklist: Vec<FileHarm>,
    pub existing_task_checks: Vec<ExistingTaskCheck>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExistingTaskCheck {
    pub task_id: i64,
    pub path: String,
    pub observed: bool,
    pub rank: Option<usize>,
    pub whole_file_attachments: usize,
    pub post_compaction_attachments: usize,
    pub sessions_with_post_compaction_attachment: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileHarm {
    pub rank: usize,
    pub path: String,
    pub covered_by_existing_task: Option<i64>,
    pub whole_file_attachments: usize,
    /// Whole attachments beyond the first occurrence in each session.
    pub repeat_whole_file_attachments: usize,
    pub sessions_with_repeat_attachment: usize,
    /// At most one attachment per file per compaction cycle, and only when the
    /// same file was attached in an earlier compaction epoch.
    pub post_compaction_attachments: usize,
    pub sessions_with_post_compaction_attachment: usize,
    /// Upper-middle value for an even population.
    pub median_jsonl_record_characters: usize,
    pub max_jsonl_record_characters: usize,
    pub median_jsonl_record_bytes: usize,
    pub max_jsonl_record_bytes: usize,
    /// Decoded UTF-8 bytes in `attachment.content.file.content`.
    pub median_file_content_bytes: usize,
    pub max_file_content_bytes: usize,
    pub attachments_with_unknown_content_size: usize,
    pub affected_sessions: Vec<AffectedSession>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AffectedSession {
    pub session_ref: String,
    pub project_slug: String,
    pub task_ids: Vec<i64>,
    pub runs: Vec<RunJoin>,
    pub outcome: Option<String>,
    pub whole_attachment_lines: Vec<usize>,
    pub jsonl_record_characters: Vec<usize>,
    pub jsonl_record_bytes: Vec<usize>,
    pub file_content_bytes: Vec<Option<usize>>,
    pub post_compaction_attachment_lines: Vec<usize>,
    pub compaction_lines: Vec<usize>,
    pub compact_boundary_lines: Vec<usize>,
    pub compact_summary_lines: Vec<usize>,
}
