//! Primitive-level damage between two frames.
//!
//! `iced_tiny_skia`'s own `Layer::damage` pairs primitives by index, so
//! inserting or removing one (a caret blinking off) misaligns everything
//! after it and damages all of it; it also treats every live primitive as
//! changed. Here each primitive list is aligned by its common prefix and
//! suffix and only the differing middle is damaged. That is exact for draw
//! order: every item outside the middle is unchanged and keeps its relative
//! order, so pixels outside the middle items' bounds cannot change.
//!
//! Paragraphs are compared through a snapshot taken when the frame was
//! drawn. A layer only holds weak paragraph references, and a widget that
//! re-lays out a paragraph (`Arc::make_mut`) detaches the previous frame's
//! reference even when nothing visible changed, so comparing weak
//! references damages every label on every rebuild.

use crate::damage::DamageRect;
use iced_core::text::Alignment;
use iced_core::{Color, Point, Rectangle, Size, Transformation, alignment};
use iced_graphics::text::Text;
use iced_graphics::text::cosmic_text::AttrsList;
use iced_tiny_skia::Layer;

/// Glyph ink can leave a paragraph's measured bounds (overhang, hinting,
/// antialiasing); quads antialias one pixel out.
const TEXT_MARGIN: f32 = 2.0;
const QUAD_MARGIN: f32 = 1.0;

/// What a drawn frame looked like, for diffing against the next one.
pub(crate) struct Snapshot {
    layers: Vec<Layer>,
    /// Per layer, per text item.
    text: Vec<Vec<TextItem>>,
}

struct TextItem {
    clip: Rectangle,
    transformation: Transformation,
    texts: Vec<TextKey>,
    /// Logical damage bounds of the item.
    bounds: Vec<Rectangle>,
}

#[derive(PartialEq)]
enum TextKey {
    Paragraph {
        position: Point,
        color: Color,
        clip: Rectangle,
        transformation: Transformation,
        min_bounds: Size,
        align: (Alignment, alignment::Vertical),
        /// `None` if the paragraph was already gone: never equal.
        content: Option<Content>,
    },
    Other(Text),
}

#[derive(PartialEq)]
struct Content {
    lines: Vec<(String, AttrsList)>,
    metrics: (f32, f32),
    size: (Option<f32>, Option<f32>),
}

fn key(text: &Text) -> TextKey {
    match text {
        Text::Paragraph {
            paragraph,
            position,
            color,
            clip_bounds,
            transformation,
        } => TextKey::Paragraph {
            position: *position,
            color: *color,
            clip: *clip_bounds,
            transformation: *transformation,
            min_bounds: paragraph.min_bounds,
            align: (paragraph.align_x, paragraph.align_y),
            content: paragraph.upgrade().map(|p| {
                let buffer = p.buffer();
                let metrics = buffer.metrics();
                Content {
                    lines: buffer
                        .lines
                        .iter()
                        .map(|l| (l.text().to_owned(), l.attrs_list().clone()))
                        .collect(),
                    metrics: (metrics.font_size, metrics.line_height),
                    size: buffer.size(),
                }
            }),
        },
        other => TextKey::Other(other.clone()),
    }
}

fn key_eq(a: &TextKey, b: &TextKey) -> bool {
    match (a, b) {
        (TextKey::Paragraph { content: None, .. }, _)
        | (_, TextKey::Paragraph { content: None, .. }) => false,
        _ => a == b,
    }
}

impl Snapshot {
    pub(crate) fn new(layers: &[Layer]) -> Self {
        // `layer::Item` is not exported, so items are read through its public
        // methods: the slice, the clip bounds and the transformation.
        let text = layers
            .iter()
            .map(|layer| {
                layer
                    .text
                    .iter()
                    .map(|item| {
                        let t = item.transformation();
                        TextItem {
                            clip: item.clip_bounds(),
                            transformation: t,
                            texts: item.as_slice().iter().map(key).collect(),
                            bounds: item
                                .as_slice()
                                .iter()
                                .filter_map(Text::visible_bounds)
                                .map(|r| r.expand(TEXT_MARGIN) * t)
                                .collect(),
                        }
                    })
                    .collect()
            })
            .collect();
        Self {
            layers: layers.to_vec(),
            text,
        }
    }
}

