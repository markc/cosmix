//! Shared read-only catalogue for vendor transcripts and Foreman run joins.
//!
//! Both reconciliation and transcript analysis need the same authoritative
//! join: the JSONL filename is the vendor session UUID, and `runs.session_ref`
//! stores that UUID.  Keep that rule here so investigative commands do not
//! grow subtly different matching heuristics.

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPopulation {
    Foreman,
    Operator,
}

impl SessionPopulation {
    pub fn label(self) -> &'static str {
        match self {
            Self::Foreman => "foreman",
            Self::Operator => "operator",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RunJoin {
    pub run_id: i64,
    pub task_id: i64,
    pub verdict: Option<String>,
    pub delivery: Option<String>,
    pub quality: Option<String>,
}

impl RunJoin {
    pub fn outcome_label(&self) -> Option<String> {
        if let Some(verdict) = self.verdict.as_deref() {
            return Some(format!("verdict:{verdict}"));
        }
        let delivery = self.delivery.as_deref().filter(|value| *value != "unknown");
        let quality = self.quality.as_deref().filter(|value| *value != "unknown");
        match (delivery, quality) {
            (Some(delivery), Some(quality)) => {
                Some(format!("delivery:{delivery},quality:{quality}"))
            }
            (Some(delivery), None) => Some(format!("delivery:{delivery}")),
            (None, Some(quality)) => Some(format!("quality:{quality}")),
            (None, None) => None,
        }
    }
}

#[derive(Debug, Default)]
pub struct LedgerJoins {
    pub by_session: BTreeMap<String, Vec<RunJoin>>,
    pub available: bool,
    pub complete: bool,
    pub notes: Vec<String>,
}

/// Recursively inventory JSONL transcripts without opening their bodies.
pub fn collect_jsonl(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    collect_jsonl_into(root, &mut out)?;
    out.sort_unstable();
    Ok(out)
}

fn collect_jsonl_into(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    let mut entries = std::fs::read_dir(dir)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_unstable_by_key(|entry| entry.path());
    for entry in entries {
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_jsonl_into(&entry.path(), out)?;
        } else if file_type.is_file()
            && entry.path().extension().and_then(|ext| ext.to_str()) == Some("jsonl")
        {
            out.push(entry.path());
        }
    }
    Ok(())
}

pub fn session_id(path: &Path) -> Option<String> {
    path.file_stem()?.to_str().map(str::to_owned)
}

pub fn project_slug(path: &Path) -> Option<String> {
    path.parent()?.file_name()?.to_str().map(str::to_owned)
}

pub fn classify_population(slug: &str, cwd: Option<&str>) -> SessionPopulation {
    if slug.contains("--cmctl--foreman") || cwd.is_some_and(|cwd| cwd.contains("/.foreman/")) {
        SessionPopulation::Foreman
    } else {
        SessionPopulation::Operator
    }
}

pub fn task_from_slug(slug: &str) -> Option<i64> {
    let marker = "-task-";
    let offset = slug.rfind(marker)? + marker.len();
    slug[offset..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .ok()
}

pub fn task_from_cwd(cwd: &str) -> Option<i64> {
    Path::new(cwd).ancestors().find_map(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix("task-"))
            .and_then(|task| task.parse().ok())
    })
}

