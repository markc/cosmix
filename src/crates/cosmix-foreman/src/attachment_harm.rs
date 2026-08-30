//! Measure whole-file attachment churn across Claude transcript compactions.
//!
//! This is an investigative, read-only surface. JSONL records are processed
//! one at a time, attachment content is measured and immediately discarded,
//! and reports contain paths, sizes and record positions only.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::agent_sessions::{
    SessionPopulation, classify_population, collect_jsonl, load_ledger_joins, project_slug,
    session_id, task_from_cwd, task_from_slug,
};

mod model;
mod output;
mod report;
#[cfg(test)]
mod tests;

pub use model::*;
pub use output::render_text;
use report::{aggregate_session, population_report};

pub(super) const MAX_JSONL_RECORD_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Default)]
struct PopulationAggregate {
    sessions: usize,
    whole: usize,
    slices: usize,
    unknown_extent: usize,
    unknown_content_size: usize,
    compactions: usize,
    files: BTreeMap<String, FileAggregate>,
}

#[derive(Debug, Default)]
struct FileAggregate {
    whole: usize,
    repeats: usize,
    repeat_sessions: usize,
    post_compaction: usize,
    post_compaction_sessions: usize,
    record_sizes: Vec<usize>,
    record_char_counts: Vec<usize>,
    content_sizes: Vec<usize>,
    unknown_content_size: usize,
    affected: Vec<AffectedSession>,
}

#[derive(Debug)]
struct SessionScan {
    id: String,
    slug: String,
    cwd: Option<String>,
    population: SessionPopulation,
    task_hint: Option<i64>,
    first_timestamp: Option<DateTime<Utc>>,
    last_timestamp: Option<DateTime<Utc>>,
    complete: bool,
    malformed_records: usize,
    oversized_records: usize,
    slices: usize,
    unknown_extent: usize,
    compaction_lines: Vec<usize>,
    compact_boundary_lines: Vec<usize>,
    compact_summary_lines: Vec<usize>,
    compact_epoch: usize,
    boundary_awaiting_summary: bool,
    files: BTreeMap<String, SessionFile>,
    transcript_outcome: Option<String>,
}

impl SessionScan {
    fn new(path: &Path) -> Option<Self> {
        let id = session_id(path)?;
        let slug = project_slug(path)?;
        let population = classify_population(&slug, None);
        let task_hint = task_from_slug(&slug);
        Some(Self {
            id,
            slug,
            cwd: None,
            population,
            task_hint,
            first_timestamp: None,
            last_timestamp: None,
            complete: true,
            malformed_records: 0,
            oversized_records: 0,
            slices: 0,
            unknown_extent: 0,
            compaction_lines: Vec::new(),
            compact_boundary_lines: Vec::new(),
            compact_summary_lines: Vec::new(),
            compact_epoch: 0,
            boundary_awaiting_summary: false,
            files: BTreeMap::new(),
            transcript_outcome: None,
        })
    }

    fn observe_value(
        &mut self,
        value: &Value,
        line: usize,
        record_bytes: usize,
        record_characters: usize,
    ) {
        if let Some(cwd) = value.get("cwd").and_then(Value::as_str) {
            self.cwd.get_or_insert_with(|| cwd.to_owned());
            self.population = classify_population(&self.slug, self.cwd.as_deref());
            self.task_hint = self.task_hint.or_else(|| task_from_cwd(cwd));
        }
        if let Some(timestamp) = value
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_timestamp)
        {
            self.first_timestamp = Some(
                self.first_timestamp
                    .map_or(timestamp, |current| current.min(timestamp)),
            );
            self.last_timestamp = Some(
                self.last_timestamp
                    .map_or(timestamp, |current| current.max(timestamp)),
            );
        }

        if is_compact_boundary(value) {
            self.compact_epoch += 1;
            self.compaction_lines.push(line);
            self.compact_boundary_lines.push(line);
            self.boundary_awaiting_summary = true;
            return;
        }
        if is_compact_summary(value) {
            self.compact_summary_lines.push(line);
            if self.boundary_awaiting_summary {
                self.boundary_awaiting_summary = false;
            } else {
                self.compact_epoch += 1;
                self.compaction_lines.push(line);
            }
            return;
        }

