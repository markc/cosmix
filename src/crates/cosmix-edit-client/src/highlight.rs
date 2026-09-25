//! Highlighting state for one buffer view (ced E1 plan §4.3). Stage S
//! freezes the API; Stage E1e implements it.
//!
//! Contracts:
//! - lsh languages: `cosmix_lsh::cache` checkpoints every 1024 lines over a
//!   `LineSource` built on `Text::chunk_at`; a cold seek far past the last
//!   checkpoint advances by at most [`SliceBudget`] per frame — `spans`
//!   returns `None` for a line not yet reached (drawn plain).
//! - Mix (`mix`, `scene`, `mix-data`; feature `mix`): whole-buffer relex off
//!   the UI thread through `cosmix_lib_mix::lexer::highlight`, requested
//!   150 ms after the last delta (the caller debounces and runs the worker);
//!   buffers over [`MIX_MAX_BYTES`] stay plain.
//! - Results carry a [`ResultTag`]. A result for another epoch, buffer,
//!   language or config is discarded. A result for an OLDER `gen` is used only
//!   for lines before the first line touched since that gen; lines from there
//!   on draw plain until a fresh result lands (a quote or comment delimiter
//!   must never leave stale tokens).
//!
//! Implementation notes (E1e):
//! - Interior mutability: the widget draws from `&Highlight`, so the caches
//!   live in a `RefCell` and [`Highlight::with_spans`] is the `&self` twin of
//!   [`Highlight::spans`].
//! - Beyond the 1024-line lsh checkpoints, a state is kept every
//!   [`FINE_INTERVAL`] lines the view has parsed, so scrolling back a line
//!   re-parses at most that many lines instead of up to 1023.
//! - [`Highlight::behind`] holds while the most recent `spans` call returned
//!   `None` (drawn top to bottom, so a frame ends behind iff a visible line
//!   was not reached).

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use cosmix_edit_core::text::Text;
use cosmix_lsh::cache::{Cache, INTERVAL};
use cosmix_lsh::defs::HighlightKind;
use cosmix_lsh::highlighter::{Highlighter, HighlighterState, Span};
use cosmix_lsh::runtime::Language;

use crate::model::line_of;
use crate::types::{DeltaKind, ViewDelta};

/// Mix buffers larger than this are not highlighted.
pub const MIX_MAX_BYTES: usize = 2 * 1024 * 1024;

/// Colour classes; ced maps each to a design token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HlClass {
    Plain,
    Comment,
    Keyword,
    String,
    Number,
    Constant,
    Type,
    Function,
    Variable,
    Operator,
    Punctuation,
    Meta,
    Inserted,
    Deleted,
    Heading,
    Link,
    Invalid,
}

/// Identity of an asynchronous result (highlight or lint).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ResultTag {
    pub epoch: String,
    pub buffer: String,
    pub view_gen: u64,
    pub language: String,
    /// Hash of the settings the result depends on.
    pub cfg: u64,
}

/// Work allowed in one frame for cold lsh seeks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SliceBudget {
    pub max_lines: usize,
}

impl Default for SliceBudget {
    /// Roughly 2 ms of lsh work.
    fn default() -> Self {
        Self { max_lines: 2000 }
    }
}


/// A highlighter state is kept every this many lines the view parses.
pub const FINE_INTERVAL: usize = 32;

/// Per-line span cache size before it is trimmed around the latest line.
const MAX_CACHED_LINES: usize = 4096;
/// Fine states kept before they are trimmed around the latest line.
const MAX_FINE_STATES: usize = 1024;
/// Touched-line log entries kept for stale-result decisions.
const MAX_TOUCHED: usize = 4096;

type LineSpans = Vec<(Range<usize>, HlClass)>;

const EMPTY: &[(Range<usize>, HlClass)] = &[];

#[derive(Clone, Copy)]
enum Engine {
    Plain,
    Lsh(&'static Language),
    /// Mix lexer; `data` = the `for_data` flavour (scene, mix-data).
    Mix { data: bool },
}

struct MixState {
    view_gen: u64,
    spans: LineSpans,
    /// Lines before this one are still valid for `spans`.
    valid_lines: usize,
}

#[derive(Default)]
struct Inner {
    cache: Cache,
    /// State at the start of line `k` (1-based), every `FINE_INTERVAL` lines.
    fine: BTreeMap<usize, HighlighterState>,
    lines: HashMap<usize, LineSpans>,
    /// The first line a `spans` call could not reach this frame.
    pending: Option<usize>,