/// Load the exact `runs.session_ref` join from a read-only SQLite handle.
///
/// Schema discovery is deliberate. Older ledgers may lack the advisory
/// outcome columns, but an exact session/task join is still useful.
pub fn load_ledger_joins(path: &Path) -> LedgerJoins {
    let mut joins = LedgerJoins::default();
    let connection = match open_read_only(path) {
        Ok(connection) => connection,
        Err(error) => {
            joins.notes.push(format!(
                "foreman ledger unavailable at {}: {error}",
                path.display()
            ));
            return joins;
        }
    };
    joins.available = true;

    let columns = match table_columns(&connection, "runs") {
        Ok(columns) => columns,
        Err(error) => {
            joins
                .notes
                .push(format!("foreman runs schema unavailable: {error}"));
            return joins;
        }
    };
    if !columns.contains("id") || !columns.contains("task_id") || !columns.contains("session_ref") {
        joins.notes.push(
            "foreman runs.id/task_id/session_ref unavailable; transcript joins skipped".into(),
        );
        return joins;
    }

    let selected = ["verdict", "delivery", "quality"]
        .map(|name| column_expr(&columns, name))
        .join(", ");
    let sql = format!(
        "SELECT id, task_id, session_ref, {selected} FROM runs \
         WHERE session_ref IS NOT NULL AND session_ref != '' ORDER BY id"
    );
    let mut statement = match connection.prepare(&sql) {
        Ok(statement) => statement,
        Err(error) => {
            joins
                .notes
                .push(format!("foreman session join query unavailable: {error}"));
            return joins;
        }
    };
    let mut rows = match statement.query([]) {
        Ok(rows) => rows,
        Err(error) => {
            joins
                .notes
                .push(format!("foreman session join rows unavailable: {error}"));
            return joins;
        }
    };

    let mut complete = true;
    loop {
        match rows.next() {
            Ok(Some(row)) => {
                let id = row.get::<_, i64>(0);
                let task_id = row.get::<_, i64>(1);
                let session_ref = row.get::<_, String>(2);
                let (Ok(run_id), Ok(task_id), Ok(session_ref)) = (id, task_id, session_ref) else {
                    complete = false;
                    continue;
                };
                joins
                    .by_session
                    .entry(session_ref)
                    .or_default()
                    .push(RunJoin {
                        run_id,
                        task_id,
                        verdict: row.get(3).ok(),
                        delivery: row.get(4).ok(),
                        quality: row.get(5).ok(),
                    });
            }
            Ok(None) => break,
            Err(error) => {
                joins
                    .notes
                    .push(format!("foreman session join became unavailable: {error}"));
                complete = false;
                break;
            }
        }
    }
    joins.complete = complete;
    joins
}

fn table_columns(connection: &Connection, table: &str) -> rusqlite::Result<BTreeSet<String>> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
    rows.collect()
}

fn column_expr(columns: &BTreeSet<String>, name: &str) -> String {
    if columns.contains(name) {
        name.into()
    } else {
        "NULL".into()
    }
}

fn open_read_only(path: &Path) -> rusqlite::Result<Connection> {
    let mut encoded = String::with_capacity(path.as_os_str().as_bytes().len());
    for byte in path.as_os_str().as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(char::from(*byte));
            }
            byte => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    Connection::open_with_flags(
        format!("file:{encoded}?mode=ro"),
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_URI,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn population_and_task_hints_follow_worktree_identity() {
        assert_eq!(
            classify_population("-home-alpha--cmctl--foreman-task-114", None),
            SessionPopulation::Foreman
        );
        assert_eq!(
            classify_population("-home-alpha--cos", Some("/home/alpha/.cos")),
            SessionPopulation::Operator
        );
        assert_eq!(
            task_from_slug("-home-alpha--cmctl--foreman-task-114"),
            Some(114)
        );
        assert_eq!(task_from_cwd("/tmp/fleet/task-87/src"), Some(87));
    }

    #[test]
    fn exact_session_join_is_read_only_and_tolerates_old_outcome_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE runs (
                    id INTEGER PRIMARY KEY,
                    task_id INTEGER NOT NULL,
                    session_ref TEXT,
                    verdict TEXT
                 );
                 INSERT INTO runs VALUES (1, 114, 'session-a', 'pass');
                 INSERT INTO runs VALUES (2, 99, NULL, NULL);",
            )
            .unwrap();
        drop(connection);

        let joins = load_ledger_joins(&path);
        assert!(joins.available);
        assert!(joins.complete);
        assert_eq!(joins.by_session.len(), 1);
        let run = &joins.by_session["session-a"][0];
        assert_eq!(run.task_id, 114);
        assert_eq!(run.outcome_label().as_deref(), Some("verdict:pass"));
        assert_eq!(run.delivery, None);
    }
}