/// A quad's damage bounds: its border is drawn inside the bounds, but a
/// wide stroke and antialiasing at fractional scale reach a little further.
fn quad_bounds(quad: &iced_core::renderer::Quad) -> Rectangle {
    quad.bounds.expand(QUAD_MARGIN + quad.border.width.max(0.0))
}

/// The area a quad's shadow can cover, as `iced_tiny_skia` computes it.
fn shadow_bounds(quad: &iced_core::renderer::Quad) -> Option<Rectangle> {
    let shadow = quad.shadow;
    (shadow.color.a > 0.0).then(|| {
        Rectangle {
            x: quad.bounds.x + shadow.offset.x - shadow.blur_radius,
            y: quad.bounds.y + shadow.offset.y - shadow.blur_radius,
            width: quad.bounds.width + shadow.blur_radius * 2.0,
            height: quad.bounds.height + shadow.blur_radius * 2.0,
        }
        .expand(QUAD_MARGIN)
    })
}

/// Grows the damage to cover what the renderer paints without the clip mask.
///
/// `iced_tiny_skia` draws a quad's shadow pixmap unmasked whenever the quad
/// body meets the damage, and skips the mask for a paragraph whose measured
/// bounds lie inside it, so glyph ink overhanging those bounds also escapes.
/// Those pixels are rewritten, so they have to be inside the damage: the
/// caller is told what changed, and a format swap covers them exactly once.
pub(crate) fn expand_unclipped(
    rects: &mut Vec<Rectangle>,
    previous: &Snapshot,
    current: &Snapshot,
) {
    // Growing the damage can reach another shadow or text run, so this
    // repeats until nothing new is added (bounded: each pass either adds a
    // rectangle that then contains its candidate, or stops).
    for _ in 0..8 {
        let before = rects.len();
        for snapshot in [previous, current] {
            for (index, layer) in snapshot.layers.iter().enumerate() {
                for (quad, _) in &layer.quads {
                    let Some(shadow) = shadow_bounds(quad) else {
                        continue;
                    };
                    let body = quad_bounds(quad);
                    if touches(rects, &body) && !covered(rects, &shadow) {
                        rects.push(shadow);
                    }
                }
                for item in &snapshot.text[index] {
                    for ink in &item.bounds {
                        if touches(rects, ink) && !covered(rects, ink) {
                            rects.push(*ink);
                        }
                    }
                }
            }
        }
        if rects.len() == before {
            return;
        }
    }
}

fn touches(rects: &[Rectangle], other: &Rectangle) -> bool {
    rects.iter().any(|r| r.intersects(other))
}

fn covered(rects: &[Rectangle], other: &Rectangle) -> bool {
    rects.iter().any(|r| {
        r.x <= other.x
            && r.y <= other.y
            && r.x + r.width >= other.x + other.width
            && r.y + r.height >= other.y + other.height
    })
}

/// Logical rectangles that changed between `previous` and `current`.
pub(crate) fn damage(previous: &Snapshot, current: &Snapshot) -> Vec<Rectangle> {
    let mut out = Vec::new();
    let (pl, cl) = (&previous.layers, &current.layers);
    for i in 0..pl.len().max(cl.len()) {
        match (pl.get(i), cl.get(i)) {
            (Some(a), Some(b)) => layer(a, b, &previous.text[i], &current.text[i], &mut out),
            (Some(only), None) | (None, Some(only)) => {
                if !is_empty(only) {
                    out.push(only.bounds);
                }
            }
            (None, None) => {}
        }
    }
    out
}

fn is_empty(layer: &Layer) -> bool {
    layer.quads.is_empty()
        && layer.text.is_empty()
        && layer.primitives.is_empty()
        && layer.images.is_empty()
}

