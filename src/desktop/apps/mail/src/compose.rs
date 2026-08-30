//! The compose pane.
//!
//! Reply quoting is the only interesting part: it is built from the reader's
//! **projection**, not from the raw body. That is deliberate — the projection
//! is already sanitised, already resolved to blocks and runs, and already
//! carries quote depth, so quoting it cannot smuggle markup into the reply.
//!
//! It does *not* follow that the quote matches what was on screen, and an
//! earlier version of this comment claimed it did. `ProjectedBlock::text()`
//! concatenates the block's spans and special-cases only `Rule`
//! (`ctk/src/body_view.rs:998`); a list item's marker lives in the private
//! `list_item_start` field, whose type `ProjectedListItem` is private too
//! (`:994`, `:1073`). The renderer draws those markers and nothing in
//! `BodyProjection` exposes them, so quoting the newsletter fixture turns three
//! bulleted items into three unmarked lines.
//!
//! Precisely: unreachable *through the projection*, not unreachable full stop.
//! `SanitizedBody::sanitized_html()` and `SanitizedHtml::as_str()` are public
//! (`ctk/src/body_view.rs:426`, `:343`), so an app could parse that HTML itself
//! and recover `<ul>/<ol>/<li>` context. Rejected, not overlooked: it means a
//! second HTML parser in the app, duplicating CTK's projection semantics (quote
//! depth, block splitting, whitespace) well enough that the quote and the pane
//! agree — and the two would then drift independently. The fix belongs in CTK:
//! a public accessor, or a `text()` that includes the marker. Recorded as a
//! finding rather than worked around, and
//! `quoting_drops_list_markers_a_known_ctk_gap` pins the current behaviour so
//! the day CTK closes it, this fails loudly instead of quietly staying wrong.
//!
//! Known gap, stated rather than hidden: the app does not enable ctk's
//! `system-clipboard` feature, so copy and paste in this pane use Bevy's
//! in-process clipboard and do not reach the desktop clipboard. That is a
//! one-line feature flag whose cost is a host clipboard backend binding; it is
//! left off until the slice needs it, and the omission is a choice with a
//! reason, not an oversight.

use ctk::prelude::{BodyProjection, ProjectedBlockKind};

/// Longest quoted reply the compose pane will prefill.
///
/// A quoted 400-paragraph digest helps nobody and would push the editor into
/// its own performance question, which is not what this slice is measuring.
pub const MAX_QUOTE_CHARS: usize = 4_000;

/// Build the quoted reply body: an attribution line naming `sender`, followed by
/// `projection`'s blocks quoted at their own nesting depth.
///
/// The subject is not involved — [`reply_subject`] handles that separately.
pub fn reply_quote(sender: &str, projection: &BodyProjection) -> String {
    let mut quoted = String::with_capacity(512);
    quoted.push_str(&format!("\n\n{sender} wrote:\n"));
    let mut budget = MAX_QUOTE_CHARS;
    let mut truncated = false;

    for block in &projection.blocks {
        if matches!(block.kind, ProjectedBlockKind::Rule) {
            continue;
        }
        let text = block.text();
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        // One extra '>' per quote level preserves the nesting the reader shows.
        let marker = ">".repeat(block.quote_depth + 1);
        for line in text.lines() {
            let line = line.trim_end();
            let cost = marker.len() + 1 + line.chars().count() + 1;
            if cost > budget {
                truncated = true;
                break;
            }
            budget -= cost;
            quoted.push_str(&marker);
            quoted.push(' ');
            quoted.push_str(line);
            quoted.push('\n');
        }
        if truncated {
            break;
        }
    }

    if truncated {
        quoted.push_str("> […]\n");
    }
    quoted
}

