//! `mix man gotchas` is executable, not prose.
//!
//! The gotchas page exists because Mix has almost no training-data
//! presence and a model's first guess is often silently wrong. A page
//! of confident claims about a language nobody can autocomplete is
//! exactly the thing that rots into confident fiction — so every row of
//! its table is run here through the real binary and compared to the
//! stated result.
//!
//! The table's contract, parsed below:
//!
//! ```text
//! | you will guess | Mix is | probe | prints |
//! ```
//!
//! * **probe** — Mix source, backticked, run as `mix -c <probe>`.
//! * **prints** — backticked. Plain text must equal the probe's trimmed
//!   stdout. A leading `!` means the probe must FAIL, with that text
//!   appearing in stderr.
//!
//! A row that stops being true fails the build. A page that loses its
//! table, or its rows, fails too — a doctest that silently checks
//! nothing is worse than no doctest, because it reads as coverage.
#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::process::Command;

/// Rows below this are the load-bearing content. If the page is ever
/// trimmed under it, the harness has stopped testing what it claims to.
const MIN_ROWS: usize = 20;

struct Row {
    probe: String,
    expect: String,
}

fn page_path() -> PathBuf {
    // tests/ -> cosmix-mix/ -> crates/ -> src/ -> repo root
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../docs/mix/gotchas.md")
        .canonicalize()
        .expect("docs/mix/gotchas.md must exist — it is the page mix man reads")
}

/// Strip one layer of surrounding backticks from a table cell.
fn unbacktick(cell: &str) -> Option<String> {
    let t = cell.trim();
    let inner = t.strip_prefix('`')?.strip_suffix('`')?;
    Some(inner.to_string())
}

/// Parse the table, returning the rows AND every body line that failed
/// to parse.
///
/// The rejects matter as much as the rows: a cell containing a `|`, or a
/// probe that lost its backticks, would otherwise make a row vanish from
/// the harness while still reading as tested on the page. A doctest that
/// silently checks nothing is worse than none, so an unparsed body line
/// is a hard failure rather than a skip.
fn rows(markdown: &str) -> (Vec<Row>, Vec<String>) {
    let mut out = Vec::new();
    let mut rejected = Vec::new();
    let mut in_table = false;

    for line in markdown.lines() {
        let line = line.trim();
        if !line.starts_with('|') || !line.ends_with('|') {
            in_table = false;
            continue;
        }
        // The header row, then the `|---|---|---|---|` separator that
        // opens the body.
        if line.starts_with("| you will guess ") {
            continue;
        }
        if line.chars().all(|c| c == '|' || c == '-' || c == ':') {
            in_table = true;
            continue;
        }
        if !in_table {
            continue;
        }
        // `|a|b|c|d|` splits to ["", a, b, c, d, ""].
        let cells: Vec<&str> = line.split('|').collect();
        match cells.len() {
            6 => match (unbacktick(cells[3]), unbacktick(cells[4])) {
                (Some(probe), Some(expect)) => out.push(Row { probe, expect }),
                _ => rejected.push(format!(
                    "probe/prints cell is not a single backticked span: {line}"
                )),
            },
            n => rejected.push(format!(
                "row split into {n} fields, not 6 — an unescaped '|' inside a \
                 cell would hide this row from the harness: {line}"
            )),
        }
    }
    (out, rejected)
}

fn run_probe(src: &str) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_mix"))
        .args(["-c", src])
        .env("MIX_STATS", "off")
        // The probes are pure language; no rc, no aliases, no PATH from
        // whoever is running the suite.
        .env_remove("MIXRC")
        .output()
        .expect("run mix -c");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
        String::from_utf8_lossy(&out.stderr).trim().to_string(),
    )
}

#[test]
fn the_page_has_a_table_this_harness_can_read() {
    let page = std::fs::read_to_string(page_path()).expect("read gotchas.md");
    let (parsed, rejected) = rows(&page);
    assert!(
        rejected.is_empty(),
        "docs/mix/gotchas.md has {} table row(s) this harness cannot read, so \
         they are documented but NOT tested:\n\n{}\n",
        rejected.len(),
        rejected.join("\n")
    );
    let n = parsed.len();
    assert!(
        n >= MIN_ROWS,
        "parsed only {n} probe row(s) from docs/mix/gotchas.md — expected at \
         least {MIN_ROWS}. Either the page was gutted or the table's 4-column \
         `| guess | Mix is | probe | prints |` shape changed and this harness \
         is now testing nothing."
    );
}

#[test]
fn every_row_of_the_gotchas_table_is_true() {
    let page = std::fs::read_to_string(page_path()).expect("read gotchas.md");
    let (parsed, _) = rows(&page);
    let mut failures = Vec::new();

    for row in parsed {
        let (ok, stdout, stderr) = run_probe(&row.probe);

        if let Some(needle) = row.expect.strip_prefix('!') {
            if ok {
                failures.push(format!(
                    "probe `{}` was expected to FAIL with {needle:?} but exited 0 \
                     (stdout {stdout:?})",
                    row.probe
                ));
            } else if !stderr.contains(needle) {
                failures.push(format!(
                    "probe `{}` failed as expected but the error no longer says \
                     {needle:?}\n  stderr: {stderr}",
                    row.probe
                ));
            }
            continue;
        }

        if !ok {
            failures.push(format!(
                "probe `{}` was expected to print {:?} but failed\n  stderr: {stderr}",
                row.probe, row.expect
            ));
        } else if stdout != row.expect {
            failures.push(format!(
                "probe `{}`\n  page says: {:?}\n  mix says:  {stdout:?}",
                row.probe, row.expect
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "docs/mix/gotchas.md is out of date with the binary:\n\n{}\n",
        failures.join("\n\n")
    );
}

/// The page is only useful if `mix man gotchas` reaches it, which means
/// it must be listed in the manual index like every other topic.
#[test]
fn the_page_is_linked_from_the_manual_index() {
    let index = page_path().with_file_name("README.md");
    let body = std::fs::read_to_string(&index).expect("read docs/mix/README.md");
    assert!(
        body.contains("gotchas.md"),
        "docs/mix/README.md is the `mix man` index and does not link gotchas.md"
    );
}
