use iced_core::Rectangle;

/// Byte layout of the caller's buffer. Both are premultiplied alpha.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// wl_shm `ARGB8888`/`XRGB8888`: native-endian `0xAARRGGBB`, stored as
    /// B, G, R, A bytes on little-endian hosts. tiny-skia draws this directly.
    Argb8888,
    /// R, G, B, A bytes, as for `wgpu::TextureFormat::Rgba8Unorm(Srgb)`.
    Rgba8,
}

/// A damaged region in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DamageRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl DamageRect {
    pub fn area(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }

    pub fn contains(&self, x: u32, y: u32) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.width && y < self.y + self.height
    }

    /// Whether this rectangle lies inside `outer` (both physical).
    pub fn is_within(&self, outer: &DamageRect) -> bool {
        self.x >= outer.x
            && self.y >= outer.y
            && self.x + self.width <= outer.x + outer.width
            && self.y + self.height <= outer.y + outer.height
    }

    /// The smallest whole-pixel rectangle covering `logical * scale`,
    /// clipped to `width` x `height`.
    pub fn from_logical(logical: Rectangle, scale: f32, width: u32, height: u32) -> Option<Self> {
        let x0 = (logical.x * scale).floor().max(0.0);
        let y0 = (logical.y * scale).floor().max(0.0);
        let x1 = ((logical.x + logical.width) * scale)
            .ceil()
            .min(width as f32);
        let y1 = ((logical.y + logical.height) * scale)
            .ceil()
            .min(height as f32);
        if !(x1 > x0 && y1 > y0) {
            return None;
        }
        Some(Self {
            x: x0 as u32,
            y: y0 as u32,
            width: (x1 - x0) as u32,
            height: (y1 - y0) as u32,
        })
    }

    pub(crate) fn to_logical(self, scale: f32) -> Rectangle {
        Rectangle {
            x: self.x as f32 / scale,
            y: self.y as f32 / scale,
            width: self.width as f32 / scale,
            height: self.height as f32 / scale,
        }
    }
}

/// Swaps the R and B channels of every pixel in `rect`; rows are
/// `row_pixels` wide.
pub(crate) fn swap_red_blue(buffer: &mut [u8], row_pixels: u32, rect: DamageRect) {
    let stride = row_pixels as usize * 4;
    for row in rect.y..rect.y + rect.height {
        let start = row as usize * stride + rect.x as usize * 4;
        let end = start + rect.width as usize * 4;
        for pixel in buffer[start..end].chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logical_rect_rounds_outward_and_clips() {
        let rect = DamageRect::from_logical(
            Rectangle::new((0.3, 1.1).into(), (10.0, 2.0).into()),
            2.5,
            20,
            100,
        )
        .unwrap();
        assert_eq!(
            rect,
            DamageRect {
                x: 0,
                y: 2,
                width: 20,
                height: 6
            }
        );
        assert!(
            DamageRect::from_logical(
                Rectangle::new((50.0, 0.0).into(), (1.0, 1.0).into()),
                1.0,
                20,
                20
            )
            .is_none()
        );
    }

    #[test]
    fn swap_touches_only_the_rect() {
        let mut buffer: Vec<u8> = (0..4 * 4).flat_map(|_| [1, 2, 3, 4]).collect();
        swap_red_blue(
            &mut buffer,
            2,
            DamageRect {
                x: 1,
                y: 1,
                width: 1,
                height: 1,
            },
        );
        assert_eq!(&buffer[..12], &[1, 2, 3, 4, 1, 2, 3, 4, 1, 2, 3, 4]);
        assert_eq!(&buffer[12..16], &[3, 2, 1, 4]);
    }
}
