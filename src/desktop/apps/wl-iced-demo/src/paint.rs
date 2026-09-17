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

/// Writes an ARGB8888 buffer as an 8-bit RGB PNG, for a measurement that
/// needs the pixels the client actually committed: a screen capture of a
/// scaled output is resampled by the compositor (and this compositor's
/// screencopy hands back a logical-size image), so it cannot answer whether
/// the client drew at physical size. `WL_DEMO_DUMP` names the file; the demo
/// writes its first committed frame and stops.
///
/// The PNG is written by hand rather than with an image crate: the demo
/// should not carry an encoder dependency for a diagnostic. Rows are stored
/// uncompressed (deflate stored blocks), so the file is large but valid.
pub fn dump_png(
    path: &std::path::Path,
    pixels: &[u8],
    width: u32,
    height: u32,
) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut raw = Vec::with_capacity((width as usize * 3 + 1) * height as usize);
    for row in pixels
        .chunks_exact(width as usize * 4)
        .take(height as usize)
    {
        raw.push(0); // filter: none
        for pixel in row.chunks_exact(4) {
            raw.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
        }
    }
    let mut png = Vec::with_capacity(raw.len() + 4096);
    png.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit, truecolour
    chunk(&mut png, b"IHDR", &ihdr);
    chunk(&mut png, b"IDAT", &zlib_stored(&raw));
    chunk(&mut png, b"IEND", &[]);
    std::fs::File::create(path)?.write_all(&png)
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// A zlib stream of deflate "stored" blocks: no compression, always valid.
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let mut chunks = data.chunks(0xffff).peekable();
    if data.is_empty() {
        out.extend_from_slice(&[0x01, 0, 0, 0xff, 0xff]);
    }
    while let Some(block) = chunks.next() {
        out.push(u8::from(chunks.peek().is_none()));
        let len = block.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(block);
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for byte in data {
        a = (a + u32::from(*byte)) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_dump_is_a_valid_truecolour_image() {
        let dir = std::env::temp_dir().join(format!("wl-demo-png-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dump.png");
        // Two rows of two ARGB pixels: red, green / blue, white.
        let pixels: Vec<u8> = vec![
            0, 0, 255, 255, 0, 255, 0, 255, 255, 0, 0, 255, 255, 255, 255, 255,
        ];
        dump_png(&path, &pixels, 2, 2).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(
            &bytes[..8],
            &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]
        );
        assert_eq!(&bytes[8..16], &[0, 0, 0, 13, b'I', b'H', b'D', b'R']);
        assert_eq!(&bytes[16..24], &[0, 0, 0, 2, 0, 0, 0, 2]);
        assert_eq!(&bytes[24..29], &[8, 2, 0, 0, 0]);
        assert!(bytes.ends_with(&[b'I', b'E', b'N', b'D', 0xae, 0x42, 0x60, 0x82]));
        // The stored deflate stream carries each row with a filter byte, in
        // R, G, B order.
        let idat = bytes.windows(4).position(|w| w == b"IDAT").unwrap() + 4;
        assert_eq!(&bytes[idat..idat + 2], &[0x78, 0x01]);
        let raw = &bytes[idat + 7..idat + 7 + 14];
        assert_eq!(raw, &[0, 255, 0, 0, 0, 255, 0, 0, 0, 0, 255, 255, 255, 255]);
        assert_eq!(crc32(b"IEND"), 0xae42_6082);
        assert_eq!(adler32(b"abc"), 0x024d_0127);
        std::fs::remove_dir_all(&dir).ok();
    }

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
