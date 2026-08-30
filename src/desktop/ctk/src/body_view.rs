//! Sanitised, text-first message-body reading for native CTK applications.
//!
//! Applications own the raw [`BodySource`] and must cross the sanitizer
//! boundary with [`BodySource::sanitize`] before construction. CTK owns the
//! resulting immutable [`SanitizedHtml`], text projection, scrolling,
//! projected-text selection, copy policy, link activation and accessibility
//! tree. The widget API
//! accepts [`SanitizedBody`] only: unsanitised HTML cannot reach projection or
//! layout through the safe public API.
//!
//! [`RenderArm::Text`] is the permanent fallback and the only Stage A
//! implementation. Requesting [`RenderArm::Engine`] records the per-instance
//! preference but gracefully renders the text arm; Stage B can replace that
//! resolution without changing callers or the sanitised input boundary.
//!
//! Bevy 0.19 publicly exposes each ordinary text node's shaped Parley layout
//! through `ComputedTextBlock::buffer`, and `TextLayoutInfo::selection_rects`
//! is the selection-paint input consumed by `bevy_ui_render`. CTK keeps one
//! logical anchor/focus pair in projection block + byte-offset space, maps
//! pointer coordinates through each text node's physical-pixel layout, and
//! reapplies Parley selection geometry after every Bevy text-layout pass.
//! Press-drag-release selects across blocks and Shift-click extends. The
//! document remains one keyboard tab stop, while pointer-focused blocks remain
//! the empty-selection copy units. Ctrl/Command+C prefers a non-empty projected
//! selection and otherwise copies the existing focused block or document.
//! Link nodes remain separately focusable and emit [`LinkActivated`]; CTK never
//! opens a URL.
//! Copy reaches the OS clipboard only when the application enables CTK's
//! `system-clipboard` feature; otherwise Bevy keeps it in its in-process
//! clipboard buffer.
//! Plain-body document copy is byte-exact against the canonical LF input.
//! HTML cannot round-trip through a text projection: its copy form is the
//! readable projected blocks, in document order, separated by one LF, with
//! rendered list markers and rules retained and the synthetic truncation alert
//! omitted.
//!
//! # Consumer contract
//!
//! The application MUST validate or confirm every [`LinkActivated`] URL before
//! opening it; CTK never opens URLs itself. It MUST NOT automatically fetch
//! [`RemoteRefs`], and MUST treat [`RemoteRefs::is_complete`] being false as an
//! incomplete inventory rather than evidence that no other remote references
//! exist. The widget's no-network property follows from its dependency shape
//! and code inspection, not from a runtime sandbox.
//!
//! Targets longer than [`BODY_VIEW_MAX_URL_BYTES`] lose link semantics and are
//! not added to [`RemoteRefs`]; that inventory is for suppressed fetch
//! resources, not navigation targets.
//!
//! The widget performs no automatic remote resource fetching. The sanitizer
//! removes every non-embedded resource attribute before projection, hostile
//! fixtures assert that inventoried remote URLs never survive in
//! [`SanitizedHtml`], and `cargo tree --features body-view -e normal` confirms
//! that the feature's dependency tree contains no HTTP-client crate. Remote
//! anchor targets survive only as [`LinkActivated`] payloads. CSS is discarded
//! entirely, so `url()` cannot become a later layout or paint fetch.
//!
//! The sanitizer allow-list and remote-resource inventory are one security
//! boundary and must stay synchronised. Adding a resource-bearing tag,
//! attribute or CSS form to one without the other makes [`RemoteRefs`]
//! incomplete and could let a future engine arm see a fetch CTK did not report.
//! Allowed tag attributes come from one constant, and a unit test rejects every
//! new attribute until it is classified as inert, navigation-only or covered by
//! the inventory walker. For every fetch-bearing classification it also proves
//! that a representative hostile attribute is absent from sanitizer output;
//! classification alone is not accepted as suppression.
//! Fetch attributes on controls which never survive sanitisation (`input.src`,
//! form `action`/`formaction`), media/object fallbacks (`poster`, `data`),
//! hyperlink audit pings and the obsolete app-cache manifest are inventoried
//! from the raw DOM too. An anchor's ordinary `href` remains deliberately
//! navigation-only: it cannot fetch until a consumer handles `LinkActivated`.
//! [`RemoteRefs::is_complete`] is false when capped input, the CSS depth guard,
//! foreign markup, an opaque nested document or an unclassifiable resource URL
//! prevents a complete inventory.
//!
//! Stage A is deliberately non-virtualised. Raw HTML is bounded to
//! [`BODY_VIEW_MAX_INPUT_BYTES`] before parsing; plain text is canonicalised
//! from CRLF or lone CR to LF and then bounded to the same byte count. The more
//! generous [`BODY_VIEW_MAX_BLOCKS`] and
//! [`BODY_VIEW_MAX_SPANS`] defaults stop pathological DOM-to-entity expansion,
//! and [`CtkBodyViewProps`] lets applications tune those two content budgets.
//! Caller-supplied block and span budgets are trusted and are not clamped back
//! to the defaults. The non-configurable [`BODY_VIEW_MAX_ENTITIES`] ceiling
//! separately counts every entity the widget spawns, including its five-entity
//! scroll shell, list containers and markers, quote wrappers, content, text
//! runs and the truncation alert. A representative spawn-shape test compares
//! that ledger exactly with the live Bevy entity count.
//! [`BODY_VIEW_MAX_TEXT_RUN_BYTES`] separately bounds any one projected text
//! run. Longer accepted content is split across further bounded runs; it does
//! not truncate the body. A visible, accessible "Message truncated." alert is
//! added only when a whole-body ingress, block, span or entity budget is
//! exhausted. That alert is deliberately exempt from both content budgets, but
//! is counted by the total entity ceiling, so the documented block/span maxima
//! describe application content.
//! Chunk boundaries preserve extended grapheme clusters and prefer a nearby
//! Unicode word boundary, so independently shaped Bevy text entities do not
//! corrupt combining marks, joined emoji or ordinary words. A single
//! pathological grapheme larger than the nominal run bound remains whole:
//! preserving body content and shaping correctness takes precedence over that
//! per-run target.
//! Plain-text whitespace also survives ordinary Bevy 0.19 `Text` layout:
//! Bevy's `TextPipeline` uses Parley's ranged builder with its
//! `WhiteSpaceCollapse::Preserve` default, so leading and repeated spaces are
//! shaped rather than collapsed. Projection tests assert each retained span
//! byte-for-byte; this is not a pixel-snapshot claim about a particular font.
//! `virtual_list` integration is deferred beyond v1.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use accesskit::{Action, Role};
use ammonia::{Builder, Url, UrlRelative};
use bevy::a11y::{AccessibilityNode, ActionRequest as AccessibilityActionRequest};
use bevy::app::{App, Last, Plugin, PostUpdate};
use bevy::clipboard::Clipboard;
use bevy::ecs::event::EntityEvent;
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::observer::On;
use bevy::ecs::schedule::common_conditions::resource_changed;
use bevy::feathers::theme::{ThemeBackgroundColor, ThemeBorderColor, ThemeTextColor, UiTheme};
use bevy::input::keyboard::{Key, KeyboardInput};
use bevy::input::ButtonState;
use bevy::input_focus::tab_navigation::TabIndex;
use bevy::input_focus::{FocusCause, FocusedInput, InputFocus};
use bevy::picking::events::{
    Cancel as PointerCancel, Click, Drag, DragEnd, Pointer, PointerState, Press, Release,
};
use bevy::picking::hover::{HoverMap, Hovered};
use bevy::picking::pointer::PointerId;
use bevy::picking::Pickable;
use bevy::prelude::*;
use bevy::text::{
    ComputedTextBlock, FontSource, FontStyle, FontWeight, TextCursorStyle, TextLayoutInfo,
    Underline,
};
use bevy::ui::{ComputedUiRenderTargetInfo, Overflow, ScrollPosition, UiGlobalTransform, UiScale};
use bevy::ui_widgets::{ControlOrientation, ScrollArea, Scrollbar, ScrollbarThumb};
use cssparser::{Parser as CssParser, ParserInput as CssParserInput, Token as CssToken};
use html5ever::parse_document;
use html5ever::tendril::TendrilSink;
use markup5ever_rcdom::{Handle, NodeData, RcDom};
use parley::editing::{Cursor as ParleyCursor, Selection as ParleySelection};
use parley::layout::Affinity;
use unicode_segmentation::UnicodeSegmentation;

use crate::theme::{ctk_color, tokens};

const MAX_PROJECTION_DEPTH: usize = 64;
const MAX_QUOTE_DEPTH: usize = 16;
const MAX_CSS_WALK_DEPTH: usize = 64;
const BODY_VIEW_SHELL_ENTITIES: usize = 5;
const TRUNCATION_ENTITIES: usize = 2;
const TEXT_RUN_WORD_BOUNDARY_LOOKBACK_BYTES: usize = 1024;
/// Hostile markup can create tens of thousands of styled runs. Logical copy
/// remains exact, while one frame paints or fallback-hit-tests at most this many
/// text entities so selection cannot monopolise the UI thread.
const BODY_VIEW_MAX_SELECTION_RUNS_PER_FRAME: usize = 4_096;
const BODY_VIEW_MAX_ACTIVE_SELECTION_POINTERS: usize = 16;
const SANITIZER_ALLOWED_TAGS: &[&str] = &[
    "a",
    "address",
    "article",
    "aside",
    "b",
    "blockquote",
    "br",
    "caption",
    "center",
    "code",
    "dd",
    "del",
    "details",
    "dialog",
    "div",
    "dl",
    "dt",
    "em",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    // `form` is admitted only for the sanitizer pass, with every attribute
    // removed, then converted to `div` below. This preserves its block
    // boundary and readable descendants without retaining form semantics.
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hgroup",
    "hr",
    "i",
    "img",
    "li",
    "main",
    "menu",
    "nav",
    "ol",
    "p",
    "pre",
    "search",
    "section",
    "span",
    "strong",
    "summary",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "tr",
    "u",
    "ul",
];
const SAFE_BLOCK_TAGS: &[&str] = &[
    "address",
    "article",
    "aside",
    "blockquote",
    "caption",
    "center",
    "dd",
    "details",
    "dialog",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hgroup",
    "hr",
    "li",
    "main",
    "menu",
    "nav",
    "ol",
    "p",
    "pre",
    "search",
    "section",
    "summary",
    "table",
    "tbody",
    "tfoot",
    "thead",
    "tr",
    "ul",
];
const DANGEROUS_CONTENT_TAGS: &[&str] = &["script", "style", "iframe", "object", "embed"];
const SANITIZER_GENERIC_ATTRIBUTES: &[&str] = &["lang", "title"];
const SANITIZER_TAG_ATTRIBUTES: &[(&str, &[&str])] = &[
    ("a", &["href", "title"]),
    ("img", &["alt", "height", "src", "title", "width"]),
    ("ol", &["start"]),
];
static NEXT_PROJECTION_ID: AtomicU64 = AtomicU64::new(1);

/// Maximum raw HTML bytes, or canonicalised plain-text bytes, accepted.
///
/// HTML is capped before parsing. Plain text first normalises CRLF and lone CR
/// line endings to LF, then applies this cap to the canonical form; its raw
/// input can therefore be larger when normalisation removes CR bytes.
pub const BODY_VIEW_MAX_INPUT_BYTES: usize = 2 * 1024 * 1024;
/// Maximum bytes retained in one projected text run.
pub const BODY_VIEW_MAX_TEXT_RUN_BYTES: usize = 64 * 1024;
/// Maximum bytes retained for one navigation target.
///
/// Longer anchors render as ordinary text and their target is dropped. It is
/// not reported through [`RemoteRefs`], which inventories fetch resources.
pub const BODY_VIEW_MAX_URL_BYTES: usize = 8 * 1024;
/// Default maximum projected content blocks, excluding the truncation alert.
pub const BODY_VIEW_MAX_BLOCKS: usize = 4_096;
/// Default maximum styled content spans, excluding the truncation alert.
pub const BODY_VIEW_MAX_SPANS: usize = 32_768;
/// Maximum total entities spawned by one body view, including fixed scroll
/// shell, projected content, semantic wrappers and the truncation alert.
pub const BODY_VIEW_MAX_ENTITIES: usize = 40_000;

/// Raw application input. HTML in this type is not safe to render.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BodySource {
    Plain(String),
    Html(String),
}

impl BodySource {
    /// Cross the mandatory trust boundary into a body accepted by the widget.
    /// Raw HTML beyond [`BODY_VIEW_MAX_INPUT_BYTES`] is discarded. Plain text
    /// is first canonicalised to LF, then its canonical bytes are capped. A
    /// capped body keeps its accepted head and gains an accessible truncation
    /// alert.
    pub fn sanitize(self) -> SanitizedBody {
        match self {
            Self::Plain(value) => {
                let (value, truncated) = normalise_plain_ingress(value, BODY_VIEW_MAX_INPUT_BYTES);
                SanitizedBody {
                    content: SanitizedContent::Plain(value),
                    remote_refs: RemoteRefs::default(),
                    input_truncated: truncated,
                }
            }
            Self::Html(value) => {
                let (html, remote_refs) = sanitize_html(&value);
                let input_truncated = html.input_truncated;
                SanitizedBody {
                    content: SanitizedContent::Html(html),
                    remote_refs,
                    input_truncated,
                }
            }
        }
    }
}

/// HTML which has passed CTK's fixed allow-list and remote-resource policy.
///
/// The inner string is private and this type has no unchecked constructor.
#[derive(Clone, PartialEq, Eq)]
pub struct SanitizedHtml {
    value: String,
    input_truncated: bool,
}

impl SanitizedHtml {
    pub fn as_str(&self) -> &str {
        &self.value
    }

    /// Whether raw HTML exceeded [`BODY_VIEW_MAX_INPUT_BYTES`].
    pub fn input_truncated(&self) -> bool {
        self.input_truncated
    }
}

impl fmt::Debug for SanitizedHtml {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SanitizedHtml")
            .field("value", &self.value)
            .field("input_truncated", &self.input_truncated)
            .finish()
    }
}

/// Resource URLs suppressed while sanitising one HTML body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteRefs {
    urls: Vec<String>,
    complete: bool,
}

impl Default for RemoteRefs {
    fn default() -> Self {
        Self {
            urls: Vec::new(),
            complete: true,
        }
    }
}

impl RemoteRefs {
    /// Number of suppressed references. Repeated tracking URLs remain repeated.
    pub fn count(&self) -> usize {
        self.urls.len()
    }

    pub fn is_empty(&self) -> bool {
        self.urls.is_empty()
    }

    pub fn urls(&self) -> &[String] {
        &self.urls
    }

    /// Whether every resource-bearing construct in the accepted input was
    /// confidently inventoried. Capped input, over-deep CSS, foreign markup,
    /// opaque nested documents and unclassifiable resource URLs make this
    /// false.
    pub fn is_complete(&self) -> bool {
        self.complete
    }
}

/// Sanitised body accepted by [`CtkBodyViewProps`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SanitizedBody {
    content: SanitizedContent,
    remote_refs: RemoteRefs,
    input_truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SanitizedContent {
    Plain(String),
    Html(SanitizedHtml),
}

impl SanitizedBody {
    pub fn remote_refs(&self) -> &RemoteRefs {
        &self.remote_refs
    }

    /// Whether raw HTML, or canonicalised plain text, exceeded
    /// [`BODY_VIEW_MAX_INPUT_BYTES`].
    pub fn input_truncated(&self) -> bool {
        self.input_truncated
    }

    pub fn sanitized_html(&self) -> Option<&SanitizedHtml> {
        match &self.content {
            SanitizedContent::Plain(_) => None,
            SanitizedContent::Html(html) => Some(html),
        }
    }
}

/// Sanitize raw HTML and return both the unforgeable result and suppressed
/// resource inventory.
///
/// Raw HTML is capped at [`BODY_VIEW_MAX_INPUT_BYTES`] before either DOM parsing
/// pass. [`SanitizedHtml::input_truncated`] reports that condition.
pub fn sanitize_html(raw: &str) -> (SanitizedHtml, RemoteRefs) {
    let (raw, input_truncated) = truncate_str(raw, BODY_VIEW_MAX_INPUT_BYTES);
    let raw_dom = parse_document(RcDom::default(), Default::default()).one(raw);
    let mut remote_refs = collect_remote_refs(&raw_dom.document);
    remote_refs.complete &= !input_truncated;

    let mut builder = Builder::new();
    builder
        .tags(SANITIZER_ALLOWED_TAGS.iter().copied().collect())
        .clean_content_tags(DANGEROUS_CONTENT_TAGS.iter().copied().collect())
        .generic_attributes(SANITIZER_GENERIC_ATTRIBUTES.iter().copied().collect())
        .tag_attributes(
            SANITIZER_TAG_ATTRIBUTES
                .iter()
                .map(|(tag, attributes)| (*tag, attributes.iter().copied().collect::<HashSet<_>>()))
                .collect(),
        )
        .url_schemes(
            ["cid", "data", "http", "https", "mailto", "tel"]
                .into_iter()
                .collect(),
        )
        .url_relative(UrlRelative::Deny)
        .link_rel(Some("noopener noreferrer"))
        .attribute_filter(filter_attribute);

    let cleaned = builder.clean(raw).to_string();
    // Ammonia has separate "unwrap" and "remove with contents" behaviours,
    // but no boundary-preserving tag replacement. Its serialised output is
    // canonical and every form attribute is rejected by `filter_attribute`,
    // so this final trust-boundary rewrite removes form semantics while keeping
    // an inert block boundary.
    let cleaned = cleaned
        .replace("<form>", "<div>")
        .replace("</form>", "</div>");
    // This is a release-mode trust-boundary assertion, not a debug aid. If a
    // future allow-list change lets a form attribute survive, the bare-tag
    // rewrite above will no longer match. Failing closed here prevents active
    // form semantics from silently entering `SanitizedHtml`.
    assert!(
        !cleaned.contains("<form"),
        "sanitizer invariant failed: a form boundary retained attributes"
    );
    (
        SanitizedHtml {
            value: cleaned,
            input_truncated,
        },
        remote_refs,
    )
}

fn normalise_plain_ingress(value: String, max_bytes: usize) -> (String, bool) {
    let mut normalised = String::with_capacity(value.len().min(max_bytes));
    let mut chars = value.chars().peekable();
    let mut truncated = false;
    while let Some(character) = chars.next() {
        let character = if character == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            '\n'
        } else {
            character
        };
        if normalised.len().saturating_add(character.len_utf8()) > max_bytes {
            truncated = true;
            break;
        }
        normalised.push(character);
    }
    if truncated {
        normalised.shrink_to_fit();
    }
    (normalised, truncated)
}

fn truncate_str(value: &str, max_bytes: usize) -> (&str, bool) {
    if value.len() <= max_bytes {
        return (value, false);
    }
    (&value[..floor_char_boundary(value, max_bytes)], true)
}

fn floor_char_boundary(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn is_css_collapsible_whitespace(character: char) -> bool {
    matches!(character, ' ' | '\t' | '\n' | '\r' | '\u{000C}')
}

fn is_css_collapsible_whitespace_byte(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | b'\x0C')
}

fn trim_css_whitespace(value: &str) -> &str {
    value.trim_matches(is_css_collapsible_whitespace)
}

fn trim_css_whitespace_start(value: &str) -> &str {
    value.trim_start_matches(is_css_collapsible_whitespace)
}

fn trim_css_whitespace_end(value: &str) -> &str {
    value.trim_end_matches(is_css_collapsible_whitespace)
}

fn filter_attribute<'a>(element: &str, attribute: &str, value: &'a str) -> Option<Cow<'a, str>> {
    if element == "form" {
        return None;
    }
    // Belt-and-braces: `style` is absent from every allow-list above as the
    // primary defence, and remains rejected here if that configuration changes.
    if attribute.eq_ignore_ascii_case("style") || attribute.starts_with("on") {
        return None;
    }
    if remote_resource_attribute(element, attribute).is_some() {
        if element == "img" && attribute == "src" && is_embedded_image(value) {
            return Some(Cow::Borrowed(value));
        }
        return None;
    }
    if element == "a" && attribute == "href" {
        let value = trim_css_whitespace(value);
        if value.len() > BODY_VIEW_MAX_URL_BYTES
            || value
                .get(..11)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("javascript:"))
            || value
                .get(..5)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("data:"))
        {
            return None;
        }
    }
    Some(Cow::Borrowed(value))
}

fn is_embedded_image(value: &str) -> bool {
    let value = trim_css_whitespace(value);
    if value
        .get(..4)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("cid:"))
    {
        return !trim_css_whitespace(&value[4..]).is_empty();
    }
    if !value
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("data:"))
    {
        return false;
    }
    let Some((header, _payload)) = value[5..].split_once(',') else {
        return false;
    };
    let media_type = trim_css_whitespace(
        header
            .split_once(';')
            .map_or(header, |(media_type, _)| media_type),
    );
    const RASTER_TYPES: [&str; 7] = [
        "image/avif",
        "image/gif",
        "image/jpeg",
        "image/jpg",
        "image/png",
        "image/webp",
        "image/bmp",
    ];
    RASTER_TYPES
        .iter()
        .any(|candidate| media_type.eq_ignore_ascii_case(candidate))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RemoteUrlClassification {
    Embedded,
    Remote,
    AmbiguousRemote,
}

impl RemoteUrlClassification {
    fn is_remote(self) -> bool {
        !matches!(self, Self::Embedded)
    }

    fn is_complete(self) -> bool {
        !matches!(self, Self::AmbiguousRemote)
    }
}

fn classify_remote_url(value: &str) -> RemoteUrlClassification {
    let parsed = if value
        .trim_start_matches(|character: char| character <= '\u{20}')
        .starts_with("//")
    {
        let base = Url::parse("https://remote-inventory.invalid/")
            .expect("fixed remote-inventory base URL must parse");
        Url::options().base_url(Some(&base)).parse(value)
    } else {
        Url::parse(value)
    };
    let Ok(url) = parsed else {
        return RemoteUrlClassification::AmbiguousRemote;
    };
    match url.scheme() {
        "cid" | "data" => RemoteUrlClassification::Embedded,
        "http" | "https" | "ftp" | "ws" | "wss" => RemoteUrlClassification::Remote,
        _ => RemoteUrlClassification::AmbiguousRemote,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RemoteResourceAttribute {
    Url,
    UrlList,
    Srcset,
    Css,
    OpaqueNestedDocument,
}

fn remote_resource_attribute(tag: &str, attribute: &str) -> Option<RemoteResourceAttribute> {
    if attribute == "style" {
        return Some(RemoteResourceAttribute::Css);
    }
    if attribute == "background" {
        return Some(RemoteResourceAttribute::Url);
    }
    match (tag, attribute) {
        (
            "img" | "input" | "video" | "audio" | "source" | "track" | "embed" | "script"
            | "iframe",
            "src",
        )
        | ("video", "poster")
        | ("link", "href")
        | (
            "image" | "use" | "feImage" | "feimage" | "filter" | "pattern" | "linearGradient"
            | "lineargradient" | "radialGradient" | "radialgradient" | "textPath" | "textpath"
            | "mpath" | "animate" | "set" | "cursor" | "script",
            "href",
        )
        | ("object", "data")
        | ("form", "action")
        | ("button" | "input", "formaction")
        | ("html", "manifest") => Some(RemoteResourceAttribute::Url),
        ("a" | "area", "ping") => Some(RemoteResourceAttribute::UrlList),
        ("img" | "source", "srcset") | ("link", "imagesrcset") => {
            Some(RemoteResourceAttribute::Srcset)
        }
        ("iframe", "srcdoc") => Some(RemoteResourceAttribute::OpaqueNestedDocument),
        _ => None,
    }
}

fn collect_remote_refs(root: &Handle) -> RemoteRefs {
    let mut urls = Vec::new();
    let mut complete = true;
    let mut stack = vec![root.clone()];
    while let Some(node) = stack.pop() {
        if let NodeData::Element { name, attrs, .. } = &node.data {
            let tag = name.local.as_ref();
            let attrs = attrs.borrow();
            // html5ever identifies SVG/MathML precisely, but this inventory is
            // not a complete foreign-content loader model. Record direct URLs
            // below while refusing to call the overall catalogue exhaustive.
            complete &= name.ns.as_ref() == "http://www.w3.org/1999/xhtml";
            for attribute in attrs.iter() {
                let value = attribute.value.as_ref();
                match remote_resource_attribute(tag, attribute.name.local.as_ref()) {
                    Some(RemoteResourceAttribute::Url) => {
                        complete &= collect_remote_url(Some(value), &mut urls);
                    }
                    Some(RemoteResourceAttribute::UrlList) => {
                        complete &= collect_remote_url_list(value, &mut urls);
                    }
                    Some(RemoteResourceAttribute::Srcset) => {
                        complete &= collect_srcset_remote_refs(Some(value), &mut urls);
                    }
                    Some(RemoteResourceAttribute::Css) => {
                        complete &= collect_css_remote_refs(value, &mut urls);
                    }
                    Some(RemoteResourceAttribute::OpaqueNestedDocument) => {
                        complete = false;
                    }
                    None => {}
                }
            }
            if tag == "meta"
                && attrs.iter().any(|attribute| {
                    attribute.name.local.as_ref() == "http-equiv"
                        && trim_css_whitespace(attribute.value.as_ref())
                            .eq_ignore_ascii_case("refresh")
                })
            {
                // The refresh URL is embedded in a grammar inside `content`,
                // not a URL-valued attribute. Sanitisation removes the element;
                // the raw inventory must still admit it cannot list it safely.
                complete = false;
            }
            if tag == "style" {
                let (style, text_complete) = raw_text_bounded(&node, BODY_VIEW_MAX_INPUT_BYTES);
                complete &= text_complete && collect_css_remote_refs(&style, &mut urls);
            }
        }
        let children = node.children.borrow();
        stack.extend(children.iter().rev().cloned());
    }
    RemoteRefs { urls, complete }
}

fn collect_remote_url(value: Option<&str>, urls: &mut Vec<String>) -> bool {
    let Some(value) = value else {
        return true;
    };
    let classification = classify_remote_url(value);
    if classification.is_remote() {
        urls.push(trim_css_whitespace(value).to_owned());
    }
    classification.is_complete()
}

fn collect_remote_url_list(value: &str, urls: &mut Vec<String>) -> bool {
    let mut complete = true;
    for candidate in value
        .split(is_css_collapsible_whitespace)
        .filter(|candidate| !candidate.is_empty())
    {
        // Do not short-circuit: completeness and the full attempted-fetch
        // inventory are independent outputs.
        complete &= collect_remote_url(Some(candidate), urls);
    }
    complete
}

fn collect_srcset_remote_refs(value: Option<&str>, urls: &mut Vec<String>) -> bool {
    let Some(value) = value else {
        return true;
    };
    let mut complete = true;
    let bytes = value.as_bytes();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        while bytes
            .get(cursor)
            .is_some_and(|byte| is_css_collapsible_whitespace_byte(*byte) || *byte == b',')
        {
            cursor += 1;
        }
        let start = cursor;
        while bytes
            .get(cursor)
            .is_some_and(|byte| !is_css_collapsible_whitespace_byte(*byte))
        {
            cursor += 1;
        }
        let candidate = value[start..cursor].trim_end_matches(',');
        let had_trailing_comma = candidate.len() != cursor.saturating_sub(start);
        complete &= collect_remote_url(Some(candidate), urls);
        if had_trailing_comma {
            continue;
        }
        let mut parens = 0usize;
        while let Some(byte) = bytes.get(cursor) {
            match *byte {
                b'(' => parens = parens.saturating_add(1),
                b')' => parens = parens.saturating_sub(1),
                b',' if parens == 0 => {
                    cursor += 1;
                    break;
                }
                _ => {}
            }
            cursor += 1;
        }
    }
    complete
}

fn collect_css_remote_refs(css: &str, urls: &mut Vec<String>) -> bool {
    let mut input = CssParserInput::new(css);
    let mut parser = CssParser::new(&mut input);
    collect_css_tokens(&mut parser, urls, false, 0)
}

fn collect_css_tokens(
    parser: &mut CssParser<'_, '_>,
    urls: &mut Vec<String>,
    image_set_strings: bool,
    depth: usize,
) -> bool {
    let mut complete = true;
    while let Ok(token) = parser.next().cloned() {
        match token {
            CssToken::AtKeyword(name) if name.eq_ignore_ascii_case("import") => {
                if let Ok(value) = parser.try_parse(CssParser::expect_url_or_string) {
                    complete &= collect_remote_url(Some(value.as_ref()), urls);
                }
            }
            CssToken::UnquotedUrl(value) => {
                complete &= collect_remote_url(Some(value.as_ref()), urls);
            }
            CssToken::QuotedString(value) if image_set_strings => {
                complete &= collect_remote_url(Some(value.as_ref()), urls);
            }
            CssToken::Function(name) if name.eq_ignore_ascii_case("url") => {
                if depth >= MAX_CSS_WALK_DEPTH {
                    complete = false;
                    let _ =
                        parser.parse_nested_block(|_| Ok::<(), cssparser::ParseError<'_, ()>>(()));
                    continue;
                }
                let parsed = parser.parse_nested_block(|nested| {
                    let value = nested.expect_string_cloned()?;
                    nested.expect_exhausted()?;
                    Ok::<String, cssparser::ParseError<'_, ()>>(value.as_ref().to_owned())
                });
                if let Ok(value) = parsed {
                    complete &= collect_remote_url(Some(&value), urls);
                } else {
                    complete = false;
                }
            }
            CssToken::Function(name)
                if name.eq_ignore_ascii_case("image-set")
                    || name.eq_ignore_ascii_case("-webkit-image-set") =>
            {
                if depth >= MAX_CSS_WALK_DEPTH {
                    complete = false;
                    let _ =
                        parser.parse_nested_block(|_| Ok::<(), cssparser::ParseError<'_, ()>>(()));
                    continue;
                }
                let mut nested_complete = true;
                let parsed = parser.parse_nested_block(|nested| {
                    nested_complete = collect_css_tokens(nested, urls, true, depth + 1);
                    Ok::<(), cssparser::ParseError<'_, ()>>(())
                });
                complete &= parsed.is_ok() && nested_complete;
            }
            CssToken::Function(_)
            | CssToken::ParenthesisBlock
            | CssToken::SquareBracketBlock
            | CssToken::CurlyBracketBlock => {
                if depth >= MAX_CSS_WALK_DEPTH {
                    complete = false;
                    let _ =
                        parser.parse_nested_block(|_| Ok::<(), cssparser::ParseError<'_, ()>>(()));
                    continue;
                }
                let mut nested_complete = true;
                let parsed = parser.parse_nested_block(|nested| {
                    nested_complete = collect_css_tokens(nested, urls, false, depth + 1);
                    Ok::<(), cssparser::ParseError<'_, ()>>(())
                });
                complete &= parsed.is_ok() && nested_complete;
            }
            _ => {}
        }
    }
    complete && parser.is_exhausted()
}

/// Runtime renderer preference. Stage A resolves both variants to text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RenderArm {
    #[default]
    Text,
    /// Reserved for Stage B. Currently falls back to [`Self::Text`].
    Engine,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct ProjectionId(u64);

impl ProjectionId {
    fn next() -> Self {
        let id = NEXT_PROJECTION_ID
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .expect("body-view projection identity space exhausted");
        Self(id)
    }
}

/// Stable styled-text projection produced before any Bevy entities are built.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BodyProjection {
    pub blocks: Vec<ProjectedBlock>,
    projection_id: ProjectionId,
    link_targets: Vec<Arc<str>>,
    entity_count: usize,
    copy_mode: ProjectionCopyMode,
}

impl Default for BodyProjection {
    fn default() -> Self {
        Self {
            blocks: Vec::new(),
            projection_id: ProjectionId::next(),
            link_targets: Vec::new(),
            entity_count: 0,
            copy_mode: ProjectionCopyMode::HtmlBlocks,
        }
    }
}

impl BodyProjection {
    /// Resolve an opaque span target handle without cloning its URL.
    ///
    /// A handle created by a different projection fails closed.
    pub fn link_target(&self, target: LinkTarget) -> Option<&str> {
        resolve_link_target(self.projection_id, &self.link_targets, target)
    }

    /// Number of distinct retained navigation targets.
    pub fn link_target_count(&self) -> usize {
        self.link_targets.len()
    }
}

fn resolve_link_target(
    projection_id: ProjectionId,
    link_targets: &[Arc<str>],
    target: LinkTarget,
) -> Option<&str> {
    (target.projection_id == projection_id)
        .then(|| link_targets.get(target.index))
        .flatten()
        .map(AsRef::as_ref)
}

/// Semantic kind of one projected text block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProjectedBlockKind {
    Paragraph,
    Heading(u8),
    Preformatted,
    Rule,
    Truncated,
}

/// One paragraph-like projection block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectedBlock {
    pub kind: ProjectedBlockKind,
    pub quote_depth: usize,
    pub spans: Vec<ProjectedSpan>,
    parent_list_item: Option<ListItemKey>,
    list_item_start: Option<ProjectedListItem>,
}

impl ProjectedBlock {
    pub fn text(&self) -> String {
        let mut value = match self.kind {
            ProjectedBlockKind::Rule => "────────".to_owned(),
            _ => String::new(),
        };
        for span in &self.spans {
            value.push_str(&span.text);
        }
        value
    }
}

/// One style run within a projection block.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectedSpan {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    pub link_target: Option<LinkTarget>,
    pub anchor_occurrence: Option<AnchorOccurrence>,
}

