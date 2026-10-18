//! RFC 5256 §2.1 "base subject" normalization.
//!
//! Used as the threading-key fallback after In-Reply-To / References
//! resolution misses (JMAP §4.1.5, Phase 8e gate 4). The output is
//! stored in `mail_envelopes.normalized_subject` (cosmix-mds v1.7)
//! and looked up via the `mail_envelopes_normsubj_idx` index when
//! resolving the `mail_threads.thread_id` for a freshly-created
//! email.
//!
//! ## Algorithm (RFC 5256 §2.1, verbatim)
//!
//! ```text
//! subj-refwd      = ("re" / ("fw" ["d"])) *WSP [subj-blob] ":"
//! subj-blob       = "[" *BLOBCHAR "]" *WSP
//! subj-fwd-hdr    = "[fwd:"
//! subj-fwd-trl    = "]"
//! subj-leader     = (*subj-blob subj-refwd) *WSP
//! subj-trailer    = "(fwd)" / WSP
//! BLOBCHAR        = %x01-5a / %x5c / %x5e-7f
//! ```
//!
//! 1. Convert RFC 2047 encoded-words to UTF-8 (already done by
//!    `mail_parser` upstream — the caller passes a decoded subject).
//!    Convert tabs / continuations / multi-WSP runs to a single
//!    space.
//! 2. Strip trailing `subj-trailer` repeatedly.
//! 3. Strip leading `subj-leader` (zero or more `subj-blob` then a
//!    `subj-refwd`, then any whitespace).
//! 4. If a `subj-blob` prefix remains and removing it leaves
//!    non-empty, strip it.
//! 5. Repeat steps 3+4 until no match.
//! 6. If the result is wrapped in `[fwd: ... ]`, strip the wrapper
//!    and restart from step 2.
//! 7. The remaining text is the base subject; this implementation
//!    lowercases it for case-insensitive equality matching at lookup.
//!
//! ## Why lowercase
//!
//! RFC 5256 §2.1 specifies case-insensitive *comparison* of the
//! base subject. Storing the lowercased form in the index column
//! lets the lookup be a plain `WHERE normalized_subject = ?`
//! without `COLLATE NOCASE` clauses (which would defeat the BTREE
//! index). The lowercasing happens once at write time; readers
//! lowercase their query key before lookup.
//!
//! ## Empty result
//!
//! Returns `""` when the input is empty, whitespace-only, or
//! reduces to nothing after stripping (e.g. `"Re:"` alone). Callers
//! MUST treat empty as "no fallback key" and skip the index lookup
//! — otherwise every empty-subject email would cluster into one
//! giant thread, which is exactly the JMAP §4.1.5 "avoid splitting
//! / falsely joining" failure mode.

/// Compute the RFC 5256 §2.1 base subject, lowercased for indexed
/// equality matching.
///
/// Returns `""` when no meaningful threading key remains.
pub(super) fn normalize_subject(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }

    let collapsed = collapse_whitespace(s);
    let mut cur = collapsed.trim().to_string();

    // Bounded outer loop: each iteration either makes progress
    // (strips at least one character) or breaks. The hard cap is a
    // belt-and-braces safeguard against pathological inputs that
    // could otherwise oscillate.
    for _ in 0..256 {
        cur = strip_trailers(&cur);
        let after_leaders = strip_leaders(&cur);
        if let Some(inner) = strip_fwd_wrapper(&after_leaders) {
            cur = inner;
            continue;
        }
        if after_leaders == cur {
            break;
        }
        cur = after_leaders;
    }

    cur.to_lowercase()
}

/// Convert tabs / CR / LF to spaces and collapse runs of whitespace
/// into a single space. Equivalent to the RFC 5256 §2.1 step-1
/// "convert all tabs and continuations to space; convert all
/// multiple spaces to a single space."
fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_ws = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !prev_ws {
                out.push(' ');
                prev_ws = true;
            }
        } else {
            out.push(c);
            prev_ws = false;
        }
    }
    out
}

/// Strip trailing `subj-trailer` (= "(fwd)" / WSP) repeatedly.
///
/// "(fwd)" matching is ASCII-case-insensitive — every other RFC 5256
/// §2.1 literal in this module already treats case as folded (refwd
/// tokens, [fwd: ... ] wrapper), and real-world MUAs emit the trailer
/// in mixed case (e.g. "(FWD)"). Treating it case-sensitively would
/// leave non-empty residue that the fallback's empty-subject skip
/// can no longer guard against.
fn strip_trailers(s: &str) -> String {
    let mut cur = s;
    loop {
        let trimmed = cur.trim_end();
        if trimmed.len() >= 5
            && trimmed.is_char_boundary(trimmed.len() - 5)
            && trimmed[trimmed.len() - 5..].eq_ignore_ascii_case("(fwd)")
        {
            cur = &trimmed[..trimmed.len() - 5];
            continue;
        }
        if trimmed.len() < cur.len() {
            cur = trimmed;
            continue;
        }
        return cur.to_string();
    }
}

