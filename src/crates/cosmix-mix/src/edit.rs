//! `mix edit FILE OLD NEW` — the one-line exact-match file edit.
//!
//! ```text
//! mix edit [--all] [-n|--dry-run] FILE OLD NEW
//! ```
//!
//! This exists to remove the `sed -i` reflex. The reflex was never about
//! the language: a one-line edit *from a shell prompt* had no Mix shape
//! shorter than writing a `.mix` file, so an agent under time pressure
//! reached for `sed` — and `sed` is a regex engine that silently edits
//! every match, or none, and says nothing either way.
//!
//! So the contract here is the opposite of `sed`'s on every axis that
//! bit us:
//!
//! * **Exact match, never a pattern.** OLD is compared byte-for-byte.
//!   No metacharacters, no delimiter to choose, no escaping.
//! * **Refuses an ambiguous edit.** More than one occurrence and
//!   nothing is written — the run exits 2 and names every line that
//!   matched. `--all` is the explicit opt-in to edit them all.
//! * **Refuses a silent miss.** A needle that is not present exits 1
//!   with the file untouched. `sed`'s "0 substitutions, exit 0" is the
//!   failure mode that shipped three wrong files in one session.
//! * **Prints what it did.** The changed line(s) as a `-`/`+` pair, so
//!   the caller sees the edit without a second `read_file`.
//!
//! Exit codes: 0 = edited; 1 = OLD absent, file untouched; 2 = OLD
//! ambiguous (>1 occurrence without `--all`), file untouched; 3 = usage
//! error, or the file could not be read or written.
//!
//! The write is atomic: a sibling temp file is written, fsynced, and
//! renamed over the original, carrying the original's permission bits.
//! An interrupted `mix edit` therefore leaves the old file intact
//! rather than a truncated one.
//!
//! Companion to the `replace_must()` builtin — same semantics, CLI
//! surface.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

const USAGE: &str = "Usage: mix edit [--all] [-n|--dry-run] FILE OLD NEW";

/// Exit code for a usage error or an I/O failure — kept distinct from
/// the 1/2 "the edit did not apply" band so a caller can tell "your
/// needle was wrong" from "I could not read the file".
const RC_USAGE: i32 = 3;

#[derive(Debug)]
struct Options {
    all: bool,
    dry_run: bool,
    file: PathBuf,
    old: String,
    new: String,
}

/// Entry point for the one-shot `mix edit` CLI.
pub fn run_edit(args: &[String]) -> i32 {
    let opts = match parse_args(args) {
        Ok(Some(opts)) => opts,
        // `--help` is a successful request for the usage, not an error.
        Ok(None) => {
            println!("{USAGE}");
            return 0;
        }
        Err(msg) => {
            eprintln!("mix edit: {msg}");
            eprintln!("{USAGE}");
            return RC_USAGE;
        }
    };

    let content = match fs::read(&opts.file) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(_) => {
                eprintln!(
                    "mix edit: {}: not valid UTF-8 (mix edit is a text edit)",
                    opts.file.display()
                );
                return RC_USAGE;
            }
        },
        Err(e) => {
            eprintln!("mix edit: {}: {e}", opts.file.display());
            return RC_USAGE;
        }
    };

    // Witness the file as it was READ, so the write can refuse if
    // someone else has landed on it in the meantime. Taken after the
    // read, so a writer between the two is caught rather than blessed.
    let before = fs::metadata(&opts.file).ok();

    let hits = match_offsets(&content, &opts.old);

    if hits.is_empty() {
        eprintln!(
            "mix edit: {}: OLD not found ({} byte(s)); file unchanged",
            opts.file.display(),
            opts.old.len()
        );
        return 1;
    }

    if hits.len() > 1 && !opts.all {
        eprintln!(
            "mix edit: {}: OLD occurs {} times; file unchanged",
            opts.file.display(),
            hits.len()
        );
        for offset in &hits {
            let (line, text) = span_lines(&content, *offset, opts.old.len());
            eprintln!(
                "  {}:{}: {}",
                opts.file.display(),
                line,
                text.join(" ⏎ ")
            );
        }
        eprintln!("Pass --all to edit every occurrence, or give a longer OLD.");
        return 2;
    }

    // Report BEFORE writing, from the pre-edit content — the line
    // numbers a reader wants are the ones they can still see on disk if
    // the write then fails. The `+` side is read out of the FULLY
    // edited text, never a per-hit preview: with `--all` on a line
    // holding two hits, a per-hit preview prints two `+` lines and
    // neither of them is what lands on disk.
    let edited = apply(&content, &hits, &opts.old, &opts.new);
    for group in report_groups(&content, &hits, opts.old.len(), opts.new.len()) {
        let (line, before) = span_lines(&content, group.before_at, group.before_len);
        let (_, after) = span_lines(&edited, group.after_at, group.after_len);
        println!("{}:{}", opts.file.display(), line);
        for l in &before {
            println!("- {l}");
        }
        for l in &after {
            println!("+ {l}");
        }
    }

    if opts.dry_run {
        println!(
            "mix edit: --dry-run, {} occurrence(s) matched; nothing written",
            hits.len()
        );
        return 0;
    }

    if let Err(e) = write_atomic(&opts.file, &edited, before.as_ref()) {
        eprintln!("mix edit: {}: {e}", opts.file.display());
        return RC_USAGE;
    }
    0
}

