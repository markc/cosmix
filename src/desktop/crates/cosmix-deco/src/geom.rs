//! Minimal geometry and colour types.
//!
//! These mirror the small subset of `bevy_math` / `bevy_color` the theme
//! engine needs, so the crate stays dependency-free. cosmix-comp converts at
//! the boundary (`Rect` → `bevy::math::Rect`, `Srgba` → `bevy::color::Color`).
//! All values are **logical pixels**; multiply by the output scale factor at
//! render time.

/// A 2D point/size in logical pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Vec2 {
    pub x: f32,
    pub y: f32,
}

pub const fn vec2(x: f32, y: f32) -> Vec2 {
    Vec2 { x, y }
}

/// Axis-aligned rectangle, origin at top-left, y-down (surface coordinates —
/// the same convention Wayland surfaces and `SurfaceLayout` use).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

pub const fn rect(x: f32, y: f32, w: f32, h: f32) -> Rect {
    Rect { x, y, w, h }
}

impl Rect {
    pub fn contains(&self, p: Vec2) -> bool {
        p.x >= self.x && p.x < self.x + self.w && p.y >= self.y && p.y < self.y + self.h
    }

    pub fn center(&self) -> Vec2 {
        vec2(self.x + self.w / 2.0, self.y + self.h / 2.0)
    }

    /// Grow by `d` on every side (negative shrinks).
    pub fn inflate(&self, d: f32) -> Rect {
        rect(self.x - d, self.y - d, self.w + 2.0 * d, self.h + 2.0 * d)
    }
}

/// Non-linear sRGB colour with straight alpha, components in `0.0..=1.0`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Srgba {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Srgba {
    pub const TRANSPARENT: Srgba = Srgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 };

    pub const fn new(r: f32, g: f32, b: f32, a: f32) -> Srgba {
        Srgba { r, g, b, a }
    }

    /// `0xRRGGBB` with full alpha — the form colour specs are quoted in.
    pub const fn hex(rgb: u32) -> Srgba {
        Srgba {
            r: ((rgb >> 16) & 0xff) as f32 / 255.0,
            g: ((rgb >> 8) & 0xff) as f32 / 255.0,
            b: (rgb & 0xff) as f32 / 255.0,
            a: 1.0,
        }
    }

    pub const fn with_alpha(self, a: f32) -> Srgba {
        Srgba { a, ..self }
    }
}

/// Oklch → sRGB, matching the ctk/web design-system colour space so the
/// CosMix chrome preset can derive its accents from the *same* scheme hues
/// as ctk widgets (`L` in 0..100 as ctk quotes it, `C` chroma, `H` degrees).
///
/// Out-of-gamut results are clamped per channel — identical in effect to the
/// clamping Bevy applies when it converts `Oklcha` for rendering.
pub fn oklch(l: f32, c: f32, h: f32) -> Srgba {
    let l = l / 100.0;
    let hr = h.to_radians();
    let (a, b) = (c * hr.cos(), c * hr.sin());

    // Oklab → LMS (cube roots undone), then LMS → linear sRGB.
    let l_ = l + 0.396_337_78 * a + 0.215_803_76 * b;
    let m_ = l - 0.105_561_346 * a - 0.063_854_17 * b;
    let s_ = l - 0.089_484_18 * a - 1.291_485_5 * b;
    let (l3, m3, s3) = (l_ * l_ * l_, m_ * m_ * m_, s_ * s_ * s_);
    let lin = [
        4.076_741_7 * l3 - 3.307_711_6 * m3 + 0.230_969_94 * s3,
        -1.268_438 * l3 + 2.609_757_4 * m3 - 0.341_319_38 * s3,
        -0.004_196_086_3 * l3 - 0.703_418_6 * m3 + 1.707_614_7 * s3,
    ];
    let enc = |v: f32| {
        let v = v.clamp(0.0, 1.0);
        if v <= 0.003_130_8 { 12.92 * v } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 }
    };
    Srgba::new(enc(lin[0]), enc(lin[1]), enc(lin[2]), 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip() {
        let c = Srgba::hex(0xFF5F57);
        assert!((c.r - 1.0).abs() < 1e-6);
        assert!((c.g - 0x5F as f32 / 255.0).abs() < 1e-6);
        assert!((c.b - 0x57 as f32 / 255.0).abs() < 1e-6);
        assert_eq!(c.a, 1.0);
    }

    #[test]
    fn oklch_white_black_greys() {
        let w = oklch(100.0, 0.0, 0.0);
        assert!(w.r > 0.99 && w.g > 0.99 && w.b > 0.99);
        let k = oklch(0.0, 0.0, 0.0);
        assert!(k.r < 0.01 && k.g < 0.01 && k.b < 0.01);
        // Achromatic in = achromatic out.
        let g = oklch(50.0, 0.0, 123.0);
        assert!((g.r - g.g).abs() < 1e-3 && (g.g - g.b).abs() < 1e-3);
    }

    #[test]
    fn rect_contains_is_half_open() {
        let r = rect(10.0, 10.0, 5.0, 5.0);
        assert!(r.contains(vec2(10.0, 10.0)));
        assert!(!r.contains(vec2(15.0, 10.0)));
        assert!(!r.contains(vec2(10.0, 15.0)));
    }
}
