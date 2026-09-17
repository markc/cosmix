//! Minimal software painting into an ARGB8888 (B, G, R, A byte order)
//! buffer. Opaque colours only.

use crate::font::Font;
use cosmix_wl_app::Rect;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

pub struct Canvas<'a> {
    pub pixels: &'a mut [u8],
    pub width: u32,
    pub height: u32,
    pub stride: u32,
}

impl Canvas<'_> {
    fn clip(&self, r: Rect) -> Option<Rect> {
        r.intersect(&Rect::new(0, 0, self.width as i32, self.height as i32))
    }

    pub fn fill(&mut self, r: Rect, c: Rgb) {
        let Some(r) = self.clip(r) else {
            return;
        };
        let px = [c.2, c.1, c.0, 0xff];
        for y in r.y..r.bottom() {
            let row = y as usize * self.stride as usize;
            let line = &mut self.pixels[row + r.x as usize * 4..row + r.right() as usize * 4];
            for p in line.chunks_exact_mut(4) {
                p.copy_from_slice(&px);
            }
        }
    }

    /// Draw `ch` in the cell at (`x`, `y`), clipped to the cell.
    pub fn glyph(&mut self, font: &mut Font, x: i32, y: i32, ch: char, fg: Rgb) {
        let cell = Rect::new(x, y, font.cell_w as i32, font.cell_h as i32);
        let baseline = font.baseline;
        let Some(clip) = self.clip(cell) else {
            return;
        };
        let Some(image) = font.glyph(ch) else {
            return;
        };
        let p = image.placement;
        for gy in 0..p.height as i32 {
            let dy = y + baseline - p.top + gy;
            if dy < clip.y || dy >= clip.bottom() {
                continue;
            }
            for gx in 0..p.width as i32 {
                let dx = x + p.left + gx;
                if dx < clip.x || dx >= clip.right() {
                    continue;
                }
                let a = u32::from(image.data[(gy as u32 * p.width + gx as u32) as usize]);
                if a == 0 {
                    continue;
                }
                let o = dy as usize * self.stride as usize + dx as usize * 4;
                let px = &mut self.pixels[o..o + 4];
                for (i, c) in [fg.2, fg.1, fg.0].into_iter().enumerate() {
                    px[i] = ((u32::from(c) * a + u32::from(px[i]) * (255 - a)) / 255) as u8;
                }
            }
        }
    }

    pub fn text(&mut self, font: &mut Font, x: i32, y: i32, s: &str, fg: Rgb) {
        let w = font.cell_w as i32;
        for (i, ch) in s.chars().enumerate() {
            self.glyph(font, x + i as i32 * w, y, ch, fg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_clips_and_writes_bgra() {
        let mut buf = vec![0u8; 4 * 4 * 3];
        let mut c = Canvas {
            pixels: &mut buf,
            width: 4,
            height: 3,
            stride: 16,
        };
        c.fill(Rect::new(3, 2, 10, 10), Rgb(1, 2, 3));
        assert_eq!(&buf[(2 * 16 + 12)..], &[3, 2, 1, 255]);
        assert!(buf[..2 * 16 + 12].iter().all(|b| *b == 0));
    }
}

/// Writes an ARGB8888 buffer as a binary PPM, for a measurement that needs
/// the pixels the client actually committed (a screen capture of a scaled
/// output would show the compositor's resampling instead). `WL_DEMO_DUMP`
/// names the file; the demo writes its first committed frame and stops.
pub fn dump_ppm(
    path: &std::path::Path,
    pixels: &[u8],
    width: u32,
    height: u32,
) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut out = Vec::with_capacity(pixels.len() / 4 * 3 + 32);
    out.extend_from_slice(format!("P6\n{width} {height}\n255\n").as_bytes());
    for pixel in pixels.chunks_exact(4) {
        out.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
    }
    std::fs::File::create(path)?.write_all(&out)
}
