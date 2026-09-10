//! Rustyline 15 `FileHistory` compatibility, with no filesystem I/O.
//!
//! `history.rs::{save_to,load_from}` in rustyline is the format authority:
//! `#V2\n`, LF-delimited UTF-8 records, LF encoded as `\\n`, backslash as
//! `\\\\`. CR, tabs and Unicode are otherwise literal. Legacy files have no
//! header and no unescaping. An invalid escape preserves the entire raw record.
//! Like rustyline's `BufRead::lines`, loading accepts CRLF and drops blank
//! records. Saving canonicalises to V2/LF; arbitrary malformed/legacy input is
//! not byte-preserving. Canonical V2 fixtures are verified against its writer.
//!
//! Mix calls `add_history_entry(line_buf.trim())`, then `save_history` (full
//! rewrite, not concurrent-session append). Defaults: last 100 nonempty entries,
//! suppress consecutive duplicates, retain nonconsecutive duplicates. Loaded
//! records are NOT trimmed. File locking/permissions/atomic replacement belong
//! to the later I/O owner; `encode` produces bytes, never opens a history file.

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct History {
    entries: Vec<String>,
    capacity: usize,
}

impl Default for History {
    fn default() -> Self {
        Self::new(100)
    }
}

impl History {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: Vec::new(),
            capacity,
        }
    }
    pub fn entries(&self) -> &[String] {
        &self.entries
    }
    pub fn append(&mut self, submitted: &str) -> bool {
        self.add_record(submitted.trim())
    }
    fn add_record(&mut self, record: &str) -> bool {
        if self.capacity == 0
            || record.is_empty()
            || self.entries.last().is_some_and(|last| last == record)
        {
            return false;
        }
        if self.entries.len() == self.capacity {
            self.entries.remove(0);
        }
        self.entries.push(record.to_owned());
        true
    }
    /// Add file records in order, matching rustyline load into existing history.
    pub fn load(&mut self, file: &str) {
        let mut lines = file.lines();
        let Some(first) = lines.next() else {
            return;
        };
        let v2 = first == "#V2";
        if !v2 {
            self.add_record(first);
        }
        for line in lines {
            if v2 {
                self.add_record(&decode_record(line));
            } else {
                self.add_record(line);
            }
        }
    }
    pub fn encode(&self) -> String {
        let mut file = String::from("#V2\n");
        for entry in &self.entries {
            for ch in entry.chars() {
                match ch {
                    '\n' => file.push_str("\\n"),
                    '\\' => file.push_str("\\\\"),
                    _ => file.push(ch),
                }
            }
            file.push('\n');
        }
        file
    }
}