/// `Ok(None)` is `--help`: print the usage and succeed.
///
/// **Flags come first, then FILE OLD NEW verbatim.** Option parsing
/// stops at the first non-flag token (or an explicit `--`), and
/// everything after it is positional whatever it looks like. That rule
/// exists because OLD and NEW are arbitrary text: `mix edit f.rs "=>"
/// "->"` and `mix edit f.rs 1 -1` are ordinary edits, and a parser that
/// kept scanning for flags would reject them as unknown options — which
/// is exactly what the first version of this did.
///
/// A dash-leading token BEFORE any positional is still a hard error
/// rather than a filename: a typo'd `--al` must not become a confusing
/// ENOENT on a file called `--al`.
fn parse_args(args: &[String]) -> Result<Option<Options>, String> {
    let mut all = false;
    let mut dry_run = false;
    let mut i = 0;

    while i < args.len() {
        match args[i].as_str() {
            "--" => {
                i += 1;
                break;
            }
            "--all" => all = true,
            "-n" | "--dry-run" => dry_run = true,
            "-h" | "--help" => return Ok(None),
            // A lone "-" is a plausible filename-ish token; anything
            // else leading with a dash is a typo'd flag.
            other if other.starts_with('-') && other.len() > 1 => {
                return Err(format!("unknown option '{other}'"));
            }
            _ => break,
        }
        i += 1;
    }

    let positional: Vec<&str> = args[i..].iter().map(String::as_str).collect();

    if positional.len() != 3 {
        let trailing_flag = positional
            .iter()
            .skip(1)
            .any(|a| matches!(*a, "--all" | "-n" | "--dry-run"));
        let hint = if trailing_flag {
            " — flags go BEFORE FILE (`mix edit --all FILE OLD NEW`)"
        } else {
            ""
        };
        return Err(format!(
            "expected FILE OLD NEW ({} argument(s) given){hint}",
            positional.len()
        ));
    }
    if positional[1].is_empty() {
        return Err("OLD is empty (it would match at every position)".to_string());
    }
    if positional[1] == positional[2] {
        return Err("OLD and NEW are identical; nothing to do".to_string());
    }

    Ok(Some(Options {
        all,
        dry_run,
        file: PathBuf::from(positional[0]),
        old: positional[1].to_string(),
        new: positional[2].to_string(),
    }))
}

/// Byte offsets of every NON-OVERLAPPING occurrence of `needle`, the
/// same occurrences `apply` will replace. Scanning forward past the end
/// of each hit is what makes the count and the edit agree: for
/// `needle = "aa"` in `"aaa"` this reports ONE hit, and `apply`
/// performs one replacement. Counting overlaps here would report an
/// ambiguity the edit would not have had.
fn match_offsets(haystack: &str, needle: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut from = 0usize;
    while from <= haystack.len() {
        match haystack[from..].find(needle) {
            Some(rel) => {
                let at = from + rel;
                out.push(at);
                from = at + needle.len();
            }
            None => break,
        }
    }
    out
}

