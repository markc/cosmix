use std::fs::File;
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::*;

fn write_session(root: &Path, slug: &str, id: &str, records: &[Value]) -> PathBuf {
    let dir = root.join(slug);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{id}.jsonl"));
    let mut file = File::create(&path).unwrap();
    for record in records {
        serde_json::to_writer(&mut file, record).unwrap();
        writeln!(file).unwrap();
    }
    path
}

fn boundary() -> Value {
    serde_json::json!({"type":"system", "subtype":"compact_boundary"})
}

fn summary() -> Value {
    serde_json::json!({"type":"user", "isCompactSummary":true, "message":{"content":"secret summary"}})
}

fn file(path: &str, content: &str, start: u64, lines: u64, total: u64) -> Value {
    serde_json::json!({
        "type":"attachment",
        "cwd":"/fleet/task-100",
        "attachment": {
            "type":"file",
            "filename":path,
            "displayPath":"src/main.rs",
            "content": {"type":"text", "file": {
                "filePath":path,
                "content":content,
                "startLine":start,
                "numLines":lines,
                "totalLines":total
            }}
        }
    })
}

#[test]
fn paired_boundary_and_summary_start_one_epoch() {
    let dir = tempfile::tempdir().unwrap();
    let secret = "do-not-surface—\n".repeat(8_000);
    let mut records = Vec::new();
    for _ in 0..3 {
        records.push(boundary());
        records.push(summary());
        records.push(file(
            "/fleet/task-100/src/main.rs",
            &secret,
            1,
            8_000,
            8_000,
        ));
    }
    write_session(
        dir.path(),
        "-home-alpha--cmctl--foreman-task-100",
        "synthetic-session",
        &records,
    );

    let report = analyse(&AnalysisOptions {
        claude_projects: dir.path().to_path_buf(),
        ledger: None,
        limit: 10,
    })
    .unwrap();
    let foreman = &report.populations[0];
    assert_eq!(foreman.compactions, 3);
    let main = &foreman.worklist[0];
    assert_eq!(main.whole_file_attachments, 3);
    assert_eq!(main.repeat_whole_file_attachments, 2);
    assert_eq!(main.post_compaction_attachments, 2);
    assert_eq!(
        main.affected_sessions[0].post_compaction_attachment_lines,
        [6, 9]
    );
    assert_eq!(main.affected_sessions[0].compact_boundary_lines, [1, 4, 7]);
    assert_eq!(main.affected_sessions[0].compact_summary_lines, [2, 5, 8]);
    assert_eq!(main.affected_sessions[0].jsonl_record_bytes.len(), 3);
    assert!(
        main.affected_sessions[0].jsonl_record_characters[0]
            < main.affected_sessions[0].jsonl_record_bytes[0]
    );
    let json = serde_json::to_string(&report).unwrap();
    assert!(!json.contains("do-not-surface"));
    assert!(!render_text(&report).contains("do-not-surface"));
}

#[test]
fn slices_and_unknown_extents_never_enter_the_worklist() {
    let dir = tempfile::tempdir().unwrap();
    let mut unknown = file("/fleet/task-100/src/unknown.rs", "secret", 1, 1, 1);
    unknown["attachment"]["content"]["file"]
        .as_object_mut()
        .unwrap()
        .remove("totalLines");
    write_session(
        dir.path(),
        "-home-alpha--cmctl--foreman-task-100",
        "session",
        &[
            boundary(),
            file("/fleet/task-100/src/slice.rs", "secret", 100, 20, 500),
            unknown,
        ],
    );
    let report = analyse(&AnalysisOptions {
        claude_projects: dir.path().to_path_buf(),
        ledger: None,
        limit: 10,
    })
    .unwrap();
    let foreman = &report.populations[0];
    assert!(foreman.worklist.is_empty());
    assert_eq!(foreman.sliced_file_attachments_excluded, 1);
    assert_eq!(foreman.file_attachments_with_unknown_extent_excluded, 1);
}

#[test]
fn summary_without_boundary_still_starts_one_cycle() {
    let dir = tempfile::tempdir().unwrap();
    write_session(
        dir.path(),
        "-home-alpha--cos",
        "operator",
        &[summary(), file("/fleet/task-100/src/main.rs", "x", 1, 1, 1)],
    );
    let report = analyse(&AnalysisOptions {
        claude_projects: dir.path().to_path_buf(),
        ledger: None,
        limit: 10,
    })
    .unwrap();
    let operator = &report.populations[1];
    assert_eq!(operator.compactions, 1);
    assert_eq!(operator.worklist[0].post_compaction_attachments, 0);
}

#[test]
fn first_attachment_after_compaction_is_not_a_reattachment() {
    let dir = tempfile::tempdir().unwrap();
    write_session(
        dir.path(),
        "-home-alpha--cos",
        "operator",
        &[
            boundary(),
            file("/fleet/task-100/src/late.rs", "first", 1, 1, 1),
            file("/fleet/task-100/src/late.rs", "same epoch", 1, 1, 1),
            boundary(),
            file("/fleet/task-100/src/late.rs", "later epoch", 1, 1, 1),
        ],
    );
    let report = analyse(&AnalysisOptions {
        claude_projects: dir.path().to_path_buf(),
        ledger: None,
        limit: 10,
    })
    .unwrap();
    let late = &report.populations[1].worklist[0];
    assert_eq!(late.whole_file_attachments, 3);
    assert_eq!(late.repeat_whole_file_attachments, 2);
    assert_eq!(late.post_compaction_attachments, 1);
    assert_eq!(
        late.affected_sessions[0].post_compaction_attachment_lines,
        [5]
    );
}