fn decode_record(record: &str) -> String {
    let mut result = String::with_capacity(record.len());
    let mut chars = record.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            result.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => result.push('\n'),
            Some('\\') => result.push('\\'),
            _ => return record.to_owned(),
        }
    }
    result
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Forward,
    Reverse,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchKind {
    Prefix,
    FullText,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchMatch {
    pub index: usize,
    pub byte_offset: usize,
}

/// Case-sensitive search with an inclusive starting index, like rustyline.
/// Empty terms or out-of-range starts return no match; no wrapping/queueing.
pub fn search(
    entries: &[String],
    term: &str,
    start: usize,
    direction: Direction,
    kind: SearchKind,
) -> Option<SearchMatch> {
    if term.is_empty() || start >= entries.len() {
        return None;
    }
    let check = |index: usize| {
        let offset = match kind {
            SearchKind::Prefix => entries[index].starts_with(term).then_some(0),
            SearchKind::FullText => entries[index].find(term),
        };
        offset.map(|byte_offset| SearchMatch { index, byte_offset })
    };
    match direction {
        Direction::Forward => (start..entries.len()).find_map(check),
        Direction::Reverse => (0..=start).rev().find_map(check),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustyline::history::{DefaultHistory, History as RustyHistory};
    const FIXTURE: &str = include_str!("fixtures/history-v2.txt");
    #[test]
    fn v2_fixture_roundtrip_byte_exact() {
        let mut h = History::default();
        h.load(FIXTURE);
        assert_eq!(
            h.entries(),
            &[
                "print(\"hello\")",
                "if true then\n  print(\"界👩‍👩‍👧‍👦\")\nend",
                "literal \\n and \\ and tab\there",
                "e\u{301}cho"
            ]
        );
        assert_eq!(h.encode().as_bytes(), FIXTURE.as_bytes());
    }
    #[test]
    fn actual_rustyline_writer_and_reader_agree() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history");
        let mut h = History::default();
        h.load(FIXTURE);
        let mut rusty = DefaultHistory::new();
        for entry in h.entries() {
            rusty.add(entry).unwrap();
        }
        rusty.save(&path).unwrap();
        let actual = std::fs::read(&path).unwrap();
        assert_eq!(actual, FIXTURE.as_bytes());
        let mut loaded = History::default();
        loaded.load(std::str::from_utf8(&actual).unwrap());
        assert_eq!(loaded.encode().as_bytes(), actual);
        let second = dir.path().join("owned");
        std::fs::write(&second, loaded.encode()).unwrap();
        let mut reread = DefaultHistory::new();
        reread.load(&second).unwrap();
        assert_eq!(
            reread.iter().collect::<Vec<_>>(),
            h.entries().iter().collect::<Vec<_>>()
        );
    }
    #[test]
    fn legacy_crlf_unknown_escape_and_trailing_backslash() {
        let mut h = History::default();
        h.load("one\\n\r\ntwo\n\n");
        assert_eq!(h.entries(), &["one\\n", "two"]);
        let mut h = History::default();
        h.load("#V2\r\na\\nb\r\na\\nb\\q\ntrailing\\\n");
        assert_eq!(h.entries(), &["a\nb", "a\\nb\\q", "trailing\\"]);
    }
    #[test]
    fn append_trims_and_only_deduplicates_neighbours() {
        let mut h = History::new(3);
        assert!(!h.append(" \n "));
        assert!(h.append(" a "));
        assert!(!h.append("a"));
        h.append("b");
        h.append("a");
        assert_eq!(h.entries(), &["a", "b", "a"]);
        h.append("c");
        assert_eq!(h.entries(), &["b", "a", "c"]);
        assert!(!History::new(0).append("a"));
        let mut h = History::default();
        h.load("#V2\n  spaced  \n  spaced  \n\n");
        assert_eq!(h.entries(), &["  spaced  "]);
        for i in 0..110 {
            h.append(&i.to_string());
        }
        assert_eq!(h.entries().len(), 100);
        assert_eq!(h.entries()[0], "10");
    }
    #[test]
    fn searches_direction_unicode_offsets_and_edges() {
        let entries = vec!["echo 世界".into(), "世界 echo".into(), "echo again".into()];
        assert_eq!(
            search(&entries, "echo", 2, Direction::Reverse, SearchKind::Prefix),
            Some(SearchMatch {
                index: 2,
                byte_offset: 0
            })
        );
        assert_eq!(
            search(
                &entries,
                "echo",
                1,
                Direction::Reverse,
                SearchKind::FullText
            ),
            Some(SearchMatch {
                index: 1,
                byte_offset: 7
            })
        );
        assert_eq!(
            search(&entries, "echo", 1, Direction::Forward, SearchKind::Prefix),
            Some(SearchMatch {
                index: 2,
                byte_offset: 0
            })
        );
        for term in ["", "missing", "ECHO"] {
            assert_eq!(
                search(&entries, term, 0, Direction::Forward, SearchKind::FullText),
                None
            );
        }
        assert_eq!(
            search(&entries, "echo", 3, Direction::Reverse, SearchKind::Prefix),
            None
        );
        assert_eq!(
            search(&[], "echo", 0, Direction::Reverse, SearchKind::Prefix),
            None
        );
    }
}