/// Subject line for a reply, without stacking `Re:` prefixes.
pub fn reply_subject(subject: &str) -> String {
    let trimmed = subject.trim();
    // Case-insensitive because the prefix arrives however the sending client
    // felt like writing it, and "Re: RE: Re:" is the classic failure.
    // Taken by characters, not bytes: `&trimmed[..3]` panics on a subject
    // whose first character is multi-byte, which is most non-Latin mail.
    let prefix: String = trimmed.chars().take(3).collect();
    if prefix.eq_ignore_ascii_case("re:") {
        return trimmed.to_string();
    }
    format!("Re: {trimmed}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ctk::prelude::{project_body, BodySource};

    fn projection(html: &str) -> BodyProjection {
        project_body(&BodySource::Html(html.to_string()).sanitize())
    }

    #[test]
    fn quote_markers_track_nesting_depth() {
        let projection = projection(
            "<p>outer</p><blockquote><p>inner</p><blockquote><p>deepest</p></blockquote></blockquote>",
        );
        let quoted = reply_quote("A Colleague", &projection);
        assert!(quoted.contains("> outer"));
        assert!(quoted.contains(">> inner"));
        assert!(quoted.contains(">>> deepest"));
    }

    #[test]
    fn quoting_is_bounded_and_says_so() {
        let long = format!(
            "<html><body>{}</body></html>",
            "<p>filler line</p>".repeat(2_000)
        );
        let quoted = reply_quote("Build Robot", &projection(&long));
        assert!(quoted.chars().count() < MAX_QUOTE_CHARS + 256);
        assert!(
            quoted.ends_with("> […]\n"),
            "a truncated quote must show that it was truncated"
        );
    }

    #[test]
    fn an_empty_body_still_produces_an_attribution_line() {
        let quoted = reply_quote("Nobody", &projection("<html><body></body></html>"));
        assert!(quoted.contains("Nobody wrote:"));
    }

    #[test]
    fn re_prefixes_do_not_stack() {
        assert_eq!(reply_subject("Deploy plan"), "Re: Deploy plan");
        assert_eq!(reply_subject("Re: Deploy plan"), "Re: Deploy plan");
        assert_eq!(reply_subject("RE: Deploy plan"), "RE: Deploy plan");
        assert_eq!(reply_subject("  re: Deploy plan  "), "re: Deploy plan");
    }

    #[test]
    fn a_short_subject_is_not_mistaken_for_a_prefix() {
        assert_eq!(reply_subject("Hi"), "Re: Hi");
        assert_eq!(reply_subject(""), "Re: ");
    }

    #[test]
    fn a_multibyte_subject_does_not_panic_on_the_prefix_check() {
        // Three bytes long but one character: byte slicing would panic here.
        assert_eq!(reply_subject("日"), "Re: 日");
        assert_eq!(reply_subject("メール件名"), "Re: メール件名");
        assert_eq!(reply_subject("re: メール件名"), "re: メール件名");
    }

    #[test]
    fn markup_cannot_reach_the_reply_through_the_projection() {
        let quoted = reply_quote(
            "Attacker",
            &projection("<p>plain<script>alert(1)</script><b>bold</b></p>"),
        );
        assert!(!quoted.contains("<script"));
        assert!(!quoted.contains("alert(1)"));
        assert!(quoted.contains("bold"));
    }

    #[test]
    fn quoting_drops_list_markers_a_known_ctk_gap() {
        // Pins current behaviour, does not endorse it. `ProjectedBlock::text()`
        // cannot see `list_item_start` (private field, private type), so the
        // quote loses list structure the reader displays. When CTK exposes the
        // marker this assertion fails — which is the point: the gap should not
        // be able to close silently while the quote stays wrong.
        let quoted = reply_quote(
            "Sender",
            &projection("<ul><li>first item</li><li>second item</li></ul>"),
        );
        assert!(quoted.contains("first item"), "item text must survive");
        assert!(quoted.contains("second item"));
        assert!(
            !quoted.contains("• first item") && !quoted.contains("- first item"),
            "if a marker now reaches the quote, CTK exposed it — delete this \
             test, the honest gap in the module header, and Finding 5 in the \
             slice write-up (cmctl:_doc/2026-07-31-mail-vertical-slice-findings.md \
             — a private repo; this one has no _doc/ and must not grow one).  \
             Got: {quoted}"
        );
    }
}