#[test]
fn ranking_never_uses_size_as_a_tie_breaker() {
    let dir = tempfile::tempdir().unwrap();
    write_session(
        dir.path(),
        "-home-alpha--cmctl--foreman-task-100",
        "session",
        &[
            file("/fleet/task-100/z.rs", &"z".repeat(100_000), 1, 1, 1),
            file("/fleet/task-100/a.rs", "a", 1, 1, 1),
            boundary(),
            file("/fleet/task-100/z.rs", &"z".repeat(100_000), 1, 1, 1),
            file("/fleet/task-100/a.rs", "a", 1, 1, 1),
        ],
    );
    let report = analyse(&AnalysisOptions {
        claude_projects: dir.path().to_path_buf(),
        ledger: None,
        limit: 10,
    })
    .unwrap();
    let worklist = &report.populations[0].worklist;
    assert_eq!(worklist[0].path, "a.rs");
    assert_eq!(worklist[1].path, "z.rs");
}

#[test]
#[ignore = "requires the private known-answer Claude transcript"]
fn real_f2eca5dd_known_answer() {
    let transcript = std::env::var_os("FOREMAN_ATTACHMENT_HARM_KNOWN_SESSION")
        .expect("set FOREMAN_ATTACHMENT_HARM_KNOWN_SESSION to the f2eca5dd transcript");
    let scan = scan_session(Path::new(&transcript))
        .unwrap()
        .expect("known transcript must have a project/session identity");
    assert!(scan.id.starts_with("f2eca5dd"));
    assert_eq!(scan.compaction_lines, [109, 128, 147]);
    assert_eq!(scan.compact_boundary_lines, [109, 128, 147]);
    assert_eq!(scan.compact_summary_lines, [110, 129, 148]);

    let (_, main) = scan
        .files
        .iter()
        .find(|(path, _)| path.ends_with("/src/main.rs"))
        .expect("known transcript must contain whole main.rs attachments");
    assert_eq!(main.whole_lines, [111, 130, 149]);
    // The hand-recorded 113,077 "byte" size was a JSONL character count.
    // Preserve it while also checking the unambiguous UTF-8 and content sizes.
    assert_eq!(main.record_char_counts, [113_077, 113_077, 113_077]);
    assert_eq!(main.record_sizes, [113_189, 113_189, 113_189]);
    assert_eq!(
        main.content_sizes,
        [Some(109_205), Some(109_205), Some(109_205)]
    );
    assert_eq!(main.repeats, 2);
    assert_eq!(main.post_compaction_lines, [130, 149]);
}

#[test]
#[ignore = "requires the private Claude corpus and optional foreman ledger"]
fn real_corpus_reports_top_ten_and_split_task_coverage() {
    let claude_projects = std::env::var_os("FOREMAN_ATTACHMENT_HARM_CORPUS")
        .expect("set FOREMAN_ATTACHMENT_HARM_CORPUS to the private Claude projects root");
    let ledger = std::env::var_os("FOREMAN_ATTACHMENT_HARM_LEDGER").map(PathBuf::from);
    let report = analyse(&AnalysisOptions {
        claude_projects: PathBuf::from(claude_projects),
        ledger,
        limit: 10,
    })
    .unwrap();
    let foreman = report
        .populations
        .iter()
        .find(|population| population.population == SessionPopulation::Foreman)
        .unwrap();
    let main = foreman
        .worklist
        .iter()
        .find(|file| file.path == "src/crates/cosmix-foreman/src/main.rs")
        .expect("main.rs must independently surface in the live top ten");
    assert!(main.post_compaction_attachments > 0);
    assert_eq!(
        foreman
            .existing_task_checks
            .iter()
            .map(|check| check.task_id)
            .collect::<Vec<_>>(),
        [111, 112, 113]
    );

    println!(
        "sessions={} complete={} observation={}..{}",
        report.sessions_considered,
        report.sessions_complete,
        report.observation_start.as_deref().unwrap_or("unknown"),
        report.observation_end.as_deref().unwrap_or("unknown")
    );
    for population in &report.populations {
        println!(
            "population={:?} sessions={}",
            population.population, population.sessions_considered
        );
        for file in &population.worklist {
            println!(
                "rank={} harm={} sessions={} whole={} median-content-bytes={} task={:?} path={}",
                file.rank,
                file.post_compaction_attachments,
                file.sessions_with_post_compaction_attachment,
                file.whole_file_attachments,
                file.median_file_content_bytes,
                file.covered_by_existing_task,
                file.path
            );
        }
        for check in &population.existing_task_checks {
            println!(
                "task={} observed={} rank={:?} harm={} path={}",
                check.task_id,
                check.observed,
                check.rank,
                check.post_compaction_attachments,
                check.path
            );
        }
    }
}

#[test]
fn bounded_reader_drains_an_oversized_record_before_the_next() {
    let mut reader = BufReader::new(std::io::Cursor::new(b"12345\n{}\n"));
    let (first, bytes, eof) = read_bounded_record(&mut reader, 4).unwrap();
    assert!(first.is_none());
    assert_eq!(bytes, 6);
    assert!(!eof);
    let (second, bytes, _) = read_bounded_record(&mut reader, 4).unwrap();
    assert_eq!(second.as_deref(), Some(b"{}".as_slice()));
    assert_eq!(bytes, 3);
}