/// Opaque owner-scoped index into one [`BodyProjection`]'s interned link table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LinkTarget {
    projection_id: ProjectionId,
    index: usize,
}

impl LinkTarget {
    pub fn index(self) -> usize {
        self.index
    }
}

/// Opaque identity of one source `<a>` element within a projection.
///
/// URL targets are interned independently: two occurrences can therefore
/// resolve to the same [`LinkTarget`] without becoming one accessible link.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AnchorOccurrence {
    projection_id: ProjectionId,
    index: usize,
}

impl AnchorOccurrence {
    pub fn index(self) -> usize {
        self.index
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct InlineFormat {
    bold: bool,
    italic: bool,
    code: bool,
    link_target: Option<LinkTarget>,
    anchor_occurrence: Option<AnchorOccurrence>,
}

#[derive(Clone, Copy, Debug, Default)]
struct ProjectionContext {
    quote_depth: usize,
    list_depth: usize,
    list_item: Option<ListItemKey>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct ListItemKey {
    list_id: usize,
    position: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ProjectedListItem {
    key: ListItemKey,
    ordered: bool,
    depth: usize,
    index: i64,
    set_size: usize,
    parent_list_item: Option<ListItemKey>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProjectionCopyMode {
    PlainExact,
    HtmlBlocks,
}

struct ProjectionState {
    blocks: Vec<ProjectedBlock>,
    projection_id: ProjectionId,
    span_count: usize,
    truncated: bool,
    halted: bool,
    next_list_id: usize,
    next_anchor_occurrence: usize,
    max_blocks: usize,
    max_spans: usize,
    entity_count: usize,
    spawned_lists: HashSet<usize>,
    link_targets: Vec<Arc<str>>,
    link_indices: HashMap<Arc<str>, LinkTarget>,
    copy_mode: ProjectionCopyMode,
}

impl ProjectionState {
    fn new(
        max_blocks: usize,
        max_spans: usize,
        input_truncated: bool,
        copy_mode: ProjectionCopyMode,
    ) -> Self {
        Self {
            blocks: Vec::new(),
            projection_id: ProjectionId::next(),
            span_count: 0,
            truncated: input_truncated,
            halted: false,
            next_list_id: 0,
            next_anchor_occurrence: 0,
            max_blocks,
            max_spans,
            entity_count: BODY_VIEW_SHELL_ENTITIES,
            spawned_lists: HashSet::new(),
            link_targets: Vec::new(),
            link_indices: HashMap::new(),
            copy_mode,
        }
    }

    fn halt_with_truncation(&mut self) {
        self.truncated = true;
        self.halted = true;
    }

    fn finish(mut self) -> BodyProjection {
        if self.truncated {
            debug_assert!(
                self.entity_count.saturating_add(TRUNCATION_ENTITIES) <= BODY_VIEW_MAX_ENTITIES
            );
            if self.entity_count.saturating_add(TRUNCATION_ENTITIES) <= BODY_VIEW_MAX_ENTITIES {
                self.entity_count += TRUNCATION_ENTITIES;
                self.blocks.push(ProjectedBlock {
                    kind: ProjectedBlockKind::Truncated,
                    quote_depth: 0,
                    spans: vec![ProjectedSpan {
                        text: "Message truncated.".to_owned(),
                        ..default()
                    }],
                    parent_list_item: None,
                    list_item_start: None,
                });
            }
        }
        if self.blocks.is_empty()
            && !self.truncated
            && self.max_blocks > 0
            && self.max_spans > 0
            && self
                .entity_count
                .saturating_add(TRUNCATION_ENTITIES)
                .saturating_add(2)
                <= BODY_VIEW_MAX_ENTITIES
        {
            self.span_count += 1;
            self.entity_count += 2;
            self.blocks.push(ProjectedBlock {
                kind: ProjectedBlockKind::Paragraph,
                quote_depth: 0,
                spans: vec![ProjectedSpan::default()],
                parent_list_item: None,
                list_item_start: None,
            });
        }
        BodyProjection {
            blocks: self.blocks,
            projection_id: self.projection_id,
            link_targets: self.link_targets,
            entity_count: self.entity_count,
            copy_mode: self.copy_mode,
        }
    }

    fn allocate_list_id(&mut self) -> usize {
        let id = self.next_list_id;
        self.next_list_id = self.next_list_id.saturating_add(1);
        id
    }

    fn allocate_anchor_occurrence(&mut self) -> AnchorOccurrence {
        let occurrence = AnchorOccurrence {
            projection_id: self.projection_id,
            index: self.next_anchor_occurrence,
        };
        self.next_anchor_occurrence = self.next_anchor_occurrence.saturating_add(1);
        occurrence
    }

    fn intern_link(&mut self, value: &str) -> Option<LinkTarget> {
        let value = trim_css_whitespace(value);
        if value.is_empty() || value.len() > BODY_VIEW_MAX_URL_BYTES {
            return None;
        }
        if let Some(target) = self.link_indices.get(value).copied() {
            return Some(target);
        }
        let value = Arc::<str>::from(value);
        let target = LinkTarget {
            projection_id: self.projection_id,
            index: self.link_targets.len(),
        };
        self.link_targets.push(value.clone());
        self.link_indices.insert(value, target);
        Some(target)
    }

    fn begin_list_item(&mut self, index: usize, item: ProjectedListItem) -> bool {
        let Some(block) = self.blocks.get(index) else {
            return false;
        };
        let new_list = !self.spawned_lists.contains(&item.key.list_id);
        let old_cost = projected_block_entity_cost(
            block.kind,
            block.quote_depth,
            block.spans.len(),
            false,
            false,
        );
        let new_cost = projected_block_entity_cost(
            block.kind,
            block.quote_depth,
            block.spans.len(),
            new_list,
            true,
        );
        let additional = new_cost.saturating_sub(old_cost);
        if self
            .entity_count
            .saturating_add(additional)
            .saturating_add(TRUNCATION_ENTITIES)
            > BODY_VIEW_MAX_ENTITIES
        {
            let removed = self.blocks.remove(index);
            self.span_count = self.span_count.saturating_sub(removed.spans.len());
            self.entity_count = self.entity_count.saturating_sub(old_cost);
            self.halt_with_truncation();
            return false;
        }
        self.entity_count += additional;
        if new_list {
            self.spawned_lists.insert(item.key.list_id);
        }
        self.blocks[index].list_item_start = Some(item);
        true
    }
}

/// Project one already-sanitised body into ordinary text blocks.
pub fn project_body(body: &SanitizedBody) -> BodyProjection {
    project_body_with_budgets(body, BODY_VIEW_MAX_BLOCKS, BODY_VIEW_MAX_SPANS)
}

fn project_body_with_budgets(
    body: &SanitizedBody,
    max_blocks: usize,
    max_spans: usize,
) -> BodyProjection {
    match &body.content {
        SanitizedContent::Plain(value) => {
            project_plain(value, max_blocks, max_spans, body.input_truncated)
        }
        SanitizedContent::Html(html) => project_html(html, max_blocks, max_spans),
    }
}

fn project_plain(
    value: &str,
    max_blocks: usize,
    max_spans: usize,
    input_truncated: bool,
) -> BodyProjection {
    let mut state = ProjectionState::new(
        max_blocks,
        max_spans,
        input_truncated,
        ProjectionCopyMode::PlainExact,
    );
    let mut paragraph_start = 0usize;
    let mut cursor = 0usize;
    for segment in value.split_inclusive('\n') {
        let line = segment.strip_suffix('\n').unwrap_or(segment);
        let next = cursor.saturating_add(segment.len());
        if line.is_empty() {
            // Keep the complete separator in the preceding projected slice.
            // Concatenating projected span text must reproduce the canonical
            // plain input exactly, including leading, trailing and repeated
            // zero-length lines.
            project_plain_paragraph(&value[paragraph_start..next], &mut state);
            paragraph_start = next;
        }
        cursor = next;
    }
    if !state.halted {
        project_plain_paragraph(&value[paragraph_start..], &mut state);
    }
    state.finish()
}

fn project_plain_paragraph(paragraph: &str, state: &mut ProjectionState) {
    if paragraph.is_empty() || state.halted {
        return;
    }
    let mut spans = Vec::new();
    append_span(state, &mut spans, paragraph, &InlineFormat::default(), true);
    push_plain_block(
        state,
        ProjectedBlockKind::Paragraph,
        ProjectionContext::default(),
        spans,
    );
}

fn project_html(html: &SanitizedHtml, max_blocks: usize, max_spans: usize) -> BodyProjection {
    let dom = parse_document(RcDom::default(), Default::default()).one(html.as_str());
    let mut state = ProjectionState::new(
        max_blocks,
        max_spans,
        html.input_truncated,
        ProjectionCopyMode::HtmlBlocks,
    );
    let root = find_element(&dom.document, "body").unwrap_or_else(|| dom.document.clone());
    project_container(&root, ProjectionContext::default(), &mut state, 0);
    state.finish()
}

fn find_element(node: &Handle, wanted: &str) -> Option<Handle> {
    let mut stack = vec![node.clone()];
    while let Some(current) = stack.pop() {
        if element_name(&current) == Some(wanted) {
            return Some(current);
        }
        let children = current.children.borrow();
        stack.extend(children.iter().rev().cloned());
    }
    None
}

fn has_block_descendant(node: &Handle) -> bool {
    let mut stack = node.children.borrow().iter().cloned().collect::<Vec<_>>();
    while let Some(current) = stack.pop() {
        if element_name(&current).is_some_and(is_block_tag) {
            return true;
        }
        stack.extend(current.children.borrow().iter().cloned());
    }
    false
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FlowChildKind {
    Inline,
    Block,
    BlockAnchor,
}

fn flow_child_kind(node: &Handle) -> FlowChildKind {
    match element_name(node) {
        Some(tag) if is_block_tag(tag) => FlowChildKind::Block,
        Some("a") if has_block_descendant(node) => FlowChildKind::BlockAnchor,
        _ => FlowChildKind::Inline,
    }
}

fn apply_link_identity(
    spans: &mut [ProjectedSpan],
    link: (Option<LinkTarget>, Option<AnchorOccurrence>),
) {
    let (Some(target), Some(occurrence)) = link else {
        return;
    };
    for span in spans {
        span.link_target = Some(target);
        span.anchor_occurrence = Some(occurrence);
    }
}

fn project_block_anchor(
    node: &Handle,
    context: ProjectionContext,
    state: &mut ProjectionState,
    depth: usize,
) {
    let link = intern_link_attribute(node, state);
    let first_block = state.blocks.len();
    project_container(node, context, state, depth);
    for block in &mut state.blocks[first_block..] {
        apply_link_identity(&mut block.spans, link);
    }
}

fn project_container(
    node: &Handle,
    context: ProjectionContext,
    state: &mut ProjectionState,
    depth: usize,
) {
    if state.halted {
        return;
    }
    if depth >= MAX_PROJECTION_DEPTH {
        let mut spans = Vec::new();
        collect_inline(
            node,
            InlineFormat::default(),
            &mut spans,
            state,
            depth,
            false,
        );
        push_html_block(state, ProjectedBlockKind::Paragraph, context, spans);
        return;
    }

    let children = node.children.borrow().clone();
    let mut pending = Vec::new();
    for child in children {
        match flow_child_kind(&child) {
            FlowChildKind::Block => {
                push_html_block(
                    state,
                    ProjectedBlockKind::Paragraph,
                    context,
                    std::mem::take(&mut pending),
                );
                project_block(&child, context, state, depth + 1);
            }
            FlowChildKind::BlockAnchor => {
                push_html_block(
                    state,
                    ProjectedBlockKind::Paragraph,
                    context,
                    std::mem::take(&mut pending),
                );
                project_block_anchor(&child, context, state, depth + 1);
            }
            FlowChildKind::Inline => {
                collect_inline(
                    &child,
                    InlineFormat::default(),
                    &mut pending,
                    state,
                    depth + 1,
                    false,
                );
            }
        }
    }
    push_html_block(state, ProjectedBlockKind::Paragraph, context, pending);
}

fn project_block(
    node: &Handle,
    context: ProjectionContext,
    state: &mut ProjectionState,
    depth: usize,
) {
    if state.halted {
        return;
    }
    if depth >= MAX_PROJECTION_DEPTH {
        let mut spans = Vec::new();
        collect_inline(
            node,
            InlineFormat::default(),
            &mut spans,
            state,
            depth,
            false,
        );
        push_html_block(state, ProjectedBlockKind::Paragraph, context, spans);
        return;
    }
    let Some(tag) = element_name(node) else {
        return;
    };
    match tag {
        "p" => {
            let mut spans = Vec::new();
            collect_inline(
                node,
                InlineFormat::default(),
                &mut spans,
                state,
                depth,
                false,
            );
            push_html_block(state, ProjectedBlockKind::Paragraph, context, spans);
        }
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            let mut spans = Vec::new();
            collect_inline(
                node,
                InlineFormat {
                    bold: true,
                    ..default()
                },
                &mut spans,
                state,
                depth,
                false,
            );
            let level = tag[1..].parse().unwrap_or(6);
            push_html_block(state, ProjectedBlockKind::Heading(level), context, spans);
        }
        "blockquote" => project_container(
            node,
            ProjectionContext {
                quote_depth: context.quote_depth.saturating_add(1),
                ..context
            },
            state,
            depth,
        ),
        "ul" | "menu" => project_list(node, false, context, state, depth),
        "ol" => project_list(node, true, context, state, depth),
        "pre" => {
            let (text, complete) = raw_text_bounded(node, BODY_VIEW_MAX_INPUT_BYTES);
            let mut spans = Vec::new();
            append_span(
                state,
                &mut spans,
                &text,
                &InlineFormat {
                    code: true,
                    ..default()
                },
                true,
            );
            if !complete {
                state.halt_with_truncation();
            }
            push_html_block(state, ProjectedBlockKind::Preformatted, context, spans);
        }
        "hr" => push_html_block(state, ProjectedBlockKind::Rule, context, Vec::new()),
        "tr" => project_table_row(node, context, state, depth),
        _ => project_container(node, context, state, depth),
    }
}

fn project_list(
    node: &Handle,
    ordered: bool,
    context: ProjectionContext,
    state: &mut ProjectionState,
    depth: usize,
) {
    if state.halted {
        return;
    }
    if depth >= MAX_PROJECTION_DEPTH {
        let mut spans = Vec::new();
        collect_inline(
            node,
            InlineFormat::default(),
            &mut spans,
            state,
            depth,
            false,
        );
        push_html_block(state, ProjectedBlockKind::Paragraph, context, spans);
        return;
    }
    let start = if ordered {
        attribute(node, "start")
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(1)
    } else {
        1
    };
    let children: Vec<Handle> = node
        .children
        .borrow()
        .iter()
        .filter(|child| element_name(child) == Some("li"))
        .cloned()
        .collect();
    let set_size = children.len();
    let list_id = state.allocate_list_id();
    for (position, child) in children.into_iter().enumerate() {
        if state.halted {
            return;
        }
        let key = ListItemKey { list_id, position };
        let item_context = ProjectionContext {
            list_depth: context.list_depth.saturating_add(1),
            list_item: Some(key),
            ..context
        };
        let item = ProjectedListItem {
            key,
            ordered,
            depth: context.list_depth,
            index: start.saturating_add(i64::try_from(position).unwrap_or(i64::MAX)),
            set_size,
            parent_list_item: context.list_item,
        };
        let first_block = state.blocks.len();
        // This is the sole child block/inline dispatch. List items carry
        // ownership metadata, but their children use exactly the same
        // traversal as document flow and table cells.
        project_container(&child, item_context, state, depth + 1);
        let starter = state.blocks[first_block..]
            .iter()
            .position(|block| {
                block.parent_list_item == Some(key) && block.list_item_start.is_none()
            })
            .map(|offset| first_block + offset);
        let starter = match starter {
            Some(index) if index == first_block => index,
            _ if state.halted => return,
            // A nested list can precede any directly owned text. Establish the
            // outer item before that nested list so the projected/spawned
            // ownership order remains List → ListItem → List.
            _ => {
                insert_empty_list_item_block(state, first_block, item_context);
                first_block
            }
        };
        if !state.begin_list_item(starter, item) {
            return;
        }
    }
}

fn insert_empty_list_item_block(
    state: &mut ProjectionState,
    index: usize,
    context: ProjectionContext,
) {
    if state.blocks.len() >= state.max_blocks {
        state.halt_with_truncation();
        return;
    }
    let entity_cost = projected_block_entity_cost(
        ProjectedBlockKind::Paragraph,
        context.quote_depth,
        0,
        false,
        false,
    );
    if state
        .entity_count
        .saturating_add(entity_cost)
        .saturating_add(TRUNCATION_ENTITIES)
        > BODY_VIEW_MAX_ENTITIES
    {
        state.halt_with_truncation();
        return;
    }
    state.entity_count += entity_cost;
    state.blocks.insert(
        index,
        ProjectedBlock {
            kind: ProjectedBlockKind::Paragraph,
            quote_depth: context.quote_depth,
            spans: Vec::new(),
            parent_list_item: context.list_item,
            list_item_start: None,
        },
    );
}

fn project_table_row(
    node: &Handle,
    context: ProjectionContext,
    state: &mut ProjectionState,
    depth: usize,
) {
    if state.halted {
        return;
    }
    if depth >= MAX_PROJECTION_DEPTH {
        let mut spans = Vec::new();
        collect_inline(
            node,
            InlineFormat::default(),
            &mut spans,
            state,
            depth,
            false,
        );
        push_html_block(state, ProjectedBlockKind::Paragraph, context, spans);
        return;
    }
    let cells = node
        .children
        .borrow()
        .iter()
        .filter(|child| matches!(element_name(child), Some("td" | "th")))
        .cloned()
        .collect::<Vec<_>>();
    let has_structural_cell = cells.iter().any(|cell| {
        cell.children
            .borrow()
            .iter()
            .any(|child| flow_child_kind(child) != FlowChildKind::Inline)
    });
    if has_structural_cell {
        for cell in cells {
            project_container(&cell, context, state, depth + 1);
        }
        return;
    }

    let mut spans = Vec::new();
    for cell in cells {
        if !spans.is_empty() {
            append_span(state, &mut spans, "  ", &InlineFormat::default(), true);
        }
        collect_inline(
            &cell,
            InlineFormat::default(),
            &mut spans,
            state,
            depth + 1,
            false,
        );
    }
    push_html_block(state, ProjectedBlockKind::Paragraph, context, spans);
}

fn collect_inline(
    node: &Handle,
    mut format: InlineFormat,
    spans: &mut Vec<ProjectedSpan>,
    state: &mut ProjectionState,
    depth: usize,
    preserve: bool,
) {
    if state.halted {
        return;
    }
    if depth >= MAX_PROJECTION_DEPTH {
        let (text, complete) = raw_text_bounded(node, BODY_VIEW_MAX_INPUT_BYTES);
        append_span(state, spans, &text, &format, preserve);
        if !complete {
            state.halt_with_truncation();
        }
        return;
    }
    match &node.data {
        NodeData::Text { contents } => {
            append_span(state, spans, &contents.borrow(), &format, preserve);
        }
        NodeData::Element { name, .. } => {
            let tag = name.local.as_ref();
            match tag {
                "b" | "strong" => format.bold = true,
                "i" | "em" => format.italic = true,
                "code" => format.code = true,
                "a" => {
                    let (target, occurrence) = intern_link_attribute(node, state);
                    format.link_target = target;
                    format.anchor_occurrence = occurrence;
                }
                "br" => {
                    append_span(state, spans, "\n", &format, true);
                    return;
                }
                "img" => {
                    append_image_alt(node, spans, state, &format);
                    return;
                }
                "pre" => {
                    format.code = true;
                    let (text, complete) = raw_text_bounded(node, BODY_VIEW_MAX_INPUT_BYTES);
                    append_span(state, spans, &text, &format, true);
                    if !complete {
                        state.halt_with_truncation();
                    }
                    return;
                }
                _ => {}
            }
            for child in node.children.borrow().clone() {
                collect_inline(&child, format, spans, state, depth + 1, preserve);
            }
        }
        _ => {
            for child in node.children.borrow().clone() {
                collect_inline(&child, format, spans, state, depth + 1, preserve);
            }
        }
    }
}

fn intern_link_attribute(
    node: &Handle,
    state: &mut ProjectionState,
) -> (Option<LinkTarget>, Option<AnchorOccurrence>) {
    // Source-anchor identity is intentionally allocated independently of URL
    // interning. Adjacent anchors with the same href remain distinct links.
    let occurrence = state.allocate_anchor_occurrence();
    let NodeData::Element { attrs, .. } = &node.data else {
        return (None, None);
    };
    let attrs = attrs.borrow();
    let target = attrs
        .iter()
        .find(|attribute| attribute.name.local.as_ref() == "href")
        .and_then(|attribute| state.intern_link(attribute.value.as_ref()));
    (target, target.map(|_| occurrence))
}

fn append_image_alt(
    node: &Handle,
    spans: &mut Vec<ProjectedSpan>,
    state: &mut ProjectionState,
    format: &InlineFormat,
) {
    let NodeData::Element { attrs, .. } = &node.data else {
        return;
    };
    let attrs = attrs.borrow();
    let alt = attrs
        .iter()
        .find(|attribute| attribute.name.local.as_ref() == "alt")
        .map_or("image", |attribute| attribute.value.as_ref());
    append_span(state, spans, "[image: ", format, true);
    append_span(state, spans, alt, format, false);
    append_span(state, spans, "]", format, true);
}

fn append_span(
    state: &mut ProjectionState,
    spans: &mut Vec<ProjectedSpan>,
    source: &str,
    format: &InlineFormat,
    preserve: bool,
) {
    if state.halted {
        return;
    }
    if preserve {
        append_preserved_chunks(state, spans, source, format);
    } else {
        append_normalised_chunks(state, spans, source, format);
    }
}

fn append_preserved_chunks(
    state: &mut ProjectionState,
    spans: &mut Vec<ProjectedSpan>,
    source: &str,
    format: &InlineFormat,
) {
    let mut offset = 0;
    while offset < source.len() && !state.halted {
        let remaining = &source[offset..];
        let accepted = preferred_text_run_boundary(remaining, BODY_VIEW_MAX_TEXT_RUN_BYTES);
        debug_assert!(accepted > 0);
        push_span_text(state, spans, remaining[..accepted].to_owned(), format);
        offset = offset.saturating_add(accepted);
    }
}

fn append_normalised_chunks(
    state: &mut ProjectionState,
    spans: &mut Vec<ProjectedSpan>,
    source: &str,
    format: &InlineFormat,
) {
    let mut text = String::with_capacity(source.len().min(BODY_VIEW_MAX_TEXT_RUN_BYTES));
    let mut output_ends_with_space = spans
        .last()
        .is_none_or(|span| span.text.ends_with(is_css_collapsible_whitespace));
    let mut pending_space = false;
    for grapheme in source.graphemes(true) {
        if state.halted {
            return;
        }
        if grapheme.chars().all(is_css_collapsible_whitespace) {
            pending_space = true;
            continue;
        }
        if pending_space && !output_ends_with_space {
            append_normalised_grapheme(state, spans, &mut text, " ", format);
        }
        pending_space = false;
        append_normalised_grapheme(state, spans, &mut text, grapheme, format);
        output_ends_with_space = false;
    }
    if pending_space && !output_ends_with_space {
        append_normalised_grapheme(state, spans, &mut text, " ", format);
    }
    push_span_text(state, spans, text, format);
}

fn append_normalised_grapheme(
    state: &mut ProjectionState,
    spans: &mut Vec<ProjectedSpan>,
    text: &mut String,
    grapheme: &str,
    format: &InlineFormat,
) {
    if !text.is_empty() && text.len().saturating_add(grapheme.len()) > BODY_VIEW_MAX_TEXT_RUN_BYTES
    {
        let accepted = preferred_word_boundary(text, text.len());
        let tail = text.split_off(accepted);
        push_span_text(state, spans, std::mem::replace(text, tail), format);
        if state.halted {
            return;
        }
    }
    text.push_str(grapheme);
    while text.len() > BODY_VIEW_MAX_TEXT_RUN_BYTES {
        let accepted = preferred_text_run_boundary(text, BODY_VIEW_MAX_TEXT_RUN_BYTES);
        let tail = text.split_off(accepted);
        push_span_text(state, spans, std::mem::replace(text, tail), format);
        if state.halted {
            return;
        }
    }
}

fn preferred_text_run_boundary(value: &str, max_bytes: usize) -> usize {
    if value.len() <= max_bytes {
        return value.len();
    }

    let mut hard_boundary = value
        .grapheme_indices(true)
        .map(|(index, grapheme)| index.saturating_add(grapheme.len()))
        .take_while(|end| *end <= max_bytes)
        .last()
        .unwrap_or_else(|| value.graphemes(true).next().map_or(0, str::len));
    if hard_boundary > max_bytes {
        return hard_boundary;
    }
    let original_hard_boundary = hard_boundary;
    while hard_boundary > 0 && boundary_touches_no_break_character(value, hard_boundary) {
        hard_boundary = value[..hard_boundary]
            .grapheme_indices(true)
            .map(|(index, _)| index)
            .next_back()
            .unwrap_or(0);
    }
    if hard_boundary == 0 {
        hard_boundary = original_hard_boundary;
    }

    preferred_word_boundary(value, hard_boundary)
}

fn boundary_touches_no_break_character(value: &str, boundary: usize) -> bool {
    value[..boundary]
        .chars()
        .next_back()
        .is_some_and(is_no_break_character)
        || value[boundary..]
            .chars()
            .next()
            .is_some_and(is_no_break_character)
}

fn is_no_break_character(character: char) -> bool {
    matches!(character, '\u{00A0}' | '\u{202F}' | '\u{2060}' | '\u{FEFF}')
}

fn preferred_word_boundary(value: &str, hard_boundary: usize) -> usize {
    let lookback_start = hard_boundary.saturating_sub(TEXT_RUN_WORD_BOUNDARY_LOOKBACK_BYTES);
    value[..hard_boundary]
        .char_indices()
        .filter_map(|(index, character)| {
            is_css_collapsible_whitespace(character)
                .then_some(index.saturating_add(character.len_utf8()))
        })
        .rfind(|index| *index >= lookback_start)
        .unwrap_or(hard_boundary)
}

fn push_span_text(
    state: &mut ProjectionState,
    spans: &mut Vec<ProjectedSpan>,
    text: String,
    format: &InlineFormat,
) {
    if text.is_empty() || state.halted {
        return;
    }
    debug_assert!(text.len() <= BODY_VIEW_MAX_TEXT_RUN_BYTES || text.graphemes(true).count() == 1);
    if let Some(last) = spans.last_mut() {
        if last.bold == format.bold
            && last.italic == format.italic
            && last.code == format.code
            && last.link_target == format.link_target
            && last.anchor_occurrence == format.anchor_occurrence
            && last.text.len().saturating_add(text.len()) <= BODY_VIEW_MAX_TEXT_RUN_BYTES
        {
            last.text.push_str(&text);
            return;
        }
    }
    if state.span_count >= state.max_spans {
        state.halt_with_truncation();
        return;
    }
    state.span_count += 1;
    spans.push(ProjectedSpan {
        text,
        bold: format.bold,
        italic: format.italic,
        code: format.code,
        link_target: format.link_target,
        anchor_occurrence: format.anchor_occurrence,
    });
}

fn push_plain_block(
    state: &mut ProjectionState,
    kind: ProjectedBlockKind,
    context: ProjectionContext,
    spans: Vec<ProjectedSpan>,
) {
    push_block(state, kind, context, spans, BlockWhitespace::Preserve);
}

fn push_html_block(
    state: &mut ProjectionState,
    kind: ProjectedBlockKind,
    context: ProjectionContext,
    spans: Vec<ProjectedSpan>,
) {
    push_block(state, kind, context, spans, BlockWhitespace::HtmlCollapsed);
}

fn push_block(
    state: &mut ProjectionState,
    kind: ProjectedBlockKind,
    context: ProjectionContext,
    mut spans: Vec<ProjectedSpan>,
    whitespace: BlockWhitespace,
) {
    if whitespace == BlockWhitespace::HtmlCollapsed && kind != ProjectedBlockKind::Preformatted {
        trim_spans(&mut spans);
    }
    let permits_empty = kind == ProjectedBlockKind::Rule;
    if !permits_empty && spans.iter().all(|span| span.text.is_empty()) {
        return;
    }
    if state.blocks.len() >= state.max_blocks {
        state.halt_with_truncation();
        return;
    }
    let entity_cost =
        projected_block_entity_cost(kind, context.quote_depth, spans.len(), false, false);
    if state
        .entity_count
        .saturating_add(entity_cost)
        .saturating_add(TRUNCATION_ENTITIES)
        > BODY_VIEW_MAX_ENTITIES
    {
        state.halt_with_truncation();
        return;
    }
    state.entity_count += entity_cost;
    state.blocks.push(ProjectedBlock {
        kind,
        quote_depth: context.quote_depth,
        spans,
        parent_list_item: context.list_item,
        list_item_start: None,
    });
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BlockWhitespace {
    Preserve,
    HtmlCollapsed,
}

fn projected_block_entity_cost(
    kind: ProjectedBlockKind,
    quote_depth: usize,
    spans: usize,
    new_list: bool,
    starts_list_item: bool,
) -> usize {
    let marker = usize::from(kind == ProjectedBlockKind::Rule);
    1usize
        .saturating_add(quote_depth.min(MAX_QUOTE_DEPTH))
        .saturating_add(marker)
        .saturating_add(usize::from(new_list))
        // Accessible list-item wrapper, accessible first-row wrapper and marker.
        .saturating_add(usize::from(starts_list_item).saturating_mul(3))
        .saturating_add(spans)
}

fn trim_spans(spans: &mut Vec<ProjectedSpan>) {
    while spans.first().is_some_and(|span| span.text.is_empty()) {
        spans.remove(0);
    }
    while spans.last().is_some_and(|span| span.text.is_empty()) {
        spans.pop();
    }
    if let Some(first) = spans.first_mut() {
        first.text = trim_css_whitespace_start(&first.text).to_owned();
    }
    if let Some(last) = spans.last_mut() {
        last.text = trim_css_whitespace_end(&last.text).to_owned();
    }
}

fn element_name(node: &Handle) -> Option<&str> {
    match &node.data {
        NodeData::Element { name, .. } => Some(name.local.as_ref()),
        _ => None,
    }
}

fn attribute(node: &Handle, wanted: &str) -> Option<String> {
    let NodeData::Element { attrs, .. } = &node.data else {
        return None;
    };
    attrs
        .borrow()
        .iter()
        .find(|attribute| attribute.name.local.as_ref() == wanted)
        .map(|attribute| attribute.value.to_string())
}

fn raw_text_bounded(node: &Handle, max_bytes: usize) -> (String, bool) {
    let mut value = String::with_capacity(max_bytes.min(1024));
    let mut stack = vec![node.clone()];
    let mut complete = true;
    while let Some(current) = stack.pop() {
        if let NodeData::Text { contents } = &current.data {
            let contents = contents.borrow();
            let available = max_bytes.saturating_sub(value.len());
            let accepted = floor_char_boundary(&contents, available);
            value.push_str(&contents[..accepted]);
            if accepted < contents.len() {
                complete = false;
                break;
            }
        }
        let children = current.children.borrow();
        stack.extend(children.iter().rev().cloned());
    }
    if !complete {
        value.shrink_to_fit();
    }
    (value, complete)
}

fn is_block_tag(tag: &str) -> bool {
    SAFE_BLOCK_TAGS.contains(&tag)
}

/// Construction properties for a [`CtkBodyView`].
#[derive(Clone, Debug)]
pub struct CtkBodyViewProps {
    body: SanitizedBody,
    pub accessible_label: String,
    pub render_arm: RenderArm,
    pub viewport_height: f32,
    pub max_blocks: usize,
    pub max_spans: usize,
}

impl CtkBodyViewProps {
    pub fn new(body: SanitizedBody, accessible_label: impl Into<String>) -> Self {
        Self {
            body,
            accessible_label: accessible_label.into(),
            render_arm: RenderArm::Text,
            viewport_height: 480.0,
            max_blocks: BODY_VIEW_MAX_BLOCKS,
            max_spans: BODY_VIEW_MAX_SPANS,
        }
    }

    pub fn render_arm(mut self, render_arm: RenderArm) -> Self {
        self.render_arm = render_arm;
        self
    }

    pub fn viewport_height(mut self, viewport_height: f32) -> Self {
        self.viewport_height = viewport_height.max(1.0);
        self
    }

    /// Set the maximum content blocks. The truncation alert is exempt.
    pub fn max_blocks(mut self, max_blocks: usize) -> Self {
        self.max_blocks = max_blocks;
        self
    }

    /// Set the maximum styled content spans. The truncation alert is exempt.
    pub fn max_spans(mut self, max_spans: usize) -> Self {
        self.max_spans = max_spans;
        self
    }
}

/// Root state for one message-body view.
#[derive(Component, Clone, Debug)]
pub struct CtkBodyView {
    requested_arm: RenderArm,
    effective_arm: RenderArm,
    remote_refs: RemoteRefs,
    projection_id: ProjectionId,
    link_targets: Vec<Arc<str>>,
}

impl CtkBodyView {
    pub fn requested_arm(&self) -> RenderArm {
        self.requested_arm
    }

    pub fn effective_arm(&self) -> RenderArm {
        self.effective_arm
    }

    pub fn remote_refs(&self) -> &RemoteRefs {
        &self.remote_refs
    }
}

/// Root, viewport and accessible document entities.
#[derive(Clone, Copy, Debug)]
pub struct CtkBodyViewEntities {
    pub root: Entity,
    pub viewport: Entity,
    pub document: Entity,
    pub scrollbar: Entity,
}

#[derive(Component, Clone, Debug)]
struct BodyBlock {
    view: Entity,
    text: String,
    selection_block: Option<usize>,
}

#[derive(Component, Clone, Debug)]
struct BodyDocumentCopy {
    view: Entity,
    text: String,
}

#[derive(Component, Clone, Debug)]
struct BodyTextRun {
    view: Entity,
    block: usize,
    range: Range<usize>,
}

#[derive(Component, Clone, Copy, Debug)]
struct BodyTextVisual;

/// Inventory marker for every AccessKit node created by one projection.
///
/// Keeping this separate from `AccessibilityNode` lets the ancestry audit
/// detect a missing node on an expected intermediate rather than filtering the
/// broken entity out of its candidate set.
#[derive(Component, Clone, Copy, Debug)]
#[allow(dead_code)] // Read by the exhaustive test inventory, not runtime policy.
struct BodyAccessibleNode(ProjectionId);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct BodyTextPosition {
    block: usize,
    offset: usize,
}

#[derive(Clone, Debug)]
struct BodySelectionDocument {
    text: String,
    blocks: Vec<BodySelectionBlock>,
}

#[derive(Clone, Copy, Debug)]
struct BodySelectionBlock {
    document_start: usize,
    len: usize,
}

impl BodySelectionDocument {
    fn from_projection(projection: &BodyProjection) -> Self {
        let mut text = String::new();
        let mut blocks = Vec::new();
        for block in &projection.blocks {
            if block.kind == ProjectedBlockKind::Truncated {
                continue;
            }
            if projection.copy_mode == ProjectionCopyMode::HtmlBlocks && !text.is_empty() {
                text.push('\n');
            }
            let block_text = projected_block_copy_text(block);
            blocks.push(BodySelectionBlock {
                document_start: text.len(),
                len: block_text.len(),
            });
            text.push_str(&block_text);
        }
        Self { text, blocks }
    }

    fn clamp_position(&self, position: BodyTextPosition) -> Option<BodyTextPosition> {
        let block = position.block.min(self.blocks.len().checked_sub(1)?);
        let block_info = self.blocks[block];
        let text = &self.text
            [block_info.document_start..block_info.document_start.saturating_add(block_info.len)];
        Some(BodyTextPosition {
            block,
            offset: floor_char_boundary(text, position.offset.min(text.len())),
        })
    }

    fn document_offset(&self, position: BodyTextPosition) -> Option<usize> {
        let position = self.clamp_position(position)?;
        Some(
            self.blocks[position.block]
                .document_start
                .saturating_add(position.offset),
        )
    }

    fn selected_range(
        &self,
        anchor: BodyTextPosition,
        focus: BodyTextPosition,
    ) -> Option<Range<usize>> {
        let anchor = self.clamp_position(anchor)?;
        let focus = self.clamp_position(focus)?;
        let (start, end) = if anchor <= focus {
            (anchor, focus)
        } else {
            (focus, anchor)
        };
        let start = self.document_offset(start)?;
        let end = self.document_offset(end)?;
        (start != end).then_some(start..end)
    }

    fn selected_text(&self, anchor: BodyTextPosition, focus: BodyTextPosition) -> Option<&str> {
        let range = self.selected_range(anchor, focus)?;
        self.text.get(range)
    }

    #[cfg(test)]
    fn select_all_positions(&self) -> Option<(BodyTextPosition, BodyTextPosition)> {
        let last = self.blocks.len().checked_sub(1)?;
        Some((
            BodyTextPosition {
                block: 0,
                offset: 0,
            },
            BodyTextPosition {
                block: last,
                offset: self.blocks[last].len,
            },
        ))
    }
}

#[derive(Clone, Debug)]
struct RuntimeSelectionRun {
    entity: Entity,
    range: Range<usize>,
}

#[derive(Clone, Debug)]
struct RuntimeSelectionBlock {
    focus_entity: Entity,
    runs: Vec<RuntimeSelectionRun>,
}

#[derive(Component, Clone, Debug)]
struct BodyProjectionRuntime {
    document: BodySelectionDocument,
    blocks: Vec<RuntimeSelectionBlock>,
}

#[derive(Clone, Copy, Debug)]
struct BodySelectionGesture {
    pointer: PointerId,
    owns_selection: bool,
    saw_drag: bool,
    suppress_click: bool,
    suppression_consumed: bool,
}

#[derive(Component, Clone, Debug, Default)]
struct BodyTextSelection {
    anchor: Option<BodyTextPosition>,
    focus: Option<BodyTextPosition>,
    gestures: Vec<BodySelectionGesture>,
    painted_entities: Vec<Entity>,
}

impl BodyTextSelection {
    fn set_caret(&mut self, position: BodyTextPosition) {
        self.anchor = Some(position);
        self.focus = Some(position);
    }

    fn extend_to(&mut self, position: BodyTextPosition) {
        if self.anchor.is_none() {
            self.anchor = Some(position);
        }
        self.focus = Some(position);
    }

    fn clear_selection(&mut self) {
        self.anchor = None;
        self.focus = None;
    }

    fn selected_text<'a>(&self, document: &'a BodySelectionDocument) -> Option<&'a str> {
        document.selected_text(self.anchor?, self.focus?)
    }

    fn begin_drag(&mut self, pointer: PointerId) -> Option<bool> {
        if let Some(active) = self
            .gestures
            .iter()
            .find(|active| active.pointer == pointer)
        {
            return Some(active.owns_selection);
        }
        if self.gestures.len() >= BODY_VIEW_MAX_ACTIVE_SELECTION_POINTERS {
            return None;
        }
        // One anchor/focus pair cannot be meaningfully extended by two fingers.
        // The first active pointer owns it; later pointers are retained so they
        // cannot clobber that gesture and their clicks remain inert.
        let owns_selection = !self.gestures.iter().any(|active| active.owns_selection);
        self.gestures.push(BodySelectionGesture {
            pointer,
            owns_selection,
            saw_drag: false,
            suppress_click: !owns_selection,
            suppression_consumed: false,
        });
        Some(owns_selection)
    }

    fn note_drag_started(&mut self, pointer: PointerId) -> Option<bool> {
        let active = self
            .gestures
            .iter_mut()
            .find(|active| active.pointer == pointer)?;
        active.saw_drag = true;
        Some(active.owns_selection)
    }

    fn begin_extend(&mut self, pointer: PointerId) -> Option<bool> {
        // A Shift press is admitted on the same terms as any other — the first
        // active pointer owns the selection — but it extends a range, so its
        // click must not also activate a link. The bit is latched at admission
        // rather than after a range exists, because the press may still resolve
        // no text position under it and `Pointer<Click>` carries no modifier
        // snapshot to fall back on. Refused by the same tracking bound as
        // `begin_drag`, past which the gesture degrades to an ordinary click.
        let owns_selection = self.begin_drag(pointer)?;
        if let Some(active) = self
            .gestures
            .iter_mut()
            .find(|active| active.pointer == pointer)
        {
            active.suppress_click = true;
        }
        Some(owns_selection)
    }

    fn note_drag_selection(&mut self, document: &BodySelectionDocument, pointer: PointerId) {
        // Bevy starts drag delivery on pointer motion, including tiny jitter.
        // Suppress a link click only after a Drag event has made the logical
        // selection non-empty. Motion within one cursor position remains a
        // normal click, while selecting away and back still counts as a drag.
        let selection_became_non_empty = self.selected_text(document).is_some();
        if selection_became_non_empty {
            if let Some(active) = self
                .gestures
                .iter_mut()
                .find(|active| active.pointer == pointer && active.owns_selection)
            {
                active.suppress_click = true;
            }
        }
    }

    fn release_pointer(&mut self, pointer: PointerId) -> bool {
        let Some(index) = self
            .gestures
            .iter()
            .position(|active| active.pointer == pointer)
        else {
            return false;
        };
        // A drag emits DragEnd after Release. Keep its record until DragEnd so
        // both events remain owned and stop at the body view.
        if !self.gestures[index].saw_drag {
            self.gestures.remove(index);
        }
        true
    }

    fn finish_drag(&mut self, pointer: PointerId) -> bool {
        self.remove_pointer(pointer)
    }

    fn cancel_pointer(&mut self, pointer: PointerId) -> bool {
        self.remove_pointer(pointer)
    }

    fn remove_pointer(&mut self, pointer: PointerId) -> bool {
        let Some(index) = self
            .gestures
            .iter()
            .position(|active| active.pointer == pointer)
        else {
            return false;
        };
        self.gestures.remove(index);
        true
    }

    fn consume_click_suppression(&mut self, pointer: PointerId) -> bool {
        // Bevy 0.19 triggers Click before Release and DragEnd. Consume only the
        // pointer-local suppression bit; gesture teardown belongs to those
        // later lifecycle events.
        let Some(active) = self
            .gestures
            .iter_mut()
            .find(|active| active.pointer == pointer)
        else {
            // No record means this pointer never mutated the selection: every
            // mutation path first looks its gesture up, and the press observer
            // only moves anchor or focus once `begin_drag` admitted the
            // pointer. So there is no selection for suppression to protect,
            // whether the pointer was turned away by the capacity bound or its
            // press missed the hit-test entirely, and the click is an ordinary
            // one. The 16-entry bound is a tracking bound, not an activation
            // boundary — keying this on current occupancy instead would make
            // the same pointer's click depend on whether some unrelated
            // pointer happened to release first.
            return false;
        };
        if active.suppress_click && !active.suppression_consumed {
            active.suppression_consumed = true;
            return true;
        }
        false
    }

    #[cfg(test)]
    fn select_all(&mut self, document: &BodySelectionDocument) {
        let Some((anchor, focus)) = document.select_all_positions() else {
            self.clear_selection();
            return;
        };
        self.anchor = Some(anchor);
        self.focus = Some(focus);
    }
}

#[derive(Component, Clone, Debug)]
struct BodyLink {
    view: Entity,
    target: LinkTarget,
    focus_entity: Entity,
}

/// Emitted on the body-view root after primary-pointer, Enter/Space or
/// accessibility activation. Secondary and middle pointer clicks are ignored
/// so applications retain ownership of context-menu and background-open
/// policy. Consumers decide whether and how to open the safe href.
#[derive(EntityEvent, Clone, Debug, PartialEq, Eq)]
pub struct LinkActivated {
    #[event_target]
    pub body_view: Entity,
    pub href: String,
}

#[derive(EntityEvent, Clone, Copy, Debug, PartialEq, Eq)]
struct SetRenderArm {
    #[event_target]
    body_view: Entity,
    arm: RenderArm,
}

/// Change an instance's requested renderer. Stage A keeps the effective text
/// arm even when the engine arm is requested.
pub fn set_body_render_arm(commands: &mut Commands, body_view: Entity, arm: RenderArm) {
    commands.trigger(SetRenderArm { body_view, arm });
}

/// Installs link activation, block copy and renderer-seam behaviour.
pub struct CtkBodyViewPlugin;

impl Plugin for CtkBodyViewPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<InputFocus>()
            .init_resource::<UiTheme>()
            .init_resource::<Clipboard>()
            .init_resource::<PointerState>()
            .add_message::<AccessibilityActionRequest>()
            .add_observer(on_body_pressed)
            .add_observer(on_body_dragged)
            .add_observer(on_body_released)
            .add_observer(on_body_drag_ended)
            .add_observer(on_body_cancelled)
            .add_observer(on_link_clicked)
            .add_observer(on_body_clicked)
            .add_observer(on_body_keyboard)
            .add_observer(on_set_render_arm)
            .add_systems(
                Update,
                (
                    activate_accessibility_links,
                    (paint_body_block_copy_focus, paint_body_document_copy_focus).run_if(
                        resource_changed::<InputFocus>.or_else(resource_changed::<UiTheme>),
                    ),
                    (
                        sync_added_body_selection_styles,
                        sync_all_body_selection_styles.run_if(resource_changed::<UiTheme>),
                    )
                        .chain(),
                ),
            )
            .add_systems(
                PostUpdate,
                apply_body_selection_geometry.after(bevy::ui::widget::text_system),
            )
            .add_systems(Last, sweep_released_selection_gestures);
    }
}

fn sweep_released_selection_gestures(
    pointer_state: Res<PointerState>,
    mut selections: Query<&mut BodyTextSelection>,
) {
    // bevy_picking 0.19 runs `pointer_events` in PreUpdate
    // (`bevy_picking-0.19.0/src/lib.rs:445-455`) and queues every trigger with
    // `Commands::trigger`
    // (`bevy_ecs-0.19.0/src/system/commands/mod.rs:1169-1171`). The schedule
    // applies its final deferred buffers before returning
    // (`bevy_ecs-0.19.0/src/schedule/executor/multi_threaded.rs:293-302`), so
    // Last observes settled PointerState after every queued Click, Release,
    // DragEnd and Cancel observer, however many inputs the run batched.
    // One definition of "still live", used by both the change-detection probe
    // and the retain, so the two can never answer differently.
    let still_pressed = |gesture: &BodySelectionGesture| {
        pointer_state
            .get(gesture.pointer, PointerButton::Primary)
            .is_some_and(|state| !state.pressing.is_empty())
    };
    for mut selection in &mut selections {
        // Read through the immutable deref first: touching `gestures` mutably
        // would mark every body view changed on every frame.
        if selection.gestures.iter().all(still_pressed) {
            continue;
        }
        selection.gestures.retain(still_pressed);
    }
}

/// Spawn a scrollable text projection. The raw [`BodySource`] cannot be passed
/// here; callers must supply a [`SanitizedBody`].
pub fn spawn_body_view(commands: &mut Commands, props: CtkBodyViewProps) -> CtkBodyViewEntities {
    let projection = project_body_with_budgets(&props.body, props.max_blocks, props.max_spans);
    let selection_document = BodySelectionDocument::from_projection(&projection);
    let document_copy = selection_document.text.clone();
    let remote_refs = props.body.remote_refs.clone();
    let projection_id = projection.projection_id;
    let link_targets = projection.link_targets.clone();
    let root = commands.spawn_empty().id();

    let document = commands
        .spawn((
            Node {
                width: percent(100),
                flex_shrink: 0.0,
                flex_direction: FlexDirection::Column,
                row_gap: px(10),
                padding: UiRect::all(px(14)),
                ..default()
            },
            document_accessibility(&props.accessible_label),
            BodyAccessibleNode(projection_id),
            TabIndex(0),
            BackgroundColor(Color::NONE),
            BodyDocumentCopy {
                view: root,
                text: document_copy,
            },
        ))
        .id();

    let viewport = commands
        .spawn((
            Node {
                width: percent(100),
                height: percent(100),
                overflow: Overflow::scroll_y(),
                ..default()
            },
            ScrollArea,
            ScrollPosition::default(),
        ))
        .add_child(document)
        .id();

    let scrollbar = commands
        .spawn((
            Node {
                width: px(8),
                height: percent(100),
                ..default()
            },
            Scrollbar::new(viewport, ControlOrientation::Vertical, 12.0),
        ))
        .with_child((
            Hovered::default(),
            Pickable::default(),
            ThemeBackgroundColor(tokens::CONTROL),
            ScrollbarThumb {
                border_radius: BorderRadius::all(px(4)),
                border: UiRect::ZERO,
            },
        ))
        .id();

    commands
        .entity(root)
        .insert((
            Node {
                display: Display::Grid,
                width: percent(100),
                height: px(props.viewport_height),
                grid_template_columns: vec![
                    RepeatedGridTrack::flex(1, 1.0),
                    RepeatedGridTrack::px(1, 8.0),
                ],
                grid_template_rows: vec![RepeatedGridTrack::flex(1, 1.0)],
                column_gap: px(2),
                overflow: Overflow::clip(),
                ..default()
            },
            ThemeBackgroundColor(tokens::PANEL),
            CtkBodyView {
                requested_arm: props.render_arm,
                effective_arm: RenderArm::Text,
                remote_refs,
                projection_id,
                link_targets,
            },
            BodyTextSelection::default(),
        ))
        .add_children(&[viewport, scrollbar]);

    let blocks = spawn_projection(commands, root, document, &projection);
    commands.entity(root).insert(BodyProjectionRuntime {
        document: selection_document,
        blocks,
    });

    CtkBodyViewEntities {
        root,
        viewport,
        document,
        scrollbar,
    }
}

#[cfg(test)]
fn projection_copy_text(projection: &BodyProjection) -> String {
    BodySelectionDocument::from_projection(projection).text
}

fn projected_block_copy_text(block: &ProjectedBlock) -> String {
    let mut text = String::new();
    if let Some(item) = block.list_item_start {
        if item.ordered {
            text.push_str(&format!("{}. ", item.index));
        } else {
            text.push_str("• ");
        }
    }
    text.push_str(&block.text());
    text
}

struct SpawnedProjectedBlock {
    outer: Entity,
    semantic: Entity,
    focus_entity: Entity,
    selection_runs: Vec<RuntimeSelectionRun>,
}

struct AnchorSpawnGroup {
    target: LinkTarget,
    accessible_name: String,
    last_block: usize,
    primary: Option<Entity>,
    valid: bool,
}

fn collect_anchor_spawn_groups(
    projection: &BodyProjection,
) -> HashMap<AnchorOccurrence, AnchorSpawnGroup> {
    let mut groups = HashMap::<AnchorOccurrence, AnchorSpawnGroup>::new();
    for (block_index, block) in projection.blocks.iter().enumerate() {
        for span in &block.spans {
            let Some((target, occurrence)) = span.link_identity() else {
                continue;
            };
            if let Some(group) = groups.get_mut(&occurrence) {
                group.valid &= group.target == target;
                if group.last_block != block_index {
                    let joined_needs_space = !group.accessible_name.is_empty()
                        && !span.text.is_empty()
                        && group
                            .accessible_name
                            .chars()
                            .next_back()
                            .is_some_and(|character| !is_css_collapsible_whitespace(character))
                        && span
                            .text
                            .chars()
                            .next()
                            .is_some_and(|character| !is_css_collapsible_whitespace(character));
                    if joined_needs_space {
                        group.accessible_name.push(' ');
                    }
                    group.last_block = block_index;
                }
                group.accessible_name.push_str(&span.text);
            } else {
                groups.insert(
                    occurrence,
                    AnchorSpawnGroup {
                        target,
                        accessible_name: span.text.clone(),
                        last_block: block_index,
                        primary: None,
                        valid: true,
                    },
                );
            }
        }
    }
    groups
}

fn spawn_projection(
    commands: &mut Commands,
    view: Entity,
    document: Entity,
    projection: &BodyProjection,
) -> Vec<RuntimeSelectionBlock> {
    let mut lists = HashMap::<usize, Entity>::new();
    let mut list_items = HashMap::<ListItemKey, Entity>::new();
    let mut anchor_groups = collect_anchor_spawn_groups(projection);
    let mut runtime_blocks = Vec::new();
    for block in &projection.blocks {
        let selection_block =
            (block.kind != ProjectedBlockKind::Truncated).then_some(runtime_blocks.len());
        let Some(item) = block.list_item_start else {
            let spawned = spawn_projected_block(
                commands,
                view,
                projection.projection_id,
                block,
                selection_block,
                &mut anchor_groups,
            );
            let parent = block
                .parent_list_item
                .and_then(|key| list_items.get(&key).copied())
                .unwrap_or(document);
            commands.entity(parent).add_child(spawned.outer);
            if selection_block.is_some() {
                runtime_blocks.push(RuntimeSelectionBlock {
                    focus_entity: spawned.focus_entity,
                    runs: spawned.selection_runs,
                });
            }
            continue;
        };

        let list_entity = if let Some(entity) = lists.get(&item.key.list_id).copied() {
            entity
        } else {
            let mut accessible = accesskit::Node::new(Role::List);
            accessible.set_size_of_set(item.set_size);
            let list_entity = commands
                .spawn((
                    Node {
                        width: percent(100),
                        flex_direction: FlexDirection::Column,
                        row_gap: px(10),
                        ..default()
                    },
                    AccessibilityNode::from(accessible),
                    BodyAccessibleNode(projection.projection_id),
                ))
                .id();
            let parent = item
                .parent_list_item
                .and_then(|key| list_items.get(&key).copied())
                .unwrap_or(document);
            commands.entity(parent).add_child(list_entity);
            lists.insert(item.key.list_id, list_entity);
            list_entity
        };

        let spawned = spawn_projected_block(
            commands,
            view,
            projection.projection_id,
            block,
            selection_block,
            &mut anchor_groups,
        );
        commands.entity(list_entity).add_child(spawned.outer);
        list_items.insert(item.key, spawned.semantic);
        if selection_block.is_some() {
            runtime_blocks.push(RuntimeSelectionBlock {
                focus_entity: spawned.focus_entity,
                runs: spawned.selection_runs,
            });
        }
    }
    runtime_blocks
}

fn spawn_projected_block(
    commands: &mut Commands,
    view: Entity,
    projection_id: ProjectionId,
    block: &ProjectedBlock,
    selection_block: Option<usize>,
    anchor_groups: &mut HashMap<AnchorOccurrence, AnchorSpawnGroup>,
) -> SpawnedProjectedBlock {
    let text = block.text();
    let mut accessible = accesskit::Node::new(match block.kind {
        ProjectedBlockKind::Heading(_) => Role::Heading,
        ProjectedBlockKind::Preformatted => Role::Code,
        ProjectedBlockKind::Truncated => Role::Alert,
        _ => Role::Paragraph,
    });
    accessible.set_value(text.clone());
    if let ProjectedBlockKind::Heading(level) = block.kind {
        accessible.set_level(level.into());
    }
    let content = commands
        .spawn((
            Node {
                width: percent(100),
                min_width: px(0),
                flex_wrap: FlexWrap::Wrap,
                align_items: AlignItems::Baseline,
                ..default()
            },
            Pickable::default(),
            BackgroundColor(Color::NONE),
            AccessibilityNode::from(accessible.clone()),
            BodyAccessibleNode(projection_id),
            BodyBlock {
                view,
                text: projected_block_copy_text(block),
                selection_block,
            },
        ))
        .id();

    let marker_len = block.list_item_start.map_or(0, |item| {
        if item.ordered {
            format!("{}. ", item.index).len()
        } else {
            "• ".len()
        }
    });
    let mut selection_runs = Vec::new();
    if block.kind == ProjectedBlockKind::Rule {
        let rule_range = marker_len..marker_len.saturating_add("────────".len());
        let marker = spawn_text_run(
            commands,
            view,
            projection_id,
            &ProjectedSpan {
                text: "────────".to_owned(),
                ..default()
            },
            13.0,
            LinkRunSemantics::Unlinked,
            selection_block.map(|block| BodyTextRun {
                view,
                block,
                range: rule_range.clone(),
            }),
        );
        if selection_block.is_some() {
            selection_runs.push(RuntimeSelectionRun {
                entity: marker,
                range: rule_range,
            });
        }
        commands.entity(content).add_child(marker);
    }

    let font_size = match block.kind {
        ProjectedBlockKind::Heading(1) => 20.0,
        ProjectedBlockKind::Heading(2) => 18.0,
        ProjectedBlockKind::Heading(3) => 16.0,
        ProjectedBlockKind::Heading(_) => 14.0,
        _ => 13.0,
    };
    selection_runs.extend(spawn_projected_spans(
        commands,
        view,
        projection_id,
        content,
        &block.spans,
        font_size,
        selection_block,
        marker_len,
        anchor_groups,
    ));

    let mut outer = content;
    for _ in 0..block.quote_depth.min(MAX_QUOTE_DEPTH) {
        outer = commands
            .spawn((
                Node {
                    width: percent(100),
                    border: UiRect::left(px(2)),
                    padding: UiRect::left(px(9)),
                    ..default()
                },
                BorderColor::all(Color::NONE),
                ThemeBorderColor(tokens::TEXT_DIM),
                quote_accessibility(),
                BodyAccessibleNode(projection_id),
            ))
            .add_child(outer)
            .id();
    }
    if let Some(item) = block.list_item_start {
        let marker_text = if item.ordered {
            format!("{}. ", item.index)
        } else {
            "• ".to_owned()
        };
        let marker = spawn_text_run(
            commands,
            view,
            projection_id,
            &ProjectedSpan {
                text: marker_text.clone(),
                ..default()
            },
            13.0,
            LinkRunSemantics::ListMarker,
            selection_block.map(|block| BodyTextRun {
                view,
                block,
                range: 0..marker_text.len(),
            }),
        );
        if selection_block.is_some() {
            selection_runs.insert(
                0,
                RuntimeSelectionRun {
                    entity: marker,
                    range: 0..marker_text.len(),
                },
            );
        }
        let first_row = commands
            .spawn((
                Node {
                    width: percent(100),
                    min_width: px(0),
                    flex_direction: FlexDirection::Row,
                    align_items: AlignItems::Baseline,
                    ..default()
                },
                AccessibilityNode::from(accesskit::Node::new(Role::GenericContainer)),
                BodyAccessibleNode(projection_id),
            ))
            .add_children(&[marker, outer])
            .id();

        let mut item_accessible = accesskit::Node::new(Role::ListItem);
        item_accessible.set_value(format!("{marker_text}{text}"));
        // AccessKit is zero-based; the AT-SPI adapter performs the +1.
        item_accessible.set_position_in_set(item.key.position);
        item_accessible.set_size_of_set(item.set_size);
        let semantic = commands
            .spawn((
                Node {
                    width: percent(100),
                    flex_direction: FlexDirection::Column,
                    row_gap: px(10),
                    margin: UiRect::left(px(item.depth.min(16) as f32 * 18.0)),
                    ..default()
                },
                Pickable::default(),
                BackgroundColor(Color::NONE),
                AccessibilityNode::from(item_accessible),
                BodyAccessibleNode(projection_id),
            ))
            .add_child(first_row)
            .id();
        SpawnedProjectedBlock {
            outer: semantic,
            semantic,
            focus_entity: content,
            selection_runs,
        }
    } else {
        SpawnedProjectedBlock {
            outer,
            semantic: content,
            focus_entity: content,
            selection_runs,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_projected_spans(
    commands: &mut Commands,
    view: Entity,
    projection_id: ProjectionId,
    content: Entity,
    spans: &[ProjectedSpan],
    font_size: f32,
    selection_block: Option<usize>,
    mut block_offset: usize,
    anchor_groups: &mut HashMap<AnchorOccurrence, AnchorSpawnGroup>,
) -> Vec<RuntimeSelectionRun> {
    let mut runs = Vec::with_capacity(spans.len());
    for span in spans {
        let range = block_offset..block_offset.saturating_add(span.text.len());
        block_offset = range.end;
        let selection_run = selection_block.map(|block| BodyTextRun {
            view,
            block,
            range: range.clone(),
        });
        let Some((target, occurrence)) = span.link_identity() else {
            let entity = spawn_text_run(
                commands,
                view,
                projection_id,
                span,
                font_size,
                LinkRunSemantics::Unlinked,
                selection_run,
            );
            if selection_block.is_some() {
                runs.push(RuntimeSelectionRun { entity, range });
            }
            commands.entity(content).add_child(entity);
            continue;
        };
        let Some(group) = anchor_groups
            .get_mut(&occurrence)
            .filter(|group| group.valid && group.target == target)
        else {
            let entity = spawn_text_run(
                commands,
                view,
                projection_id,
                span,
                font_size,
                LinkRunSemantics::Unlinked,
                selection_run,
            );
            if selection_block.is_some() {
                runs.push(RuntimeSelectionRun { entity, range });
            }
            commands.entity(content).add_child(entity);
            continue;
        };
        let entity = if let Some(primary) = group.primary {
            spawn_text_run(
                commands,
                view,
                projection_id,
                span,
                font_size,
                LinkRunSemantics::Continuation(primary),
                selection_run,
            )
        } else {
            let primary = spawn_text_run(
                commands,
                view,
                projection_id,
                span,
                font_size,
                LinkRunSemantics::Primary(&group.accessible_name),
                selection_run,
            );
            group.primary = Some(primary);
            primary
        };
        if selection_block.is_some() {
            runs.push(RuntimeSelectionRun { entity, range });
        }
        commands.entity(content).add_child(entity);
    }
    runs
}

impl ProjectedSpan {
    fn link_identity(&self) -> Option<(LinkTarget, AnchorOccurrence)> {
        self.link_target.zip(self.anchor_occurrence)
    }
}

#[derive(Clone, Copy)]
enum LinkRunSemantics<'a> {
    Unlinked,
    ListMarker,
    Primary(&'a str),
    Continuation(Entity),
}

fn spawn_text_run(
    commands: &mut Commands,
    view: Entity,
    projection_id: ProjectionId,
    span: &ProjectedSpan,
    font_size: f32,
    link_semantics: LinkRunSemantics<'_>,
    selection_run: Option<BodyTextRun>,
) -> Entity {
    let mut font = TextFont::from_font_size(font_size);
    if span.bold {
        font.weight = FontWeight::BOLD;
    }
    if span.italic {
        font.style = FontStyle::Italic;
    }
    if span.code {
        font.font = FontSource::UiMonospace;
    }

    let mut entity = commands.spawn((
        Node {
            min_width: px(0),
            flex_shrink: 1.0,
            ..default()
        },
        Text::new(span.text.clone()),
        TextLayout::default(),
        font,
        ThemeTextColor(tokens::TEXT),
        TextCursorStyle::default(),
        BodyTextVisual,
        Pickable::default(),
    ));
    if let Some(selection_run) = selection_run {
        entity.insert(selection_run);
    }
    let entity_id = entity.id();
    match link_semantics {
        LinkRunSemantics::Primary(accessible_name) => {
            let (target, _) = span
                .link_identity()
                .expect("primary link run must retain its source-anchor identity");
            let mut accessible = accesskit::Node::new(Role::Link);
            accessible.set_label(accessible_name);
            accessible.add_action(Action::Click);
            entity.insert((
                Underline,
                Pickable::default(),
                TabIndex(0),
                AccessibilityNode::from(accessible),
                BodyAccessibleNode(projection_id),
                BodyLink {
                    view,
                    target,
                    focus_entity: entity_id,
                },
            ));
        }
        LinkRunSemantics::Continuation(focus_entity) => {
            let (target, _) = span
                .link_identity()
                .expect("continuation link run must retain its source-anchor identity");
            entity.insert((
                Underline,
                Pickable::default(),
                BodyLink {
                    view,
                    target,
                    focus_entity,
                },
            ));
        }
        LinkRunSemantics::Unlinked => {
            let mut accessible =
                accesskit::Node::new(if span.code { Role::Code } else { Role::TextRun });
            accessible.set_value(span.text.clone());
            entity.insert((
                AccessibilityNode::from(accessible),
                BodyAccessibleNode(projection_id),
            ));
        }
        LinkRunSemantics::ListMarker => {
            let mut accessible = accesskit::Node::new(Role::ListMarker);
            accessible.set_value(span.text.clone());
            entity.insert((
                AccessibilityNode::from(accessible),
                BodyAccessibleNode(projection_id),
            ));
        }
    }
    entity_id
}

fn document_accessibility(label: &str) -> AccessibilityNode {
    let mut node = accesskit::Node::new(Role::Document);
    node.set_label(label);
    AccessibilityNode::from(node)
}

fn quote_accessibility() -> AccessibilityNode {
    AccessibilityNode::from(accesskit::Node::new(Role::Blockquote))
}

fn selection_target_body_view(
    mut entity: Entity,
    parents: &Query<&ChildOf>,
    text_runs: &Query<&BodyTextRun>,
    blocks: &Query<&BodyBlock>,
    links: &Query<&BodyLink>,
    documents: &Query<&BodyDocumentCopy>,
) -> Option<Entity> {
    loop {
        if let Ok(run) = text_runs.get(entity) {
            return Some(run.view);
        }
        if let Ok(block) = blocks.get(entity) {
            return Some(block.view);
        }
        if let Ok(link) = links.get(entity) {
            return Some(link.view);
        }
        if let Ok(document) = documents.get(entity) {
            return Some(document.view);
        }
        // Selection eligibility comes from projected-text ownership, not from
        // merely reaching the CtkBodyView root. The root also owns controls
        // such as the scrollbar, and Bevy runs every observer on the current
        // entity even when another observer stops propagation.
        entity = parents.get(entity).ok()?.parent();
    }
}

type BodyTextLayoutQuery<'w, 's> = Query<
    'w,
    's,
    (
        &'static BodyTextRun,
        &'static ComputedNode,
        &'static ComputedUiRenderTargetInfo,
        &'static UiGlobalTransform,
        &'static ComputedTextBlock,
        &'static TextLayoutInfo,
    ),
>;

#[allow(clippy::too_many_arguments)]
fn hit_test_text_run(
    screen_position: Vec2,
    ui_scale: f32,
    run: &BodyTextRun,
    node: &ComputedNode,
    target: &ComputedUiRenderTargetInfo,
    transform: &UiGlobalTransform,
    computed: &ComputedTextBlock,
    layout_info: &TextLayoutInfo,
) -> Option<BodyTextPosition> {
    let layout = computed.buffer();
    // Bevy shapes ordinary UI text in physical pixels. Its own editable-text
    // pointer path performs the same target/UI-scale conversion before
    // entering local space. `TextLayoutInfo::scale_factor` records that Parley
    // scale; reject stale/mismatched pairs instead of mixing coordinate spaces.
    if (layout.scale() - layout_info.scale_factor).abs() > 1.0e-4 {
        return None;
    }
    let inverse = transform.try_inverse()?;
    let local = inverse
        .transform_point2(screen_position * target.scale_factor() / ui_scale.max(f32::EPSILON))
        - node.content_box().min;
    let offset = ParleyCursor::from_point(layout, local.x, local.y)
        .index()
        .min(run.range.len());
    Some(BodyTextPosition {
        block: run.block,
        offset: run.range.start.saturating_add(offset),
    })
}

fn node_distance_squared(
    screen_position: Vec2,
    ui_scale: f32,
    node: &ComputedNode,
    target: &ComputedUiRenderTargetInfo,
    transform: &UiGlobalTransform,
) -> Option<f32> {
    let inverse = transform.try_inverse()?;
    let local = inverse
        .transform_point2(screen_position * target.scale_factor() / ui_scale.max(f32::EPSILON))
        - node.content_box().min;
    let size = node.content_box().size();
    let dx = if local.x < 0.0 {
        -local.x
    } else if local.x > size.x {
        local.x - size.x
    } else {
        0.0
    };
    let dy = if local.y < 0.0 {
        -local.y
    } else if local.y > size.y {
        local.y - size.y
    } else {
        0.0
    };
    Some(dx.mul_add(dx, dy * dy))
}

fn hit_test_runtime_block(
    screen_position: Vec2,
    ui_scale: f32,
    block: &RuntimeSelectionBlock,
    text_layouts: &BodyTextLayoutQuery<'_, '_>,
) -> Option<BodyTextPosition> {
    block
        .runs
        .iter()
        .take(BODY_VIEW_MAX_SELECTION_RUNS_PER_FRAME)
        .filter_map(|runtime_run| {
            let (run, node, target, transform, computed, layout_info) =
                text_layouts.get(runtime_run.entity).ok()?;
            let distance =
                node_distance_squared(screen_position, ui_scale, node, target, transform)?;
            let position = hit_test_text_run(
                screen_position,
                ui_scale,
                run,
                node,
                target,
                transform,
                computed,
                layout_info,
            )?;
            Some((distance, position))
        })
        .min_by(|left, right| left.0.total_cmp(&right.0))
        .map(|(_, position)| position)
}

fn nearest_runtime_block(
    screen_position: Vec2,
    ui_scale: f32,
    runtime: &BodyProjectionRuntime,
    block_nodes: &Query<(
        &ComputedNode,
        &ComputedUiRenderTargetInfo,
        &UiGlobalTransform,
    )>,
) -> Option<usize> {
    let block_centre = |index: usize| {
        let block = runtime.blocks.get(index)?;
        let (_, target, transform) = block_nodes.get(block.focus_entity).ok()?;
        Some(
            transform.translation.y * ui_scale.max(f32::EPSILON) / target.scale_factor()
                - screen_position.y,
        )
    };
    let mut low = 0usize;
    let mut high = runtime.blocks.len();
    while low < high {
        let middle = low + (high - low) / 2;
        if block_centre(middle)? < 0.0 {
            low = middle + 1;
        } else {
            high = middle;
        }
    }
    match (
        low.checked_sub(1),
        (low < runtime.blocks.len()).then_some(low),
    ) {
        (Some(before), Some(after)) => {
            let before_distance = block_centre(before)?.abs();
            let after_distance = block_centre(after)?.abs();
            Some(if before_distance <= after_distance {
                before
            } else {
                after
            })
        }
        (Some(before), None) => Some(before),
        (None, Some(after)) => Some(after),
        (None, None) => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn body_selection_hit_test(
    view: Entity,
    candidate: Option<Entity>,
    screen_position: Vec2,
    ui_scale: f32,
    runtime: &BodyProjectionRuntime,
    parents: &Query<&ChildOf>,
    block_components: &Query<&BodyBlock>,
    text_layouts: &BodyTextLayoutQuery<'_, '_>,
    block_nodes: &Query<(
        &ComputedNode,
        &ComputedUiRenderTargetInfo,
        &UiGlobalTransform,
    )>,
) -> Option<BodyTextPosition> {
    let mut candidate = candidate;
    let mut candidate_block = None;
    while let Some(entity) = candidate {
        if let Ok((run, node, target, transform, computed, layout_info)) = text_layouts.get(entity)
        {
            if run.view == view {
                return hit_test_text_run(
                    screen_position,
                    ui_scale,
                    run,
                    node,
                    target,
                    transform,
                    computed,
                    layout_info,
                );
            }
        }
        if let Ok(block) = block_components.get(entity) {
            if block.view == view {
                candidate_block = block.selection_block;
                break;
            }
        }
        candidate = parents.get(entity).ok().map(ChildOf::parent);
    }
    let block = candidate_block
        .or_else(|| nearest_runtime_block(screen_position, ui_scale, runtime, block_nodes))?;
    hit_test_runtime_block(
        screen_position,
        ui_scale,
        runtime.blocks.get(block)?,
        text_layouts,
    )
}

fn hovered_body_text_entity(
    hover_map: Option<&HoverMap>,
    pointer: PointerId,
    view: Entity,
    text_runs: &Query<&BodyTextRun>,
) -> Option<Entity> {
    hover_map?
        .get(&pointer)?
        .iter()
        .filter_map(|(entity, hit)| {
            text_runs
                .get(*entity)
                .ok()
                .filter(|run| run.view == view)
                .map(|_| (*entity, hit.depth))
        })
        .min_by(|left, right| left.1.total_cmp(&right.1))
        .map(|(entity, _)| entity)
}

#[allow(clippy::too_many_arguments)]
fn on_body_pressed(
    mut press: On<Pointer<Press>>,
    keys: Res<ButtonInput<Key>>,
    ui_scale: Option<Res<UiScale>>,
    parents: Query<&ChildOf>,
    text_run_components: Query<&BodyTextRun>,
    block_components: Query<&BodyBlock>,
    links: Query<&BodyLink>,
    documents: Query<&BodyDocumentCopy>,
    text_layouts: BodyTextLayoutQuery<'_, '_>,
    block_nodes: Query<(
        &ComputedNode,
        &ComputedUiRenderTargetInfo,
        &UiGlobalTransform,
    )>,
    mut runtimes: Query<(&BodyProjectionRuntime, &mut BodyTextSelection)>,
    mut input_focus: ResMut<InputFocus>,
) {
    if press.button != PointerButton::Primary {
        return;
    }
    let target = press.original_event_target();
    let Some(view) = selection_target_body_view(
        target,
        &parents,
        &text_run_components,
        &block_components,
        &links,
        &documents,
    ) else {
        return;
    };
    let Ok((runtime, mut selection)) = runtimes.get_mut(view) else {
        return;
    };
    // A Shift press is admitted before the hit-test, because it is a range
    // gesture whether or not a text position resolves under the pointer, and it
    // must never fall through to link activation. The intent cannot be
    // recovered later: `Pointer<Click>` carries no modifier snapshot, and
    // `ButtonInput<Key>` answers whether Shift is held at click time, which is a
    // different question. A plain press is admitted after the hit-test instead,
    // so a press that resolves no position still starts no drag.
    let shift = keys.pressed(Key::Shift);
    let admitted = if shift {
        let Some(owns_selection) = selection.begin_extend(press.pointer_id) else {
            return;
        };
        Some(owns_selection)
    } else {
        None
    };

    let Some(position) = body_selection_hit_test(
        view,
        Some(target),
        press.pointer_location.position,
        ui_scale.as_deref().map_or(1.0, |scale| scale.0),
        runtime,
        &parents,
        &block_components,
        &text_layouts,
        &block_nodes,
    ) else {
        return;
    };

    let owns_selection = match admitted {
        Some(owns_selection) => owns_selection,
        None => {
            let Some(owns_selection) = selection.begin_drag(press.pointer_id) else {
                return;
            };
            owns_selection
        }
    };
    if owns_selection {
        if shift {
            selection.extend_to(position);
        } else {
            selection.set_caret(position);
        }
        if let Some(block) = runtime.blocks.get(position.block) {
            input_focus.set(block.focus_entity, FocusCause::Pressed);
        }
    }
    press.propagate(false);
}

#[allow(clippy::too_many_arguments)]
fn on_body_dragged(
    mut drag: On<Pointer<Drag>>,
    hover_map: Option<Res<HoverMap>>,
    ui_scale: Option<Res<UiScale>>,
    parents: Query<&ChildOf>,
    text_run_components: Query<&BodyTextRun>,
    block_components: Query<&BodyBlock>,
    links: Query<&BodyLink>,
    documents: Query<&BodyDocumentCopy>,
    text_layouts: BodyTextLayoutQuery<'_, '_>,
    block_nodes: Query<(
        &ComputedNode,
        &ComputedUiRenderTargetInfo,
        &UiGlobalTransform,
    )>,
    mut runtimes: Query<(&BodyProjectionRuntime, &mut BodyTextSelection)>,
) {
    if drag.button != PointerButton::Primary {
        return;
    }
    let target = drag.original_event_target();
    let Some(view) = selection_target_body_view(
        target,
        &parents,
        &text_run_components,
        &block_components,
        &links,
        &documents,
    ) else {
        return;
    };
    let hovered = hovered_body_text_entity(
        hover_map.as_deref(),
        drag.pointer_id,
        view,
        &text_run_components,
    );
    let Ok((runtime, mut selection)) = runtimes.get_mut(view) else {
        return;
    };
    let Some(owns_selection) = selection.note_drag_started(drag.pointer_id) else {
        return;
    };
    drag.propagate(false);
    if !owns_selection {
        return;
    }
    let Some(position) = body_selection_hit_test(
        view,
        hovered,
        drag.pointer_location.position,
        ui_scale.as_deref().map_or(1.0, |scale| scale.0),
        runtime,
        &parents,
        &block_components,
        &text_layouts,
        &block_nodes,
    ) else {
        return;
    };
    selection.extend_to(position);
    selection.note_drag_selection(&runtime.document, drag.pointer_id);
}

#[allow(clippy::too_many_arguments)]
fn on_body_released(
    mut release: On<Pointer<Release>>,
    parents: Query<&ChildOf>,
    text_runs: Query<&BodyTextRun>,
    blocks: Query<&BodyBlock>,
    links: Query<&BodyLink>,
    documents: Query<&BodyDocumentCopy>,
    mut selections: Query<&mut BodyTextSelection>,
) {
    if release.button != PointerButton::Primary {
        return;
    }
    let Some(view) = selection_target_body_view(
        release.original_event_target(),
        &parents,
        &text_runs,
        &blocks,
        &links,
        &documents,
    ) else {
        return;
    };
    let Ok(mut selection) = selections.get_mut(view) else {
        return;
    };
    if selection.release_pointer(release.pointer_id) {
        release.propagate(false);
    }
}

#[allow(clippy::too_many_arguments)]
fn on_body_drag_ended(
    mut drag_end: On<Pointer<DragEnd>>,
    parents: Query<&ChildOf>,
    text_runs: Query<&BodyTextRun>,
    blocks: Query<&BodyBlock>,
    links: Query<&BodyLink>,
    documents: Query<&BodyDocumentCopy>,
    mut selections: Query<&mut BodyTextSelection>,
) {
    if drag_end.button != PointerButton::Primary {
        return;
    }
    let Some(view) = selection_target_body_view(
        drag_end.original_event_target(),
        &parents,
        &text_runs,
        &blocks,
        &links,
        &documents,
    ) else {
        return;
    };
    let Ok(mut selection) = selections.get_mut(view) else {
        return;
    };
    if selection.finish_drag(drag_end.pointer_id) {
        drag_end.propagate(false);
    }
}

#[allow(clippy::too_many_arguments)]
fn on_body_cancelled(
    mut cancel: On<Pointer<PointerCancel>>,
    parents: Query<&ChildOf>,
    text_runs: Query<&BodyTextRun>,
    blocks: Query<&BodyBlock>,
    links: Query<&BodyLink>,
    documents: Query<&BodyDocumentCopy>,
    mut selections: Query<&mut BodyTextSelection>,
) {
    let Some(view) = selection_target_body_view(
        cancel.original_event_target(),
        &parents,
        &text_runs,
        &blocks,
        &links,
        &documents,
    ) else {
        return;
    };
    let Ok(mut selection) = selections.get_mut(view) else {
        return;
    };
    if selection.cancel_pointer(cancel.pointer_id) {
        cancel.propagate(false);
    }
}

fn on_link_clicked(
    mut click: On<Pointer<Click>>,
    links: Query<&BodyLink>,
    views: Query<&CtkBodyView>,
    mut selections: Query<&mut BodyTextSelection>,
    mut commands: Commands,
) {
    if click.button != PointerButton::Primary {
        return;
    }
    let Ok(link) = links.get(click.entity) else {
        return;
    };
    // Shift-click belongs to projected-text range extension, not link
    // activation. The press observer latched that intent into the gesture's
    // suppression bit, which is the single source of truth here: sampling the
    // keyboard again would misread a gesture whose Shift state changed between
    // press and release, in both directions.
    if selections
        .get_mut(link.view)
        .is_ok_and(|mut selection| selection.consume_click_suppression(click.pointer_id))
    {
        click.propagate(false);
        return;
    }
    let Some(event) = link_activation(link, &views) else {
        return;
    };
    click.propagate(false);
    let entity = link.focus_entity;
    commands.queue(move |world: &mut World| {
        world
            .resource_mut::<InputFocus>()
            .set(entity, FocusCause::Pressed);
        world.trigger(event);
    });
}

fn on_body_clicked(
    click: On<Pointer<Click>>,
    blocks: Query<&BodyBlock>,
    links: Query<(), With<BodyLink>>,
    parents: Query<&ChildOf>,
    mut commands: Commands,
) {
    let mut entity = click.original_event_target();
    loop {
        if links.contains(entity) {
            return;
        }
        if blocks.contains(entity) {
            commands.queue(move |world: &mut World| {
                world
                    .resource_mut::<InputFocus>()
                    .set(entity, FocusCause::Pressed);
            });
            return;
        }
        let Ok(parent) = parents.get(entity) else {
            return;
        };
        entity = parent.parent();
    }
}

#[allow(clippy::too_many_arguments)]
fn on_body_keyboard(
    mut event: On<FocusedInput<KeyboardInput>>,
    keys: Res<ButtonInput<Key>>,
    copy_targets: Query<(
        Option<&BodyBlock>,
        Option<&BodyDocumentCopy>,
        Option<&BodyLink>,
    )>,
    links: Query<&BodyLink>,
    views: Query<&CtkBodyView>,
    mut selections: Query<(&BodyProjectionRuntime, &mut BodyTextSelection)>,
    mut clipboard: ResMut<Clipboard>,
    mut commands: Commands,
) {
    if event.input.state != ButtonState::Pressed || event.input.repeat {
        return;
    }
    #[cfg(target_os = "macos")]
    let command = keys.pressed(Key::Super);
    #[cfg(not(target_os = "macos"))]
    let command = keys.pressed(Key::Control);

    let target = copy_targets.get(event.focused_entity).ok();
    let target_view = target.and_then(|(block, document, link)| {
        block
            .map(|block| block.view)
            .or_else(|| document.map(|document| document.view))
            .or_else(|| link.map(|link| link.view))
    });

    if matches!(event.input.logical_key, Key::Escape) {
        if let Some(view) = target_view {
            if let Ok((_, mut selection)) = selections.get_mut(view) {
                selection.clear_selection();
                event.propagate(false);
                return;
            }
        }
    }

    let copy_requested = command
        && matches!(
            &event.input.logical_key,
            Key::Character(value) if value.eq_ignore_ascii_case("c")
        );
    if copy_requested {
        if let Some(view) = target_view {
            if let Ok((runtime, selection)) = selections.get_mut(view) {
                if let Some(text) = selection.selected_text(&runtime.document) {
                    event.propagate(false);
                    copy_text(text, &mut clipboard);
                    return;
                }
            }
        }
        if let Some((block, document, _)) = target {
            if let Some(text) = block
                .map(|block| block.text.as_str())
                .or_else(|| document.map(|document| document.text.as_str()))
            {
                event.propagate(false);
                copy_text(text, &mut clipboard);
                return;
            }
        }
    }

    if let Ok(link) = links.get(event.focused_entity) {
        if matches!(event.input.logical_key, Key::Enter | Key::Space) {
            event.propagate(false);
            if let Some(activation) = link_activation(link, &views) {
                commands.trigger(activation);
            }
        }
    }
}

fn activate_accessibility_links(
    mut requests: MessageReader<AccessibilityActionRequest>,
    links: Query<&BodyLink>,
    views: Query<&CtkBodyView>,
    mut commands: Commands,
) {
    for request in requests.read() {
        if request.action != Action::Click {
            continue;
        }
        let entity = Entity::from_bits(request.target_node.0);
        if let Ok(link) = links.get(entity) {
            if let Some(activation) = link_activation(link, &views) {
                commands.trigger(activation);
            }
        }
    }
}

fn link_activation(link: &BodyLink, views: &Query<&CtkBodyView>) -> Option<LinkActivated> {
    let view = views.get(link.view).ok()?;
    let href = resolve_link_target(view.projection_id, &view.link_targets, link.target)?.to_owned();
    Some(LinkActivated {
        body_view: link.view,
        href,
    })
}

fn copy_text(text: &str, clipboard: &mut Clipboard) {
    let _ = clipboard.set_text(text.to_owned());
}

fn apply_selection_style(style: &mut TextCursorStyle, theme: &UiTheme) {
    let selection = ctk_color(theme, &tokens::ROW_SELECTED);
    style.selection_color = selection;
    style.unfocused_selection_color = selection;
}

fn sync_added_body_selection_styles(
    theme: Res<UiTheme>,
    mut runs: Query<&mut TextCursorStyle, Added<BodyTextVisual>>,
) {
    for mut style in &mut runs {
        apply_selection_style(&mut style, &theme);
    }
}

fn sync_all_body_selection_styles(
    theme: Res<UiTheme>,
    mut runs: Query<&mut TextCursorStyle, With<BodyTextVisual>>,
) {
    for mut style in &mut runs {
        apply_selection_style(&mut style, &theme);
    }
}

fn bounding_box_to_rect(bounds: parley::BoundingBox) -> Rect {
    Rect {
        min: Vec2::new(bounds.x0 as f32, bounds.y0 as f32),
        max: Vec2::new(bounds.x1 as f32, bounds.y1 as f32),
    }
}

fn apply_body_selection_geometry(
    mut views: Query<(&BodyProjectionRuntime, &mut BodyTextSelection)>,
    mut text_runs: Query<(&ComputedTextBlock, &mut TextLayoutInfo), With<BodyTextRun>>,
) {
    for (runtime, mut selection) in &mut views {
        for entity in selection.painted_entities.drain(..) {
            if let Ok((_, mut layout_info)) = text_runs.get_mut(entity) {
                layout_info.selection_rects.clear();
            }
        }

        let (Some(anchor), Some(focus)) = (selection.anchor, selection.focus) else {
            continue;
        };
        let (Some(anchor), Some(focus)) = (
            runtime.document.clamp_position(anchor),
            runtime.document.clamp_position(focus),
        ) else {
            continue;
        };
        let (start, end) = if anchor <= focus {
            (anchor, focus)
        } else {
            (focus, anchor)
        };
        if runtime.document.selected_range(start, end).is_none() {
            continue;
        }

        let mut painted = Vec::new();
        'blocks: for block_index in start.block..=end.block {
            let Some(block) = runtime.blocks.get(block_index) else {
                continue;
            };
            let block_start = if block_index == start.block {
                start.offset
            } else {
                0
            };
            let block_end = if block_index == end.block {
                end.offset
            } else {
                runtime.document.blocks[block_index].len
            };
            if block_start >= block_end {
                continue;
            }
            for run in &block.runs {
                if painted.len() >= BODY_VIEW_MAX_SELECTION_RUNS_PER_FRAME {
                    break 'blocks;
                }
                let selected_start = block_start.max(run.range.start);
                let selected_end = block_end.min(run.range.end);
                if selected_start >= selected_end {
                    continue;
                }
                let Ok((computed, mut layout_info)) = text_runs.get_mut(run.entity) else {
                    continue;
                };
                let layout = computed.buffer();
                if (layout.scale() - layout_info.scale_factor).abs() > 1.0e-4 {
                    continue;
                }
                let local_start = selected_start - run.range.start;
                let local_end = selected_end - run.range.start;
                let parley_selection = ParleySelection::new(
                    ParleyCursor::from_byte_index(layout, local_start, Affinity::Downstream),
                    ParleyCursor::from_byte_index(layout, local_end, Affinity::Upstream),
                );
                layout_info.selection_rects.clear();
                parley_selection.geometry_with(layout, |bounds, _| {
                    layout_info
                        .selection_rects
                        .push(bounding_box_to_rect(bounds));
                });
                painted.push(run.entity);
            }
        }
        selection.painted_entities = painted;
    }
}

fn paint_body_block_copy_focus(
    focus: Res<InputFocus>,
    theme: Res<UiTheme>,
    mut blocks: Query<(Entity, &mut BackgroundColor), With<BodyBlock>>,
) {
    for (entity, mut background) in &mut blocks {
        paint_body_copy_target(entity, &mut background, &focus, &theme);
    }
}

fn paint_body_document_copy_focus(
    focus: Res<InputFocus>,
    theme: Res<UiTheme>,
    mut documents: Query<(Entity, &mut BackgroundColor), With<BodyDocumentCopy>>,
) {
    for (entity, mut background) in &mut documents {
        paint_body_copy_target(entity, &mut background, &focus, &theme);
    }
}

fn paint_body_copy_target(
    entity: Entity,
    background: &mut BackgroundColor,
    focus: &InputFocus,
    theme: &UiTheme,
) {
    let desired = ctk_color(
        theme,
        &if focus.get() == Some(entity) {
            tokens::ROW_HOVER
        } else {
            tokens::PANEL
        },
    );
    if background.0 != desired {
        background.0 = desired;
    }
}

fn on_set_render_arm(event: On<SetRenderArm>, mut views: Query<&mut CtkBodyView>) {
    let Ok(mut view) = views.get_mut(event.body_view) else {
        return;
    };
    view.requested_arm = event.arm;
    view.effective_arm = RenderArm::Text;
}

#[cfg(test)]
mod tests {
    use super::*;

    type SelectionTargetQueries<'w, 's> = (
        Query<'w, 's, &'static ChildOf>,
        Query<'w, 's, &'static BodyTextRun>,
        Query<'w, 's, &'static BodyBlock>,
        Query<'w, 's, &'static BodyLink>,
        Query<'w, 's, &'static BodyDocumentCopy>,
    );

    const SCRIPT: &str = include_str!("../tests/fixtures/html/script-injection.html");
    const EVENTS: &str = include_str!("../tests/fixtures/html/event-handlers.html");
    const CSS: &str = include_str!("../tests/fixtures/html/css-url-exfil.html");
    const PIXEL: &str = include_str!("../tests/fixtures/html/remote-tracking-pixel.html");
    const MALFORMED: &str = include_str!("../tests/fixtures/html/malformed-nesting.html");
    const QUOTES: &str = include_str!("../tests/fixtures/html/deeply-nested-quotes.html");
    const DEEP_LIST: &str = include_str!("../tests/fixtures/html/deeply-nested-list.html");
    const REMOTE_SOURCES: &str =
        include_str!("../tests/fixtures/html/remote-reference-sources.html");
    const CSS_VALUE_TOKENISATION: &str =
        include_str!("../tests/fixtures/html/css-value-tokenisation.html");
    const ANCHORED_CARD: &str = include_str!("../tests/fixtures/html/anchored-card.html");
    const TABLE_CELL_STRUCTURAL_CONTENT: &str =
        include_str!("../tests/fixtures/html/table-cell-structural-content.html");

    #[test]
    fn sanitizer_strips_executable_and_embedding_surfaces() {
        for fixture in [SCRIPT, EVENTS] {
            let (safe, _) = sanitize_html(fixture);
            let lower = safe.as_str().to_ascii_lowercase();
            for forbidden in [
                "<script",
                "<style",
                "<iframe",
                "<object",
                "<embed",
                "<form",
                "onclick",
                "onerror",
                "onload",
                "javascript:",
                "script_payload_sentinel_8d4f",
                "style_payload_sentinel_7a2c",
            ] {
                assert!(
                    !lower.contains(forbidden),
                    "{forbidden:?} survived in {lower}"
                );
            }
        }
    }

    #[test]
    fn sanitizer_preserves_safe_block_boundaries_and_only_drops_dangerous_content() {
        for tag in SAFE_BLOCK_TAGS {
            assert!(
                SANITIZER_ALLOWED_TAGS.contains(tag),
                "known safe block <{tag}> would be unwrapped and could concatenate adjacent text"
            );
        }

        let boundary_fixtures = [
            (
                "<center>first</center><center>second</center>",
                ["first", "second"],
            ),
            (
                "<address>first</address><address>second</address>",
                ["first", "second"],
            ),
            (
                "<aside>first</aside><aside>second</aside>",
                ["first", "second"],
            ),
            (
                "<details>first</details><details>second</details>",
                ["first", "second"],
            ),
            (
                "<dialog>first</dialog><dialog>second</dialog>",
                ["first", "second"],
            ),
            (
                "<fieldset>first</fieldset><fieldset>second</fieldset>",
                ["first", "second"],
            ),
            (
                "<figure>first</figure><figcaption>second</figcaption>",
                ["first", "second"],
            ),
            (
                "<header>first</header><footer>second</footer>",
                ["first", "second"],
            ),
            ("<main>first</main><nav>second</nav>", ["first", "second"]),
            (
                "<search>first</search><section>second</section>",
                ["first", "second"],
            ),
            (
                "<dl><dt>Term</dt><dd>Definition</dd></dl>",
                ["Term", "Definition"],
            ),
            (
                "<form><h2>first</h2></form><form><p>second</p></form>",
                ["first", "second"],
            ),
        ];
        for (raw, expected) in boundary_fixtures {
            let (safe, _) = sanitize_html(raw);
            assert!(
                !safe.as_str().contains("<form"),
                "form semantics survived: {safe:?}"
            );
            let body = SanitizedBody {
                content: SanitizedContent::Html(safe),
                remote_refs: RemoteRefs::default(),
                input_truncated: false,
            };
            let projection = project_body(&body);
            assert_eq!(
                projection
                    .blocks
                    .iter()
                    .filter(|block| block.kind != ProjectedBlockKind::Truncated)
                    .map(ProjectedBlock::text)
                    .collect::<Vec<_>>(),
                expected,
                "safe block boundary was lost for {raw:?}"
            );
        }

        for tag in DANGEROUS_CONTENT_TAGS {
            assert!(!SANITIZER_ALLOWED_TAGS.contains(tag));
            if *tag == "embed" {
                // HTML defines embed as void, so it cannot own fallback text.
                // Its element and resource URL must still disappear.
                let (safe, _) = sanitize_html(
                    "<p>before</p><embed src=\"https://embed.invalid/x\"><p>after</p>",
                );
                assert!(!safe.as_str().contains("<embed"));
                assert!(!safe.as_str().contains("embed.invalid"));
                continue;
            }
            let sentinel = format!("dangerous-{tag}-contents");
            let (safe, _) = sanitize_html(&format!(
                "<p>before</p><{tag}>{sentinel}</{tag}><p>after</p>"
            ));
            assert!(
                !safe.as_str().contains(&sentinel),
                "dangerous <{tag}> contents survived: {safe:?}"
            );
        }

        let (safe, _) = sanitize_html(
            "<form action=\"https://submit.invalid\"><h2>Visible heading</h2>\
             <p>Visible paragraph</p><button>Visible button label</button></form>",
        );
        assert!(!safe.as_str().contains("<form"));
        assert!(!safe.as_str().contains("action="));
        assert!(safe.as_str().contains("Visible heading"));
        assert!(safe.as_str().contains("Visible paragraph"));
        assert!(safe.as_str().contains("Visible button label"));
    }

    #[test]
    fn sanitizer_allow_list_classifies_and_suppresses_fetch_attributes() {
        assert_eq!(SANITIZER_GENERIC_ATTRIBUTES, ["lang", "title"]);
        for (tag, attributes) in SANITIZER_TAG_ATTRIBUTES {
            for attribute in *attributes {
                let inert = matches!(
                    (*tag, *attribute),
                    ("a", "title")
                        | ("img", "alt" | "height" | "title" | "width")
                        | ("ol", "start")
                );
                let navigation = (*tag, *attribute) == ("a", "href");
                let resource_kind = remote_resource_attribute(tag, attribute);
                let inventoried = resource_kind.is_some();
                assert!(
                    inert || navigation || inventoried,
                    "allowed {tag}.{attribute} is neither inert, navigation, nor inventoried"
                );
                let Some(resource_kind) = resource_kind else {
                    continue;
                };

                let hostile_url = format!("https://fetch-guard.invalid/{tag}-{attribute}");
                let hostile_value = match resource_kind {
                    RemoteResourceAttribute::Url => hostile_url.clone(),
                    RemoteResourceAttribute::UrlList => {
                        format!("{hostile_url} https://fetch-guard.invalid/second")
                    }
                    RemoteResourceAttribute::Srcset => format!("{hostile_url} 1x"),
                    RemoteResourceAttribute::Css => {
                        format!("background-image:url({hostile_url})")
                    }
                    RemoteResourceAttribute::OpaqueNestedDocument => {
                        format!(r#"<img src='{hostile_url}'>"#)
                    }
                };
                let raw =
                    format!(r#"<{tag} {attribute}="{hostile_value}" alt="guard">guard</{tag}>"#);
                let control = if *tag == "img" {
                    r#"<img src="cid:fetch-guard-control" alt="guard">"#.to_owned()
                } else {
                    format!(r#"<{tag} title="guard">guard</{tag}>"#)
                };
                let (control, _) = sanitize_html(&control);
                assert!(
                    control.as_str().contains(&format!("<{tag}")),
                    "fetch-suppression fixture became vacuous because allowed <{tag}> did not survive a safe control: {control:?}"
                );

                let (safe, refs) = sanitize_html(&raw);
                let (_, attribute_survived) =
                    sanitized_output_contains(safe.as_str(), tag, attribute);
                assert!(
                    !attribute_survived && !safe.as_str().contains(&hostile_url),
                    "classified fetch attribute {tag}.{attribute} survived sanitizer output: {safe:?}"
                );
                assert!(
                    refs.urls().iter().any(|url| url == &hostile_url),
                    "classified fetch attribute {tag}.{attribute} was not inventoried"
                );
            }
        }
    }

    #[test]
    fn remote_inventory_covers_fetch_attributes_on_suppressed_controls_and_resources() {
        let raw = r#"
            <input type="image" src="https://fetch.invalid/input-src">
            <video poster="https://fetch.invalid/video-poster"></video>
            <object data="https://fetch.invalid/object-data"></object>
            <form action="https://fetch.invalid/form-action">
                <button formaction="https://fetch.invalid/button-formaction">send</button>
                <input formaction="https://fetch.invalid/input-formaction">
            </form>
            <a href="https://navigation.invalid/"
               ping="https://fetch.invalid/ping-one https://fetch.invalid/ping-two">link</a>
            <html manifest="https://fetch.invalid/app-cache"><body>body</body></html>
        "#;
        let (safe, refs) = sanitize_html(raw);
        let expected = [
            "https://fetch.invalid/input-src",
            "https://fetch.invalid/video-poster",
            "https://fetch.invalid/object-data",
            "https://fetch.invalid/form-action",
            "https://fetch.invalid/button-formaction",
            "https://fetch.invalid/input-formaction",
            "https://fetch.invalid/ping-one",
            "https://fetch.invalid/ping-two",
            "https://fetch.invalid/app-cache",
        ];
        assert!(refs.is_complete(), "{:?}", refs.urls());
        assert_eq!(refs.count(), expected.len(), "{:?}", refs.urls());
        for url in expected {
            assert!(
                refs.urls().iter().any(|actual| actual == url),
                "{url} was not inventoried"
            );
            assert!(!safe.as_str().contains(url), "{url} survived cleaning");
        }
        assert!(
            !refs
                .urls()
                .iter()
                .any(|url| url == "https://navigation.invalid/"),
            "ordinary anchor navigation must not be reported as an automatic fetch"
        );
    }

    #[test]
    fn remote_inventory_records_svg_hrefs_but_fails_honest_for_foreign_markup() {
        let raw = r#"
            <svg>
                <image href="https://fetch.invalid/svg-image"></image>
                <use href="https://fetch.invalid/svg-use"></use>
                <feImage xlink:href="https://fetch.invalid/svg-filter"></feImage>
            </svg>
        "#;
        let (safe, refs) = sanitize_html(raw);
        for expected in [
            "https://fetch.invalid/svg-image",
            "https://fetch.invalid/svg-use",
            "https://fetch.invalid/svg-filter",
        ] {
            assert!(
                refs.urls().iter().any(|actual| actual == expected),
                "{expected} was not inventoried: {:?}",
                refs.urls()
            );
            assert!(!safe.as_str().contains(expected));
        }
        assert!(
            !refs.is_complete(),
            "foreign-content inventory must not claim to model every SVG fetch construct"
        );
    }

    #[test]
    fn remote_inventory_marks_opaque_nested_documents_incomplete() {
        let nested = "https://tracker.example/srcdoc-pixel";
        let (safe, refs) = sanitize_html(&format!(
            r#"<iframe srcdoc="<img src='{nested}'>"></iframe>"#
        ));
        assert!(refs.is_empty());
        assert!(
            !refs.is_complete(),
            "an unparsed srcdoc must make catalogue completeness false"
        );
        assert!(!safe.as_str().contains("iframe"));
        assert!(!safe.as_str().contains(nested));

        let (_, refresh) = sanitize_html(
            r#"<meta http-equiv="refresh" content="0;url=https://tracker.example/refresh">"#,
        );
        assert!(!refresh.is_complete());
    }

    fn sanitized_output_contains(html: &str, tag: &str, attribute: &str) -> (bool, bool) {
        let dom = parse_document(RcDom::default(), Default::default()).one(html);
        let mut stack = vec![dom.document];
        let mut tag_survived = false;
        let mut attribute_survived = false;
        while let Some(node) = stack.pop() {
            if let NodeData::Element { name, attrs, .. } = &node.data {
                if name.local.as_ref() == tag {
                    tag_survived = true;
                    attribute_survived |= attrs
                        .borrow()
                        .iter()
                        .any(|candidate| candidate.name.local.as_ref() == attribute);
                }
            }
            stack.extend(node.children.borrow().iter().cloned());
        }
        (tag_survived, attribute_survived)
    }

    #[test]
    fn remote_inventory_walks_resource_dom_and_scoped_css_only() {
        let (safe, refs) = sanitize_html(REMOTE_SOURCES);
        let expected = [
            "https://assets.example/font.woff2",
            "https://assets.example/preload-1.png",
            "https://assets.example/preload-2.png",
            "https://css.example/theme.css",
            "https://css.example/escaped.png",
            "https://assets.example/legacy.png",
            "https://css.example/entity-decoded.png?a=1&b=2",
            "https://media.example/image.png",
            "https://media.example/image-1.png",
            "https://media.example/image-2.png",
            "https://media.example/video.mp4",
            "https://media.example/poster.jpg",
            "https://media.example/audio.mp3",
            "https://media.example/source.png",
            "https://media.example/source-1.png",
            "https://media.example/source-2.png",
            "https://media.example/captions.vtt",
            "https://media.example/embed.bin",
            "https://media.example/object.bin",
        ];
        assert_eq!(refs.count(), expected.len(), "{:?}", refs.urls());
        for url in expected {
            assert!(refs.urls().iter().any(|actual| actual == url), "{url}");
            assert!(!safe.as_str().contains(url), "{url} survived cleaning");
        }
        for inert in [
            "https://false-positive.example/title.png",
            "https://false-positive.example/body-text.png",
            "https://false-positive.example/title-attribute.png",
        ] {
            assert!(
                !refs.urls().iter().any(|actual| actual == inert),
                "inventoried inert text {inert}"
            );
        }
    }

    #[test]
    fn remote_inventory_uses_whatwg_url_parsing_and_fails_closed() {
        for value in [
            r"https:\\img.example\a.png",
            "https:/img.example/b.png",
            "https:img.example/c.png",
            "HTTPS://IMG.EXAMPLE/upper.png",
            " \tHTTPS://img.example/spaced.png\n ",
            "https:\t//img.example/control.png",
            "\u{0001}https://img.example/c0.png\u{0002}",
        ] {
            assert_eq!(
                classify_remote_url(value),
                RemoteUrlClassification::Remote,
                "{value:?}"
            );
        }
        for value in ["cid:part-1", "data:image/png;base64,AAAA"] {
            assert_eq!(
                classify_remote_url(value),
                RemoteUrlClassification::Embedded,
                "{value:?}"
            );
        }

        let mut urls = Vec::new();
        assert!(!collect_remote_url(
            Some("\0not a classifiable URL"),
            &mut urls
        ));
        assert_eq!(urls, ["\0not a classifiable URL"]);

        let raw = r#"
            <img src="https:\\img.example\a.png">
            <img src="https:/img.example/b.png">
            <img src="https:img.example/c.png">
            <img src="  HTTPS://IMG.EXAMPLE/upper.png  ">
            <img src="https:	//img.example/control.png">
        "#;
        let (safe, refs) = sanitize_html(raw);
        assert_eq!(refs.count(), 5, "{:?}", refs.urls());
        assert!(refs.is_complete());
        assert!(!safe.as_str().contains("src="));

        let (_, ambiguous) = sanitize_html(r#"<img src="relative/image.png">"#);
        assert_eq!(ambiguous.urls(), &["relative/image.png".to_owned()]);
        assert!(!ambiguous.is_complete());
    }

    #[test]
    fn sanitizer_drops_css_fetches_and_inventories_their_urls() {
        let (safe, refs) = sanitize_html(CSS);
        assert!(!safe.as_str().contains("style="));
        assert!(!safe.as_str().contains("<style"));
        assert!(!safe.as_str().contains("https://exfil.example"));
        assert_eq!(
            refs.urls(),
            &[
                "https://exfil.example/sheet.gif?token=secret".to_owned(),
                "https://exfil.example/inline.gif".to_owned(),
            ]
        );
    }

    #[test]
    fn css_value_tokens_collect_image_set_strings_but_ignore_quoted_url_text() {
        let (_, refs) = sanitize_html(CSS_VALUE_TOKENISATION);
        assert_eq!(
            refs.urls(),
            &[
                "https://images.example/hero-1x.png".to_owned(),
                "https://images.example/hero-2x.png".to_owned(),
                "https://images.example/legacy-1x.png".to_owned(),
            ]
        );
        assert!(!refs
            .urls()
            .iter()
            .any(|url| url.contains("false-positive.example")));
    }

    #[test]
    fn css_walk_stops_at_its_depth_guard_and_marks_inventory_incomplete() {
        const HOSTILE_DEPTH: usize = 50_000;
        let css = format!(
            "{}url(\"https://deep.example/pixel.png\"){}",
            "f(".repeat(HOSTILE_DEPTH),
            ")".repeat(HOSTILE_DEPTH)
        );
        let mut urls = Vec::new();
        assert!(!collect_css_remote_refs(&css, &mut urls));
        assert!(urls.is_empty());

        let (_, refs) = sanitize_html(&format!("<style>{css}</style>"));
        assert!(!refs.is_complete());
    }

    #[test]
    fn quoted_css_url_is_recorded_only_after_the_function_parses() {
        let mut urls = Vec::new();
        let complete = collect_css_remote_refs(
            r#"url("https://false-positive.example/pixel.png" trailing)"#,
            &mut urls,
        );
        assert!(!complete);
        assert!(urls.is_empty());
    }

    #[test]
    fn sanitizer_removes_remote_pixels_but_keeps_embedded_images() {
        let (safe, refs) = sanitize_html(PIXEL);
        assert_eq!(refs.count(), 2);
        assert!(refs
            .urls()
            .iter()
            .any(|url| url == "https://tracker.example/open.gif?id=42"));
        assert!(!safe.as_str().contains("tracker.example"));
        assert!(!safe.as_str().contains("//cdn.example"));
        assert!(safe.as_str().contains("cid:part-1"));
        assert!(safe.as_str().contains("data:image/png"));
        assert!(!safe.as_str().contains("data:text/html"));
        let (safe, _) = sanitize_html(
            r#"<img src="data:image/png-evil;base64,AAAA">
               <img src="data:image/svg+xml,%3Csvg/%3E">"#,
        );
        assert!(!safe.as_str().contains("data:image/"));
    }

    #[test]
    fn malformed_and_deep_html_projects_without_executable_content() {
        for fixture in [MALFORMED, QUOTES, DEEP_LIST] {
            let body = BodySource::Html(fixture.to_owned()).sanitize();
            let projection = project_body(&body);
            assert!(!projection.blocks.is_empty());
            assert!(projection
                .blocks
                .iter()
                .all(|block| block.quote_depth <= MAX_PROJECTION_DEPTH));
            assert!(!projection
                .blocks
                .iter()
                .any(|block| block.text().contains("alert(")));
        }
        let deep = project_body(&BodySource::Html(DEEP_LIST.to_owned()).sanitize());
        assert!(deep
            .blocks
            .iter()
            .any(|block| block.text().contains("deep sentinel")));
        assert!(deep.blocks.iter().all(|block| {
            block
                .list_item_start
                .is_none_or(|item| item.depth < MAX_PROJECTION_DEPTH)
        }));
    }

    #[test]
    fn hostile_deep_list_flattens_at_the_projection_guard() {
        const HOSTILE_DEPTH: usize = 4_096;
        let html = format!(
            "{}hostile deep-list sentinel{}",
            "<ul><li>".repeat(HOSTILE_DEPTH),
            "</li></ul>".repeat(HOSTILE_DEPTH)
        );
        let projection = project_body(&BodySource::Html(html).sanitize());
        assert!(projection
            .blocks
            .iter()
            .any(|block| block.text().contains("hostile deep-list sentinel")));
        assert!(projection.blocks.iter().all(|block| {
            block
                .list_item_start
                .is_none_or(|item| item.depth < MAX_PROJECTION_DEPTH)
        }));
    }

    #[test]
    fn plain_ingress_normalises_lf_crlf_and_lone_cr_before_projection() {
        for input in [
            "alpha\n\nbeta\n\ngamma",
            "alpha\r\n\r\nbeta\r\n\r\ngamma",
            "alpha\r\n\r\nbeta\r\rgamma\n\n",
        ] {
            let body = BodySource::Plain(input.to_owned()).sanitize();
            let SanitizedContent::Plain(value) = &body.content else {
                unreachable!();
            };
            assert!(!value.contains('\r'));
            let projected = project_body(&body)
                .blocks
                .iter()
                .flat_map(|block| &block.spans)
                .map(|span| span.text.as_str())
                .collect::<String>();
            assert_eq!(projected, *value);
        }

        let large = (0..80)
            .map(|index| format!("paragraph-{index:02}-{}", "x".repeat(1_010)))
            .collect::<Vec<_>>()
            .join("\r\n\r\n");
        assert!(large.len() > BODY_VIEW_MAX_TEXT_RUN_BYTES);
        let body = BodySource::Plain(large).sanitize();
        assert!(!body.input_truncated());
        let projection = project_body(&body);
        assert_eq!(projection.blocks.len(), 80);
        assert!(!projection
            .blocks
            .iter()
            .any(|block| block.kind == ProjectedBlockKind::Truncated));
        assert!(projection
            .blocks
            .last()
            .is_some_and(|block| block.text().starts_with("paragraph-79-")));

        let raw_crlf = "\r\n".repeat(BODY_VIEW_MAX_INPUT_BYTES);
        assert!(raw_crlf.len() > BODY_VIEW_MAX_INPUT_BYTES);
        let body = BodySource::Plain(raw_crlf).sanitize();
        let SanitizedContent::Plain(value) = &body.content else {
            unreachable!();
        };
        assert_eq!(value.len(), BODY_VIEW_MAX_INPUT_BYTES);
        assert!(!body.input_truncated());
    }

    fn projected_span_texts(projection: &BodyProjection) -> Vec<Vec<&str>> {
        projection
            .blocks
            .iter()
            .map(|block| block.spans.iter().map(|span| span.text.as_str()).collect())
            .collect()
    }

    #[test]
    fn plain_projection_round_trips_every_canonical_byte() {
        let corpus = [
            "",
            "\n",
            "\n\n\n",
            "\nleading",
            "\n\nleading",
            "trailing\n",
            "trailing\n\n\n",
            "alpha\n\nbeta",
            "alpha\n\n\nbeta",
            "alpha\n\n\n\nbeta",
            "alpha\n\n\n\n\n\nbeta",
            "alpha\n   \n\n\t \n\nbeta",
            "no trailing newline",
            "    fn main() {\n        println!(\"hello\");\n        return;\n    }",
            "alpha\n   \nbeta",
            "alpha\n\t\t\nbeta",
            "first line  \nsecond line",
            "NAME      SIZE    STATE\nalpha     10      ready\nbeta      200     held",
            "    the very first line is indented\nnext line",
            "  indented\nnext  ",
            " \t\n  \n\t",
            "alpha\r\n\r\n\r\nbeta\r\n",
        ];
        for input in corpus {
            let body = BodySource::Plain(input.to_owned()).sanitize();
            let SanitizedContent::Plain(canonical) = &body.content else {
                unreachable!();
            };
            let expected = input.replace("\r\n", "\n").replace('\r', "\n");
            assert_eq!(
                canonical, &expected,
                "plain ingress changed bytes beyond documented LF normalisation for {input:?}"
            );
            let projection = project_body(&body);
            let reconstructed = projection
                .blocks
                .iter()
                .flat_map(|block| &block.spans)
                .map(|span| span.text.as_str())
                .collect::<String>();
            assert_eq!(
                reconstructed, expected,
                "plain projection changed canonical bytes for {input:?}"
            );
            assert_eq!(
                projection_copy_text(&projection),
                expected,
                "whole-document copy changed canonical bytes for {input:?}"
            );
            let document = BodySelectionDocument::from_projection(&projection);
            let mut selection = BodyTextSelection::default();
            selection.select_all(&document);
            assert_eq!(
                selection.selected_text(&document),
                (!expected.is_empty()).then_some(expected.as_str()),
                "programmatic select-all changed canonical bytes for {input:?}"
            );
        }
        // BodySource::Plain documents CRLF/lone-CR ingress normalisation. The
        // round trip is therefore exact against that canonical LF form.
        let crlf = BodySource::Plain("alpha\r\n\r\nbeta\r\n".to_owned()).sanitize();
        let projection = project_body(&crlf);
        assert_eq!(projection_copy_text(&projection), "alpha\n\nbeta\n");
    }

    #[test]
    fn html_copy_uses_one_lf_between_readable_blocks_and_keeps_rendered_markers() {
        let projection = project_body(
            &BodySource::Html(
                "<h2>Title</h2><ul><li>first</li><li>second</li></ul><hr>".to_owned(),
            )
            .sanitize(),
        );
        assert_eq!(
            projection_copy_text(&projection),
            "Title\n• first\n• second\n────────"
        );
    }

    #[test]
    fn ordered_lists_preserve_negative_starts_and_handle_extreme_values() {
        let negative = project_body(
            &BodySource::Html(r#"<ol start="-2"><li>A</li><li>B</li><li>C</li></ol>"#.to_owned())
                .sanitize(),
        );
        assert_eq!(projection_copy_text(&negative), "-2. A\n-1. B\n0. C");
        assert_eq!(
            negative
                .blocks
                .iter()
                .filter_map(|block| block.list_item_start.map(|item| item.index))
                .collect::<Vec<_>>(),
            [-2, -1, 0]
        );

        let minimum = project_body(
            &BodySource::Html(
                r#"<ol start="-9223372036854775808"><li>A</li><li>B</li></ol>"#.to_owned(),
            )
            .sanitize(),
        );
        assert_eq!(
            projection_copy_text(&minimum),
            "-9223372036854775808. A\n-9223372036854775807. B"
        );

        let overflow = project_body(
            &BodySource::Html(
                r#"<ol start="-9223372036854775809"><li>A</li><li>B</li></ol>"#.to_owned(),
            )
            .sanitize(),
        );
        assert_eq!(
            projection_copy_text(&overflow),
            "1. A\n2. B",
            "garbage or out-of-range start values retain the existing default"
        );
    }

    #[test]
    fn logical_selection_stitches_exact_document_substrings_exhaustively() {
        let projection = project_body(
            &BodySource::Html("<p>alpha</p><p>beta</p><p>aéz</p>".to_owned()).sanitize(),
        );
        let document = BodySelectionDocument::from_projection(&projection);
        let position = |block, offset| BodyTextPosition { block, offset };

        assert_eq!(
            document.selected_text(position(0, 1), position(0, 4)),
            Some("lph"),
            "same-block range"
        );
        assert_eq!(
            document.selected_text(position(0, 2), position(1, 2)),
            Some("pha\nbe"),
            "cross-block range"
        );
        assert_eq!(
            document.selected_text(position(0, 0), position(2, "aéz".len())),
            Some("alpha\nbeta\naéz"),
            "whole document"
        );
        assert_eq!(
            document.selected_text(position(1, 2), position(1, 2)),
            None,
            "empty range"
        );
        assert_eq!(
            document.selected_text(position(1, 2), position(0, 2)),
            Some("pha\nbe"),
            "reversed range"
        );
        assert_eq!(
            document.selected_text(position(2, 2), position(usize::MAX, usize::MAX)),
            Some("éz"),
            "degenerate offsets clamp to valid block and UTF-8 boundaries"
        );
    }

    #[test]
    fn spawned_selection_runs_cover_each_logical_block_once_and_in_order() {
        let body = BodySource::Html(
            r#"
                <ol start="-2">
                    <li><hr></li>
                    <li>alpha <strong>beta</strong>
                        <a href="https://example.com">gamma</a></li>
                </ol>
                <p>tail</p>
            "#
            .to_owned(),
        )
        .sanitize();
        let mut app = App::new();
        app.add_plugins(CtkBodyViewPlugin);
        let mut commands = app.world_mut().commands();
        let entities = spawn_body_view(
            &mut commands,
            CtkBodyViewProps::new(body, "Selection range invariant"),
        );
        app.world_mut().flush();

        let runtime = app
            .world()
            .get::<BodyProjectionRuntime>(entities.root)
            .unwrap();
        assert_eq!(runtime.blocks.len(), runtime.document.blocks.len());
        for (block_index, block) in runtime.blocks.iter().enumerate() {
            let logical = &runtime.document.blocks[block_index];
            let logical_text = &runtime.document.text
                [logical.document_start..logical.document_start.saturating_add(logical.len)];
            let mut cursor = 0usize;
            let mut rendered = String::new();
            for run in &block.runs {
                assert_eq!(
                    run.range.start, cursor,
                    "block {block_index} has a gap, overlap or out-of-order run at {:?}",
                    run.range
                );
                assert!(
                    run.range.end <= logical.len,
                    "block {block_index} run {:?} exceeds logical length {}",
                    run.range,
                    logical.len
                );
                let component = app.world().get::<BodyTextRun>(run.entity).unwrap();
                assert_eq!(component.block, block_index);
                assert_eq!(component.range, run.range);
                rendered.push_str(&app.world().get::<Text>(run.entity).unwrap().0);
                cursor = run.range.end;
            }
            assert_eq!(
                cursor, logical.len,
                "block {block_index} runs do not collectively cover its logical copy text"
            );
            assert_eq!(
                rendered, logical_text,
                "block {block_index} rendered runs diverge from logical copy text"
            );
        }
        assert_eq!(
            runtime.document.text,
            "-2. ────────\n-1. alpha beta gamma\ntail"
        );
    }

    #[test]
    fn live_hit_test_extremes_respect_the_parley_physical_scale() {
        use bevy::asset::AssetPlugin;
        use bevy::camera::{
            Camera, Camera2d, CameraPlugin, ComputedCameraValues, RenderTarget, RenderTargetInfo,
            Viewport,
        };
        use bevy::image::{ImagePlugin, TextureAtlasPlugin};
        use bevy::input::InputPlugin;
        use bevy::math::UVec2;
        use bevy::mesh::MeshPlugin;
        use bevy::picking::DefaultPickingPlugins;
        use bevy::prelude::MinimalPlugins;
        use bevy::text::TextPlugin;
        use bevy::transform::TransformPlugin;
        use bevy::ui::{UiPlugin, UiTargetCamera};
        use bevy::window::{PrimaryWindow, Window, WindowPlugin, WindowRef};

        const SCALE: f32 = 2.0;
        let physical_size = UVec2::new(800, 600);
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_plugins(bevy::a11y::AccessibilityPlugin)
            .add_plugins(AssetPlugin::default())
            .add_plugins(TransformPlugin)
            .add_plugins(CameraPlugin)
            .add_plugins(ImagePlugin::default())
            .add_plugins(TextureAtlasPlugin)
            .add_plugins(MeshPlugin)
            .add_plugins(WindowPlugin {
                primary_window: None,
                ..default()
            })
            .add_plugins(InputPlugin)
            .add_plugins(DefaultPickingPlugins)
            .add_plugins(TextPlugin)
            .add_plugins(UiPlugin)
            .add_plugins(CtkBodyViewPlugin);
        let window = app
            .world_mut()
            .spawn((Window::default(), PrimaryWindow))
            .id();
        let camera = app
            .world_mut()
            .spawn((
                Camera2d,
                Camera {
                    computed: ComputedCameraValues {
                        target_info: Some(RenderTargetInfo {
                            physical_size,
                            scale_factor: SCALE,
                        }),
                        ..default()
                    },
                    viewport: Some(Viewport {
                        physical_size,
                        ..default()
                    }),
                    ..default()
                },
                RenderTarget::Window(WindowRef::Entity(window)),
            ))
            .id();
        let mut commands = app.world_mut().commands();
        let entities = spawn_body_view(
            &mut commands,
            CtkBodyViewProps::new(
                BodySource::Html("<p>first</p><p>last</p>".to_owned()).sanitize(),
                "Scaled hit test",
            ),
        );
        app.world_mut().flush();
        app.world_mut()
            .entity_mut(entities.root)
            .insert(UiTargetCamera(camera));
        app.world_mut()
            .spawn(Node {
                width: px(400),
                height: px(500),
                ..default()
            })
            .add_child(entities.root);
        app.finish();
        app.cleanup();
        app.update();
        app.update();

        let runtime = app
            .world()
            .get::<BodyProjectionRuntime>(entities.root)
            .unwrap();
        let first = runtime.blocks.first().unwrap().runs.first().unwrap().entity;
        let last_block_index = runtime.blocks.len() - 1;
        let last = runtime.blocks[last_block_index].runs.last().unwrap().entity;
        for (entity, far_x, expected) in [
            (
                first,
                -10_000.0,
                BodyTextPosition {
                    block: 0,
                    offset: 0,
                },
            ),
            (
                last,
                10_000.0,
                BodyTextPosition {
                    block: last_block_index,
                    offset: runtime.document.blocks[last_block_index].len,
                },
            ),
        ] {
            let world = app.world_mut();
            let mut query = world.query::<(
                &BodyTextRun,
                &ComputedNode,
                &ComputedUiRenderTargetInfo,
                &UiGlobalTransform,
                &ComputedTextBlock,
                &TextLayoutInfo,
            )>();
            let (run, node, target, transform, computed, layout_info) =
                query.get(world, entity).unwrap();
            assert!((computed.buffer().scale() - SCALE).abs() < 1.0e-4);
            assert!((layout_info.scale_factor - computed.buffer().scale()).abs() < 1.0e-4);
            let local = Vec2::new(far_x, computed.buffer().height() * 0.5);
            let screen = transform
                .affine()
                .transform_point2(node.content_box().min + local)
                / target.scale_factor();
            assert_eq!(
                hit_test_text_run(
                    screen,
                    1.0,
                    run,
                    node,
                    target,
                    transform,
                    computed,
                    layout_info,
                ),
                Some(expected)
            );
        }
    }

    #[test]
    fn html_projection_still_trims_collapsible_block_edges() {
        let projection = project_body(
            &BodySource::Html("<p>   alpha <strong>  beta  </strong> gamma   </p>".to_owned())
                .sanitize(),
        );
        assert_eq!(
            projected_span_texts(&projection),
            vec![vec!["alpha ", "beta ", "gamma"]]
        );
    }

    #[test]
    fn html_projection_preserves_non_css_unicode_spaces_exactly() {
        for (html, expected) in [
            ("<p>A&nbsp;&nbsp;B</p>", "A\u{00A0}\u{00A0}B"),
            ("<p>10&nbsp;km</p>", "10\u{00A0}km"),
            ("<p>&nbsp;edge&nbsp;</p>", "\u{00A0}edge\u{00A0}"),
            ("<p>漢\u{3000}字</p>", "漢\u{3000}字"),
            ("<p>12\u{202F}345</p>", "12\u{202F}345"),
            ("<p>A&nbsp; &nbsp;B</p>", "A\u{00A0} \u{00A0}B"),
        ] {
            let projection = project_body(&BodySource::Html(html.to_owned()).sanitize());
            assert_eq!(
                projected_span_texts(&projection),
                vec![vec![expected]],
                "{html:?}"
            );
        }

        let exact_plain = "A\u{00A0}\u{00A0}B  漢\u{3000}字  12\u{202F}345";
        let projection = project_body(&BodySource::Plain(exact_plain.to_owned()).sanitize());
        assert_eq!(projected_span_texts(&projection), vec![vec![exact_plain]]);
    }

    #[test]
    fn quote_only_lines_are_never_discarded() {
        let raw = "plain\n>\nquoted\n>>\nnested\n> >\nspaced";
        let projection = project_body(&BodySource::Plain(raw.to_owned()).sanitize());
        assert_eq!(projection.blocks.len(), 1);
        assert_eq!(projection.blocks[0].text(), raw);
        assert_eq!(
            projection.blocks[0].text().lines().collect::<Vec<_>>(),
            ["plain", ">", "quoted", ">>", "nested", "> >", "spaced"]
        );
    }

    #[test]
    fn long_quoted_replies_preserve_the_final_content_for_all_line_endings() {
        for line_ending in ["\n", "\r\n", "\r"] {
            let separator = format!("{line_ending}>{line_ending}");
            let mut paragraphs = (0..80)
                .map(|index| format!("> quoted-{index:02}-{}", "x".repeat(1_010)))
                .collect::<Vec<_>>();
            paragraphs.push("> final quoted sentinel 73c1".to_owned());
            let raw = paragraphs.join(&separator);
            assert!(raw.len() > BODY_VIEW_MAX_TEXT_RUN_BYTES);
            let body = BodySource::Plain(raw).sanitize();
            assert!(!body.input_truncated(), "line ending {line_ending:?}");

            let projection = project_body(&body);
            assert!(
                projection
                    .blocks
                    .iter()
                    .any(|block| block.text().contains("final quoted sentinel 73c1")),
                "line ending {line_ending:?}"
            );
            assert!(
                !projection
                    .blocks
                    .iter()
                    .any(|block| block.kind == ProjectedBlockKind::Truncated),
                "line ending {line_ending:?}"
            );
            assert!(projection
                .blocks
                .iter()
                .flat_map(|block| &block.spans)
                .all(|span| span.text.len() <= BODY_VIEW_MAX_TEXT_RUN_BYTES));
        }
    }

    #[test]
    fn projection_bounds_large_messages_with_accessible_marker() {
        let plain = (0..10_000)
            .map(|index| format!("paragraph {index}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let body = BodySource::Plain(plain).sanitize();
        let projection = project_body(&body);
        assert!(projection.blocks.len() <= BODY_VIEW_MAX_BLOCKS + 1);
        assert!(matches!(
            projection.blocks.last().map(|block| block.kind),
            Some(ProjectedBlockKind::Truncated)
        ));
        assert_eq!(
            projection
                .blocks
                .last()
                .map(ProjectedBlock::text)
                .as_deref(),
            Some("Message truncated.")
        );

        let mut app = App::new();
        app.add_plugins(CtkBodyViewPlugin);
        let mut commands = app.world_mut().commands();
        spawn_body_view(
            &mut commands,
            CtkBodyViewProps::new(body, "Large message body"),
        );
        app.world_mut().flush();
        let world = app.world_mut();
        let mut rendered = world.query::<(&BodyBlock, &AccessibilityNode)>();
        let rendered = rendered.iter(world).collect::<Vec<_>>();
        assert!(rendered.len() <= BODY_VIEW_MAX_BLOCKS + 1);
        assert!(rendered.iter().any(|(block, node)| {
            block.text == "Message truncated." && node.role() == Role::Alert
        }));

        let mut alternating = String::from("<p>");
        for index in 0..(BODY_VIEW_MAX_SPANS + 100) {
            alternating.push_str(if index % 2 == 0 {
                "<strong>x</strong>"
            } else {
                "<em>y</em>"
            });
        }
        alternating.push_str("</p>");
        let projection = project_body(&BodySource::Html(alternating).sanitize());
        let spans = projection
            .blocks
            .iter()
            .map(|block| block.spans.len())
            .sum::<usize>();
        assert!(spans <= BODY_VIEW_MAX_SPANS + 1);
        assert!(matches!(
            projection.blocks.last().map(|block| block.kind),
            Some(ProjectedBlockKind::Truncated)
        ));
    }

    #[test]
    fn span_budget_keeps_the_accepted_head_of_the_in_progress_block() {
        const SPAN_BUDGET: usize = 4_096;
        let mut html = String::from("<p>");
        for index in 0..=SPAN_BUDGET {
            html.push_str(if index % 2 == 0 {
                "<strong>x</strong>"
            } else {
                "<em>y</em>"
            });
        }
        html.push_str("</p>");

        let body = BodySource::Html(html).sanitize();
        let projection = project_body_with_budgets(&body, BODY_VIEW_MAX_BLOCKS, SPAN_BUDGET);
        assert_eq!(projection.blocks.len(), 2);
        assert_eq!(projection.blocks[0].spans.len(), SPAN_BUDGET);
        assert_eq!(projection.blocks[0].text().len(), SPAN_BUDGET);
        assert_eq!(
            projection.blocks[0]
                .text()
                .chars()
                .take(4)
                .collect::<String>(),
            "xyxy"
        );
        assert_eq!(projection.blocks[1].kind, ProjectedBlockKind::Truncated);
    }

    #[test]
    fn body_view_props_tune_block_and_span_budgets() {
        let body = BodySource::Html(
            "<p><strong>accepted</strong><em>overflow span</em></p><p>overflow block</p>"
                .to_owned(),
        )
        .sanitize();
        let mut app = App::new();
        app.add_plugins(CtkBodyViewPlugin);
        let mut commands = app.world_mut().commands();
        spawn_body_view(
            &mut commands,
            CtkBodyViewProps::new(body, "Tuned budgets")
                .max_blocks(1)
                .max_spans(1),
        );
        app.world_mut().flush();
        let world = app.world_mut();
        let mut blocks = world.query::<&BodyBlock>();
        let values = blocks
            .iter(world)
            .map(|block| block.text.as_str())
            .collect::<Vec<_>>();
        assert_eq!(values, ["accepted", "Message truncated."]);
    }

    #[test]
    fn raw_input_byte_cap_keeps_the_head_and_adds_an_accessible_alert() {
        let html = format!(
            "<p>accepted ingress head</p>{}<p>overflow tail</p>",
            " ".repeat(BODY_VIEW_MAX_INPUT_BYTES)
        );
        let body = BodySource::Html(html).sanitize();
        assert!(body.sanitized_html().unwrap().input_truncated());
        let projection = project_body(&body);
        assert!(projection
            .blocks
            .iter()
            .any(|block| block.text().contains("accepted ingress head")));
        assert!(!projection
            .blocks
            .iter()
            .any(|block| block.text().contains("overflow tail")));
        assert_accessible_truncation(body);
    }

    #[test]
    fn bounded_accumulation_chunks_plain_pre_and_alt_runs() {
        let body = BodySource::Plain("x".repeat(BODY_VIEW_MAX_INPUT_BYTES + 4_096)).sanitize();
        let SanitizedContent::Plain(value) = &body.content else {
            unreachable!();
        };
        assert_eq!(value.len(), BODY_VIEW_MAX_INPUT_BYTES);
        assert!(value.capacity() <= BODY_VIEW_MAX_INPUT_BYTES);

        let pre = BodySource::Html(format!(
            "<pre>{}</pre>",
            "p".repeat(BODY_VIEW_MAX_TEXT_RUN_BYTES + 4_096)
        ))
        .sanitize();
        let projection = project_body(&pre);
        assert_eq!(
            projection.blocks[0]
                .spans
                .iter()
                .map(|span| span.text.len())
                .sum::<usize>(),
            BODY_VIEW_MAX_TEXT_RUN_BYTES + 4_096
        );
        assert!(projection.blocks[0]
            .spans
            .iter()
            .all(|span| span.text.len() <= BODY_VIEW_MAX_TEXT_RUN_BYTES
                && span.text.capacity() <= BODY_VIEW_MAX_TEXT_RUN_BYTES));
        assert!(!projection
            .blocks
            .iter()
            .any(|block| block.kind == ProjectedBlockKind::Truncated));

        let alt = BodySource::Html(format!(
            r#"<p><img alt="{}"></p>"#,
            "a".repeat(BODY_VIEW_MAX_TEXT_RUN_BYTES + 4_096)
        ))
        .sanitize();
        let projection = project_body(&alt);
        assert!(projection.blocks[0].text().ends_with(']'));
        assert!(projection.blocks[0]
            .spans
            .iter()
            .all(|span| span.text.len() <= BODY_VIEW_MAX_TEXT_RUN_BYTES
                && span.text.capacity() <= BODY_VIEW_MAX_TEXT_RUN_BYTES));
        assert!(!projection
            .blocks
            .iter()
            .any(|block| block.kind == ProjectedBlockKind::Truncated));
    }

    #[test]
    fn oversized_links_degrade_to_plain_text_and_retained_targets_are_interned() {
        let oversized = format!("https://example.com/{}", "x".repeat(1_500 * 1024));
        let mut html = format!(r#"<p><a href="{oversized}">"#);
        for index in 0..BODY_VIEW_MAX_SPANS {
            html.push_str(if index % 2 == 0 {
                "<b>x</b>"
            } else {
                "<i>y</i>"
            });
        }
        html.push_str("</a></p>");
        assert!(html.len() < BODY_VIEW_MAX_INPUT_BYTES);
        let body = BodySource::Html(html).sanitize();
        assert!(!body.sanitized_html().unwrap().as_str().contains("href="));
        let projection = project_body(&body);
        assert_eq!(projection.link_target_count(), 0);
        assert!(projection
            .blocks
            .iter()
            .flat_map(|block| &block.spans)
            .all(|span| span.link_target.is_none()));

        let body = BodySource::Html(
            r#"<p><a href="https://example.com/one"><b>a</b><i>b</i><code>c</code></a>
               <a href="https://example.com/one"><b>d</b></a></p>"#
                .to_owned(),
        )
        .sanitize();
        let projection = project_body(&body);
        assert_eq!(projection.link_target_count(), 1);
        let targets = projection
            .blocks
            .iter()
            .flat_map(|block| &block.spans)
            .filter_map(|span| span.link_target)
            .collect::<Vec<_>>();
        assert!(!targets.is_empty());
        assert!(targets.iter().all(|target| *target == targets[0]));
        assert_eq!(
            projection.link_target(targets[0]),
            Some("https://example.com/one")
        );
    }

    #[test]
    fn link_target_from_a_foreign_projection_fails_closed() {
        let first = project_body(
            &BodySource::Html(r#"<p><a href="https://first.example/">first</a></p>"#.to_owned())
                .sanitize(),
        );
        let second = project_body(
            &BodySource::Html(r#"<p><a href="https://second.example/">second</a></p>"#.to_owned())
                .sanitize(),
        );
        let target = first.blocks[0].spans[0].link_target.unwrap();
        assert_eq!(first.link_target(target), Some("https://first.example/"));
        assert_eq!(target.index(), 0);
        assert_eq!(second.link_target(target), None);
    }

    #[test]
    fn text_run_byte_cap_chunks_without_truncating_the_body() {
        let html = format!(
            "<p>{}final run sentinel</p>",
            "x".repeat(BODY_VIEW_MAX_TEXT_RUN_BYTES + 100)
        );
        let body = BodySource::Html(html).sanitize();
        let projection = project_body(&body);
        assert!(projection.blocks[0].text().ends_with("final run sentinel"));
        assert!(projection.blocks[0].spans.len() >= 2);
        assert!(projection
            .blocks
            .iter()
            .flat_map(|block| &block.spans)
            .all(|span| span.text.len() <= BODY_VIEW_MAX_TEXT_RUN_BYTES));
        assert!(!projection
            .blocks
            .iter()
            .any(|block| block.kind == ProjectedBlockKind::Truncated));
    }

    #[test]
    fn text_run_chunks_keep_combining_and_zwj_graphemes_whole() {
        let combining = format!(
            "<p>{}e\u{301}</p>",
            "x".repeat(BODY_VIEW_MAX_TEXT_RUN_BYTES - 1)
        );
        let projection = project_body(&BodySource::Html(combining).sanitize());
        let spans = &projection.blocks[0].spans;
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].text.len(), BODY_VIEW_MAX_TEXT_RUN_BYTES - 1);
        assert_eq!(spans[1].text, "e\u{301}");
        assert!(!spans[0].text.ends_with('e'));
        assert!(!spans[1].text.starts_with('\u{301}'));

        let joined_emoji = "👩‍💻";
        let zwj = format!(
            "{}{joined_emoji}tail",
            "x".repeat(BODY_VIEW_MAX_TEXT_RUN_BYTES - 1)
        );
        let projection = project_body(&BodySource::Plain(zwj).sanitize());
        let spans = &projection.blocks[0].spans;
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].text.len(), BODY_VIEW_MAX_TEXT_RUN_BYTES - 1);
        assert!(spans[1].text.starts_with(joined_emoji), "{spans:?}");
        assert!(!spans[0].text.contains('\u{200d}'));
    }

    #[test]
    fn text_run_chunks_prefer_a_nearby_word_boundary() {
        let prefix = "x".repeat(BODY_VIEW_MAX_TEXT_RUN_BYTES - 20);
        let word = "boundarywordcontinues";
        let html = format!("<p>{prefix} {word} tail</p>");
        let projection = project_body(&BodySource::Html(html).sanitize());
        let spans = &projection.blocks[0].spans;
        assert_eq!(spans.len(), 2);
        assert!(spans[0].text.ends_with(' '), "{spans:?}");
        assert!(spans[1].text.starts_with(word), "{spans:?}");
        assert!(spans
            .iter()
            .all(|span| span.text.len() <= BODY_VIEW_MAX_TEXT_RUN_BYTES));
    }

    #[test]
    fn text_run_chunking_does_not_create_a_break_at_no_break_spaces() {
        for no_break_space in ['\u{00A0}', '\u{202F}'] {
            let text = format!(
                "{}x{no_break_space}tail",
                "x".repeat(BODY_VIEW_MAX_TEXT_RUN_BYTES - no_break_space.len_utf8() - 1)
            );
            let projection = project_body(&BodySource::Plain(text.clone()).sanitize());
            assert_eq!(
                projection.blocks[0]
                    .spans
                    .iter()
                    .map(|span| span.text.as_str())
                    .collect::<String>(),
                text
            );
            assert!(projection.blocks[0].spans.windows(2).all(|spans| {
                !spans[0].text.ends_with(no_break_space)
                    && !spans[1].text.starts_with(no_break_space)
            }));
        }
    }

    fn assert_accessible_truncation(body: SanitizedBody) {
        let mut app = App::new();
        app.add_plugins(CtkBodyViewPlugin);
        let mut commands = app.world_mut().commands();
        spawn_body_view(
            &mut commands,
            CtkBodyViewProps::new(body, "Bounded message"),
        );
        app.world_mut().flush();
        let world = app.world_mut();
        let mut blocks = world.query::<(&BodyBlock, &AccessibilityNode)>();
        assert!(blocks.iter(world).any(|(block, node)| {
            block.text == "Message truncated." && node.role() == Role::Alert
        }));
    }

    #[test]
    fn projection_preserves_blocks_lists_quotes_links_and_emphasis() {
        let body = BodySource::Html(
            r#"
                <h2>Heading</h2>
                <p>Hello <strong>bold</strong>, <em>italic</em>,
                   <a href="https://example.com/read">read this</a>.</p>
                <ul><li>one</li><li>two<ol><li>nested</li></ol></li></ul>
                <blockquote><blockquote><p>quoted <code>code</code></p></blockquote></blockquote>
                <pre>  keep
space</pre>
            "#
            .to_owned(),
        )
        .sanitize();
        let projection = project_body(&body);
        assert!(matches!(
            projection.blocks[0].kind,
            ProjectedBlockKind::Heading(2)
        ));
        assert!(projection.blocks.iter().any(|block| {
            block.spans.iter().any(|span| {
                span.bold && span.text == "bold"
                    || span.italic && span.text == "italic"
                    || span
                        .link_target
                        .and_then(|target| projection.link_target(target))
                        == Some("https://example.com/read")
            })
        }));
        assert!(projection.blocks.iter().any(|block| {
            block
                .list_item_start
                .is_some_and(|item| item.ordered && item.depth == 1)
        }));
        assert!(projection.blocks.iter().any(|block| block.quote_depth == 2));
        assert!(projection.blocks.iter().any(|block| {
            block.kind == ProjectedBlockKind::Preformatted && block.text().contains("  keep\nspace")
        }));
    }

    #[test]
    fn list_item_block_children_keep_boundaries_and_quote_depth() {
        let body = BodySource::Html(
            r#"<ul><li><p>first paragraph</p><p>second paragraph</p>
               <blockquote><p>nested quotation</p></blockquote></li></ul>"#
                .to_owned(),
        )
        .sanitize();
        let projection = project_body(&body);
        assert_eq!(
            projection
                .blocks
                .iter()
                .filter(|block| block.text().contains("paragraph"))
                .count(),
            2
        );
        assert!(projection
            .blocks
            .iter()
            .any(|block| block.text().contains("nested quotation") && block.quote_depth == 1));
    }

    #[test]
    fn wrapped_nested_lists_keep_their_kind_and_direct_lists_keep_document_order() {
        let wrapped = project_body(
            &BodySource::Html("<ul><li><div><ul><li>nested</li></ul></div></li></ul>".to_owned())
                .sanitize(),
        );
        let list_items = wrapped
            .blocks
            .iter()
            .filter_map(|block| {
                block
                    .list_item_start
                    .map(|item| (item.depth, item.key.list_id, block.text()))
            })
            .collect::<Vec<_>>();
        assert_eq!(list_items.len(), 2);
        assert_eq!(list_items[0].0, 0);
        assert_eq!(list_items[1].0, 1);
        assert_ne!(list_items[0].1, list_items[1].1);
        assert!(list_items[1].2.contains("nested"));

        let ordered = project_body(
            &BodySource::Html("<ul><li>before<ul><li>nested</li></ul>after</li></ul>".to_owned())
                .sanitize(),
        );
        let texts = ordered
            .blocks
            .iter()
            .map(ProjectedBlock::text)
            .collect::<Vec<_>>();
        let before = texts
            .iter()
            .position(|text| text.contains("before"))
            .unwrap();
        let nested = texts
            .iter()
            .position(|text| text.contains("nested"))
            .unwrap();
        let after = texts
            .iter()
            .position(|text| text.contains("after"))
            .unwrap();
        assert!(before < nested && nested < after, "{texts:?}");
    }

    type SpanShape = (String, bool, bool, bool, Option<String>, Option<usize>);
    type BlockShape = (ProjectedBlockKind, usize, Vec<SpanShape>);

    fn projection_shape(projection: &BodyProjection) -> Vec<BlockShape> {
        projection
            .blocks
            .iter()
            .map(|block| {
                (
                    block.kind,
                    block.quote_depth,
                    block
                        .spans
                        .iter()
                        .map(|span| {
                            (
                                span.text.clone(),
                                span.bold,
                                span.italic,
                                span.code,
                                span.link_target
                                    .and_then(|target| projection.link_target(target))
                                    .map(str::to_owned),
                                span.anchor_occurrence.map(AnchorOccurrence::index),
                            )
                        })
                        .collect(),
                )
            })
            .collect()
    }

    #[test]
    fn table_cells_use_the_same_structural_projection_as_top_level_flow() {
        let top_level = project_body(
            &BodySource::Html(
                r#"
                    <a href="https://example.com/story">
                      <h2>Quarterly results</h2>
                      <p>Read the complete report.</p>
                    </a>
                    <ul><li>Apples</li><li>Bananas</li></ul>
                "#
                .to_owned(),
            )
            .sanitize(),
        );
        let in_cells =
            project_body(&BodySource::Html(TABLE_CELL_STRUCTURAL_CONTENT.to_owned()).sanitize());
        assert_eq!(
            projection_shape(&in_cells),
            projection_shape(&top_level),
            "table cells must delegate structural flow to the one container traversal"
        );
        assert_eq!(
            in_cells
                .blocks
                .iter()
                .map(|block| block.kind)
                .collect::<Vec<_>>(),
            [
                ProjectedBlockKind::Heading(2),
                ProjectedBlockKind::Paragraph,
                ProjectedBlockKind::Paragraph,
                ProjectedBlockKind::Paragraph,
            ]
        );
        assert_eq!(
            in_cells
                .blocks
                .iter()
                .filter_map(|block| block.list_item_start.map(|item| item.key.position))
                .collect::<Vec<_>>(),
            [0, 1]
        );

        let mut app = App::new();
        app.add_plugins(CtkBodyViewPlugin);
        let mut commands = app.world_mut().commands();
        spawn_body_view(
            &mut commands,
            CtkBodyViewProps::new(
                BodySource::Html(TABLE_CELL_STRUCTURAL_CONTENT.to_owned()).sanitize(),
                "Table-cell message",
            ),
        );
        app.world_mut().flush();
        let links = accessible_link_entities(app.world_mut());
        assert_eq!(links.len(), 1);
        assert_eq!(
            app.world()
                .get::<AccessibilityNode>(links[0])
                .unwrap()
                .label(),
            Some("Quarterly results Read the complete report.")
        );
    }

    #[test]
    fn every_context_uses_one_structural_child_dispatcher() {
        let payload = r#"
            <h2>Heading item</h2>
            <p>Detail <a href="https://example.com/detail">link</a></p>
            <ul><li>nested item</li></ul>
            <table><tr><td><p>nested cell</p></td></tr></table>
        "#;
        let sources = [
            payload.to_owned(),
            format!("<table><tr><td>{payload}</td></tr></table>"),
            format!("<ul><li>{payload}</li></ul>"),
        ];
        let projections = sources
            .iter()
            .map(|source| project_body(&BodySource::Html(source.clone()).sanitize()))
            .collect::<Vec<_>>();
        let expected_shape = projection_shape(&projections[0]);
        for (context, projection) in ["top level", "table cell", "list item"]
            .into_iter()
            .zip(&projections)
        {
            assert_eq!(
                projection_shape(projection),
                expected_shape,
                "{context} introduced an independent block/inline classification"
            );
            assert_eq!(
                projection
                    .blocks
                    .iter()
                    .map(|block| block.kind)
                    .collect::<Vec<_>>(),
                [
                    ProjectedBlockKind::Heading(2),
                    ProjectedBlockKind::Paragraph,
                    ProjectedBlockKind::Paragraph,
                    ProjectedBlockKind::Paragraph,
                ]
            );
        }

        for source in &sources {
            let body = BodySource::Html(source.clone()).sanitize();
            let limited = project_body_with_budgets(&body, 2, BODY_VIEW_MAX_SPANS);
            assert_eq!(
                limited
                    .blocks
                    .iter()
                    .map(|block| block.kind)
                    .collect::<Vec<_>>(),
                [
                    ProjectedBlockKind::Heading(2),
                    ProjectedBlockKind::Paragraph,
                    ProjectedBlockKind::Truncated,
                ],
                "block budget and halt behaviour drifted by container context"
            );
            let span_limited = project_body_with_budgets(&body, BODY_VIEW_MAX_BLOCKS, 2);
            assert_eq!(
                span_limited
                    .blocks
                    .iter()
                    .map(|block| block.kind)
                    .collect::<Vec<_>>(),
                [
                    ProjectedBlockKind::Heading(2),
                    ProjectedBlockKind::Paragraph,
                    ProjectedBlockKind::Truncated,
                ],
                "span budget and halt behaviour drifted by container context"
            );
            if source.starts_with("<ul>") {
                assert!(
                    span_limited.blocks[0].list_item_start.is_some(),
                    "halting inside a list item must not detach its accepted blocks"
                );
            }

            let mut app = App::new();
            app.add_plugins(CtkBodyViewPlugin);
            let mut commands = app.world_mut().commands();
            spawn_body_view(
                &mut commands,
                CtkBodyViewProps::new(body.clone(), "Unified roles"),
            );
            app.world_mut().flush();
            let rendered_roles = {
                let world = app.world_mut();
                let mut query = world.query::<(&BodyBlock, &AccessibilityNode)>();
                ["Heading item", "Detail link", "nested item", "nested cell"].map(|needle| {
                    query
                        .iter(world)
                        .find_map(|(block, node)| {
                            block.text.contains(needle).then_some(node.role())
                        })
                        .unwrap_or_else(|| panic!("missing rendered block {needle:?}"))
                })
            };
            assert_eq!(
                rendered_roles,
                [
                    Role::Heading,
                    Role::Paragraph,
                    Role::Paragraph,
                    Role::Paragraph,
                ],
                "semantic roles drifted by container context"
            );
            assert_entity_ledger_matches(body, "Unified structural traversal");
        }

        for source in sources.map(|source| {
            source.replace(
                "<h2>Heading item</h2>",
                &format!(
                    "{}<h2>depth-guard sentinel</h2>{}",
                    "<div>".repeat(MAX_PROJECTION_DEPTH * 2),
                    "</div>".repeat(MAX_PROJECTION_DEPTH * 2)
                ),
            )
        }) {
            let projection = project_body(&BodySource::Html(source).sanitize());
            assert!(projection
                .blocks
                .iter()
                .any(|block| block.text().contains("depth-guard sentinel")));
        }
    }

    #[test]
    fn inline_only_table_cells_keep_the_compact_row_projection() {
        let projection = project_body(
            &BodySource::Html(
                "<table><tr><td>One <strong>bold</strong></td><td>Next</td></tr></table>"
                    .to_owned(),
            )
            .sanitize(),
        );
        assert_eq!(projection.blocks.len(), 1);
        assert_eq!(projection.blocks[0].kind, ProjectedBlockKind::Paragraph);
        assert_eq!(projection.blocks[0].text(), "One bold  Next");
        assert_eq!(
            projected_span_texts(&projection),
            vec![vec!["One ", "bold", "  Next"]]
        );
    }

    #[test]
    fn zero_content_budgets_do_not_spawn_an_empty_fallback() {
        let body = BodySource::Plain(String::new()).sanitize();
        let projection = project_body_with_budgets(&body, 0, 0);
        assert!(projection.blocks.is_empty());

        let mut app = App::new();
        app.add_plugins(CtkBodyViewPlugin);
        let mut commands = app.world_mut().commands();
        spawn_body_view(
            &mut commands,
            CtkBodyViewProps::new(body, "Empty")
                .max_blocks(0)
                .max_spans(0),
        );
        app.world_mut().flush();
        let world = app.world_mut();
        let mut blocks = world.query::<&BodyBlock>();
        assert_eq!(blocks.iter(world).count(), 0);
    }

    #[test]
    fn total_entity_cap_counts_quote_wrappers_and_every_spawned_entity() {
        let html = format!(
            "{}{}{}",
            "<blockquote>".repeat(MAX_QUOTE_DEPTH),
            "<p>x</p>".repeat(BODY_VIEW_MAX_BLOCKS),
            "</blockquote>".repeat(MAX_QUOTE_DEPTH)
        );
        let body = BodySource::Html(html).sanitize();
        let expected_entities = project_body(&body).entity_count;
        let mut app = App::new();
        app.add_plugins(CtkBodyViewPlugin);
        let before = {
            let world = app.world_mut();
            let mut entities = world.query::<Entity>();
            entities.iter(world).count()
        };
        let mut commands = app.world_mut().commands();
        spawn_body_view(&mut commands, CtkBodyViewProps::new(body, "Entity-bounded"));
        app.world_mut().flush();
        let world = app.world_mut();
        let mut entities = world.query::<Entity>();
        let spawned = entities.iter(world).count() - before;
        let mut body_blocks = world.query::<&BodyBlock>();
        let body_block_count = body_blocks.iter(world).count();
        let mut a11y = world.query::<&AccessibilityNode>();
        let quote_count = a11y
            .iter(world)
            .filter(|node| node.role() == Role::Blockquote)
            .count();
        assert!(
            spawned <= BODY_VIEW_MAX_ENTITIES,
            "spawned {spawned} entities for {body_block_count} blocks and {quote_count} quotes"
        );
        assert_eq!(spawned, expected_entities);
        let mut alerts = world.query::<(&BodyBlock, &AccessibilityNode)>();
        assert!(alerts.iter(world).any(|(block, node)| {
            block.text == "Message truncated." && node.role() == Role::Alert
        }));
    }

    #[test]
    fn entity_ledger_matches_all_representative_spawn_sites() {
        let body = BodySource::Html(
            r#"
                <h2>heading</h2>
                <p>plain <strong>bold</strong>
                   <a href="https://example.com/">link</a></p>
                <blockquote><ol><li>ordered
                    <ul><li>nested</li></ul>
                    <p>continuation</p>
                </li></ol></blockquote>
                <pre>preformatted
content</pre>
                <table>
                    <thead><tr><th>head</th></tr></thead>
                    <tbody><tr><td>cell</td></tr></tbody>
                </table>
                <hr>
            "#
            .to_owned(),
        )
        .sanitize();
        let projection = project_body(&body);
        assert!(projection
            .blocks
            .iter()
            .any(|block| matches!(block.kind, ProjectedBlockKind::Heading(_))));
        assert!(projection
            .blocks
            .iter()
            .any(|block| block.kind == ProjectedBlockKind::Preformatted));
        assert!(projection
            .blocks
            .iter()
            .any(|block| block.text().contains("head")));
        assert!(projection
            .blocks
            .iter()
            .any(|block| block.text().contains("cell")));
        assert_entity_ledger_matches(body, "Ledger block coverage");

        let empty = BodySource::Plain(String::new()).sanitize();
        let empty_projection = project_body(&empty);
        assert_eq!(empty_projection.blocks.len(), 1);
        assert_eq!(
            empty_projection.blocks[0].kind,
            ProjectedBlockKind::Paragraph
        );
        assert_eq!(empty_projection.blocks[0].spans, [ProjectedSpan::default()]);
        assert_entity_ledger_matches(empty, "Ledger empty fallback");
    }

    fn assert_entity_ledger_matches(body: SanitizedBody, label: &str) {
        let expected_entities = project_body(&body).entity_count;
        let mut app = App::new();
        app.add_plugins(CtkBodyViewPlugin);
        let before = {
            let world = app.world_mut();
            let mut entities = world.query::<Entity>();
            entities.iter(world).count()
        };
        let mut commands = app.world_mut().commands();
        spawn_body_view(&mut commands, CtkBodyViewProps::new(body, label.to_owned()));
        app.world_mut().flush();
        let world = app.world_mut();
        let mut entities = world.query::<Entity>();
        assert_eq!(entities.iter(world).count() - before, expected_entities);
    }

    #[derive(Resource, Default)]
    struct SeenLinks(Vec<LinkActivated>);

    fn record_link(event: On<LinkActivated>, mut seen: ResMut<SeenLinks>) {
        seen.0.push(event.clone());
    }

    #[derive(Resource, Default)]
    struct AncestorPointerLifecycle {
        cancels: usize,
        releases: usize,
        drag_ends: usize,
    }

    #[test]
    fn selection_targets_exclude_scrollbar_subtree_but_keep_projected_text_padding() {
        use bevy::ecs::system::SystemState;

        let body = BodySource::Html(
            "<p>first selectable block</p><p>second selectable block</p>".to_owned(),
        )
        .sanitize();
        let mut app = App::new();
        app.add_plugins(CtkBodyViewPlugin);
        let mut commands = app.world_mut().commands();
        let entities = spawn_body_view(
            &mut commands,
            CtkBodyViewProps::new(body, "Selection targets"),
        );
        app.world_mut().flush();

        let scrollbar = {
            let world = app.world_mut();
            let mut query = world.query_filtered::<Entity, With<Scrollbar>>();
            query.single(world).unwrap()
        };
        let thumb = {
            let world = app.world_mut();
            let mut query = world.query_filtered::<Entity, With<ScrollbarThumb>>();
            query.single(world).unwrap()
        };
        let block = {
            let world = app.world_mut();
            let mut query = world.query_filtered::<Entity, With<BodyBlock>>();
            query.iter(world).next().unwrap()
        };
        let run = {
            let world = app.world_mut();
            let mut query = world.query_filtered::<Entity, With<BodyTextRun>>();
            query.iter(world).next().unwrap()
        };
        let mut state: SystemState<SelectionTargetQueries<'_, '_>> =
            SystemState::new(app.world_mut());
        let (parents, runs, blocks, links, documents) = state.get(app.world()).unwrap();

        for control in [scrollbar, thumb, entities.root] {
            assert_eq!(
                selection_target_body_view(control, &parents, &runs, &blocks, &links, &documents),
                None,
                "non-text body-view control {control:?} entered selection"
            );
        }
        for projected_target in [run, block, entities.document] {
            assert_eq!(
                selection_target_body_view(
                    projected_target,
                    &parents,
                    &runs,
                    &blocks,
                    &links,
                    &documents,
                ),
                Some(entities.root),
                "projected text/document target {projected_target:?} lost selection"
            );
        }
    }

    #[test]
    fn link_selection_suppression_survives_second_touch_and_full_event_teardown() {
        use bevy::camera::NormalizedRenderTarget;
        use bevy::picking::backend::HitData;
        use bevy::picking::pointer::{Location, PointerButton, PointerId};
        use bevy::window::{Window, WindowRef};
        use core::time::Duration;
        use std::time::Instant;

        let mut app = App::new();
        app.add_plugins(CtkBodyViewPlugin)
            .init_resource::<ButtonInput<Key>>()
            .init_resource::<SeenLinks>()
            .init_resource::<AncestorPointerLifecycle>()
            .add_observer(record_link);
        let window = app.world_mut().spawn(Window::default()).id();
        let mut commands = app.world_mut().commands();
        let entities = spawn_body_view(
            &mut commands,
            CtkBodyViewProps::new(
                BodySource::Html(
                    r#"<p><a href="https://example.com/long-link">select this link</a></p>"#
                        .to_owned(),
                )
                .sanitize(),
                "Dragged link",
            ),
        );
        app.world_mut().flush();
        app.world_mut().entity_mut(entities.root).observe(
            |_: On<Pointer<Release>>, mut seen: ResMut<AncestorPointerLifecycle>| {
                seen.releases += 1;
            },
        );
        app.world_mut().entity_mut(entities.root).observe(
            |_: On<Pointer<DragEnd>>, mut seen: ResMut<AncestorPointerLifecycle>| {
                seen.drag_ends += 1;
            },
        );
        let link = {
            let world = app.world_mut();
            let mut query = world.query_filtered::<Entity, With<BodyLink>>();
            query.iter(world).next().unwrap()
        };
        let runtime = app
            .world()
            .get::<BodyProjectionRuntime>(entities.root)
            .unwrap()
            .clone();
        let touch_a = PointerId::Touch(41);
        let touch_b = PointerId::Touch(42);
        {
            let mut root = app.world_mut().entity_mut(entities.root);
            let mut selection = root.get_mut::<BodyTextSelection>().unwrap();
            selection.set_caret(BodyTextPosition {
                block: 0,
                offset: 0,
            });
            assert_eq!(selection.begin_drag(touch_a), Some(true));
            assert_eq!(selection.note_drag_started(touch_a), Some(true));
            selection.extend_to(BodyTextPosition {
                block: 0,
                offset: "select".len(),
            });
            selection.note_drag_selection(&runtime.document, touch_a);
            // Touch B presses selectable text while A still owns the non-empty
            // selection. B is tracked and swallowed, but cannot replace A.
            assert_eq!(selection.begin_drag(touch_b), Some(false));
        }
        let location = Location {
            target: NormalizedRenderTarget::Window(
                WindowRef::Entity(window).normalize(None).unwrap(),
            ),
            position: Vec2::ZERO,
        };
        {
            let mut pointer_state = app.world_mut().resource_mut::<PointerState>();
            for pointer in [touch_a, touch_b] {
                pointer_state
                    .get_mut(pointer, PointerButton::Primary)
                    .pressing
                    .insert(
                        link,
                        (
                            location.clone(),
                            Instant::now(),
                            HitData::new(link, 0.0, None, None),
                        ),
                    );
            }
        }
        let click = || Click {
            button: PointerButton::Primary,
            hit: HitData::new(link, 0.0, None, None),
            duration: Duration::ZERO,
            count: 1,
        };

        // Bevy 0.19 emits Click, Release, then DragEnd. A must consume only its
        // own Click and remain present for both later events.
        app.world_mut()
            .trigger(Pointer::new(touch_a, location.clone(), click(), link));
        app.update();
        assert!(app.world().resource::<SeenLinks>().0.is_empty());
        assert_eq!(
            app.world()
                .get::<BodyTextSelection>(entities.root)
                .unwrap()
                .gestures
                .len(),
            2
        );

        app.world_mut().trigger(Pointer::new(
            touch_a,
            location.clone(),
            Release {
                button: PointerButton::Primary,
                hit: HitData::new(link, 0.0, None, None),
            },
            link,
        ));
        app.update();
        assert_eq!(
            app.world().resource::<AncestorPointerLifecycle>().releases,
            0,
            "selection Release leaked to the body-view ancestor"
        );
        assert_eq!(
            app.world()
                .get::<BodyTextSelection>(entities.root)
                .unwrap()
                .gestures
                .len(),
            2,
            "Release removed state still needed by DragEnd"
        );

        app.world_mut().trigger(Pointer::new(
            touch_a,
            location.clone(),
            DragEnd {
                button: PointerButton::Primary,
                distance: Vec2::new(20.0, 0.0),
            },
            link,
        ));
        app.update();
        assert_eq!(
            app.world().resource::<AncestorPointerLifecycle>().drag_ends,
            0,
            "selection DragEnd leaked to the body-view ancestor"
        );
        assert_eq!(
            app.world()
                .get::<BodyTextSelection>(entities.root)
                .unwrap()
                .gestures
                .len(),
            1,
            "DragEnd did not remove only touch A"
        );

        app.world_mut().trigger(Pointer::new(
            touch_b,
            location.clone(),
            Release {
                button: PointerButton::Primary,
                hit: HitData::new(link, 0.0, None, None),
            },
            link,
        ));
        app.update();
        assert!(app
            .world()
            .get::<BodyTextSelection>(entities.root)
            .unwrap()
            .gestures
            .is_empty());

        // Suppression is one-shot: the next ordinary primary click activates.
        app.world_mut()
            .trigger(Pointer::new(PointerId::Mouse, location, click(), link));
        app.update();
        assert_eq!(
            app.world().resource::<SeenLinks>().0,
            [LinkActivated {
                body_view: entities.root,
                href: "https://example.com/long-link".to_owned(),
            }]
        );
    }

    #[test]
    fn escape_preserves_drag_suppression_and_terminal_pointer_teardown() {
        use bevy::camera::NormalizedRenderTarget;
        use bevy::input::InputPlugin;
        use bevy::input_focus::{InputDispatchPlugin, InputFocusPlugin};
        use bevy::picking::backend::HitData;
        use bevy::picking::pointer::{Location, PointerButton, PointerId};
        use bevy::window::{PrimaryWindow, Window, WindowRef};
        use core::time::Duration;
        use std::time::Instant;

        let mut app = App::new();
        app.add_plugins((
            InputPlugin,
            InputFocusPlugin,
            InputDispatchPlugin,
            CtkBodyViewPlugin,
        ))
        .init_resource::<SeenLinks>()
        .init_resource::<AncestorPointerLifecycle>()
        .add_observer(record_link);
        let window = app
            .world_mut()
            .spawn((Window::default(), PrimaryWindow))
            .id();
        let mut commands = app.world_mut().commands();
        let entities = spawn_body_view(
            &mut commands,
            CtkBodyViewProps::new(
                BodySource::Html(
                    r#"<p><a href="https://example.com/escape">select this link</a></p>"#
                        .to_owned(),
                )
                .sanitize(),
                "Escape during selection",
            ),
        );
        app.world_mut().flush();
        app.world_mut().entity_mut(entities.root).observe(
            |_: On<Pointer<Release>>, mut seen: ResMut<AncestorPointerLifecycle>| {
                seen.releases += 1;
            },
        );
        app.world_mut().entity_mut(entities.root).observe(
            |_: On<Pointer<DragEnd>>, mut seen: ResMut<AncestorPointerLifecycle>| {
                seen.drag_ends += 1;
            },
        );
        let (link_entity, focus_entity) = {
            let world = app.world_mut();
            let mut query = world.query::<(Entity, &BodyLink)>();
            let (entity, link) = query.iter(world).next().unwrap();
            (entity, link.focus_entity)
        };
        let runtime = app
            .world()
            .get::<BodyProjectionRuntime>(entities.root)
            .unwrap()
            .clone();
        let pointer = PointerId::Touch(43);
        {
            let mut root = app.world_mut().entity_mut(entities.root);
            let mut selection = root.get_mut::<BodyTextSelection>().unwrap();
            selection.set_caret(BodyTextPosition {
                block: 0,
                offset: 0,
            });
            assert_eq!(selection.begin_drag(pointer), Some(true));
            assert_eq!(selection.note_drag_started(pointer), Some(true));
            selection.extend_to(BodyTextPosition {
                block: 0,
                offset: "select".len(),
            });
            selection.note_drag_selection(&runtime.document, pointer);
        }

        let location = Location {
            target: NormalizedRenderTarget::Window(
                WindowRef::Entity(window).normalize(None).unwrap(),
            ),
            position: Vec2::ZERO,
        };
        app.world_mut()
            .resource_mut::<PointerState>()
            .get_mut(pointer, PointerButton::Primary)
            .pressing
            .insert(
                link_entity,
                (
                    location.clone(),
                    Instant::now(),
                    HitData::new(link_entity, 0.0, None, None),
                ),
            );
        app.world_mut()
            .insert_resource(InputFocus::from_entity(focus_entity));
        app.world_mut().write_message(KeyboardInput {
            key_code: KeyCode::Escape,
            logical_key: Key::Escape,
            state: ButtonState::Pressed,
            text: None,
            repeat: false,
            window,
        });
        app.update();
        {
            let selection = app.world().get::<BodyTextSelection>(entities.root).unwrap();
            assert_eq!(selection.anchor, None);
            assert_eq!(selection.focus, None);
            assert_eq!(
                selection.gestures.len(),
                1,
                "Escape discarded the live pointer gesture"
            );
        }
        app.world_mut().trigger(Pointer::new(
            pointer,
            location.clone(),
            Click {
                button: PointerButton::Primary,
                hit: HitData::new(link_entity, 0.0, None, None),
                duration: Duration::ZERO,
                count: 1,
            },
            link_entity,
        ));
        app.update();
        assert!(
            app.world().resource::<SeenLinks>().0.is_empty(),
            "Escape let the drag-armed Click activate its link"
        );

        app.world_mut().trigger(Pointer::new(
            pointer,
            location.clone(),
            Release {
                button: PointerButton::Primary,
                hit: HitData::new(link_entity, 0.0, None, None),
            },
            link_entity,
        ));
        app.update();
        assert_eq!(
            app.world().resource::<AncestorPointerLifecycle>().releases,
            0,
            "Release lost ownership after Escape"
        );
        assert_eq!(
            app.world()
                .get::<BodyTextSelection>(entities.root)
                .unwrap()
                .gestures
                .len(),
            1,
            "Release removed state still needed by DragEnd"
        );

        app.world_mut().trigger(Pointer::new(
            pointer,
            location,
            DragEnd {
                button: PointerButton::Primary,
                distance: Vec2::new(20.0, 0.0),
            },
            link_entity,
        ));
        app.update();
        assert_eq!(
            app.world().resource::<AncestorPointerLifecycle>().drag_ends,
            0,
            "DragEnd lost ownership after Escape"
        );
        assert!(app
            .world()
            .get::<BodyTextSelection>(entities.root)
            .unwrap()
            .gestures
            .is_empty());
    }

    #[test]
    fn pointer_cancel_removes_gesture_and_allows_fresh_press() {
        use bevy::camera::NormalizedRenderTarget;
        use bevy::picking::backend::HitData;
        use bevy::picking::pointer::{Location, PointerId};
        use bevy::window::{Window, WindowRef};

        let mut app = App::new();
        app.add_plugins(CtkBodyViewPlugin)
            .init_resource::<AncestorPointerLifecycle>();
        let window = app.world_mut().spawn(Window::default()).id();
        let mut commands = app.world_mut().commands();
        let entities = spawn_body_view(
            &mut commands,
            CtkBodyViewProps::new(
                BodySource::Html("<p>cancel this selection</p>".to_owned()).sanitize(),
                "Cancelled selection",
            ),
        );
        app.world_mut().flush();
        app.world_mut().entity_mut(entities.root).observe(
            |_: On<Pointer<PointerCancel>>, mut seen: ResMut<AncestorPointerLifecycle>| {
                seen.cancels += 1;
            },
        );
        let run = {
            let world = app.world_mut();
            let mut query = world.query_filtered::<Entity, With<BodyTextRun>>();
            query.iter(world).next().unwrap()
        };
        let pointer = PointerId::Touch(44);
        {
            let mut root = app.world_mut().entity_mut(entities.root);
            let mut selection = root.get_mut::<BodyTextSelection>().unwrap();
            assert_eq!(selection.begin_drag(pointer), Some(true));
        }
        let location = Location {
            target: NormalizedRenderTarget::Window(
                WindowRef::Entity(window).normalize(None).unwrap(),
            ),
            position: Vec2::ZERO,
        };
        app.world_mut().trigger(Pointer::new(
            pointer,
            location,
            PointerCancel {
                hit: HitData::new(run, 0.0, None, None),
            },
            run,
        ));
        app.update();
        assert_eq!(
            app.world().resource::<AncestorPointerLifecycle>().cancels,
            0,
            "owned selection Cancel leaked to the body-view ancestor"
        );

        let mut root = app.world_mut().entity_mut(entities.root);
        let mut selection = root.get_mut::<BodyTextSelection>().unwrap();
        assert!(
            selection.gestures.is_empty(),
            "terminal Cancel left its gesture record behind"
        );
        assert_eq!(
            selection.begin_drag(pointer),
            Some(true),
            "the cancelled pointer did not get a fresh gesture"
        );
    }

    #[test]
    fn drag_jitter_within_one_cursor_position_does_not_suppress_link_click() {
        let projection =
            project_body(&BodySource::Html("<p>clickable text</p>".to_owned()).sanitize());
        let document = BodySelectionDocument::from_projection(&projection);
        let mut selection = BodyTextSelection::default();
        let caret = BodyTextPosition {
            block: 0,
            offset: 3,
        };
        selection.set_caret(caret);
        assert_eq!(selection.begin_drag(PointerId::Mouse), Some(true));
        assert_eq!(selection.note_drag_started(PointerId::Mouse), Some(true));
        selection.extend_to(caret);
        selection.note_drag_selection(&document, PointerId::Mouse);
        assert!(
            !selection.consume_click_suppression(PointerId::Mouse),
            "pointer motion which leaves the logical selection empty is an ordinary click"
        );
    }

    #[test]
    fn selection_pointer_table_is_bounded_without_evicting_the_owner() {
        let mut selection = BodyTextSelection::default();
        for id in 0..BODY_VIEW_MAX_ACTIVE_SELECTION_POINTERS {
            assert_eq!(
                selection.begin_drag(PointerId::Touch(id as u64)),
                Some(id == 0)
            );
        }
        assert_eq!(
            selection.begin_drag(PointerId::Touch(u64::MAX)),
            None,
            "distinct pointer IDs exceeded the fixed gesture bound"
        );
        assert_eq!(
            selection
                .gestures
                .iter()
                .filter(|gesture| gesture.owns_selection)
                .map(|gesture| gesture.pointer)
                .collect::<Vec<_>>(),
            [PointerId::Touch(0)],
            "capacity pressure evicted or replaced the selection owner"
        );
    }

    #[test]
    fn last_sweep_prunes_unpressed_gestures_before_the_next_capacity_check() {
        use bevy::camera::NormalizedRenderTarget;
        use bevy::picking::backend::HitData;
        use bevy::picking::pointer::{Location, PointerButton};
        use bevy::window::{Window, WindowRef};
        use std::time::Instant;

        let mut app = App::new();
        app.add_plugins(CtkBodyViewPlugin);
        let window = app.world_mut().spawn(Window::default()).id();
        let view = app.world_mut().spawn(BodyTextSelection::default()).id();
        {
            let mut view_entity = app.world_mut().entity_mut(view);
            let mut selection = view_entity.get_mut::<BodyTextSelection>().unwrap();
            for id in 0..BODY_VIEW_MAX_ACTIVE_SELECTION_POINTERS {
                assert!(selection.begin_drag(PointerId::Touch(id as u64)).is_some());
            }
            assert_eq!(
                selection.gestures.len(),
                BODY_VIEW_MAX_ACTIVE_SELECTION_POINTERS
            );
        }

        let live = PointerId::Touch(0);
        let location = Location {
            target: NormalizedRenderTarget::Window(
                WindowRef::Entity(window).normalize(None).unwrap(),
            ),
            position: Vec2::ZERO,
        };
        app.world_mut()
            .resource_mut::<PointerState>()
            .get_mut(live, PointerButton::Primary)
            .pressing
            .insert(
                view,
                (
                    location,
                    Instant::now(),
                    HitData::new(view, 0.0, None, None),
                ),
            );
        app.update();
        let mut view_entity = app.world_mut().entity_mut(view);
        let mut selection = view_entity.get_mut::<BodyTextSelection>().unwrap();
        assert_eq!(selection.gestures.len(), 1);
        assert_eq!(selection.gestures[0].pointer, live);
        assert!(selection.gestures[0].owns_selection);
        let fresh = PointerId::Touch(u64::MAX);
        assert_eq!(
            selection.begin_drag(fresh),
            Some(false),
            "the Last sweep did not free dead slots before the next frame's press"
        );
        assert_eq!(selection.gestures.len(), 2);
    }

    #[test]
    fn last_sweep_removes_unpressed_and_retains_pressed_gestures() {
        use bevy::camera::NormalizedRenderTarget;
        use bevy::ecs::change_detection::DetectChanges;
        use bevy::picking::backend::HitData;
        use bevy::picking::pointer::{Location, PointerButton};
        use bevy::window::{Window, WindowRef};
        use std::time::Instant;

        let mut app = App::new();
        app.add_plugins(CtkBodyViewPlugin);
        let window = app.world_mut().spawn(Window::default()).id();
        let live = PointerId::Touch(51);
        let released = PointerId::Touch(52);
        let mut selection = BodyTextSelection::default();
        assert_eq!(selection.begin_drag(live), Some(true));
        assert_eq!(selection.begin_drag(released), Some(false));
        let view = app.world_mut().spawn(selection).id();
        let location = Location {
            target: NormalizedRenderTarget::Window(
                WindowRef::Entity(window).normalize(None).unwrap(),
            ),
            position: Vec2::ZERO,
        };
        app.world_mut()
            .resource_mut::<PointerState>()
            .get_mut(live, PointerButton::Primary)
            .pressing
            .insert(
                view,
                (
                    location,
                    Instant::now(),
                    HitData::new(view, 0.0, None, None),
                ),
            );

        app.update();
        let selection = app.world().get::<BodyTextSelection>(view).unwrap();
        assert_eq!(selection.gestures.len(), 1);
        assert_eq!(selection.gestures[0].pointer, live);
        assert!(selection.gestures[0].owns_selection);
        let settled = app
            .world()
            .entity(view)
            .get_ref::<BodyTextSelection>()
            .unwrap()
            .last_changed();

        app.update();
        let unchanged = app
            .world()
            .entity(view)
            .get_ref::<BodyTextSelection>()
            .unwrap()
            .last_changed();
        assert_eq!(
            settled, unchanged,
            "the Last sweep marked an already-settled selection as changed"
        );
    }

    #[test]
    fn two_batched_presses_do_not_prune_a_record_awaiting_terminal_events() {
        let projection =
            project_body(&BodySource::Html("<p>select this link</p>".to_owned()).sanitize());
        let document = BodySelectionDocument::from_projection(&projection);
        let pointer_a = PointerId::Touch(61);
        let pointer_b = PointerId::Touch(62);
        let pointer_c = PointerId::Touch(63);
        let mut selection = BodyTextSelection::default();
        selection.set_caret(BodyTextPosition {
            block: 0,
            offset: 0,
        });
        assert_eq!(selection.begin_drag(pointer_a), Some(true));
        assert_eq!(selection.note_drag_started(pointer_a), Some(true));
        selection.extend_to(BodyTextPosition {
            block: 0,
            offset: "select".len(),
        });
        selection.note_drag_selection(&document, pointer_a);
        assert_eq!(selection.begin_drag(pointer_b), Some(false));
        assert_eq!(selection.begin_drag(pointer_c), Some(false));
        assert!(
            selection.consume_click_suppression(pointer_a),
            "two Press observers from the same batch removed A's armed suppression"
        );
        assert!(
            selection.release_pointer(pointer_a),
            "A's queued Release lost ownership after two batched Press observers"
        );
        assert!(
            selection.finish_drag(pointer_a),
            "A's queued DragEnd lost ownership after two batched Press observers"
        );
    }

    #[test]
    fn begin_drag_never_prunes_records_with_terminal_observers_still_queued() {
        let mut selection = BodyTextSelection::default();
        let awaiting_terminal = PointerId::Touch(71);
        assert_eq!(selection.begin_drag(awaiting_terminal), Some(true));
        assert_eq!(selection.note_drag_started(awaiting_terminal), Some(true));

        for id in 72..72 + BODY_VIEW_MAX_ACTIVE_SELECTION_POINTERS as u64 {
            let _ = selection.begin_drag(PointerId::Touch(id));
        }
        assert_eq!(
            selection.gestures[0].pointer, awaiting_terminal,
            "begin_drag pruned the existing owner while admitting later presses"
        );
        assert_eq!(
            selection.gestures.len(),
            BODY_VIEW_MAX_ACTIVE_SELECTION_POINTERS
        );
        assert!(selection.release_pointer(awaiting_terminal));
        assert!(selection.finish_drag(awaiting_terminal));
    }

    #[test]
    fn a_pointer_turned_away_by_the_capacity_bound_cannot_drive_selection() {
        let mut selection = BodyTextSelection::default();
        for id in 0..BODY_VIEW_MAX_ACTIVE_SELECTION_POINTERS as u64 {
            assert!(selection.begin_drag(PointerId::Touch(id)).is_some());
        }

        // The table is full, so this pointer gets no record. `PointerId::Custom`
        // means an application can always produce a seventeenth pointer, so the
        // property that has to hold is that such a pointer cannot touch the
        // selection at all.
        let turned_away = PointerId::Touch(BODY_VIEW_MAX_ACTIVE_SELECTION_POINTERS as u64);
        assert_eq!(selection.begin_drag(turned_away), None);
        assert_eq!(selection.begin_extend(turned_away), None);
        assert_eq!(selection.note_drag_started(turned_away), None);
        let projection =
            project_body(&BodySource::Html("<p>select this link</p>".to_owned()).sanitize());
        let document = BodySelectionDocument::from_projection(&projection);
        selection.note_drag_selection(&document, turned_away);
        assert!(
            selection.anchor.is_none() && selection.focus.is_none(),
            "a pointer refused by the capacity bound moved the selection"
        );

        // Having driven no selection, its click is an ordinary click — and that
        // answer must not depend on what other pointers are doing, which is why
        // it is checked again after the table has drained below capacity.
        assert!(!selection.consume_click_suppression(turned_away));
        assert!(selection.release_pointer(PointerId::Touch(0)));
        assert!(!selection.consume_click_suppression(turned_away));
    }

    #[test]
    fn shift_press_suppresses_the_link_click_it_belongs_to() {
        let projection =
            project_body(&BodySource::Html("<p>select this link</p>".to_owned()).sanitize());
        let document = BodySelectionDocument::from_projection(&projection);
        let pointer = PointerId::Touch(0);
        let mut selection = BodyTextSelection::default();
        assert_eq!(selection.begin_drag(pointer), Some(true));
        selection.set_caret(BodyTextPosition {
            block: 0,
            offset: 0,
        });
        assert!(selection.release_pointer(pointer));

        // Shift was held at press, so this gesture extends the range. Releasing
        // Shift before the button must not turn it back into a link
        // activation — the bit is latched, not re-read.
        assert_eq!(selection.begin_extend(pointer), Some(true));
        selection.extend_to(BodyTextPosition {
            block: 0,
            offset: "select".len(),
        });
        assert_eq!(selection.selected_text(&document), Some("select"));
        assert!(selection.consume_click_suppression(pointer));
        assert!(
            !selection.consume_click_suppression(pointer),
            "one press must suppress at most one click"
        );

        // The mirror case: a plain press that acquires Shift only before release
        // never extended anything, so it still activates.
        let mut plain = BodyTextSelection::default();
        assert_eq!(plain.begin_drag(pointer), Some(true));
        plain.set_caret(BodyTextPosition {
            block: 0,
            offset: 3,
        });
        assert!(!plain.consume_click_suppression(pointer));
    }

    #[test]
    fn a_shift_press_that_resolves_no_text_position_still_suppresses_its_click() {
        // The press observer admits a Shift press before hit-testing, so this is
        // the state a Shift press over a link leaves behind when no text
        // position resolves under it: nothing selected, but the click is still
        // the tail of a range gesture and must not activate the link.
        let mut selection = BodyTextSelection::default();
        let pointer = PointerId::Mouse;
        assert_eq!(selection.begin_extend(pointer), Some(true));
        assert!(
            selection.anchor.is_none() && selection.focus.is_none(),
            "admission alone must not move the selection"
        );
        assert!(selection.consume_click_suppression(pointer));
        assert!(
            !selection.consume_click_suppression(pointer),
            "one press must suppress at most one click"
        );
    }

    #[test]
    fn link_activation_accepts_primary_and_keyboard_but_not_secondary_or_middle() {
        use bevy::camera::NormalizedRenderTarget;
        use bevy::input::InputPlugin;
        use bevy::input_focus::{InputDispatchPlugin, InputFocusPlugin};
        use bevy::picking::backend::HitData;
        use bevy::picking::pointer::{Location, PointerButton, PointerId};
        use bevy::window::{PrimaryWindow, Window, WindowRef};
        use core::time::Duration;

        let mut app = App::new();
        app.add_plugins((
            InputPlugin,
            InputFocusPlugin,
            InputDispatchPlugin,
            CtkBodyViewPlugin,
        ))
        .init_resource::<SeenLinks>()
        .add_observer(record_link);
        let window = app
            .world_mut()
            .spawn((Window::default(), PrimaryWindow))
            .id();
        let body = BodySource::Html(
            r#"<p><a href="https://example.com">Read <strong>the report</strong></a>
               <a href="javascript:alert(1)">unsafe</a></p>"#
                .to_owned(),
        )
        .sanitize();
        let mut commands = app.world_mut().commands();
        let entities = spawn_body_view(&mut commands, CtkBodyViewProps::new(body, "Message body"));
        app.world_mut().flush();
        let link_runs = {
            let world = app.world_mut();
            let mut query = world.query_filtered::<Entity, With<BodyLink>>();
            query.iter(world).collect::<Vec<_>>()
        };
        assert_eq!(link_runs.len(), 2);
        let primary = link_runs
            .iter()
            .copied()
            .find(|entity| app.world().get::<TabIndex>(*entity).is_some())
            .unwrap();
        let continuation = link_runs
            .iter()
            .copied()
            .find(|entity| *entity != primary)
            .unwrap();
        assert!(app.world().get::<AccessibilityNode>(continuation).is_none());
        let accessible = app.world().get::<AccessibilityNode>(primary).unwrap();
        assert_eq!(accessible.role(), Role::Link);
        assert_eq!(accessible.label(), Some("Read the report"));
        assert!(accessible.supports_action(Action::Click));

        let location = Location {
            target: NormalizedRenderTarget::Window(
                WindowRef::Entity(window).normalize(None).unwrap(),
            ),
            position: Vec2::ZERO,
        };
        for button in [PointerButton::Secondary, PointerButton::Middle] {
            app.world_mut().trigger(Pointer::new(
                PointerId::Mouse,
                location.clone(),
                Click {
                    button,
                    hit: HitData::new(continuation, 0.0, None, None),
                    duration: Duration::ZERO,
                    count: 1,
                },
                continuation,
            ));
            app.update();
            assert!(app.world().resource::<SeenLinks>().0.is_empty());
        }
        app.world_mut().trigger(Pointer::new(
            PointerId::Mouse,
            location,
            Click {
                button: PointerButton::Primary,
                hit: HitData::new(continuation, 0.0, None, None),
                duration: Duration::ZERO,
                count: 1,
            },
            continuation,
        ));
        app.update();
        assert_eq!(app.world().resource::<InputFocus>().get(), Some(primary));
        app.world_mut()
            .insert_resource(InputFocus::from_entity(primary));
        app.world_mut().write_message(KeyboardInput {
            key_code: KeyCode::Enter,
            logical_key: Key::Enter,
            state: ButtonState::Pressed,
            text: None,
            repeat: false,
            window,
        });
        app.update();

        let seen = &app.world().resource::<SeenLinks>().0;
        assert_eq!(seen.len(), 2);
        assert!(seen.iter().all(|event| {
            event.href == "https://example.com" && event.body_view == entities.root
        }));
    }

    fn accessible_link_entities(world: &mut World) -> Vec<Entity> {
        let mut query = world.query::<(Entity, &AccessibilityNode)>();
        let mut entities = query
            .iter(world)
            .filter_map(|(entity, node)| (node.role() == Role::Link).then_some(entity))
            .collect::<Vec<_>>();
        entities.sort_by_key(|entity| entity.to_bits());
        entities
    }

    #[test]
    fn anchored_card_keeps_blocks_and_one_projection_wide_accessible_link() {
        use bevy::camera::NormalizedRenderTarget;
        use bevy::input::InputPlugin;
        use bevy::input_focus::{InputDispatchPlugin, InputFocusPlugin};
        use bevy::picking::backend::HitData;
        use bevy::picking::pointer::{Location, PointerButton, PointerId};
        use bevy::window::{PrimaryWindow, Window, WindowRef};
        use core::time::Duration;

        let body = BodySource::Html(ANCHORED_CARD.to_owned()).sanitize();
        let projection = project_body(&body);
        assert_eq!(projection.blocks.len(), 2);
        assert_eq!(projection.blocks[0].kind, ProjectedBlockKind::Heading(2));
        assert_eq!(projection.blocks[0].text(), "Quarterly results");
        assert_eq!(projection.blocks[1].kind, ProjectedBlockKind::Paragraph);
        assert_eq!(projection.blocks[1].text(), "Read the complete report.");
        let linked = projection
            .blocks
            .iter()
            .flat_map(|block| &block.spans)
            .collect::<Vec<_>>();
        assert_eq!(linked.len(), 3);
        let identity = linked[0].link_identity();
        assert!(identity.is_some());
        assert!(linked.iter().all(|span| span.link_identity() == identity));

        let mut app = App::new();
        app.add_plugins((
            InputPlugin,
            InputFocusPlugin,
            InputDispatchPlugin,
            CtkBodyViewPlugin,
        ))
        .init_resource::<SeenLinks>()
        .add_observer(record_link);
        let window = app
            .world_mut()
            .spawn((Window::default(), PrimaryWindow))
            .id();
        let mut commands = app.world_mut().commands();
        let entities = spawn_body_view(&mut commands, CtkBodyViewProps::new(body, "Message body"));
        app.world_mut().flush();

        let links = accessible_link_entities(app.world_mut());
        assert_eq!(links.len(), 1);
        let primary = links[0];
        assert_eq!(
            app.world()
                .get::<AccessibilityNode>(primary)
                .unwrap()
                .label(),
            Some("Quarterly results Read the complete report.")
        );
        assert!(app.world().get::<TabIndex>(primary).is_some());

        let link_runs = {
            let world = app.world_mut();
            let mut query = world.query::<(Entity, &BodyLink)>();
            query
                .iter(world)
                .map(|(entity, link)| {
                    assert_eq!(link.focus_entity, primary);
                    entity
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(link_runs.len(), 3);
        assert_eq!(
            link_runs
                .iter()
                .filter(|entity| app.world().get::<TabIndex>(**entity).is_some())
                .count(),
            1
        );

        let location = Location {
            target: NormalizedRenderTarget::Window(
                WindowRef::Entity(window).normalize(None).unwrap(),
            ),
            position: Vec2::ZERO,
        };
        for run in link_runs.iter().copied() {
            app.world_mut().trigger(Pointer::new(
                PointerId::Mouse,
                location.clone(),
                Click {
                    button: PointerButton::Primary,
                    hit: HitData::new(run, 0.0, None, None),
                    duration: Duration::ZERO,
                    count: 1,
                },
                run,
            ));
            app.update();
            assert_eq!(app.world().resource::<InputFocus>().get(), Some(primary));
        }
        app.world_mut()
            .insert_resource(InputFocus::from_entity(primary));
        app.world_mut().write_message(KeyboardInput {
            key_code: KeyCode::Enter,
            logical_key: Key::Enter,
            state: ButtonState::Pressed,
            text: None,
            repeat: false,
            window,
        });
        app.update();

        let seen = &app.world().resource::<SeenLinks>().0;
        assert_eq!(seen.len(), link_runs.len() + 1);
        assert!(seen.iter().all(|event| {
            event.href == "https://example.com/story" && event.body_view == entities.root
        }));
    }

    #[test]
    fn block_anchor_obeys_depth_and_content_budgets() {
        let body = BodySource::Html(ANCHORED_CARD.to_owned()).sanitize();
        let block_limited = project_body_with_budgets(&body, 1, BODY_VIEW_MAX_SPANS);
        assert_eq!(
            block_limited
                .blocks
                .iter()
                .filter(|block| block.kind != ProjectedBlockKind::Truncated)
                .count(),
            1
        );
        assert_eq!(
            block_limited.blocks.last().map(|block| block.kind),
            Some(ProjectedBlockKind::Truncated)
        );

        let span_limited = project_body_with_budgets(&body, BODY_VIEW_MAX_BLOCKS, 1);
        assert!(
            span_limited
                .blocks
                .iter()
                .filter(|block| block.kind != ProjectedBlockKind::Truncated)
                .flat_map(|block| &block.spans)
                .count()
                <= 1
        );
        assert_eq!(
            span_limited.blocks.last().map(|block| block.kind),
            Some(ProjectedBlockKind::Truncated)
        );

        let nested = BodySource::Html(format!(
            r#"<a href="https://example.com/deep">{}<h2>deep anchor sentinel</h2>{}</a>"#,
            "<div>".repeat(MAX_PROJECTION_DEPTH * 2),
            "</div>".repeat(MAX_PROJECTION_DEPTH * 2)
        ))
        .sanitize();
        let nested = project_body(&nested);
        assert!(nested
            .blocks
            .iter()
            .any(|block| block.text().contains("deep anchor sentinel")));
        let identities = nested
            .blocks
            .iter()
            .flat_map(|block| &block.spans)
            .filter_map(ProjectedSpan::link_identity)
            .collect::<Vec<_>>();
        assert!(!identities.is_empty());
        assert!(identities.iter().all(|identity| *identity == identities[0]));
    }

    #[test]
    fn adjacent_same_href_anchors_remain_two_accessible_links() {
        let body = BodySource::Html(
            r#"<p><a href="https://example.com/same">first</a><a href="https://example.com/same">second</a></p>"#
                .to_owned(),
        )
        .sanitize();
        let projection = project_body(&body);
        let linked = projection.blocks[0]
            .spans
            .iter()
            .filter(|span| span.link_target.is_some())
            .collect::<Vec<_>>();
        assert_eq!(linked.len(), 2);
        assert_eq!(linked[0].link_target, linked[1].link_target);
        assert_ne!(
            linked[0].anchor_occurrence, linked[1].anchor_occurrence,
            "URL interning must not erase source-anchor identity"
        );

        let mut app = App::new();
        app.add_plugins(CtkBodyViewPlugin);
        let mut commands = app.world_mut().commands();
        spawn_body_view(&mut commands, CtkBodyViewProps::new(body, "Message body"));
        app.world_mut().flush();
        let content = app
            .world_mut()
            .query_filtered::<Entity, With<BodyBlock>>()
            .single(app.world())
            .unwrap();
        let links = app
            .world()
            .get::<Children>(content)
            .unwrap()
            .iter()
            .filter(|entity| {
                app.world()
                    .get::<AccessibilityNode>(*entity)
                    .is_some_and(|node| node.role() == Role::Link)
            })
            .collect::<Vec<_>>();
        assert_eq!(links.len(), 2);
        assert_eq!(
            links
                .iter()
                .map(|entity| app
                    .world()
                    .get::<AccessibilityNode>(*entity)
                    .unwrap()
                    .label())
                .collect::<Vec<_>>(),
            [Some("first"), Some("second")]
        );
        assert!(links
            .iter()
            .all(|entity| app.world().get::<TabIndex>(*entity).is_some()));
    }

    #[test]
    fn chunked_anchor_has_one_complete_accessible_link() {
        let text = format!(
            "{} final anchor text",
            "x".repeat(BODY_VIEW_MAX_TEXT_RUN_BYTES + 128)
        );
        let body = BodySource::Html(format!(
            r#"<p><a href="https://example.com/long">{text}</a></p>"#
        ))
        .sanitize();
        let projection = project_body(&body);
        assert!(projection.blocks[0].spans.len() >= 2);
        assert!(projection.blocks[0]
            .spans
            .iter()
            .all(|span| span.anchor_occurrence == projection.blocks[0].spans[0].anchor_occurrence));

        let mut app = App::new();
        app.add_plugins(CtkBodyViewPlugin);
        let mut commands = app.world_mut().commands();
        spawn_body_view(&mut commands, CtkBodyViewProps::new(body, "Message body"));
        app.world_mut().flush();
        let links = accessible_link_entities(app.world_mut());
        assert_eq!(links.len(), 1);
        let link = links[0];
        assert_eq!(
            app.world().get::<AccessibilityNode>(link).unwrap().label(),
            Some(text.as_str())
        );
        assert!(app.world().get::<TabIndex>(link).is_some());
    }

    #[test]
    fn accessibility_click_uses_the_link_activation_path() {
        let mut app = App::new();
        app.add_plugins(CtkBodyViewPlugin)
            .init_resource::<SeenLinks>()
            .add_observer(record_link);
        let mut commands = app.world_mut().commands();
        let entities = spawn_body_view(
            &mut commands,
            CtkBodyViewProps::new(
                BodySource::Html(r#"<p><a href="https://example.com/a11y">open</a></p>"#.into())
                    .sanitize(),
                "Message body",
            ),
        );
        app.world_mut().flush();
        let link = app
            .world_mut()
            .query_filtered::<Entity, With<BodyLink>>()
            .single(app.world())
            .unwrap();
        app.world_mut()
            .write_message(AccessibilityActionRequest(accesskit::ActionRequest {
                action: Action::Click,
                target_tree: accesskit::TreeId::ROOT,
                target_node: accesskit::NodeId(link.to_bits()),
                data: None,
            }));
        app.update();
        assert_eq!(
            app.world().resource::<SeenLinks>().0,
            [LinkActivated {
                body_view: entities.root,
                href: "https://example.com/a11y".to_owned(),
            }]
        );
    }

    #[test]
    fn list_accessibility_groups_items_with_zero_based_positions() {
        let mut app = App::new();
        app.add_plugins(CtkBodyViewPlugin);
        let mut commands = app.world_mut().commands();
        spawn_body_view(
            &mut commands,
            CtkBodyViewProps::new(
                BodySource::Html("<ol><li>one</li><li>two</li></ol>".into()).sanitize(),
                "Message body",
            ),
        );
        app.world_mut().flush();

        let world = app.world_mut();
        let mut nodes = world.query::<&AccessibilityNode>();
        let roles = nodes
            .iter(world)
            .filter(|node| matches!(node.role(), Role::List | Role::ListItem))
            .map(|node| (node.role(), node.position_in_set(), node.size_of_set()))
            .collect::<Vec<_>>();
        assert!(roles.contains(&(Role::List, None, Some(2))));
        assert!(roles.contains(&(Role::ListItem, Some(0), Some(2))));
        assert!(roles.contains(&(Role::ListItem, Some(1), Some(2))));
    }

    #[test]
    fn nested_list_after_a_paragraph_stays_under_its_owning_list_item() {
        let mut app = App::new();
        app.add_plugins(CtkBodyViewPlugin);
        let mut commands = app.world_mut().commands();
        spawn_body_view(
            &mut commands,
            CtkBodyViewProps::new(
                BodySource::Html(
                    "<ul><li><p>owner paragraph</p><ul><li>nested</li></ul></li></ul>".into(),
                )
                .sanitize(),
                "Nested list",
            ),
        );
        app.world_mut().flush();

        let relations = accessible_relations(&mut app);
        let lists = relations
            .iter()
            .filter(|(_, role, _)| *role == Role::List)
            .collect::<Vec<_>>();
        assert_eq!(lists.len(), 2);
        assert!(lists.iter().any(|(_, _, parent)| {
            parent.is_some_and(|parent| {
                relations
                    .iter()
                    .any(|(entity, role, _)| *entity == parent && *role == Role::ListItem)
            })
        }));
    }

    #[test]
    fn quoted_list_keeps_list_item_directly_below_list() {
        let mut app = App::new();
        app.add_plugins(CtkBodyViewPlugin);
        let mut commands = app.world_mut().commands();
        let entities = spawn_body_view(
            &mut commands,
            CtkBodyViewProps::new(
                BodySource::Html(
                    "<ul><li><blockquote><p>quoted item</p></blockquote></li></ul>".into(),
                )
                .sanitize(),
                "Quoted list",
            ),
        );
        app.world_mut().flush();

        let relations = accessible_relations(&mut app);
        let list_item = relations
            .iter()
            .find(|(_, role, _)| *role == Role::ListItem)
            .expect("list item exists");
        assert!(list_item.2.is_some_and(|parent| {
            relations
                .iter()
                .any(|(entity, role, _)| *entity == parent && *role == Role::List)
        }));
        let quote = relations
            .iter()
            .find(|(_, role, _)| *role == Role::Blockquote)
            .expect("blockquote exists");
        assert_eq!(
            accessible_ancestor_roles(app.world(), quote.0, entities.document),
            [
                Role::Blockquote,
                Role::GenericContainer,
                Role::ListItem,
                Role::List,
                Role::Document,
            ]
        );
    }

    fn accessible_relations(app: &mut App) -> Vec<(Entity, Role, Option<Entity>)> {
        let world = app.world_mut();
        let mut nodes = world.query::<(Entity, &AccessibilityNode, Option<&ChildOf>)>();
        nodes
            .iter(world)
            .map(|(entity, node, parent)| (entity, node.role(), parent.map(ChildOf::parent)))
            .collect()
    }

    fn accessible_ancestor_roles(world: &World, entity: Entity, document: Entity) -> Vec<Role> {
        let mut roles = Vec::new();
        let mut current = entity;
        loop {
            let node = world.get::<AccessibilityNode>(current).unwrap_or_else(|| {
                panic!("entity {current:?} breaks the accessible ancestry chain")
            });
            roles.push(node.role());
            if current == document {
                return roles;
            }
            current = world
                .get::<ChildOf>(current)
                .unwrap_or_else(|| {
                    panic!("entity {current:?} is not physically descended from the document")
                })
                .parent();
        }
    }

    fn audit_projected_accessible_ancestry(
        world: &mut World,
        projection_id: ProjectionId,
        document: Entity,
    ) -> Result<Vec<Entity>, String> {
        let mut inventory = world.query::<(Entity, &BodyAccessibleNode)>();
        let entities = inventory
            .iter(world)
            .filter_map(|(entity, marker)| (marker.0 == projection_id).then_some(entity))
            .collect::<Vec<_>>();
        if entities.is_empty() {
            return Err("projection accessibility inventory is empty".to_owned());
        }
        for entity in entities.iter().copied() {
            let mut current = entity;
            for _ in 0..=BODY_VIEW_MAX_ENTITIES {
                if world.get::<AccessibilityNode>(current).is_none() {
                    return Err(format!(
                        "inventoried entity {current:?} has no AccessibilityNode"
                    ));
                }
                if current == document {
                    break;
                }
                let Some(parent) = world.get::<ChildOf>(current) else {
                    return Err(format!(
                        "inventoried entity {entity:?} is detached before the document"
                    ));
                };
                current = parent.parent();
            }
            if current != document {
                return Err(format!(
                    "inventoried entity {entity:?} did not reach the document"
                ));
            }
        }
        Ok(entities)
    }

    #[test]
    fn every_projected_accessible_node_has_an_unbroken_path_to_the_document() {
        let body = BodySource::Html(
            r#"
                <ul><li>
                    <h2>List heading <a href="https://example.com/list">linked</a></h2>
                    <p>List detail</p>
                    <blockquote><p>Quoted detail</p></blockquote>
                    <table><tr><td><p>Cell detail</p></td></tr></table>
                </li></ul>
                <a href="https://example.com/card">
                    <h3>Anchored heading</h3><p>Anchored detail</p>
                </a>
            "#
            .to_owned(),
        )
        .sanitize();
        let mut app = App::new();
        app.add_plugins(CtkBodyViewPlugin);
        let mut commands = app.world_mut().commands();
        let entities =
            spawn_body_view(&mut commands, CtkBodyViewProps::new(body, "Ancestry audit"));
        app.world_mut().flush();

        let projection_id = app
            .world()
            .get::<CtkBodyView>(entities.root)
            .unwrap()
            .projection_id;
        let accessible_entities =
            audit_projected_accessible_ancestry(app.world_mut(), projection_id, entities.document)
                .unwrap();
        for entity in accessible_entities {
            let roles = accessible_ancestor_roles(app.world(), entity, entities.document);
            assert_eq!(roles.last(), Some(&Role::Document));
        }

        let marker = {
            let world = app.world_mut();
            let mut query = world.query::<(Entity, &AccessibilityNode)>();
            query
                .iter(world)
                .find_map(|(entity, node)| (node.role() == Role::ListMarker).then_some(entity))
                .expect("list marker exists")
        };
        assert_eq!(
            accessible_ancestor_roles(app.world(), marker, entities.document),
            [
                Role::ListMarker,
                Role::GenericContainer,
                Role::ListItem,
                Role::List,
                Role::Document,
            ]
        );

        let list_link = {
            let world = app.world_mut();
            let mut query = world.query::<(Entity, &AccessibilityNode)>();
            query
                .iter(world)
                .find_map(|(entity, node)| {
                    (node.role() == Role::Link && node.label() == Some("linked")).then_some(entity)
                })
                .expect("list link exists")
        };
        assert_eq!(
            accessible_ancestor_roles(app.world(), list_link, entities.document),
            [
                Role::Link,
                Role::Heading,
                Role::GenericContainer,
                Role::ListItem,
                Role::List,
                Role::Document,
            ]
        );
    }

    #[test]
    fn ancestry_audit_rejects_a_list_row_missing_its_accessibility_node() {
        let body = BodySource::Html("<ul><li><p>content row</p></li></ul>".to_owned()).sanitize();
        let mut app = App::new();
        app.add_plugins(CtkBodyViewPlugin);
        let mut commands = app.world_mut().commands();
        let entities = spawn_body_view(
            &mut commands,
            CtkBodyViewProps::new(body, "Broken ancestry audit"),
        );
        app.world_mut().flush();
        let projection_id = app
            .world()
            .get::<CtkBodyView>(entities.root)
            .unwrap()
            .projection_id;
        let row = {
            let world = app.world_mut();
            let mut query =
                world.query::<(Entity, &AccessibilityNode, &BodyAccessibleNode, &Children)>();
            query
                .iter(world)
                .find_map(|(entity, node, marker, children)| {
                    (marker.0 == projection_id
                        && node.role() == Role::GenericContainer
                        && !children.is_empty())
                    .then_some(entity)
                })
                .expect("list content row exists")
        };
        app.world_mut()
            .entity_mut(row)
            .remove::<AccessibilityNode>();

        let error =
            audit_projected_accessible_ancestry(app.world_mut(), projection_id, entities.document)
                .expect_err("removing the list row AccessibilityNode must fail the audit");
        assert!(error.contains("has no AccessibilityNode"), "{error}");
    }

    #[test]
    fn engine_request_falls_back() {
        let mut app = App::new();
        app.add_plugins(CtkBodyViewPlugin);
        let view = app
            .world_mut()
            .spawn(CtkBodyView {
                requested_arm: RenderArm::Text,
                effective_arm: RenderArm::Text,
                remote_refs: RemoteRefs::default(),
                projection_id: ProjectionId::next(),
                link_targets: Vec::new(),
            })
            .id();
        app.world_mut().trigger(SetRenderArm {
            body_view: view,
            arm: RenderArm::Engine,
        });
        app.update();

        let state = app.world().get::<CtkBodyView>(view).unwrap();
        assert_eq!(state.requested_arm(), RenderArm::Engine);
        assert_eq!(state.effective_arm(), RenderArm::Text);
    }

    #[test]
    fn document_copy_is_one_tab_stop_and_blocks_remain_pointer_copy_targets() {
        let body = BodySource::Html(
            "<p>first</p><p>second</p><p><a href=\"https://example.com\">linked</a></p>".to_owned(),
        )
        .sanitize();
        let mut app = App::new();
        app.add_plugins(CtkBodyViewPlugin);
        let mut commands = app.world_mut().commands();
        let entities =
            spawn_body_view(&mut commands, CtkBodyViewProps::new(body, "Copy traversal"));
        app.world_mut().flush();

        assert!(app.world().get::<TabIndex>(entities.document).is_some());
        assert_eq!(
            app.world()
                .get::<BodyDocumentCopy>(entities.document)
                .unwrap()
                .text,
            "first\nsecond\nlinked"
        );
        let world = app.world_mut();
        let mut blocks = world.query::<(Entity, &BodyBlock, &Pickable)>();
        let blocks = blocks.iter(world).collect::<Vec<_>>();
        assert_eq!(blocks.len(), 3);
        assert!(blocks
            .iter()
            .all(|(entity, _, _)| world.get::<TabIndex>(*entity).is_none()));

        let mut tab_stops = world.query::<(Entity, &TabIndex)>();
        assert_eq!(tab_stops.iter(world).count(), 2);
    }

    #[test]
    fn non_empty_selection_precedes_the_existing_document_copy_target() {
        use bevy::input::InputPlugin;
        use bevy::input_focus::{InputDispatchPlugin, InputFocusPlugin};
        use bevy::window::{PrimaryWindow, Window};

        let mut app = App::new();
        app.add_plugins((
            InputPlugin,
            InputFocusPlugin,
            InputDispatchPlugin,
            CtkBodyViewPlugin,
        ));
        let window = app
            .world_mut()
            .spawn((Window::default(), PrimaryWindow))
            .id();
        let mut commands = app.world_mut().commands();
        let entities = spawn_body_view(
            &mut commands,
            CtkBodyViewProps::new(
                BodySource::Html("<p>alpha</p><p>beta</p>".to_owned()).sanitize(),
                "Selection copy",
            ),
        );
        app.world_mut().flush();
        {
            let world = app.world_mut();
            let runtime = world
                .get::<BodyProjectionRuntime>(entities.root)
                .unwrap()
                .clone();
            let mut root = world.entity_mut(entities.root);
            let mut selection = root.get_mut::<BodyTextSelection>().unwrap();
            selection.anchor = Some(BodyTextPosition {
                block: 0,
                offset: 2,
            });
            selection.focus = Some(BodyTextPosition {
                block: 1,
                offset: 2,
            });
            assert_eq!(selection.selected_text(&runtime.document), Some("pha\nbe"));
        }
        app.world_mut()
            .insert_resource(InputFocus::from_entity(entities.document));
        app.world_mut()
            .resource_mut::<ButtonInput<Key>>()
            .press(Key::Control);
        app.world_mut().write_message(KeyboardInput {
            key_code: KeyCode::KeyC,
            logical_key: Key::Character("c".into()),
            state: ButtonState::Pressed,
            text: Some("c".into()),
            repeat: false,
            window,
        });
        app.update();

        let mut read = app.world_mut().resource_mut::<Clipboard>().fetch_text();
        assert_eq!(
            read.poll_result()
                .expect("in-process clipboard is ready")
                .expect("selection copy succeeds"),
            "pha\nbe"
        );
    }

    #[test]
    fn block_copy_uses_the_real_clipboard_resource() {
        let mut app = App::new();
        app.add_plugins(CtkBodyViewPlugin);
        let block = BodyBlock {
            view: Entity::PLACEHOLDER,
            text: "complete paragraph".to_owned(),
            selection_block: None,
        };
        copy_text(
            &block.text,
            &mut app.world_mut().resource_mut::<Clipboard>(),
        );
        let mut read = app.world_mut().resource_mut::<Clipboard>().fetch_text();
        let value = read
            .poll_result()
            .expect("in-process clipboard is ready")
            .expect("clipboard read succeeds");
        assert_eq!(value, "complete paragraph");
    }
}
