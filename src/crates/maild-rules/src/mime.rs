//! Message parsing + MIME views.
//!
//! Parsed once per classification; subsequent matchers see the same
//! `Message`. Body views (`plain`, `html`, `combined`) are precomputed
//! as `String`s after applying `EngineConfig.body_scan_bytes` so regex
//! matchers see a bounded haystack regardless of message size.
//!
//! Implicit `mime_parse_error` is signalled by `MimeViews::parse_failed`;
//! the engine consults this before evaluating the rule set.

use mail_parser::{HeaderValue, MessageParser, MimeHeaders, PartType};

use crate::config::EngineConfig;

/// Result of parsing one inbound message into the views the matchers
/// need. `parse_failed` is set when `mail-parser` returned `None`; in
/// that case the body views are empty and only the implicit
/// `mime_parse_error` rule will fire.
pub(crate) struct MimeViews {
    /// (lower-cased header name, value) pairs in document order. Capped
    /// per `EngineConfig.header_line_cap` and `header_line_byte_cap`.
    pub headers: Vec<(String, String)>,
    /// Decoded `text/plain` content, capped at `body_scan_bytes`.
    pub plain: String,
    /// Decoded `text/html` content (raw HTML markup), capped at
    /// `body_scan_bytes`.
    pub html: String,
    /// Plain + html concatenated (plain first), capped at
    /// `body_scan_bytes` of the *combined* size.
    pub combined: String,
    /// Attachment summaries: `(filename, content_type)` for non-inline
    /// parts. Used by `attachment_count` + `structure`.
    pub attachments: Vec<Attachment>,
    /// True when `text/html` is present and `text/plain` is not.
    pub html_only: bool,
    /// True when no `Message-ID:` header was observed.
    pub missing_message_id: bool,
    /// True when MIME nesting depth exceeds 4 levels (heuristic).
    pub deep_nesting: bool,
    /// True when `mail-parser` failed to parse the message.
    pub parse_failed: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct Attachment {
    /// Carried for diagnostics / future matcher use; the `attachment_count`
    /// matcher only reads `executable` in v1.
    #[allow(dead_code)]
    pub filename: String,
    /// Carried for diagnostics / future matcher use; the `attachment_count`
    /// matcher only reads `executable` in v1.
    #[allow(dead_code)]
    pub content_type: String,
    pub executable: bool,
}

/// File extensions treated as executable for `structure` /
/// `attachment_count` rules.
pub(crate) const EXECUTABLE_EXTENSIONS: &[&str] =
    &["exe", "scr", "bat", "cmd", "com", "pif", "vbs", "js", "jar"];

/// Content types treated as executable.
pub(crate) const EXECUTABLE_CONTENT_TYPES: &[&str] =
    &["application/x-msdownload", "application/x-executable"];

impl MimeViews {
    pub(crate) fn parse(raw: &[u8], cfg: &EngineConfig) -> Self {
        let parser = MessageParser::default();
        let Some(msg) = parser.parse(raw) else {
            return Self {
                headers: Vec::new(),
                plain: String::new(),
                html: String::new(),
                combined: String::new(),
                attachments: Vec::new(),
                html_only: false,
                missing_message_id: true,
                deep_nesting: false,
                parse_failed: true,
            };
        };

        // ---- Headers (top-level only — the spec applies header rules to
        // the outer envelope headers, not to nested part headers). ----
        let mut headers = Vec::new();
        let mut missing_message_id = true;
        for h in msg.headers().iter().take(cfg.header_line_cap as usize) {
            let name_lc = h.name.as_str().to_ascii_lowercase();
            if name_lc == "message-id" {
                missing_message_id = false;
            }
            let value = render_header_value(h.value());
            let value = if value.len() > cfg.header_line_byte_cap {
                value[..cfg.header_line_byte_cap].to_string()
            } else {
                value
            };
            headers.push((name_lc, value));
        }

        // ---- Body views ----
        let cap = cfg.body_scan_bytes;
        let mut plain = String::new();
        let mut html = String::new();
        let mut attachments = Vec::new();
        let mut has_text = false;
        let mut has_html = false;

        for part in msg.parts.iter() {
            match &part.body {
                PartType::Text(text) => {
                    has_text = true;
                    if plain.len() < cap {
                        let remaining = cap - plain.len();
                        let s = text.as_ref();
                        let take = s.len().min(remaining);
                        plain.push_str(&s[..take]);
                    }
                }
                PartType::Html(htxt) => {
                    has_html = true;
                    if html.len() < cap {
                        let remaining = cap - html.len();
                        let s = htxt.as_ref();
                        let take = s.len().min(remaining);
                        html.push_str(&s[..take]);
                    }
                }
                PartType::Binary(_) | PartType::InlineBinary(_) => {
                    // Treat as attachment if it has an attachment_name.
                    if let Some(name) = part.attachment_name() {
                        let ct = if let Some(c) = part.content_type() {
                            let mut s = c.ctype().to_ascii_lowercase();
                            if let Some(sub) = c.subtype() {
                                s.push('/');
                                s.push_str(&sub.to_ascii_lowercase());
                            }
                            s
                        } else {
                            "application/octet-stream".to_string()
                        };
                        let filename = name.to_string();
                        let ext = filename
                            .rsplit('.')
                            .next()
                            .unwrap_or("")
                            .to_ascii_lowercase();
                        let executable = EXECUTABLE_EXTENSIONS.iter().any(|e| *e == ext)
                            || EXECUTABLE_CONTENT_TYPES.iter().any(|c| *c == ct);
                        attachments.push(Attachment {
                            filename,
                            content_type: ct,
                            executable,
                        });
                    }
                }
                _ => {}
            }
        }
        // mail-parser flattens parts into `msg.parts`, so we can't
        // measure tree depth directly. Use the flat part count as a
        // cheap structural-anomaly proxy: > 16 parts → deep nesting.
        let max_depth = msg.parts.len() as u32;

        let combined = {
            let mut c = String::with_capacity((plain.len() + html.len()).min(cap));
            let take_plain = plain.len().min(cap);
            c.push_str(&plain[..take_plain]);
            if c.len() < cap {
                let remaining = cap - c.len();
                let take_html = html.len().min(remaining);
                c.push_str(&html[..take_html]);
            }
            c
        };

        Self {
            headers,
            plain,
            html,
            combined,
            attachments,
            html_only: has_html && !has_text,
            missing_message_id,
            deep_nesting: max_depth > 4 || msg.parts.len() > 16,
            parse_failed: false,
        }
    }
}

fn render_header_value(value: &HeaderValue<'_>) -> String {
    match value {
        HeaderValue::Text(s) => s.to_string(),
        HeaderValue::TextList(list) => list
            .iter()
            .map(|s| s.as_ref())
            .collect::<Vec<_>>()
            .join(", "),
        HeaderValue::Address(_) | HeaderValue::DateTime(_) | HeaderValue::ContentType(_) => {
            // Render addresses/dates/content-type via Debug — sufficient
            // for substring/regex matchers; rules wanting structured
            // address comparison can use the alignment kind in future.
            format!("{:?}", value)
        }
        HeaderValue::Received(_) => format!("{:?}", value),
        _ => String::new(),
    }
}
