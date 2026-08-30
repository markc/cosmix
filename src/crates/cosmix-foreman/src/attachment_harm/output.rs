use std::fmt::Write as _;

use super::{AnalysisReport, MAX_JSONL_RECORD_BYTES};

pub fn render_text(report: &AnalysisReport) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "attachment harm: {} sessions ({} complete), {} to {}",
        report.sessions_considered,
        report.sessions_complete,
        report.observation_start.as_deref().unwrap_or("unknown"),
        report.observation_end.as_deref().unwrap_or("unknown")
    );
    let _ = writeln!(
        out,
        "ranking: post-compaction whole-file attachments; size is context only"
    );
    let _ = writeln!(
        out,
        "record gaps: {} malformed, {} over {} bytes",
        report.malformed_records, report.oversized_records, MAX_JSONL_RECORD_BYTES,
    );
    for population in &report.populations {
        let _ = writeln!(
            out,
            "\n{}: {} sessions, {} whole attachments, {} compactions, {} slices excluded, {} unknown extents excluded, {} unknown content sizes",
            population.population.label(),
            population.sessions_considered,
            population.whole_file_attachments,
            population.compactions,
            population.sliced_file_attachments_excluded,
            population.file_attachments_with_unknown_extent_excluded,
            population.whole_file_attachments_with_unknown_content_size,
        );
        for file in &population.worklist {
            let coverage = file
                .covered_by_existing_task
                .map(|task| format!("task {task}"))
                .unwrap_or_else(|| "new".into());
            let _ = writeln!(
                out,
                "{:>2}. harm={} sessions={} repeat={} whole={} record-chars={}..{} jsonl-bytes={}..{} content-bytes={}..{} unknown-content={} [{}] {}",
                file.rank,
                file.post_compaction_attachments,
                file.sessions_with_post_compaction_attachment,
                file.repeat_whole_file_attachments,
                file.whole_file_attachments,
                file.median_jsonl_record_characters,
                file.max_jsonl_record_characters,
                file.median_jsonl_record_bytes,
                file.max_jsonl_record_bytes,
                file.median_file_content_bytes,
                file.max_file_content_bytes,
                file.attachments_with_unknown_content_size,
                coverage,
                file.path,
            );
            for session in &file.affected_sessions {
                let _ = writeln!(
                    out,
                    "    session={} tasks={:?} whole-lines={:?} post-compact-lines={:?} compactions={:?} boundaries={:?} summaries={:?} outcome={}",
                    session.session_ref,
                    session.task_ids,
                    session.whole_attachment_lines,
                    session.post_compaction_attachment_lines,
                    session.compaction_lines,
                    session.compact_boundary_lines,
                    session.compact_summary_lines,
                    session.outcome.as_deref().unwrap_or("unknown"),
                );
                let _ = writeln!(
                    out,
                    "      record-chars={:?} jsonl-bytes={:?} content-bytes={:?}",
                    session.jsonl_record_characters,
                    session.jsonl_record_bytes,
                    session.file_content_bytes,
                );
            }
        }
        for check in &population.existing_task_checks {
            if check.observed {
                let _ = writeln!(
                    out,
                    "task {} check: rank={} harm={} sessions={} whole={} {}",
                    check.task_id,
                    check.rank.unwrap_or(0),
                    check.post_compaction_attachments,
                    check.sessions_with_post_compaction_attachment,
                    check.whole_file_attachments,
                    check.path,
                );
            } else {
                let _ = writeln!(
                    out,
                    "task {} check: not observed as a whole-file attachment; absence is not proof of safety {}",
                    check.task_id, check.path,
                );
            }
        }
    }
    for note in &report.notes {
        let _ = writeln!(out, "note: {note}");
    }
    out
}