/// Strip leading `subj-leader = (*subj-blob subj-refwd) *WSP`.
/// Returns the input unchanged if no leader matches.
fn strip_leaders(s: &str) -> String {
    // Repeatedly strip a (blob* refwd ws*) prefix while one matches.
    // RFC 5256 step 5: "Repeat (3) and (4) until no matches remain"
    // — but step 3 is *itself* a leader that may include blobs; the
    // outer loop in `normalize_subject` handles step-4 standalone
    // blob stripping after each leader pass.
    let mut cur = s.to_string();
    loop {
        let after = strip_leader_once(&cur);
        if after == cur {
            // No leader matched — try stripping a bare subj-blob
            // (step 4) once, but only if removing it leaves a
            // non-empty base.
            if let Some(after_blob) = strip_blob_prefix(&cur)
                && !after_blob.trim().is_empty()
            {
                cur = after_blob;
                continue;
            }
            return cur;
        }
        cur = after;
    }
}

/// Try to strip a single `subj-leader = blob* refwd ws*`. Returns
/// the input unchanged if no `refwd` is found.
fn strip_leader_once(s: &str) -> String {
    let mut rest: String = s.trim_start().to_string();
    // Strip leading subj-blob* (each blob is "[…] *WSP").
    while let Some(after) = strip_blob_prefix(&rest) {
        if after.is_empty() {
            // A run of blobs that consumes everything is NOT a
            // valid leader (it lacks the refwd). Bail out.
            return s.to_string();
        }
        rest = after.trim_start().to_string();
    }
    // Now require a subj-refwd: ("re" | "fw" | "fwd") *WSP [blob] ":"
    let after_refwd = match strip_refwd(&rest) {
        Some(r) => r,
        None => return s.to_string(),
    };
    after_refwd.trim_start().to_string()
}

/// Strip a `subj-refwd = ("re" / "fw" / "fwd") *WSP [subj-blob] ":"`
/// from the start of `s`. Case-insensitive on the "re"/"fw"/"fwd"
/// token. Returns the remainder after the trailing ":" on success.
fn strip_refwd(s: &str) -> Option<String> {
    // Check "fwd" before "fw" so the longer match wins. The "fw"
    // and "re" two-byte tokens collapse to the same slice op; folding
    // them satisfies clippy::if_same_then_else without changing
    // semantics.
    let rest = if s.get(..3).is_some_and(|p| p.eq_ignore_ascii_case("fwd")) {
        &s[3..]
    } else if s
        .get(..2)
        .is_some_and(|p| p.eq_ignore_ascii_case("fw") || p.eq_ignore_ascii_case("re"))
    {
        &s[2..]
    } else {
        return None;
    };
    let rest = rest.trim_start();
    // Optional subj-blob between the refwd token and the colon.
    let rest: String = match strip_blob_prefix(rest) {
        Some(after) => after.trim_start().to_string(),
        None => rest.to_string(),
    };
    rest.strip_prefix(':').map(|r| r.to_string())
}

/// Strip a `subj-blob = "[" *BLOBCHAR "]" *WSP` prefix. Returns the
/// remainder after the `]` and any trailing whitespace, or `None`
/// if the input does not start with a well-formed blob.
///
/// RFC 5256 §2.1 defines `BLOBCHAR = %x01-5A / %x5C / %x5E-7F` — a
/// single ASCII byte other than `[` (0x5B) or `]` (0x5D) or NUL.
/// Reject any byte outside that range so non-ASCII bracketed prefixes
/// (e.g. `[日本語]`) are NOT treated as mailing-list blobs; otherwise
/// two unrelated emails like `[日本語] Hello` and `[中文] Hello` would
/// silently subject-cluster, which broadens fallback threading beyond
/// the spec.
fn strip_blob_prefix(s: &str) -> Option<String> {
    let s = s.strip_prefix('[')?;
    // BLOBCHAR forbids '[' and ']'; the first ']' closes the blob.
    let close = s.find(']')?;
    let inner = &s[..close];
    if !inner.bytes().all(is_blobchar) {
        return None;
    }
    Some(s[close + 1..].trim_start().to_string())
}