    mix: Option<MixState>,
    /// The tag of the latest `mix_request` (its identity fields bind results).
    current: Option<ResultTag>,
    requested: Option<u64>,
    /// `(view_gen, first touched line)` per delta, oldest first.
    touched: VecDeque<(u64, usize)>,
    /// Every delta with a gen above this is in `touched`.
    touched_floor: u64,
}

pub struct Highlight {
    engine: Engine,
    language: String,
    inner: RefCell<Inner>,
}

impl std::fmt::Debug for Highlight {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let engine = match self.engine {
            Engine::Plain => "plain".to_string(),
            Engine::Lsh(l) => format!("lsh:{}", l.id),
            Engine::Mix { data } => format!("mix{}", if data { ":data" } else { "" }),
        };
        f.debug_struct("Highlight").field("language", &self.language).field("engine", &engine).finish()
    }
}

/// The lsh language for an editd language id (`cosmix_edit_core::lang`).
pub fn lsh_language(editd_language: &str) -> Option<&'static Language> {
    let id = match editd_language {
        "git_commit" => "git-commit",
        "shell" => "shellscript",
        other => other,
    };
    cosmix_lsh::language_by_id(id).or_else(|| cosmix_lsh::language_by_name(editd_language))
}

/// Run the Mix lexer for a relex request (the caller's worker thread).
#[cfg(feature = "mix")]
pub fn run_mix(language: &str, source: &str) -> Vec<(Range<usize>, cosmix_mix::lexer::TokenClass)> {
    let flavor = if is_mix_data(language) { cosmix_mix::lexer::MixFlavor::Data } else { cosmix_mix::lexer::MixFlavor::Script };
    cosmix_mix::lexer::highlight(source, flavor)
}

fn is_mix_family(language: &str) -> bool {
    matches!(language, "mix" | "scene" | "mix-data")
}

fn is_mix_data(language: &str) -> bool {
    matches!(language, "scene" | "mix-data")
}

impl Highlight {
    /// Pick the highlighter from editd's `language` first, lsh
    /// `FILE_ASSOCIATIONS` for `path` second, else plain.
    pub fn for_language(editd_language: &str, path: Option<&Path>) -> Self {
        let engine = if is_mix_family(editd_language) {
            if cfg!(feature = "mix") { Engine::Mix { data: is_mix_data(editd_language) } } else { Engine::Plain }
        } else {
            lsh_language(editd_language)
                .filter(|_| editd_language != "text")
                .or_else(|| path.and_then(cosmix_lsh::language_for_path))
                .map_or(Engine::Plain, Engine::Lsh)
        };
        Self { engine, language: editd_language.to_string(), inner: RefCell::new(Inner::default()) }
    }

    /// The editd language this was built for.
    pub fn language(&self) -> &str {
        &self.language
    }

    /// True for `mix`, `scene` and `mix-data` buffers highlighted by the Mix
    /// lexer (the caller runs the relex worker only for these).
    pub fn is_mix(&self) -> bool {
        matches!(self.engine, Engine::Mix { .. })
    }

    /// Invalidate from the first line the delta touched.
    pub fn apply_delta(&mut self, text: &Text, d: &ViewDelta) {
        let first = if d.kind == DeltaKind::Resync {
            1
        } else {
            match d.edits.iter().map(|e| e.offset).min() {
                // Bytes before the smallest step offset are untouched by every step.
                Some(p) => line_of(text, p.min(text.len())),
                None => return,
            }
        };
        let inner = self.inner.get_mut();
        inner.cache.invalidate_from(first);
        let _ = inner.fine.split_off(&(first + 1));
        inner.lines.retain(|&l, _| l < first);
        inner.pending = None;
        if let Some(m) = &mut inner.mix {
            m.valid_lines = m.valid_lines.min(first);
        }
        inner.touched.push_back((d.view_gen, first));
        while inner.touched.len() > MAX_TOUCHED {
            if let Some((g, _)) = inner.touched.pop_front() {
                inner.touched_floor = g;
            }
        }
    }

    /// Spans of 1-based `line` (byte ranges in the text), or `None` while a
    /// cold seek has not reached it yet.
    pub fn spans(&mut self, text: &Text, line: usize, budget: &mut SliceBudget) -> Option<&[(std::ops::Range<usize>, HlClass)]> {
        let engine = self.engine;
        let inner = self.inner.get_mut();
        if !ensure(engine, inner, text, line, budget) {
            return None;
        }
        Some(inner.lines.get(&line).map_or(EMPTY, Vec::as_slice))
    }