fn layer(a: &Layer, b: &Layer, at: &[TextItem], bt: &[TextItem], out: &mut Vec<Rectangle>) {
    if a.bounds != b.bounds {
        for l in [a, b] {
            if !is_empty(l) {
                out.push(l.bounds);
            }
        }
        return;
    }
    let clip = |r: Rectangle| r.intersection(&b.bounds);
    middle(
        &a.quads,
        &b.quads,
        |(quad, _)| clip(quad_bounds(quad)).into_iter().collect(),
        |x, y| x == y,
        out,
    );
    middle(
        at,
        bt,
        |item| item.bounds.iter().copied().filter_map(clip).collect(),
        |x, y| {
            x.clip == y.clip
                && x.transformation == y.transformation
                && slices_eq(&x.texts, &y.texts, key_eq)
        },
        out,
    );
    middle(
        &a.primitives,
        &b.primitives,
        |item| {
            let t = item.transformation();
            item.as_slice()
                .iter()
                .map(|p| p.visible_bounds().expand(QUAD_MARGIN) * t)
                .filter_map(clip)
                .collect()
        },
        |x, y| {
            x.clip_bounds() == y.clip_bounds()
                && x.transformation() == y.transformation()
                && slices_eq(x.as_slice(), y.as_slice(), |p, q| p == q)
        },
        out,
    );
    middle(
        &a.images,
        &b.images,
        |image| {
            clip(image.bounds().expand(QUAD_MARGIN))
                .into_iter()
                .collect()
        },
        |x, y| x == y,
        out,
    );
}

fn slices_eq<T>(a: &[T], b: &[T], eq: impl Fn(&T, &T) -> bool) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| eq(x, y))
}

/// Pushes the bounds of every item not in a longest common subsequence of
/// `a` and `b`. Items in the subsequence are unchanged and keep their
/// relative order, so only the others' old and new bounds can change pixels.
fn middle<T>(
    a: &[T],
    b: &[T],
    bounds: impl Fn(&T) -> Vec<Rectangle>,
    eq: impl Fn(&T, &T) -> bool,
    out: &mut Vec<Rectangle>,
) {
    let (a_changed, b_changed) = changed_items(a, b, eq);
    for i in a_changed {
        out.extend(bounds(&a[i]));
    }
    for i in b_changed {
        out.extend(bounds(&b[i]));
    }
}

/// Longest-common-subsequence cells above which the middle is treated as
/// wholly changed.
const LCS_LIMIT: usize = 1 << 14;

/// Indices of `a` and `b` outside a longest common subsequence. The common
/// prefix and suffix are matched first; the rest by dynamic programming when
/// small enough, otherwise all of it counts as changed.
pub(crate) fn changed_items<T>(
    a: &[T],
    b: &[T],
    eq: impl Fn(&T, &T) -> bool,
) -> (Vec<usize>, Vec<usize>) {
    let prefix = a.iter().zip(b).take_while(|(x, y)| eq(x, y)).count();
    let max_suffix = a.len().min(b.len()) - prefix;
    let suffix = a
        .iter()
        .rev()
        .zip(b.iter().rev())
        .take(max_suffix)
        .take_while(|(x, y)| eq(x, y))
        .count();
    let (am, bm) = (&a[prefix..a.len() - suffix], &b[prefix..b.len() - suffix]);
    let (n, m) = (am.len(), bm.len());
    if n == 0 || m == 0 || n * m > LCS_LIMIT {
        return (
            (prefix..prefix + n).collect(),
            (prefix..prefix + m).collect(),
        );
    }
    // lcs[i][j]: length of the LCS of am[i..] and bm[j..].
    let mut lcs = vec![0u16; (n + 1) * (m + 1)];
    let at = |i: usize, j: usize| i * (m + 1) + j;
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[at(i, j)] = if eq(&am[i], &bm[j]) {
                lcs[at(i + 1, j + 1)] + 1
            } else {
                lcs[at(i + 1, j)].max(lcs[at(i, j + 1)])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let (mut a_changed, mut b_changed) = (Vec::new(), Vec::new());
    while i < n && j < m {
        if eq(&am[i], &bm[j]) && lcs[at(i, j)] == lcs[at(i + 1, j + 1)] + 1 {
            i += 1;
            j += 1;
        } else if lcs[at(i + 1, j)] >= lcs[at(i, j + 1)] {
            a_changed.push(prefix + i);
            i += 1;
        } else {
            b_changed.push(prefix + j);
            j += 1;
        }
    }
    a_changed.extend(prefix + i..prefix + n);
    b_changed.extend(prefix + j..prefix + m);
    (a_changed, b_changed)
}

/// Physical rectangles, merged where their union wastes at most
/// `MERGE_WASTE` pixels, then made disjoint by the caller. Past `MAX_RECTS`
/// everything collapses to one bounding box.
pub(crate) fn coalesce(mut rects: Vec<DamageRect>) -> Vec<DamageRect> {
    const MERGE_WASTE: u64 = 4096;
    const MAX_RECTS: usize = 24;
    let mut merged = true;
    while merged {
        merged = false;
        'outer: for i in 0..rects.len() {
            for j in i + 1..rects.len() {
                let u = union(&rects[i], &rects[j]);
                let parts = rects[i].area() + rects[j].area();
                if u.area() <= parts + MERGE_WASTE {
                    rects.swap_remove(j);
                    rects[i] = u;
                    merged = true;
                    break 'outer;
                }
            }
        }
    }
    if rects.len() > MAX_RECTS {
        let bbox = rects.iter().skip(1).fold(rects[0], |a, r| union(&a, r));
        rects = vec![bbox];
    }
    rects
}

