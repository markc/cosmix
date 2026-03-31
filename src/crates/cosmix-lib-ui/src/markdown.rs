use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use std::path::PathBuf;

/// Render GFM markdown to HTML.
///
/// - Resolves relative image paths to data: URIs against `base_dir`
/// - Renders ` ```dot ` fenced blocks as inline SVG if `dot_renderer` is provided
pub fn render_gfm(
    markdown: &str,
    base_dir: Option<&PathBuf>,
    dot_renderer: Option<fn(&str) -> Result<String, String>>,
) -> String {
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

    let mut in_dot_block = false;
    let mut dot_source = String::new();

    let events: Vec<Event> = parser.collect();
    let mut filtered = Vec::with_capacity(events.len());

    let mut i = 0;
    while i < events.len() {
        match &events[i] {
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(lang)))
                if lang.as_ref() == "dot" && dot_renderer.is_some() =>
            {
                in_dot_block = true;
                dot_source.clear();
                i += 1;
                continue;
            }
            Event::End(TagEnd::CodeBlock) if in_dot_block => {
                in_dot_block = false;
                let render = dot_renderer.unwrap();
                let svg_html = match render(&dot_source) {
                    Ok(svg) => format!("<div class=\"dot-diagram\">{svg}</div>"),
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
            Event::Start(Tag::Image { link_type, dest_url, title, id }) => {
                let resolved = resolve_image_url(dest_url, base_dir);
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

    let mut html_output = String::with_capacity(markdown.len() * 2);
    pulldown_cmark::html::push_html(&mut html_output, filtered.into_iter());
    html_output
}

/// Simple markdown to HTML (no DOT, no image resolution). For email body rendering etc.
pub fn render_simple(markdown: &str) -> String {
    let opts = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS;
    let parser = Parser::new_ext(markdown, opts);
    let mut html_output = String::new();
    pulldown_cmark::html::push_html(&mut html_output, parser);
    html_output
}

/// Resolve relative image URL to a data: URI by reading the file and base64 encoding.
fn resolve_image_url(url: &str, base_dir: Option<&PathBuf>) -> String {
    if url.contains("://") || url.starts_with("data:") {
        return url.to_string();
    }
    if let Some(base) = base_dir {
        let resolved = base.join(url);
        if resolved.exists() {
            if let Ok(data) = std::fs::read(&resolved) {
                use base64::{Engine, engine::general_purpose::STANDARD};
                let mime = crate::util::mime_from_ext(&resolved);
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