    /// [`Self::spans`] for a shared reference (the widget draws from
    /// `&Highlight`): `f` receives the same answer.
    pub fn with_spans<R>(
        &self,
        text: &Text,
        line: usize,
        budget: &mut SliceBudget,
        f: impl FnOnce(Option<&[(Range<usize>, HlClass)]>) -> R,
    ) -> R {
        let mut inner = self.inner.borrow_mut();
        if !ensure(self.engine, &mut inner, text, line, budget) {
            return f(None);
        }
        f(Some(inner.lines.get(&line).map_or(EMPTY, Vec::as_slice)))
    }

    /// True while a cold seek is behind the visible range (the widget keeps
    /// requesting frames only while this holds).
    pub fn behind(&self) -> bool {
        self.inner.borrow().pending.is_some()
    }

    /// For Mix buffers: the relex to run now (tag + the text at that gen),
    /// once the 150 ms debounce has passed. `None` when nothing is due.
    pub fn mix_request(&mut self, text: &Text, tag: ResultTag) -> Option<(ResultTag, Arc<str>)> {
        if !self.is_mix() || tag.language != self.language {
            return None;
        }
        let inner = self.inner.get_mut();
        let same_identity = inner.current.as_ref().is_some_and(|c| same_source(c, &tag));
        if !same_identity {
            // Another epoch / buffer / config: nothing held so far applies.
            inner.mix = None;
            inner.requested = None;
            inner.lines.clear();
        }
        inner.current = Some(tag.clone());
        if text.len() > MIX_MAX_BYTES {
            inner.mix = None;
            inner.lines.clear();
            return None;
        }
        let fresh = inner.mix.as_ref().is_some_and(|m| m.view_gen == tag.view_gen && m.valid_lines == usize::MAX);
        if fresh || inner.requested == Some(tag.view_gen) {
            return None;
        }
        inner.requested = Some(tag.view_gen);
        let mut s = String::with_capacity(text.len());
        text.read(0..text.len(), &mut s);
        Some((tag, Arc::from(s)))
    }

    /// A relex result from the worker (see the tag rules in the module docs).
    #[cfg(feature = "mix")]
    pub fn mix_result(&mut self, tag: ResultTag, spans: Vec<(std::ops::Range<usize>, cosmix_mix::lexer::TokenClass)>) {
        let inner = self.inner.get_mut();
        if !inner.current.as_ref().is_some_and(|c| same_source(c, &tag)) {
            return;
        }
        if inner.requested == Some(tag.view_gen) {
            inner.requested = None;
        }
        if inner.mix.as_ref().is_some_and(|m| m.view_gen > tag.view_gen) {
            return; // an older result never replaces a newer one
        }
        if tag.view_gen < inner.touched_floor {
            return; // the touched log no longer reaches back to it
        }
        let valid_lines = inner.touched.iter().filter(|(g, _)| *g > tag.view_gen).map(|&(_, l)| l).min().unwrap_or(usize::MAX);
        let spans = spans.into_iter().filter_map(|(r, c)| Some((r, token_class(c)?))).collect();
        inner.mix = Some(MixState { view_gen: tag.view_gen, spans, valid_lines });
        inner.lines.clear();
    }
}

fn same_source(a: &ResultTag, b: &ResultTag) -> bool {
    a.epoch == b.epoch && a.buffer == b.buffer && a.language == b.language && a.cfg == b.cfg
}

/// Make `inner.lines[line]` answerable; false while a cold seek is behind.
fn ensure(engine: Engine, inner: &mut Inner, text: &Text, line: usize, budget: &mut SliceBudget) -> bool {
    let ok = match engine {
        _ if line == 0 => true,
        Engine::Plain => true,
        Engine::Mix { .. } => {
            mix_line(inner, text, line);
            true
        }
        Engine::Lsh(language) => inner.lines.contains_key(&line) || line > text.line_count() || lsh_line(language, inner, text, line, budget),
    };
    if ok {
        inner.pending = None;
    } else {
        inner.pending = Some(line);
    }
    ok
}

fn mix_line(inner: &mut Inner, text: &Text, line: usize) {
    if inner.lines.contains_key(&line) {
        return;
    }
    let Some(m) = &inner.mix else { return };
    if line >= m.valid_lines {
        return;
    }
    let Some(lr) = text.line_range(line) else { return };
    let from = m.spans.partition_point(|(r, _)| r.end <= lr.start);
    let spans: LineSpans = m.spans[from..]
        .iter()
        .take_while(|(r, _)| r.start < lr.end)
        .filter(|(_, c)| *c != HlClass::Plain)
        .map(|(r, c)| (r.start.max(lr.start)..r.end.min(lr.end), *c))
        .filter(|(r, _)| !r.is_empty())
        .collect();
    insert_line(inner, line, spans);
}

struct Src<'a>(&'a Text);

impl cosmix_lsh::LineSource for Src<'_> {
    fn read_forward(&self, offset: usize) -> &[u8] {
        self.0.chunk_at(offset)
    }
}