pub(crate) fn union(a: &DamageRect, b: &DamageRect) -> DamageRect {
    let x = a.x.min(b.x);
    let y = a.y.min(b.y);
    DamageRect {
        x,
        y,
        width: (a.x + a.width).max(b.x + b.width) - x,
        height: (a.y + a.height).max(b.y + b.height) - y,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: u32, y: u32, width: u32, height: u32) -> DamageRect {
        DamageRect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn changed_items_isolate_insertions_and_separate_edits() {
        let eq = |a: &u8, b: &u8| a == b;
        let none: Vec<usize> = vec![];
        assert_eq!(
            changed_items(&[1, 2, 3], &[1, 9, 2, 3], eq),
            (none.clone(), vec![1])
        );
        assert_eq!(
            changed_items(&[1, 9, 2, 3], &[1, 2, 3], eq),
            (vec![1], none.clone())
        );
        assert_eq!(
            changed_items(&[1, 2, 3], &[1, 2, 3], eq),
            (none.clone(), none.clone())
        );
        assert_eq!(
            changed_items(&[1, 2], &[3, 4], eq),
            (vec![0, 1], vec![0, 1])
        );
        // Repeated items: the suffix never overlaps the prefix.
        assert_eq!(
            changed_items(&[1, 1], &[1, 1, 1], eq),
            (none.clone(), vec![2])
        );
        assert_eq!(changed_items::<u8>(&[], &[5], eq), (none.clone(), vec![0]));
        // Two separate edits leave the unchanged items between them alone
        // (a text field and a status line with tab labels in between).
        assert_eq!(
            changed_items(&[1, 2, 3, 4, 5, 6], &[1, 7, 3, 4, 5, 8], eq),
            (vec![1, 5], vec![1, 5])
        );
        // A moved item: only it is damaged, at both positions.
        assert_eq!(
            changed_items(&[1, 2, 3, 4], &[2, 3, 4, 1], eq),
            (vec![0], vec![3])
        );
    }

    #[test]
    fn coalesce_keeps_distant_rects_and_merges_near_ones() {
        let rects = coalesce(vec![r(0, 0, 2, 40), r(1000, 0, 2, 40)]);
        assert_eq!(rects.len(), 2);
        let rects = coalesce(vec![r(0, 0, 10, 10), r(12, 0, 10, 10)]);
        assert_eq!(rects, vec![r(0, 0, 22, 10)]);
        let many: Vec<_> = (0..30).map(|i| r(i * 10_000, 0, 1, 1)).collect();
        assert_eq!(coalesce(many), vec![r(0, 0, 290_001, 1)]);
    }
}
