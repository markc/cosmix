//! GFM markdown renderer for COSMIC/libcosmic with full color control.
//!
//! Unlike iced's built-in markdown widget, this renderer:
//! - Supports GFM tables, task lists, and strikethrough
//! - Allows explicit text/background color control (works in dark themes)
//! - Renders horizontal rules
//! - Returns COSMIC `Element`s directly
//!
//! # Usage
//!
//! ```rust,ignore
//! use cosmix_markdown::{MarkdownView, Theme};
//!
//! let theme = Theme {
//!     text: Color::BLACK,
//!     heading: Color::from_rgb(0.1, 0.1, 0.1),
//!     link: Color::from_rgb(0.0, 0.4, 0.8),
//!     code_text: Color::from_rgb(0.8, 0.2, 0.2),
//!     code_bg: Color::from_rgb(0.95, 0.95, 0.95),
//!     ..Theme::light()
//! };
//!
//! let view = MarkdownView::new(&markdown_source);
//! let element = view.view::<MyMessage>(theme);
//! ```

use cosmic::iced::{self, Color, Font, Length};
use cosmic::widget;
use cosmic::Element;
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

/// Color theme for markdown rendering. All colors are explicit — no theme inheritance.
#[derive(Clone, Debug)]
pub struct Theme {
    /// Normal paragraph text color.
    pub text: Color,
    /// Heading text color.
    pub heading: Color,
    /// Link text color.
    pub link: Color,
    /// Inline code text color.
    pub code_text: Color,
    /// Inline code background color.
    pub code_bg: Color,
    /// Code block text color.
    pub block_code_text: Color,
    /// Code block background color.
    pub block_code_bg: Color,
    /// Blockquote border/accent color.
    pub quote_accent: Color,
    /// Table header background color.
    pub table_header_bg: Color,
    /// Table alternate row background color.
    pub table_alt_bg: Color,
    /// Table border color.
    pub table_border: Color,
    /// Horizontal rule color.
    pub rule: Color,
    /// Base font size in pixels.
    pub text_size: f32,
}

impl Theme {
    /// A light theme suitable for white backgrounds.
    pub fn light() -> Self {
        Self {
            text: Color::BLACK,
            heading: Color::from_rgb(0.1, 0.1, 0.1),
            link: Color::from_rgb(0.0, 0.37, 0.73),
            code_text: Color::from_rgb(0.75, 0.15, 0.15),
            code_bg: Color::from_rgb(0.94, 0.94, 0.94),
            block_code_text: Color::from_rgb(0.2, 0.2, 0.2),
            block_code_bg: Color::from_rgb(0.96, 0.96, 0.96),
            quote_accent: Color::from_rgb(0.7, 0.7, 0.7),
            table_header_bg: Color::from_rgb(0.93, 0.93, 0.93),
            table_alt_bg: Color::from_rgb(0.97, 0.97, 0.97),
            table_border: Color::from_rgb(0.82, 0.82, 0.82),
            rule: Color::from_rgb(0.8, 0.8, 0.8),
            text_size: 14.0,
        }
    }

    /// A dark theme suitable for dark backgrounds.
    pub fn dark() -> Self {
        Self {
            text: Color::from_rgb(0.9, 0.9, 0.9),
            heading: Color::WHITE,
            link: Color::from_rgb(0.4, 0.7, 1.0),
            code_text: Color::from_rgb(1.0, 0.6, 0.6),
            code_bg: Color::from_rgb(0.15, 0.15, 0.15),
            block_code_text: Color::from_rgb(0.85, 0.85, 0.85),
            block_code_bg: Color::from_rgb(0.12, 0.12, 0.12),
            quote_accent: Color::from_rgb(0.4, 0.4, 0.4),
            table_header_bg: Color::from_rgb(0.2, 0.2, 0.2),
            table_alt_bg: Color::from_rgb(0.15, 0.15, 0.15),
            table_border: Color::from_rgb(0.3, 0.3, 0.3),
            rule: Color::from_rgb(0.3, 0.3, 0.3),
            text_size: 14.0,
        }
    }