        self.observe_outcome(value);
        let Some(attachment) = attachment(value) else {
            return;
        };
        self.boundary_awaiting_summary = false;
        match attachment.extent {
            AttachmentExtent::Slice => self.slices += 1,
            AttachmentExtent::Unknown => self.unknown_extent += 1,
            AttachmentExtent::Whole => {
                let file = self.files.entry(attachment.path).or_default();
                file.record_sizes.push(record_bytes);
                file.record_char_counts.push(record_characters);
                file.content_sizes.push(attachment.content_bytes);
                file.whole_lines.push(line);
                if !file.seen_epochs.is_empty() {
                    file.repeats += 1;
                }
                let seen_in_earlier_epoch = file
                    .seen_epochs
                    .first()
                    .is_some_and(|epoch| *epoch < self.compact_epoch);
                if seen_in_earlier_epoch
                    && file.last_post_compaction_epoch != Some(self.compact_epoch)
                {
                    file.post_compaction_lines.push(line);
                    file.last_post_compaction_epoch = Some(self.compact_epoch);
                }
                file.seen_epochs.insert(self.compact_epoch);
            }
        }
    }

    fn observe_outcome(&mut self, value: &Value) {
        match value.get("type").and_then(Value::as_str) {
            Some("result") => {
                let subtype = value
                    .get("subtype")
                    .and_then(Value::as_str)
                    .unwrap_or("result");
                let prefix = if value.get("is_error").and_then(Value::as_bool) == Some(true) {
                    "error"
                } else {
                    "result"
                };
                self.transcript_outcome = Some(format!("{prefix}:{subtype}"));
            }
            Some("assistant")
                if value.get("isApiErrorMessage").and_then(Value::as_bool) == Some(true) =>
            {
                self.transcript_outcome = Some("error:api".into());
            }
            _ => {}
        }
    }
}

#[derive(Debug, Default)]
struct SessionFile {
    record_sizes: Vec<usize>,
    record_char_counts: Vec<usize>,
    content_sizes: Vec<Option<usize>>,
    whole_lines: Vec<usize>,
    repeats: usize,
    seen_epochs: BTreeSet<usize>,
    last_post_compaction_epoch: Option<usize>,
    post_compaction_lines: Vec<usize>,
}

struct ParsedAttachment {
    path: String,
    content_bytes: Option<usize>,
    extent: AttachmentExtent,
}

#[derive(Clone, Copy)]
enum AttachmentExtent {
    Whole,
    Slice,
    Unknown,
}

pub fn analyse(options: &AnalysisOptions) -> Result<AnalysisReport> {
    anyhow::ensure!(
        options.limit > 0,
        "attachment harm limit must be at least one"
    );
    let paths = collect_jsonl(&options.claude_projects).with_context(|| {
        format!(
            "inventorying Claude transcripts below {}",
            options.claude_projects.display()
        )
    })?;
    let ledger = options
        .ledger
        .as_deref()
        .map(load_ledger_joins)
        .unwrap_or_default();
    let mut notes = ledger.notes.clone();
    if options.ledger.is_none() {
        notes.push("foreman ledger not selected; task/run outcomes are unavailable".into());
    } else if !ledger.complete {
        notes.push(
            "foreman ledger join was unavailable or incomplete; unmatched sessions remain unknown"
                .into(),
        );
    }

    let mut populations = BTreeMap::<SessionPopulation, PopulationAggregate>::new();
    let mut observation_start = None;
    let mut observation_end = None;
    let mut sessions_complete = 0usize;
    let mut malformed_records = 0usize;
    let mut oversized_records = 0usize;

    for path in &paths {
        let scan = match scan_session(path) {
            Ok(Some(scan)) => scan,
            Ok(None) => {
                notes.push(format!(
                    "transcript identity unavailable for {}; session skipped",
                    path.display()
                ));
                continue;
            }
            Err(error) => {
                notes.push(format!(
                    "transcript unavailable at {}: {error:#}",
                    path.display()
                ));
                continue;
            }
        };
        if scan.complete {
            sessions_complete += 1;
        }
        malformed_records += scan.malformed_records;
        oversized_records += scan.oversized_records;
        if let Some(first) = scan.first_timestamp {
            observation_start =
                Some(observation_start.map_or(first, |current: DateTime<Utc>| current.min(first)));
        }
        if let Some(last) = scan.last_timestamp {
            observation_end =
                Some(observation_end.map_or(last, |current: DateTime<Utc>| current.max(last)));
        }
        let joined_runs = (scan.population == SessionPopulation::Foreman)
            .then(|| ledger.by_session.get(&scan.id))
            .flatten();
        aggregate_session(&mut populations, scan, joined_runs);
    }

    let population_reports = [SessionPopulation::Foreman, SessionPopulation::Operator]
        .into_iter()
        .map(|population| {
            population_report(
                population,
                populations.remove(&population).unwrap_or_default(),
                options.limit,
            )
        })
        .collect();

    notes.push(
        "existing-file splitting is driven by post-compaction reattachment evidence; the ~600-line guidance applies only to cheap-at-creation new files and is not a split threshold"
            .into(),
    );
    notes.push(
        "absence is observational only: a missing path may be unread, sliced, or untouched during this window"
            .into(),
    );

    Ok(AnalysisReport {
        read_only: true,
        ranking_key: "post_compaction_attachments_desc",
        transcript_root: options.claude_projects.display().to_string(),
        observation_start: observation_start.map(|time| time.to_rfc3339()),
        observation_end: observation_end.map(|time| time.to_rfc3339()),
        sessions_considered: paths.len(),
        sessions_complete,
        malformed_records,
        oversized_records,
        populations: population_reports,
        notes,
    })
}

