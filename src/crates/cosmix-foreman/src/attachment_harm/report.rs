use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::agent_sessions::{RunJoin, SessionPopulation};

use super::{
    AffectedSession, ExistingTaskCheck, FileHarm, PopulationAggregate, PopulationReport,
    SessionScan,
};

pub(super) fn aggregate_session(
    populations: &mut BTreeMap<SessionPopulation, PopulationAggregate>,
    scan: SessionScan,
    joined_runs: Option<&Vec<RunJoin>>,
) {
    let population = populations.entry(scan.population).or_default();
    population.sessions += 1;
    population.slices += scan.slices;
    population.unknown_extent += scan.unknown_extent;
    population.compactions += scan.compaction_lines.len();

    let runs = joined_runs.cloned().unwrap_or_default();
    let mut task_ids = runs.iter().map(|run| run.task_id).collect::<BTreeSet<_>>();
    if let Some(task) = scan.task_hint {
        task_ids.insert(task);
    }
    let joined_outcome = runs.iter().rev().find_map(RunJoin::outcome_label);
    let outcome = joined_outcome.or_else(|| scan.transcript_outcome.clone());

    for (physical_path, session_file) in scan.files {
        let path = logical_path(
            &physical_path,
            scan.cwd.as_deref(),
            scan.population,
            &scan.slug,
        );
        let file = population.files.entry(path).or_default();
        let whole = session_file.whole_lines.len();
        population.whole += whole;
        file.whole += whole;
        file.repeats += session_file.repeats;
        file.repeat_sessions += usize::from(session_file.repeats > 0);
        let post_compaction = session_file.post_compaction_lines.len();
        file.post_compaction += post_compaction;
        file.post_compaction_sessions += usize::from(post_compaction > 0);
        file.record_sizes
            .extend(session_file.record_sizes.iter().copied());
        file.record_char_counts
            .extend(session_file.record_char_counts.iter().copied());
        let unknown_content_size = session_file
            .content_sizes
            .iter()
            .filter(|size| size.is_none())
            .count();
        population.unknown_content_size += unknown_content_size;
        file.unknown_content_size += unknown_content_size;
        file.content_sizes
            .extend(session_file.content_sizes.iter().flatten().copied());
        file.affected.push(AffectedSession {
            session_ref: scan.id.clone(),
            project_slug: scan.slug.clone(),
            task_ids: task_ids.iter().copied().collect(),
            runs: runs.clone(),
            outcome: outcome.clone(),
            whole_attachment_lines: session_file.whole_lines,
            jsonl_record_characters: session_file.record_char_counts,
            jsonl_record_bytes: session_file.record_sizes,
            file_content_bytes: session_file.content_sizes,
            post_compaction_attachment_lines: session_file.post_compaction_lines,
            compaction_lines: scan.compaction_lines.clone(),
            compact_boundary_lines: scan.compact_boundary_lines.clone(),
            compact_summary_lines: scan.compact_summary_lines.clone(),
        });
    }
}

fn logical_path(
    physical: &str,
    cwd: Option<&str>,
    population: SessionPopulation,
    slug: &str,
) -> String {
    if let Some(cwd) = cwd
        && let Some(base) = logical_base(cwd, population)
        && let Ok(relative) = Path::new(physical).strip_prefix(base)
        && !relative.as_os_str().is_empty()
    {
        let relative = relative.to_string_lossy().into_owned();
        return match population {
            SessionPopulation::Foreman => relative,
            SessionPopulation::Operator => format!("{slug}:{relative}"),
        };
    }
    physical.to_owned()
}

fn logical_base(cwd: &str, population: SessionPopulation) -> Option<&Path> {
    let cwd = Path::new(cwd);
    match population {
        SessionPopulation::Foreman => cwd
            .ancestors()
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .and_then(|name| name.strip_prefix("task-"))
                    .is_some_and(|task| {
                        !task.is_empty() && task.chars().all(|c| c.is_ascii_digit())
                    })
            })
            .or(Some(cwd)),
        SessionPopulation::Operator => Some(cwd),
    }
}