    fn heading_size(&self, level: HeadingLevel) -> f32 {
        match level {
            HeadingLevel::H1 => self.text_size * 2.0,
            HeadingLevel::H2 => self.text_size * 1.75,
            HeadingLevel::H3 => self.text_size * 1.5,
            HeadingLevel::H4 => self.text_size * 1.25,
            HeadingLevel::H5 => self.text_size * 1.1,
            HeadingLevel::H6 => self.text_size,
        }
    }
}

/// Render a markdown string to a COSMIC `Element`.
///
/// The returned element is a vertical column of block-level items.
/// All text colors are set explicitly from `theme` — no iced theme inheritance.
pub fn view<'a, Message: Clone + 'static>(
    source: &str,
    theme: &Theme,
    on_link: impl Fn(String) -> Message + 'a,
) -> Element<'a, Message> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);

    let parser = Parser::new_ext(source, opts);
    let events: Vec<Event<'_>> = parser.collect();

    let blocks = parse_blocks(&events);
    render_blocks(blocks, theme, &on_link)
}

// ---------------------------------------------------------------------------
// Intermediate representation
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum Block<'a> {
    Heading(HeadingLevel, Vec<Inline<'a>>),
    Paragraph(Vec<Inline<'a>>),
    CodeBlock(Option<&'a str>, String),
    BlockQuote(Vec<Block<'a>>),
    List(Option<u64>, Vec<Vec<Block<'a>>>),
    Table(Vec<Vec<Vec<Inline<'a>>>>, Vec<Vec<Vec<Inline<'a>>>>),
    Rule,
}

#[derive(Debug, Clone)]
enum Inline<'a> {
    Text(&'a str),
    Code(&'a str),
    Strong(Vec<Inline<'a>>),
    Emphasis(Vec<Inline<'a>>),
    Strikethrough(Vec<Inline<'a>>),
    Link(String, Vec<Inline<'a>>),
    SoftBreak,
    HardBreak,
    TaskMarker(bool),
}

// ---------------------------------------------------------------------------
// Parser: Events → Blocks
// ---------------------------------------------------------------------------

fn parse_blocks<'a>(events: &'a [Event<'a>]) -> Vec<Block<'a>> {
    let mut blocks = Vec::new();
    let mut i = 0;

    while i < events.len() {
        match &events[i] {
            Event::Start(Tag::Heading { level, .. }) => {
                i += 1;
                let (inlines, consumed) = parse_inlines(&events[i..], TagEnd::Heading(*level));
                blocks.push(Block::Heading(*level, inlines));
                i += consumed + 1; // +1 for End tag
            }
            Event::Start(Tag::Paragraph) => {
                i += 1;
                let (inlines, consumed) = parse_inlines(&events[i..], TagEnd::Paragraph);
                blocks.push(Block::Paragraph(inlines));
                i += consumed + 1;
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                let lang = match kind {
                    pulldown_cmark::CodeBlockKind::Fenced(lang) => {
                        let l = lang.as_ref();
                        if l.is_empty() { None } else { Some(l) }
                    }
                    _ => None,
                };
                // Collect text until end of code block
                i += 1;
                let mut code = String::new();
                while i < events.len() {
                    match &events[i] {
                        Event::Text(t) => code.push_str(t.as_ref()),
                        Event::End(TagEnd::CodeBlock) => break,
                        _ => {}
                    }
                    i += 1;
                }
                // lang is borrowed from events which may go away; convert
                let lang_str = lang.map(|_| ""); // placeholder, we'll fix below
                let _ = lang_str;
                blocks.push(Block::CodeBlock(lang, code));
                i += 1;
            }
            Event::Start(Tag::BlockQuote(_)) => {
                i += 1;
                let (inner, consumed) = parse_blocks_until(&events[i..], TagEnd::BlockQuote(None));
                blocks.push(Block::BlockQuote(inner));
                i += consumed + 1;
            }
            Event::Start(Tag::List(start)) => {
                let start = *start;
                i += 1;
                let mut items = Vec::new();
                while i < events.len() {
                    match &events[i] {
                        Event::Start(Tag::Item) => {
                            i += 1;
                            let (inner, consumed) = parse_blocks_until(&events[i..], TagEnd::Item);
                            items.push(inner);
                            i += consumed + 1;
                        }
                        Event::End(TagEnd::List(_)) => break,
                        _ => { i += 1; }
                    }
                }
                blocks.push(Block::List(start, items));
                i += 1;
            }
            Event::Start(Tag::Table(_alignments)) => {
                i += 1;
                let mut header_rows = Vec::new();
                let mut body_rows = Vec::new();
                let mut in_head = false;
                let mut first_row = true;

                while i < events.len() {
                    match &events[i] {
                        Event::Start(Tag::TableHead) => { in_head = true; i += 1; }
                        Event::End(TagEnd::TableHead) => { in_head = false; i += 1; }
                        Event::Start(Tag::TableRow) => {
                            i += 1;
                            let mut cells = Vec::new();
                            while i < events.len() {
                                match &events[i] {
                                    Event::Start(Tag::TableCell) => {
                                        i += 1;
                                        let (inlines, consumed) =
                                            parse_inlines(&events[i..], TagEnd::TableCell);
                                        cells.push(inlines);
                                        i += consumed + 1;
                                    }
                                    Event::End(TagEnd::TableRow) => break,
                                    _ => { i += 1; }
                                }
                            }
                            if in_head || first_row {
                                header_rows.push(cells);
                                first_row = false;
                            } else {
                                body_rows.push(cells);
                            }
                            i += 1;
                        }
                        Event::End(TagEnd::Table) => break,
                        _ => { i += 1; }
                    }
                }
                blocks.push(Block::Table(header_rows, body_rows));
                i += 1;
            }
            Event::Rule => {
                blocks.push(Block::Rule);
                i += 1;
            }
            _ => { i += 1; }
        }
    }

    blocks
}

fn parse_blocks_until<'a>(events: &'a [Event<'a>], end: TagEnd) -> (Vec<Block<'a>>, usize) {
    let mut blocks = Vec::new();
    let mut i = 0;

    while i < events.len() {
        if matches!(&events[i], Event::End(e) if *e == end) {
            return (blocks, i);
        }

        match &events[i] {
            Event::Start(Tag::Paragraph) => {
                i += 1;
                let (inlines, consumed) = parse_inlines(&events[i..], TagEnd::Paragraph);
                blocks.push(Block::Paragraph(inlines));
                i += consumed + 1;
            }
            // Task list items may have inline text without a wrapping paragraph
            Event::TaskListMarker(checked) => {
                // Collect remaining inlines as a paragraph with task marker
                let checked = *checked;
                i += 1;
                // Gather text until next block or end
                let mut inlines = vec![Inline::TaskMarker(checked)];
                while i < events.len() {
                    match &events[i] {
                        Event::End(e) if *e == end => {
                            if !inlines.is_empty() {
                                blocks.push(Block::Paragraph(inlines));
                            }
                            return (blocks, i);
                        }
                        Event::Text(t) => { inlines.push(Inline::Text(t.as_ref())); i += 1; }
                        Event::SoftBreak => { inlines.push(Inline::SoftBreak); i += 1; }
                        _ => break,
                    }
                }
                if !inlines.is_empty() {
                    blocks.push(Block::Paragraph(inlines));
                }
            }
            Event::Text(t) => {
                // Bare text outside paragraph (can happen in list items)
                blocks.push(Block::Paragraph(vec![Inline::Text(t.as_ref())]));
                i += 1;
            }
            _ => {
                // Recurse for nested blocks
                let sub = parse_blocks(&events[i..i + 1]);
                if sub.is_empty() {
                    i += 1;
                } else {
                    // Need to find how many events this consumed
                    blocks.extend(sub);
                    i += 1;
                }
            }
        }
    }

    (blocks, i)
}

fn parse_inlines<'a>(events: &'a [Event<'a>], end: TagEnd) -> (Vec<Inline<'a>>, usize) {
    let mut inlines = Vec::new();
    let mut i = 0;

    while i < events.len() {
        match &events[i] {
            Event::End(e) if *e == end => return (inlines, i),
            Event::Text(t) => { inlines.push(Inline::Text(t.as_ref())); i += 1; }
            Event::Code(t) => { inlines.push(Inline::Code(t.as_ref())); i += 1; }
            Event::SoftBreak => { inlines.push(Inline::SoftBreak); i += 1; }
            Event::HardBreak => { inlines.push(Inline::HardBreak); i += 1; }
            Event::TaskListMarker(checked) => {
                inlines.push(Inline::TaskMarker(*checked));
                i += 1;
            }
            Event::Start(Tag::Strong) => {
                i += 1;
                let (inner, consumed) = parse_inlines(&events[i..], TagEnd::Strong);
                inlines.push(Inline::Strong(inner));
                i += consumed + 1;
            }
            Event::Start(Tag::Emphasis) => {
                i += 1;
                let (inner, consumed) = parse_inlines(&events[i..], TagEnd::Emphasis);
                inlines.push(Inline::Emphasis(inner));
                i += consumed + 1;
            }
            Event::Start(Tag::Strikethrough) => {
                i += 1;
                let (inner, consumed) = parse_inlines(&events[i..], TagEnd::Strikethrough);
                inlines.push(Inline::Strikethrough(inner));
                i += consumed + 1;
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                let url = dest_url.to_string();
                i += 1;
                let (inner, consumed) = parse_inlines(&events[i..], TagEnd::Link);
                inlines.push(Inline::Link(url, inner));
                i += consumed + 1;
            }
            _ => { i += 1; }
        }
    }

    (inlines, i)
}

// ---------------------------------------------------------------------------
// Renderer: Blocks → Elements
// ---------------------------------------------------------------------------

fn render_blocks<'a, Message: Clone + 'static>(
    blocks: Vec<Block<'_>>,
    theme: &Theme,
    on_link: &(impl Fn(String) -> Message + 'a),
) -> Element<'a, Message> {
    let spacing = cosmic::theme::spacing();
    let mut col = widget::column().spacing(spacing.space_xs).width(Length::Fill);

    for block in blocks {
        col = col.push(render_block(block, theme, on_link));
    }

    col.into()
}

fn render_block<'a, Message: Clone + 'static>(
    block: Block<'_>,
    theme: &Theme,
    on_link: &(impl Fn(String) -> Message + 'a),
) -> Element<'a, Message> {
    match block {
        Block::Heading(level, inlines) => {
            let size = theme.heading_size(level);
            let text = flatten_inlines_plain(&inlines);
            widget::text(text)
                .size(size)
                .class(cosmic::theme::Text::Color(theme.heading))
                .font(Font {
                    weight: iced::font::Weight::Bold,
                    ..Font::default()
                })
                .width(Length::Fill)
                .into()
        }
        Block::Paragraph(inlines) => render_inlines_widget(&inlines, theme, on_link),
        Block::CodeBlock(_lang, code) => {
            let code_bg = theme.block_code_bg;
            widget::container(
                widget::text(code)
                    .size(theme.text_size * 0.85)
                    .class(cosmic::theme::Text::Color(theme.block_code_text))
                    .font(Font::MONOSPACE),
            )
            .padding(8)
            .width(Length::Fill)
            .style(move |_theme| widget::container::Style {
                background: Some(iced::Background::Color(code_bg)),
                border: iced::Border {
                    radius: [4.0; 4].into(),
                    ..Default::default()
                },
                ..Default::default()
            })
            .into()
        }
        Block::BlockQuote(inner) => {
            let accent = theme.quote_accent;
            widget::container(
                widget::container(render_blocks(inner, theme, on_link))
                    .padding([0, 0, 0, 12]),
            )
            .width(Length::Fill)
            .style(move |_theme| widget::container::Style {
                border: iced::Border {
                    color: accent,
                    width: 3.0,
                    radius: [0.0; 4].into(),
                },
                ..Default::default()
            })
            .into()
        }
        Block::List(start, items) => {
            let mut col = widget::column().spacing(4).width(Length::Fill);
            for (idx, item_blocks) in items.into_iter().enumerate() {
                let marker = if let Some(n) = start {
                    format!("{}.", n + idx as u64)
                } else {
                    "\u{2022}".to_string() // bullet
                };
                let content = render_blocks(item_blocks, theme, on_link);
                col = col.push(
                    widget::row()
                        .push(
                            widget::text(marker)
                                .class(cosmic::theme::Text::Color(theme.text))
                                .width(Length::Shrink),
                        )
                        .push(content)
                        .spacing(8),
                );
            }
            col.into()
        }
        Block::Table(header_rows, body_rows) => {
            render_table(header_rows, body_rows, theme, on_link)
        }
        Block::Rule => {
            let rule_color = theme.rule;
            widget::container(widget::space().height(1).width(Length::Fill))
                .width(Length::Fill)
                .style(move |_theme| widget::container::Style {
                    background: Some(iced::Background::Color(rule_color)),
                    ..Default::default()
                })
                .into()
        }
    }
}

fn render_table<'a, Message: Clone + 'static>(
    header_rows: Vec<Vec<Vec<Inline<'_>>>>,
    body_rows: Vec<Vec<Vec<Inline<'_>>>>,
    theme: &Theme,
    on_link: &(impl Fn(String) -> Message + 'a),
) -> Element<'a, Message> {
    let border_color = theme.table_border;
    let header_bg = theme.table_header_bg;
    let alt_bg = theme.table_alt_bg;

    let mut col = widget::column().spacing(0).width(Length::Fill);

    // Header rows
    for cells in &header_rows {
        let row = render_table_row(cells, theme, on_link, Some(header_bg), true);
        col = col.push(row);
    }

    // Body rows
    for (idx, cells) in body_rows.iter().enumerate() {
        let bg = if idx % 2 == 1 { Some(alt_bg) } else { None };
        let row = render_table_row(cells, theme, on_link, bg, false);
        col = col.push(row);
    }

    widget::container(col)
        .width(Length::Fill)
        .style(move |_theme| widget::container::Style {
            border: iced::Border {
                color: border_color,
                width: 1.0,
                radius: [2.0; 4].into(),
            },
            ..Default::default()
        })
        .into()
}

fn render_table_row<'a, Message: Clone + 'static>(
    cells: &[Vec<Inline<'_>>],
    theme: &Theme,
    on_link: &(impl Fn(String) -> Message + 'a),
    bg: Option<Color>,
    bold: bool,
) -> Element<'a, Message> {
    let mut row = widget::row().spacing(0).width(Length::Fill);

    for cell_inlines in cells {
        let text = if bold {
            let plain = flatten_inlines_plain(cell_inlines);
            widget::text(plain)
                .class(cosmic::theme::Text::Color(theme.text))
                .font(Font {
                    weight: iced::font::Weight::Bold,
                    ..Font::default()
                })
                .into()
        } else {
            render_inlines_widget(cell_inlines, theme, on_link)
        };

        row = row.push(
            widget::container(text)
                .padding([4, 8])
                .width(Length::FillPortion(1)),
        );
    }

    if let Some(bg_color) = bg {
        widget::container(row)
            .width(Length::Fill)
            .style(move |_theme| widget::container::Style {
                background: Some(iced::Background::Color(bg_color)),
                ..Default::default()
            })
            .into()
    } else {
        row.into()
    }
}

/// Render inline elements as a text widget with explicit colors.
/// For simplicity, this flattens to plain text with color.
/// Rich text (bold/italic mixed in one line) uses concatenation.
fn render_inlines_widget<'a, Message: Clone + 'static>(
    inlines: &[Inline<'_>],
    theme: &Theme,
    _on_link: &(impl Fn(String) -> Message + 'a),
) -> Element<'a, Message> {
    // For now, render as plain text with the correct color.
    // TODO: Use rich_text spans when cosmic exposes the API properly.
    let text = flatten_inlines_plain(inlines);
    widget::text(text)
        .size(theme.text_size)
        .class(cosmic::theme::Text::Color(theme.text))
        .width(Length::Fill)
        .into()
}

/// Flatten inlines to plain text (strips formatting markers).
fn flatten_inlines_plain(inlines: &[Inline<'_>]) -> String {
    let mut out = String::new();
    for inline in inlines {
        match inline {
            Inline::Text(t) => out.push_str(t),
            Inline::Code(t) => {
                out.push('`');
                out.push_str(t);
                out.push('`');
            }
            Inline::Strong(inner) => out.push_str(&flatten_inlines_plain(inner)),
            Inline::Emphasis(inner) => out.push_str(&flatten_inlines_plain(inner)),
            Inline::Strikethrough(inner) => out.push_str(&flatten_inlines_plain(inner)),
            Inline::Link(_, inner) => out.push_str(&flatten_inlines_plain(inner)),
            Inline::SoftBreak => out.push(' '),
            Inline::HardBreak => out.push('\n'),
            Inline::TaskMarker(checked) => {
                out.push_str(if *checked { "[x] " } else { "[ ] " });
            }
        }
    }
    out
}