fn scan_session(path: &Path) -> Result<Option<SessionScan>> {
    let Some(mut scan) = SessionScan::new(path) else {
        return Ok(None);
    };
    let file = File::open(path)
        .with_context(|| format!("opening transcript for streaming scan: {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut line = 0usize;
    loop {
        let (record, bytes, eof) = read_bounded_record(&mut reader, MAX_JSONL_RECORD_BYTES)
            .with_context(|| format!("streaming transcript: {}", path.display()))?;
        if bytes == 0 {
            break;
        }
        line += 1;
        let Some(record) = record else {
            scan.complete = false;
            scan.oversized_records += 1;
            if eof {
                break;
            }
            continue;
        };
        let record_bytes = record.len();
        match serde_json::from_slice::<Value>(&record) {
            Ok(value) => {
                let record_characters = std::str::from_utf8(&record)
                    .expect("serde_json accepted valid UTF-8")
                    .chars()
                    .count();
                scan.observe_value(&value, line, record_bytes, record_characters);
            }
            Err(_) => {
                scan.complete = false;
                scan.malformed_records += 1;
            }
        }
        if eof {
            break;
        }
    }
    Ok(Some(scan))
}

/// Read and drain one JSONL record while retaining at most `cap` content bytes.
/// The returned byte count includes any terminator and is used only for EOF.
fn read_bounded_record(
    reader: &mut impl BufRead,
    cap: usize,
) -> std::io::Result<(Option<Vec<u8>>, usize, bool)> {
    let mut kept = Vec::new();
    let mut content_total = 0usize;
    let mut bytes_read = 0usize;
    let mut eof = false;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            eof = true;
            break;
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |index| index + 1);
        let content = newline.unwrap_or(consumed);
        content_total = content_total.saturating_add(content);
        bytes_read = bytes_read.saturating_add(consumed);
        if kept.len() < cap {
            let remaining = cap - kept.len();
            kept.extend_from_slice(&available[..content.min(remaining)]);
        }
        reader.consume(consumed);
        if newline.is_some() {
            break;
        }
    }
    if kept.last() == Some(&b'\r') {
        kept.pop();
        content_total = content_total.saturating_sub(1);
    }
    Ok(((content_total <= cap).then_some(kept), bytes_read, eof))
}

fn is_compact_boundary(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("system")
        && value.get("subtype").and_then(Value::as_str) == Some("compact_boundary")
}

fn is_compact_summary(value: &Value) -> bool {
    value.get("isCompactSummary").and_then(Value::as_bool) == Some(true)
}

fn attachment(value: &Value) -> Option<ParsedAttachment> {
    if value.get("type").and_then(Value::as_str) != Some("attachment") {
        return None;
    }
    let attachment = value.get("attachment")?;
    if attachment.get("type").and_then(Value::as_str) != Some("file") {
        return None;
    }
    let file = attachment.get("content")?.get("file")?;
    let path = file
        .get("filePath")
        .and_then(Value::as_str)
        .or_else(|| attachment.get("filename").and_then(Value::as_str))?
        .to_owned();
    let content_bytes = file.get("content").and_then(Value::as_str).map(str::len);
    let extent = match (
        file.get("startLine").and_then(Value::as_u64),
        file.get("numLines").and_then(Value::as_u64),
        file.get("totalLines").and_then(Value::as_u64),
    ) {
        (Some(1), Some(lines), Some(total)) if lines == total => AttachmentExtent::Whole,
        (Some(_), Some(_), Some(_)) => AttachmentExtent::Slice,
        _ => AttachmentExtent::Unknown,
    };
    Some(ParsedAttachment {
        path,
        content_bytes,
        extent,
    })
}

fn parse_timestamp(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|time| time.with_timezone(&Utc))
}