/// RFC 5256 §2.1 `BLOBCHAR = %x01-5A / %x5C / %x5E-7F`. Any byte
/// outside this set (NUL, '[', ']', or non-ASCII) disqualifies the
/// surrounding bracketed run from being a `subj-blob`.
fn is_blobchar(b: u8) -> bool {
    matches!(b, 0x01..=0x5A | 0x5C | 0x5E..=0x7F)
}

/// Strip a `"[fwd: ... ]"` wrapper if present. Returns the inner
/// text on success.
///
/// An empty / whitespace-only inner — e.g. `"[fwd:]"` or `"[fwd:  ]"`
/// — unwraps to `""`. The outer normalize_subject() loop treats that
/// as fully reduced, lower-casing yields the empty string, and the
/// threading-fallback caller skips index lookup on empty. Returning
/// `None` here would instead leave `"[fwd:]"` literally in the
/// normalized key, where it would bypass the empty-skip and falsely
/// cluster every `[fwd:]`-shaped subject into one thread.
fn strip_fwd_wrapper(s: &str) -> Option<String> {
    // Case-insensitive prefix/suffix check on the original — we slice
    // bytes back out of `s` so the inner segment preserves its
    // original case for downstream normalisation.
    let lower = s.to_ascii_lowercase();
    if !lower.starts_with("[fwd:") || !lower.ends_with(']') {
        return None;
    }
    let start = "[fwd:".len();
    let end = s.len().checked_sub("]".len())?;
    if end <= start {
        return Some(String::new());
    }
    Some(s[start..end].trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_returns_empty() {
        assert_eq!(normalize_subject(""), "");
        assert_eq!(normalize_subject("   "), "");
        assert_eq!(normalize_subject("\t \n"), "");
    }

    #[test]
    fn plain_subject_lowercased() {
        assert_eq!(normalize_subject("Hello World"), "hello world");
    }

    #[test]
    fn strips_single_re_prefix() {
        assert_eq!(normalize_subject("Re: Hello"), "hello");
        assert_eq!(normalize_subject("RE: Hello"), "hello");
        assert_eq!(normalize_subject("re: Hello"), "hello");
    }

    #[test]
    fn strips_recursive_re_prefixes() {
        assert_eq!(normalize_subject("Re: Re: Re: Hello"), "hello");
        assert_eq!(normalize_subject("Re:Re:Re:Hello"), "hello");
    }

    #[test]
    fn strips_fw_and_fwd_prefixes() {
        assert_eq!(normalize_subject("Fw: Hello"), "hello");
        assert_eq!(normalize_subject("Fwd: Hello"), "hello");
        assert_eq!(normalize_subject("FW: Hello"), "hello");
        assert_eq!(normalize_subject("FWD: Hello"), "hello");
    }

    #[test]
    fn strips_list_blob_then_refwd() {
        assert_eq!(normalize_subject("[list-name] Re: Hello"), "hello");
        assert_eq!(normalize_subject("[mlist] [other] Re: Hello"), "hello");
    }

    #[test]
    fn strips_standalone_list_blob() {
        // Step 4 — bare blob without a refwd, leaving non-empty base.
        assert_eq!(normalize_subject("[list-name] Hello"), "hello");
    }

    #[test]
    fn preserves_solo_blob_if_strip_would_empty() {
        // Step 4 only strips if the remainder is non-empty.
        assert_eq!(normalize_subject("[only-blob]"), "[only-blob]");
    }

    #[test]
    fn strips_fwd_trailer() {
        assert_eq!(normalize_subject("Hello (fwd)"), "hello");
        assert_eq!(normalize_subject("Hello (fwd) (fwd)"), "hello");
    }

    #[test]
    fn strips_fwd_wrapper() {
        // Step 6 — the [fwd: ... ] form Outlook still emits.
        assert_eq!(normalize_subject("[fwd: Hello]"), "hello");
        assert_eq!(normalize_subject("[fwd: Re: Hello]"), "hello");
        assert_eq!(normalize_subject("[FWD: Hello]"), "hello");
    }

    #[test]
    fn re_with_intervening_blob() {
        // RFC 5256 §2.1 grammar: refwd = (re|fw|fwd) *WSP [blob] ":"
        assert_eq!(normalize_subject("Re [foo]: Hello"), "hello");
    }

    #[test]
    fn whitespace_collapsed() {
        assert_eq!(normalize_subject("Re:\t\tHello\t  World"), "hello world");
        assert_eq!(normalize_subject("Re: Hello\r\n\tWorld"), "hello world");
    }

    #[test]
    fn empty_after_normalization() {
        // Input reduces to nothing — fallback callers must skip.
        assert_eq!(normalize_subject("Re:"), "");
        assert_eq!(normalize_subject("Re: Re: Fwd:"), "");
        assert_eq!(normalize_subject("(fwd) (fwd)"), "");
    }

    #[test]
    fn unicode_preserved_and_lowercased() {
        assert_eq!(normalize_subject("Re: Héllo Wörld"), "héllo wörld");
        // Common pitfall: ASCII case-folding works; full Unicode
        // case-folding is acceptable since we use `to_lowercase`.
        assert_eq!(normalize_subject("ΓΕΙΑ"), "γεια");
    }

    #[test]
    fn no_false_positive_on_word_starting_with_re() {
        // "Resource" must NOT be treated as "Re:" — the algorithm
        // requires the colon as the refwd terminator.
        assert_eq!(normalize_subject("Resource Planning"), "resource planning");
    }

    #[test]
    fn unmatched_bracket_left_alone() {
        // No closing ']' — not a valid subj-blob.
        assert_eq!(normalize_subject("[unclosed Hello"), "[unclosed hello");
    }

    #[test]
    fn deeply_nested_fwd_wrapper() {
        // Strip outer wrapper, restart at step 2, strip inner refwd.
        assert_eq!(normalize_subject("[fwd: [fwd: Re: Hello]]"), "hello");
    }

    #[test]
    fn does_not_panic_on_pathological_input() {
        // 500 stacked "Re:" prefixes — strip_leaders is itself a loop
        // that strips ALL refwd prefixes in a single outer-loop pass,
        // so the 256-iteration cap in normalize_subject() is not what
        // bounds this case. Pin "fully strips" rather than "doesn't
        // panic" so a future regression that breaks strip_leaders's
        // inner loop surfaces here.
        let pathological = "Re: ".repeat(500);
        assert_eq!(normalize_subject(&pathological), "");
    }

    #[test]
    fn empty_fwd_wrapper_normalizes_to_empty() {
        // MAJOR finding (Codex round 1): "[fwd:]" must NOT leak the
        // literal "[fwd:]" through to the index — callers depend on
        // the empty-result skip to avoid false subject-clustering of
        // every empty-wrapper email into one thread.
        assert_eq!(normalize_subject("[fwd:]"), "");
        assert_eq!(normalize_subject("[fwd:   ]"), "");
        assert_eq!(normalize_subject("[FWD:]"), "");
        // The wrapper-skip composes with leader/trailer skips too.
        assert_eq!(normalize_subject("[fwd: Re:]"), "");
    }

    #[test]
    fn fwd_trailer_is_case_insensitive() {
        // MAJOR finding (Codex round 1): the (fwd) trailer must match
        // case-insensitively, like every other RFC 5256 §2.1 literal
        // in this module — otherwise "(FWD)" leaves non-empty residue
        // that bypasses the empty-subject skip.
        assert_eq!(normalize_subject("Hello (FWD)"), "hello");
        assert_eq!(normalize_subject("Hello (Fwd)"), "hello");
        assert_eq!(normalize_subject("Hello (fWd)"), "hello");
        // Combined with other reducers.
        assert_eq!(normalize_subject("Re: Hello (FWD)"), "hello");
        // Pure "(FWD)" with no base reduces to empty.
        assert_eq!(normalize_subject("(FWD) (Fwd)"), "");
    }

    #[test]
    fn non_ascii_bracket_prefix_is_not_a_blob() {
        // MAJOR finding (Codex round 1): RFC 5256 §2.1 BLOBCHAR is
        // ASCII-only (%x01-5A / %x5C / %x5E-7F). A non-ASCII
        // bracketed prefix must NOT be stripped — otherwise distinct
        // subjects like "[日本語] Hello" and "[中文] Hello" would
        // falsely cluster.
        assert_eq!(normalize_subject("[日本語] Hello"), "[日本語] hello");
        assert_eq!(normalize_subject("[中文] Re: Hello"), "[中文] re: hello");
        // Distinct non-ASCII prefixes must yield distinct normalized
        // forms (the threading-key contract).
        assert_ne!(
            normalize_subject("[日本語] Hello"),
            normalize_subject("[中文] Hello")
        );
        // NUL inside a blob is also forbidden.
        assert_eq!(normalize_subject("[a\0b] Hello"), "[a\0b] hello");
        // Pure-ASCII blob still strips (regression guard).
        assert_eq!(normalize_subject("[list] Hello"), "hello");
    }
}