fn apply(content: &str, hits: &[usize], old: &str, new: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut cursor = 0usize;
    for &at in hits {
        out.push_str(&content[cursor..at]);
        out.push_str(new);
        cursor = at + old.len();
    }
    out.push_str(&content[cursor..]);
    out
}

/// One `-`/`+` entry of the report: a byte range in the ORIGINAL text
/// and the corresponding range in the EDITED text.
struct ReportGroup {
    before_at: usize,
    before_len: usize,
    after_at: usize,
    after_len: usize,
}

/// Collapse the hits into one entry per affected line (or line block).
///
/// Two things this gets right that a per-hit loop does not. Hits sharing
/// a line are merged, so `aa aa` -> `bb` under `--all` reports
/// `- aa aa` / `+ bb bb` instead of two previews that each show one
/// replacement and neither of which is the file that gets written. And
/// the `after` range is expressed in the fully-edited text, shifted by
/// the cumulative length delta of every preceding replacement, so the
/// `+` side is read out of the real result rather than reconstructed.
fn report_groups(content: &str, hits: &[usize], old_len: usize, new_len: usize) -> Vec<ReportGroup> {
    let delta = new_len as isize - old_len as isize;
    let post = |i: usize, at: usize| -> usize {
        let shifted = at as isize + delta * i as isize;
        shifted.max(0) as usize
    };

    let mut out: Vec<ReportGroup> = Vec::new();
    for (i, &at) in hits.iter().enumerate() {
        let after_at = post(i, at);
        // Merge into the previous entry when this hit starts before the
        // previous one's line has ended — i.e. they share a line.
        if let Some(last) = out.last_mut() {
            let prev_end = content[(last.before_at + last.before_len).min(content.len())..]
                .find('\n')
                .map_or(content.len(), |n| last.before_at + last.before_len + n);
            if at <= prev_end {
                last.before_len = at + old_len - last.before_at;
                last.after_len = after_at + new_len - last.after_at;
                continue;
            }
        }
        out.push(ReportGroup {
            before_at: at,
            before_len: old_len,
            after_at,
            after_len: new_len,
        });
    }
    out
}

/// The 1-based line number `offset` sits on, and every WHOLE line the
/// byte range `offset .. offset + len` touches.
///
/// Whole lines, not the matched fragment, because a diff that shows
/// `- eta` for a match inside `beta` is unreadable. All of them, not
/// just the first, because a multi-line OLD (or a NEW containing
/// newlines) would otherwise be reported as a truncated fragment — the
/// `-`/`+` pair has to be the truth or it is worse than no output.
fn span_lines(content: &str, offset: usize, len: usize) -> (usize, Vec<String>) {
    let line_no = content[..offset].matches('\n').count() + 1;
    let start = content[..offset].rfind('\n').map_or(0, |i| i + 1);
    // Clamp: a zero-length NEW at end-of-file, or any arithmetic that
    // would run off the end, must not panic on the slice below.
    let last = (offset + len).min(content.len());
    let end = content[last..]
        .find('\n')
        .map_or(content.len(), |i| last + i);
    (
        line_no,
        content[start..end].split('\n').map(str::to_string).collect(),
    )
}