fn lsh_line(language: &'static Language, inner: &mut Inner, text: &Text, line: usize, budget: &mut SliceBudget) -> bool {
    // The best known start at or before `line`: a fine state, else the lsh
    // cache's frontier / last checkpoint (`reach`), else the checkpoint of
    // the interval containing `line` (checkpoints are gap-free up to reach).
    let fine = inner.fine.range(..=line).next_back().map(|(&k, _)| k);
    let reach = inner.cache.reach();
    let ckpt = if reach <= line { reach } else { (line - 1) / INTERVAL * INTERVAL + 1 };
    let start = fine.unwrap_or(0).max(ckpt);
    let src = Src(text);
    let mut h = Highlighter::new(&src, language);
    if let Some(k) = fine.filter(|&k| k >= ckpt) {
        h.restore(&inner.fine[&k]);
    }
    let distance = line - start;
    if distance > budget.max_lines {
        inner.cache.advance(&mut h, line, budget.max_lines);
        budget.max_lines = 0;
        return false;
    }
    budget.max_lines -= distance.min(budget.max_lines);
    let mut out: Vec<Span> = Vec::new();
    for l in start..=line {
        inner.cache.parse_line(&mut h, l, &mut out);
        if l + 64 >= line {
            let spans = lsh_spans(text, l, &out);
            insert_line(inner, l, spans);
        }
        let next = h.line();
        if next % FINE_INTERVAL == 1 {
            inner.fine.insert(next, h.snapshot());
        }
    }
    if inner.fine.len() > MAX_FINE_STATES {
        let keep = line.saturating_sub(MAX_FINE_STATES / 2 * FINE_INTERVAL)..line + MAX_FINE_STATES / 2 * FINE_INTERVAL;
        inner.fine.retain(|k, _| keep.contains(k));
    }
    true
}

fn insert_line(inner: &mut Inner, line: usize, spans: LineSpans) {
    if inner.lines.len() >= MAX_CACHED_LINES {
        let keep = line.saturating_sub(MAX_CACHED_LINES / 4)..line + MAX_CACHED_LINES / 4;
        inner.lines.retain(|k, _| keep.contains(k));
    }
    inner.lines.insert(line, spans);
}

fn lsh_spans(text: &Text, line: usize, out: &[Span]) -> LineSpans {
    let end = text.line_range(line).map_or(0, |r| r.end);
    let mut spans = Vec::with_capacity(out.len());
    for (i, s) in out.iter().enumerate() {
        let stop = out.get(i + 1).map_or(end, |n| n.start).min(end);
        let class = lsh_class(s.kind);
        if class != HlClass::Plain && s.start < stop {
            spans.push((s.start..stop, class));
        }
    }
    spans
}

/// lsh scopes onto the colour classes.
pub fn lsh_class(kind: HighlightKind) -> HlClass {
    use HighlightKind as K;
    match kind {
        K::Other => HlClass::Plain,
        K::Comment => HlClass::Comment,
        K::Method => HlClass::Function,
        K::String => HlClass::String,
        K::Variable => HlClass::Variable,
        K::ConstantLanguage => HlClass::Constant,
        K::ConstantNumeric => HlClass::Number,
        K::KeywordControl | K::KeywordOther => HlClass::Keyword,
        K::KeywordPreprocessor | K::MetaHeader | K::StorageAnnotation | K::MarkupChanged => HlClass::Meta,
        K::MarkupBold | K::MarkupHeading => HlClass::Heading,
        K::MarkupItalic => HlClass::Variable,
        K::MarkupDeleted => HlClass::Deleted,
        K::MarkupInserted => HlClass::Inserted,
        K::MarkupLink => HlClass::Link,
        K::MarkupList => HlClass::Punctuation,
        K::MarkupStrikethrough => HlClass::Comment,
        K::StorageType => HlClass::Type,
    }
}

/// Mix lexer classes onto the colour classes (`None` = plain).
#[cfg(feature = "mix")]
pub fn token_class(c: cosmix_mix::lexer::TokenClass) -> Option<HlClass> {
    use cosmix_mix::lexer::TokenClass as T;
    Some(match c {
        T::Keyword => HlClass::Keyword,
        T::Identifier => return None,
        T::Variable => HlClass::Variable,
        T::String => HlClass::String,
        T::Number => HlClass::Number,
        T::Constant => HlClass::Constant,
        T::Comment => HlClass::Comment,
        T::Operator => HlClass::Operator,
        T::Punctuation => HlClass::Punctuation,
        T::Error => HlClass::Invalid,
    })
}

#[cfg(test)]
mod tests;
