//! Surface scale and size maths. Pure; no Wayland.

/// A surface scale. Fractional scales use wp_fractional_scale_v1's unit of
/// 1/120; integer scales come from `wl_surface` buffer scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scale {
    /// Buffer at physical size, shown through a viewport at logical size.
    Fractional(u32),
    /// Buffer scale set on the `wl_surface`; no viewport.
    Integer(i32),
}

impl Default for Scale {
    fn default() -> Self {
        Scale::Integer(1)
    }
}

impl Scale {
    pub fn from_f64_fractional(scale: f64) -> Self {
        Scale::Fractional((scale * 120.0).round().max(1.0) as u32)
    }

    pub fn factor(&self) -> f64 {
        match *self {
            Scale::Fractional(v) => f64::from(v) / 120.0,
            Scale::Integer(v) => f64::from(v.max(1)),
        }
    }

    /// The `wl_surface.set_buffer_scale` value to use with this scale.
    pub fn buffer_scale(&self) -> i32 {
        match *self {
            Scale::Fractional(_) => 1,
            Scale::Integer(v) => v.max(1),
        }
    }

    pub fn uses_viewport(&self) -> bool {
        matches!(self, Scale::Fractional(_))
    }

    /// One logical length in physical pixels. Fractional scale rounds half
    /// away from zero, as the fractional-scale protocol recommends.
    pub fn to_physical(&self, logical: u32) -> u32 {
        match *self {
            Scale::Fractional(v) => {
                let p = (u64::from(logical) * u64::from(v) + 60) / 120;
                u32::try_from(p).unwrap_or(u32::MAX)
            }
            Scale::Integer(v) => logical.saturating_mul(v.max(1) as u32),
        }
    }

    /// Physical to logical, rounded to nearest.
    pub fn to_logical(&self, physical: u32) -> u32 {
        match *self {
            Scale::Fractional(v) => {
                let v = u64::from(v.max(1));
                let l = (u64::from(physical) * 120 + v / 2) / v;
                u32::try_from(l).unwrap_or(u32::MAX)
            }
            Scale::Integer(v) => physical / v.max(1) as u32,
        }
    }

    pub fn size_to_physical(&self, logical: (u32, u32)) -> (u32, u32) {
        (self.to_physical(logical.0), self.to_physical(logical.1))
    }

    pub fn point_to_physical(&self, logical: (f64, f64)) -> (f64, f64) {
        let f = self.factor();
        (logical.0 * f, logical.1 * f)
    }
}

/// What the app is told about a surface.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SurfaceInfo {
    /// Size in logical (surface-local) pixels.
    pub logical: (u32, u32),
    /// Buffer size in physical pixels.
    pub physical: (u32, u32),
    pub scale: Scale,
}

impl SurfaceInfo {
    pub fn new(logical: (u32, u32), scale: Scale) -> Self {
        let logical = (logical.0.max(1), logical.1.max(1));
        Self {
            logical,
            physical: scale.size_to_physical(logical),
            scale,
        }
    }

    pub fn scale_factor(&self) -> f64 {
        self.scale.factor()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fractional_sizes() {
        let s100 = Scale::from_f64_fractional(1.0);
        let s125 = Scale::from_f64_fractional(1.25);
        let s250 = Scale::from_f64_fractional(2.5);
        assert_eq!(s125, Scale::Fractional(150));
        assert_eq!(s250, Scale::Fractional(300));
        assert_eq!(s100.size_to_physical((800, 600)), (800, 600));
        assert_eq!(s125.size_to_physical((800, 600)), (1000, 750));
        assert_eq!(s250.size_to_physical((800, 600)), (2000, 1500));
        // 1.25 * 3 = 3.75 -> 4; 1.25 * 2 = 2.5 -> 3 (half away from zero).
        assert_eq!(s125.to_physical(3), 4);
        assert_eq!(s125.to_physical(2), 3);
        assert_eq!(s250.to_physical(1), 3);
        assert_eq!(s250.to_physical(3), 8);
    }

    #[test]
    fn logical_round_trip() {
        for scale in [1.0, 1.25, 2.5] {
            let s = Scale::from_f64_fractional(scale);
            for l in [0u32, 1, 17, 640, 1921] {
                if scale >= 1.0 {
                    assert_eq!(s.to_logical(s.to_physical(l)), l, "{scale} {l}");
                }
            }
            assert_eq!(s.buffer_scale(), 1);
            assert!(s.uses_viewport());
        }
    }

    #[test]
    fn integer_fallback() {
        let s = Scale::Integer(2);
        assert_eq!(s.size_to_physical((800, 600)), (1600, 1200));
        assert_eq!(s.to_logical(1601), 800);
        assert_eq!(s.buffer_scale(), 2);
        assert!(!s.uses_viewport());
        assert_eq!(Scale::Integer(0).factor(), 1.0);
        assert_eq!(Scale::Integer(0).to_physical(10), 10);
    }

    #[test]
    fn info_and_points() {
        let info = SurfaceInfo::new((0, 10), Scale::Fractional(300));
        assert_eq!(info.logical, (1, 10));
        assert_eq!(info.physical, (3, 25));
        assert_eq!(info.scale_factor(), 2.5);
        assert_eq!(
            Scale::Fractional(150).point_to_physical((10.0, 4.0)),
            (12.5, 5.0)
        );
    }
}