/// Write `content` over `path` without a truncation window: a sibling
/// temp file is written and fsynced, then renamed into place. `rename`
/// within a directory is atomic, so a reader either sees the whole old
/// file or the whole new one.
///
/// **Symlinks are followed.** The read already followed the link, so a
/// rename over `path` itself would replace the LINK with a regular file
/// holding the target's edited bytes — silently un-linking a symlinked
/// config and leaving the real file stale. That is precisely the class
/// of silent semantic change this subcommand exists to refuse, so the
/// path is resolved first and the edit lands on the target. If it cannot
/// be resolved, the original path is used rather than failing the edit.
/// **The temp file is created O_EXCL at 0600.** `File::create` would
/// FOLLOW a pre-placed symlink at the temp path and truncate whatever it
/// points at — a hostile or merely unlucky `.foo.mix-edit.<pid>` turns an
/// edit of `foo` into a silent clobber of something else. `create_new`
/// issues `O_CREAT|O_EXCL`, which refuses both an existing file and a
/// symlink. The explicit 0600 closes the other half: the default
/// `0666 & umask` would publish a private file's contents at 0644 for
/// the length of the write, and leave them there if the run is killed.
/// The real mode is applied to the temp file just before the rename.
///
/// **The stale-content re-check is a narrowing, not a lock.** Between
/// the read and the rename another writer can land; the rename would
/// then discard their work with no sign. Re-stating identity
/// (device/inode/len/mtime) immediately before the rename shrinks that
/// window from the whole run to the syscall gap and turns the common
/// case — a concurrent edit, an editor save — into a refusal instead of
/// silent data loss. It does NOT make the sequence atomic; a writer
/// landing inside that gap still wins. Real exclusion needs a lock, and
/// that is a bigger change than this subcommand warrants.
fn write_atomic(path: &Path, content: &str, before: Option<&fs::Metadata>) -> std::io::Result<()> {
    let resolved = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let path = resolved.as_path();
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty());
    let mut tmp = match dir {
        Some(d) => d.to_path_buf(),
        None => PathBuf::from("."),
    };
    let stem = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "mix-edit".to_string());
    // Nanoseconds as well as the pid: with O_EXCL a leftover temp from a
    // killed run would otherwise make every later edit of that file fail.
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    tmp.push(format!(
        ".{}.mix-edit.{}.{unique}",
        stem,
        std::process::id()
    ));

    // Carry the original's permission bits; a script silently losing its
    // executable bit mid-session is a nasty way to learn about this.
    let mode = fs::metadata(path).ok().map(|m| m.permissions());

    let write = (|| -> std::io::Result<()> {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(content.as_bytes())?;
        f.sync_all()?;
        drop(f);
        if let Some(mode) = mode {
            fs::set_permissions(&tmp, mode)?;
        }
        if let Some(before) = before
            && let Ok(now) = fs::metadata(path)
            && !same_file(before, &now)
        {
            return Err(std::io::Error::other(
                "the file changed under us since it was read; nothing written \
                 (re-run the edit)",
            ));
        }
        fs::rename(&tmp, path)
    })();

    if write.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    write
}