pub(super) fn population_report(
    population: SessionPopulation,
    aggregate: PopulationAggregate,
    limit: usize,
) -> PopulationReport {
    let mut files = aggregate.files.into_iter().collect::<Vec<_>>();
    files.sort_by(|(left_path, left), (right_path, right)| {
        right
            .post_compaction
            .cmp(&left.post_compaction)
            .then_with(|| {
                right
                    .post_compaction_sessions
                    .cmp(&left.post_compaction_sessions)
            })
            .then_with(|| right.repeats.cmp(&left.repeats))
            .then_with(|| right.whole.cmp(&left.whole))
            .then_with(|| left_path.cmp(right_path))
    });
    let all_files = files
        .into_iter()
        .enumerate()
        .map(|(index, (path, mut aggregate))| {
            aggregate.affected.sort_by(|left, right| {
                left.session_ref
                    .cmp(&right.session_ref)
                    .then_with(|| left.project_slug.cmp(&right.project_slug))
            });
            FileHarm {
                rank: index + 1,
                covered_by_existing_task: existing_task(&path),
                path,
                whole_file_attachments: aggregate.whole,
                repeat_whole_file_attachments: aggregate.repeats,
                sessions_with_repeat_attachment: aggregate.repeat_sessions,
                post_compaction_attachments: aggregate.post_compaction,
                sessions_with_post_compaction_attachment: aggregate.post_compaction_sessions,
                median_jsonl_record_characters: median(&mut aggregate.record_char_counts),
                max_jsonl_record_characters: aggregate
                    .record_char_counts
                    .iter()
                    .copied()
                    .max()
                    .unwrap_or(0),
                median_jsonl_record_bytes: median(&mut aggregate.record_sizes),
                max_jsonl_record_bytes: aggregate.record_sizes.iter().copied().max().unwrap_or(0),
                median_file_content_bytes: median(&mut aggregate.content_sizes),
                max_file_content_bytes: aggregate.content_sizes.iter().copied().max().unwrap_or(0),
                attachments_with_unknown_content_size: aggregate.unknown_content_size,
                affected_sessions: aggregate.affected,
            }
        })
        .collect::<Vec<_>>();
    let existing_task_checks = if population == SessionPopulation::Foreman {
        [
            (111, "src/crates/cosmix-foreman/src/main.rs"),
            (112, "src/crates/cosmix-foreman/src/ledger.rs"),
            (113, "src/crates/cosmix-foreman/src/refinery.rs"),
        ]
        .into_iter()
        .map(|(task_id, path)| {
            let file = all_files.iter().find(|file| file.path == path);
            ExistingTaskCheck {
                task_id,
                path: path.into(),
                observed: file.is_some(),
                rank: file.map(|file| file.rank),
                whole_file_attachments: file.map_or(0, |file| file.whole_file_attachments),
                post_compaction_attachments: file
                    .map_or(0, |file| file.post_compaction_attachments),
                sessions_with_post_compaction_attachment: file
                    .map_or(0, |file| file.sessions_with_post_compaction_attachment),
            }
        })
        .collect()
    } else {
        Vec::new()
    };
    let worklist = all_files.into_iter().take(limit).collect();
    PopulationReport {
        population,
        sessions_considered: aggregate.sessions,
        whole_file_attachments: aggregate.whole,
        sliced_file_attachments_excluded: aggregate.slices,
        file_attachments_with_unknown_extent_excluded: aggregate.unknown_extent,
        whole_file_attachments_with_unknown_content_size: aggregate.unknown_content_size,
        compactions: aggregate.compactions,
        worklist,
        existing_task_checks,
    }
}

fn existing_task(path: &str) -> Option<i64> {
    let path = path.rsplit_once(':').map_or(path, |(_, path)| path);
    match path {
        "src/crates/cosmix-foreman/src/main.rs" => Some(111),
        "src/crates/cosmix-foreman/src/ledger.rs" => Some(112),
        "src/crates/cosmix-foreman/src/refinery.rs" => Some(113),
        _ => None,
    }
}

fn median(values: &mut [usize]) -> usize {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    values[values.len() / 2]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn existing_split_tasks_are_exact_paths_not_basenames() {
        assert_eq!(
            existing_task("src/crates/cosmix-foreman/src/main.rs"),
            Some(111)
        );
        assert_eq!(existing_task("another/main.rs"), None);
        assert_eq!(
            existing_task("-home-alpha--cos:src/crates/cosmix-foreman/src/ledger.rs"),
            Some(112)
        );
    }

    #[test]
    fn foreman_paths_are_relative_to_worktree_even_when_cwd_is_nested() {
        assert_eq!(
            logical_path(
                "/fleet/task-100/src/crates/foreman/src/main.rs",
                Some("/fleet/task-100/src"),
                SessionPopulation::Foreman,
                "slug",
            ),
            "src/crates/foreman/src/main.rs"
        );
    }
}
