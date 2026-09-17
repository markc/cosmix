//! Primitive-level damage between two frames' layers.
//!
//! `iced_tiny_skia`'s own `Layer::damage` pairs primitives by index, so
//! inserting or removing one (a caret blinking off) misaligns everything
//! after it and damages all of it; it also treats every live primitive as
//! changed, and compares paragraphs by metrics only. Here each primitive list
//! is aligned by its common prefix and suffix and only the differing middle
//! is damaged. That is exact for draw order: every item outside the middle
//! is unchanged and keeps its relative order, so pixels outside the middle
//! items' bounds cannot change.

use crate::damage::DamageRect;
use iced_core::Rectangle;
use iced_graphics::text::Text;
use iced_tiny_skia::Layer;

/// Glyph ink can leave a paragraph's measured bounds (overhang, hinting,
/// antialiasing); quads antialias one pixel out.
const TEXT_MARGIN: f32 = 2.0;
const QUAD_MARGIN: f32 = 1.0;

/// Logical rectangles that changed between `previous` and `current`.
pub(crate) fn layers(previous: &[Layer], current: &[Layer]) -> Vec<Rectangle> {
    let mut out = Vec::new();
    for i in 0..previous.len().max(current.len()) {
        match (previous.get(i), current.get(i)) {
            (Some(a), Some(b)) => layer(a, b, &mut out),
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

fn layer(a: &Layer, b: &Layer, out: &mut Vec<Rectangle>) {
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
        |(quad, _)| clip(quad.bounds.expand(QUAD_MARGIN)).into_iter().collect(),
        |x, y| x == y,
        out,
    );
    // `layer::Item` is not exported, so items are handled through its
    // public methods: the slice, the clip bounds (infinite for a live item)
    // and the transformation.
    middle(
        &a.text,
        &b.text,
        |item| {
            let t = item.transformation();
            item.as_slice()
                .iter()
                .filter_map(Text::visible_bounds)
                .map(|r| r.expand(TEXT_MARGIN) * t)
                .filter_map(clip)
                .collect()
        },
        |x, y| {
            x.clip_bounds() == y.clip_bounds()
                && x.transformation() == y.transformation()
                && slices_eq(x.as_slice(), y.as_slice(), text_eq)
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

/// Pushes the bounds of every item between the common prefix and suffix.
fn middle<T>(
    a: &[T],
    b: &[T],
    bounds: impl Fn(&T) -> Vec<Rectangle>,
    eq: impl Fn(&T, &T) -> bool,
    out: &mut Vec<Rectangle>,
) {
    let (lo, a_hi, b_hi) = changed_span(a, b, eq);
    for item in a[lo..a_hi].iter().chain(&b[lo..b_hi]) {
        out.extend(bounds(item));
    }
}

/// `(prefix, a_end, b_end)`: `a[prefix..a_end]` and `b[prefix..b_end]` are
/// the items not shared by the common prefix and suffix.
pub(crate) fn changed_span<T>(
    a: &[T],
    b: &[T],
    eq: impl Fn(&T, &T) -> bool,
) -> (usize, usize, usize) {
    let prefix = a.iter().zip(b).take_while(|(x, y)| eq(x, y)).count();
    let max_suffix = a.len().min(b.len()) - prefix;
    let suffix = a
        .iter()
        .rev()
        .zip(b.iter().rev())
        .take(max_suffix)
        .take_while(|(x, y)| eq(x, y))
        .count();
    (prefix, a.len() - suffix, b.len() - suffix)
}

fn text_eq(a: &Text, b: &Text) -> bool {
    if a != b {
        return false;
    }
    // Paragraph equality compares layout metrics only; the text itself can
    // change at identical metrics (one monospace glyph for another).
    match (a, b) {
        (Text::Paragraph { paragraph: pa, .. }, Text::Paragraph { paragraph: pb, .. }) => {
            match (pa.upgrade(), pb.upgrade()) {
                (Some(pa), Some(pb)) => {
                    let (la, lb) = (&pa.buffer().lines, &pb.buffer().lines);
                    la.len() == lb.len() && la.iter().zip(lb).all(|(x, y)| x.text() == y.text())
                }
                _ => false,
            }
        }
        _ => true,
    }
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
    fn span_isolates_an_inserted_item() {
        let eq = |a: &u8, b: &u8| a == b;
        assert_eq!(changed_span(&[1, 2, 3], &[1, 9, 2, 3], eq), (1, 1, 2));
        assert_eq!(changed_span(&[1, 9, 2, 3], &[1, 2, 3], eq), (1, 2, 1));
        assert_eq!(changed_span(&[1, 2, 3], &[1, 2, 3], eq), (3, 3, 3));
        assert_eq!(changed_span(&[1, 2], &[3, 4], eq), (0, 2, 2));
        // Repeated items: the suffix never overlaps the prefix.
        assert_eq!(changed_span(&[1, 1], &[1, 1, 1], eq), (2, 2, 3));
        assert_eq!(changed_span::<u8>(&[], &[5], eq), (0, 0, 1));
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
