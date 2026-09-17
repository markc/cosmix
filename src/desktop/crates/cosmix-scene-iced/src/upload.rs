//! Damage rectangles to texture writes: clip, merge, copy.

use crate::surface::Rect;

/// Above this many rectangles one bounding write is cheaper than many small
/// queue writes.
pub const MAX_RECTS: usize = 8;
/// Merge everything into the bounding box when it wastes at most this
/// fraction of extra area.
const BOUNDING_SLACK: f64 = 1.25;

/// One `write_texture` call: straight-alpha RGBA8 for `rect`, row-packed.
#[derive(Clone, Debug, PartialEq)]
pub struct UploadOp {
    pub rect: Rect,
    pub bytes: Vec<u8>,
}

/// Clips to the surface and merges overlapping or adjacent rectangles.
pub fn plan(damage: &[Rect], width: u32, height: u32) -> Vec<Rect> {
    let mut rects: Vec<Rect> = damage
        .iter()
        .filter_map(|rect| rect.clip(width, height))
        .collect();
    let mut merged = rects.len() <= 4 * MAX_RECTS;
    while merged {
        merged = false;
        'outer: for i in 0..rects.len() {
            for j in i + 1..rects.len() {
                if rects[i].touches(&rects[j]) {
                    let other = rects.swap_remove(j);
                    rects[i] = rects[i].union(&other);
                    merged = true;
                    break 'outer;
                }
            }
        }
    }
    if rects.len() > 1 {
        let bounds = rects[1..].iter().fold(rects[0], |acc, r| acc.union(r));
        let area: u64 = rects.iter().map(Rect::area).sum();
        if rects.len() > MAX_RECTS || bounds.area() as f64 <= area as f64 * BOUNDING_SLACK {
            rects = vec![bounds];
        }
    }
    rects.sort_by_key(|r| (r.y, r.x));
    rects
}

pub fn byte_len(rects: &[Rect]) -> u64 {
    rects.iter().map(|r| r.area() * 4).sum()
}

/// Copies `rect` out of a premultiplied buffer, converting to the straight
/// alpha that Bevy's UI pipeline blends with.
pub fn extract(buffer: &[u8], stride: u32, rect: Rect) -> UploadOp {
    let mut bytes = Vec::with_capacity((rect.area() * 4) as usize);
    for y in rect.y..rect.bottom() {
        let start = (y * stride + rect.x * 4) as usize;
        let row = &buffer[start..start + rect.w as usize * 4];
        for pixel in row.chunks_exact(4) {
            bytes.extend_from_slice(&unpremultiply(pixel));
        }
    }
    UploadOp { rect, bytes }
}

fn unpremultiply(pixel: &[u8]) -> [u8; 4] {
    let a = pixel[3];
    match a {
        255 => [pixel[0], pixel[1], pixel[2], a],
        0 => [0, 0, 0, 0],
        _ => {
            let a16 = a as u16;
            let un = |c: u8| ((c as u16 * 255 + a16 / 2) / a16).min(255) as u8;
            [un(pixel[0]), un(pixel[1]), un(pixel[2]), a]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clips_and_drops_offscreen() {
        let rects = plan(
            &[Rect::new(90, 90, 20, 20), Rect::new(200, 0, 5, 5)],
            100,
            100,
        );
        assert_eq!(rects, vec![Rect::new(90, 90, 10, 10)]);
        assert_eq!(byte_len(&rects), 400);
        assert!(plan(&[Rect::new(0, 0, 0, 10)], 100, 100).is_empty());
    }

    #[test]
    fn merges_overlapping_and_adjacent_but_not_distant() {
        let rects = plan(
            &[
                Rect::new(0, 0, 10, 10),
                Rect::new(10, 0, 10, 10),
                Rect::new(5, 5, 10, 10),
            ],
            100,
            100,
        );
        assert_eq!(rects, vec![Rect::new(0, 0, 20, 15)]);
        // Two far-apart carets stay two writes: their bounding box is mostly waste.
        let apart = [Rect::new(0, 0, 2, 16), Rect::new(90, 80, 2, 16)];
        assert_eq!(plan(&apart, 100, 100), apart.to_vec());
        assert_eq!(byte_len(&apart), 2 * 2 * 16 * 4);
    }

    #[test]
    fn many_rects_collapse_to_bounds() {
        let damage: Vec<_> = (0..MAX_RECTS as u32 + 1)
            .map(|i| Rect::new(i * 10, i * 10, 2, 2))
            .collect();
        assert_eq!(plan(&damage, 200, 200), vec![Rect::new(0, 0, 82, 82)]);
    }

    #[test]
    fn extract_reads_the_rect_and_unpremultiplies() {
        let (w, h) = (4u32, 3u32);
        let mut buffer = vec![0u8; (w * h * 4) as usize];
        let at = |x: u32, y: u32| ((y * w + x) * 4) as usize;
        buffer[at(1, 1)..at(1, 1) + 4].copy_from_slice(&[10, 20, 30, 255]);
        buffer[at(2, 1)..at(2, 1) + 4].copy_from_slice(&[50, 25, 0, 128]);
        let op = extract(&buffer, w * 4, Rect::new(1, 1, 2, 1));
        assert_eq!(op.bytes, vec![10, 20, 30, 255, 100, 50, 0, 128]);
    }
}
