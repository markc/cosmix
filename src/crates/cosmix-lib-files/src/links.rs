//! Link extraction from a markdown body (invariant #5 / §4.5).
//!
//! Dependency-free scanners for `[[wikilink]]` and `[text](target)` markdown links.
//! Image links (`![alt](src)`) are skipped — they are attachments, not document
//! links. Resolution of a `target` to an opaque id (and broken-link flagging) is
//! the daemon's job (it needs the index); this layer only extracts the raw refs.

use serde::Serialize;

/// Which syntax a link came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum LinkKind {
    /// `[[target]]` wiki-style.
    Wikilink,
    /// `[text](target)` markdown-style.
    MdLink,
}

/// A raw extracted reference (unresolved).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Link {
    /// The syntax it was written in.
    pub kind: LinkKind,
    /// The raw target (wiki name, relative path, slug, or URL). Aliases (`|`) and
    /// anchors (`#`) are stripped from wikilinks; a markdown link title is dropped.
    pub target: String,
}

/// Extract all wiki and markdown links from a body, in document order
/// (wikilinks first, then markdown links).
pub fn extract_links(body: &str) -> Vec<Link> {
    let mut out = Vec::new();
    extract_wikilinks(body, &mut out);
    extract_mdlinks(body, &mut out);
    out
}

fn extract_wikilinks(body: &str, out: &mut Vec<Link>) {
    let mut i = 0;
    while let Some(rel) = body[i..].find("[[") {
        let start = i + rel + 2;
        match body[start..].find("]]") {
            Some(crel) => {
                let inner = &body[start..start + crel];
                let target = wikilink_target(inner);
                if !target.is_empty() {
                    out.push(Link {
                        kind: LinkKind::Wikilink,
                        target,
                    });
                }
                i = start + crel + 2;
            }
            None => break,
        }
    }
}

fn wikilink_target(inner: &str) -> String {
    inner
        .split('|')
        .next()
        .unwrap_or("")
        .split('#')
        .next()
        .unwrap_or("")
        .trim()
        .to_string()
}

fn extract_mdlinks(body: &str, out: &mut Vec<Link>) {
    let bytes = body.as_bytes();
    let mut i = 0;
    while let Some(rel) = body[i..].find("](") {
        let bracket = i + rel; // index of ']'
        if let Some(open) = body[..bracket].rfind('[') {
            let is_image = open > 0 && bytes[open - 1] == b'!';
            let url_start = bracket + 2; // skip "]("
            if let Some(crel) = body[url_start..].find(')') {
                if !is_image {
                    // Drop an optional markdown link title: `(url "title")`.
                    let inside = &body[url_start..url_start + crel];
                    let target = inside.split_whitespace().next().unwrap_or("").to_string();
                    if !target.is_empty() {
                        out.push(Link {
                            kind: LinkKind::MdLink,
                            target,
                        });
                    }
                }
                i = url_start + crel + 1;
                continue;
            }
        }
        i = bracket + 2;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn targets(body: &str) -> Vec<(LinkKind, String)> {
        extract_links(body)
            .into_iter()
            .map(|l| (l.kind, l.target))
            .collect()
    }

    #[test]
    fn extracts_both_syntaxes() {
        let got = targets("See [[other-note]] and [a link](other.md).");
        assert_eq!(
            got,
            vec![
                (LinkKind::Wikilink, "other-note".to_string()),
                (LinkKind::MdLink, "other.md".to_string()),
            ]
        );
    }

    #[test]
    fn wikilink_alias_and_anchor_stripped() {
        assert_eq!(
            targets("[[name|Alias]]"),
            vec![(LinkKind::Wikilink, "name".to_string())]
        );
        assert_eq!(
            targets("[[note#section]]"),
            vec![(LinkKind::Wikilink, "note".to_string())]
        );
    }

    #[test]
    fn images_are_skipped() {
        assert!(extract_links("![alt](pic.png)").is_empty());
        // A real link right after an image still extracts.
        assert_eq!(
            targets("![alt](pic.png) then [doc](d.md)"),
            vec![(LinkKind::MdLink, "d.md".to_string())]
        );
    }

    #[test]
    fn mdlink_title_dropped_and_external_kept() {
        assert_eq!(
            targets("[x](u.md \"Title\")"),
            vec![(LinkKind::MdLink, "u.md".to_string())]
        );
        assert_eq!(
            targets("[site](https://example.com)"),
            vec![(LinkKind::MdLink, "https://example.com".to_string())]
        );
    }

    #[test]
    fn no_links_yields_empty() {
        assert!(extract_links("plain text, no links").is_empty());
    }
}
