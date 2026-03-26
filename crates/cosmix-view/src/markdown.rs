use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use std::path::PathBuf;

use crate::dot;

/// Render GFM markdown to HTML, resolving relative image paths against `base_dir`.
/// Fenced blocks tagged `dot` are rendered as inline SVG diagrams.
pub fn render_gfm(markdown: &str, base_dir: Option<&PathBuf>) -> String {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_GFM);
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_FOOTNOTES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    opts.insert(Options::ENABLE_SMART_PUNCTUATION);
    opts.insert(Options::ENABLE_HEADING_ATTRIBUTES);
    opts.insert(Options::ENABLE_DEFINITION_LIST);

    let parser = Parser::new_ext(markdown, opts);

    // We need to intercept ```dot blocks and render them as SVG,
    // and also rewrite image URLs. Process events manually.
    let mut html_output = String::with_capacity(markdown.len() * 2);
    let mut in_dot_block = false;
    let mut dot_source = String::new();

    let events: Vec<Event> = parser.collect();
    let mut filtered = Vec::with_capacity(events.len());

    let mut i = 0;
    while i < events.len() {
        match &events[i] {
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(lang)))
                if lang.as_ref() == "dot" =>
            {
                in_dot_block = true;
                dot_source.clear();
                i += 1;
                continue;
            }
            Event::End(TagEnd::CodeBlock) if in_dot_block => {
                in_dot_block = false;
                // Render DOT to SVG and inject as raw HTML
                let svg_html = match dot::render_dot(&dot_source) {
                    Ok(svg) => format!(
                        "<div class=\"dot-diagram\">{svg}</div>"
                    ),
                    Err(e) => format!(
                        "<pre class=\"dot-error\">DOT render error: {}</pre>",
                        html_escape(&e)
                    ),
                };
                filtered.push(Event::Html(svg_html.into()));
                i += 1;
                continue;
            }
            Event::Text(text) if in_dot_block => {
                dot_source.push_str(text);
                i += 1;
                continue;
            }
            // Rewrite relative image URLs
            Event::Start(Tag::Image { link_type, dest_url, title, id }) => {
                let resolved = resolve_url(dest_url, base_dir);
                filtered.push(Event::Start(Tag::Image {
                    link_type: *link_type,
                    dest_url: resolved.into(),
                    title: title.clone(),
                    id: id.clone(),
                }));
                i += 1;
                continue;
            }
            _ => {}
        }
        filtered.push(events[i].clone());
        i += 1;
    }

    pulldown_cmark::html::push_html(&mut html_output, filtered.into_iter());
    html_output
}

/// Resolve a URL: if it's a relative path and base_dir is given, convert to data: URI.
fn resolve_url(url: &str, base_dir: Option<&PathBuf>) -> String {
    if url.contains("://") || url.starts_with("data:") {
        return url.to_string();
    }
    if let Some(base) = base_dir {
        let resolved = base.join(url);
        if resolved.exists() {
            if let Ok(data) = std::fs::read(&resolved) {
                use base64::{Engine, engine::general_purpose::STANDARD};
                let mime = crate::mime_from_ext(&resolved);
                let b64 = STANDARD.encode(&data);
                return format!("data:{mime};base64,{b64}");
            }
        }
    }
    url.to_string()
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}