/// Is this the same file, unmodified? Identity (device + inode) plus
/// content witnesses (length + mtime), because an in-place rewrite keeps
/// the inode and a same-length rewrite keeps the length.
fn same_file(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if a.dev() != b.dev() || a.ino() != b.ino() {
            return false;
        }
    }
    if a.len() != b.len() {
        return false;
    }
    match (a.modified(), b.modified()) {
        (Ok(x), Ok(y)) => x == y,
        // No mtime on this platform: identity and length still stand.
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn offsets_are_non_overlapping() {
        // "aaa" contains "aa" twice by overlap but only once by the
        // scan `apply` performs; the count must match the edit.
        assert_eq!(match_offsets("aaa", "aa"), vec![0]);
        assert_eq!(match_offsets("abcabc", "abc"), vec![0, 3]);
        assert_eq!(match_offsets("abc", "zz"), Vec::<usize>::new());
    }

    #[test]
    fn apply_replaces_only_listed_hits() {
        let c = "x=1\nx=1\nx=1\n";
        let hits = match_offsets(c, "x=1");
        assert_eq!(hits.len(), 3);
        // Only the middle one.
        assert_eq!(apply(c, &hits[1..2], "x=1", "x=2"), "x=1\nx=2\nx=1\n");
        assert_eq!(apply(c, &hits, "x=1", "x=2"), "x=2\nx=2\nx=2\n");
    }

    #[test]
    fn span_lines_is_one_based_and_gives_whole_lines() {
        let c = "alpha\nbeta\ngamma\n";
        assert_eq!(span_lines(c, 0, 1), (1, vec!["alpha".to_string()]));
        assert_eq!(span_lines(c, 6, 1), (2, vec!["beta".to_string()]));
        assert_eq!(span_lines(c, 8, 1), (2, vec!["beta".to_string()]));
        assert_eq!(span_lines(c, 11, 1), (3, vec!["gamma".to_string()]));
    }

    /// A needle that spans lines, and a replacement that introduces
    /// lines, must both be reported whole — a one-line report of either
    /// is a fragment that reads as the truth.
    #[test]
    fn span_lines_covers_every_line_the_range_touches() {
        let c = "one\ntwo\nthree\n";
        let (line, lines) = span_lines(c, 0, "one\ntwo".len());
        assert_eq!(line, 1);
        assert_eq!(lines, vec!["one".to_string(), "two".to_string()]);

        // A NEW containing newlines, measured in the EDITED text.
        let edited = apply(c, &[0], "one", "A\nB");
        assert_eq!(
            span_lines(&edited, 0, "A\nB".len()),
            (1, vec!["A".to_string(), "B".to_string()])
        );
    }

    /// Byte offsets from `str::find` are char boundaries, and so is the
    /// end of each match — but the line scan slices independently, so
    /// multi-byte text on either side of the needle must not panic.
    #[test]
    fn multibyte_text_around_the_match_does_not_panic() {
        let c = "héllo → wörld\nmore ü\n";
        let hits = match_offsets(c, "→");
        assert_eq!(hits.len(), 1);
        let (line, before) = span_lines(c, hits[0], "→".len());
        assert_eq!(line, 1);
        assert_eq!(before, vec!["héllo → wörld".to_string()]);
        assert_eq!(apply(c, &hits, "→", "->"), "héllo -> wörld\nmore ü\n");
    }

    /// The range clamp: a zero-length replacement at end-of-file must
    /// not slice past the end.
    #[test]
    fn a_range_running_to_the_end_is_clamped() {
        let c = "tail";
        assert_eq!(span_lines(c, 4, 0), (1, vec!["tail".to_string()]));
        assert_eq!(span_lines(c, 0, 99), (1, vec!["tail".to_string()]));
    }

    #[test]
    fn usage_errors_are_rejected_before_any_io() {
        assert!(parse_args(&s(&["f"])).is_err());
        assert!(parse_args(&s(&["f", "a"])).is_err());
        assert!(parse_args(&s(&["f", "a", "b", "c"])).is_err());
        assert!(parse_args(&s(&["f", "", "b"])).is_err());
        assert!(parse_args(&s(&["f", "a", "a"])).is_err());
        assert!(parse_args(&s(&["--nope", "f", "a", "b"])).is_err());
    }

    #[test]
    fn help_is_a_success_not_an_error() {
        assert!(parse_args(&s(&["--help"])).unwrap().is_none());
        assert!(parse_args(&s(&["-h"])).unwrap().is_none());
    }

    #[test]
    fn double_dash_lets_old_and_new_start_with_a_dash() {
        let o = parse_args(&s(&["--", "f", "--a", "--b"]))
            .unwrap()
            .unwrap();
        assert_eq!(o.old, "--a");
        assert_eq!(o.new, "--b");
        assert!(!o.all);
    }

    #[test]
    fn flags_come_before_the_positionals() {
        let o = parse_args(&s(&["--all", "f", "a", "b"])).unwrap().unwrap();
        assert!(o.all);
        let o = parse_args(&s(&["-n", "f", "a", "b"])).unwrap().unwrap();
        assert!(o.dry_run);
        let o = parse_args(&s(&["-n", "--all", "f", "a", "b"]))
            .unwrap()
            .unwrap();
        assert!(o.dry_run && o.all);

        // A trailing flag is a usage error that says what to do.
        let e = parse_args(&s(&["f", "a", "b", "--all"])).unwrap_err();
        assert!(e.contains("flags go BEFORE FILE"), "{e}");
    }

    /// OLD and NEW are arbitrary text. `->`, `-1`, `--` inside a string:
    /// none of them are options once the positionals have started.
    #[test]
    fn old_and_new_may_look_like_flags() {
        for (old, new) in [("=>", "->"), ("1", "-1"), ("a", "--all"), ("-x", "-y")] {
            let o = parse_args(&s(&["f", old, new]))
                .unwrap_or_else(|e| panic!("{old:?} -> {new:?} rejected: {e}"))
                .unwrap();
            assert_eq!((o.old.as_str(), o.new.as_str()), (old, new));
        }
    }

    #[test]
    fn report_groups_merge_hits_that_share_a_line() {
        let c = "aa aa\nzz\naa\n";
        let hits = match_offsets(c, "aa");
        assert_eq!(hits.len(), 3);
        let g = report_groups(c, &hits, 2, 2);
        assert_eq!(g.len(), 2, "the first two hits share line 1");
        assert_eq!((g[0].before_at, g[0].before_len), (0, 5));
        assert_eq!((g[1].before_at, g[1].before_len), (9, 2));
    }

    /// The `after` range must track the cumulative length delta, or the
    /// `+` side of a multi-hit report reads the wrong bytes.
    #[test]
    fn report_groups_shift_the_after_range_by_the_running_delta() {
        let c = "a\nza\n";
        let hits = match_offsets(c, "a");
        assert_eq!(hits.len(), 2);
        // "a" -> "LONG": +3 per replacement.
        let g = report_groups(c, &hits, 1, 4);
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].after_at, 0);
        assert_eq!(g[1].after_at, hits[1] + 3);

        let edited = apply(c, &hits, "a", "LONG");
        assert_eq!(edited, "LONG\nzLONG\n");
        assert_eq!(
            span_lines(&edited, g[1].after_at, g[1].after_len),
            (2, vec!["zLONG".to_string()])
        );
    }

    /// The re-check refuses rather than clobbering when the file moved
    /// under us. It is a narrowing, not a lock — but the common case
    /// (another writer landed between read and write) must not be a
    /// silent loss.
    #[test]
    fn a_file_that_changed_since_the_read_is_refused() {
        let dir = std::env::temp_dir().join(format!("mix-edit-stale-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let p = dir.join("s.txt");
        fs::write(&p, "one\n").unwrap();
        let before = fs::metadata(&p).unwrap();

        // Someone else rewrites it (different length, so no reliance on
        // mtime granularity).
        fs::write(&p, "a completely different body\n").unwrap();

        let err = write_atomic(&p, "mine\n", Some(&before)).unwrap_err();
        assert!(err.to_string().contains("changed under us"), "{err}");
        assert_eq!(
            fs::read_to_string(&p).unwrap(),
            "a completely different body\n",
            "the other writer's content must survive"
        );

        // No temp file left behind by the refusal.
        let leftovers: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("mix-edit"))
            .collect();
        assert!(leftovers.is_empty(), "temp left: {leftovers:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unchanged_file_passes_the_recheck() {
        let dir = std::env::temp_dir().join(format!("mix-edit-fresh-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let p = dir.join("s.txt");
        fs::write(&p, "one\n").unwrap();
        let before = fs::metadata(&p).unwrap();
        write_atomic(&p, "two\n", Some(&before)).expect("unchanged file must be writable");
        assert_eq!(fs::read_to_string(&p).unwrap(), "two\n");
        let _ = fs::remove_dir_all(&dir);
    }

    /// A pre-placed file at the temp path must not be followed or
    /// truncated — `create_new` is what refuses it. Proven by handing
    /// `write_atomic` a path whose directory already holds every name it
    /// could pick is impractical, so this exercises the primitive
    /// directly: the same OpenOptions must refuse an existing file.
    #[test]
    fn the_temp_open_refuses_an_existing_path() {
        let dir = std::env::temp_dir().join(format!("mix-edit-excl-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let squatter = dir.join("squatter");
        fs::write(&squatter, "PRECIOUS").unwrap();

        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let err = opts.open(&squatter).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(&squatter).unwrap(), "PRECIOUS");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_write_preserves_mode() {
        let dir = std::env::temp_dir().join(format!("mix-edit-mode-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let p = dir.join("s.mix");
        fs::write(&p, "old\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
            write_atomic(&p, "new\n", None).unwrap();
            let mode = fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o755, "executable bit must survive the edit");
        }
        assert_eq!(fs::read_to_string(&p).unwrap(), "new\n");
        let _ = fs::remove_dir_all(&dir);
    }
}
