//! Rectangles and damage accumulation. Pure; no Wayland.

/// An axis-aligned rectangle. Buffer damage is in physical pixels; IME
/// cursor rectangles and popup anchors are in logical surface pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, width: i32, height: i32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.width <= 0 || self.height <= 0
    }

    pub fn right(&self) -> i32 {
        self.x.saturating_add(self.width)
    }

    pub fn bottom(&self) -> i32 {
        self.y.saturating_add(self.height)
    }

    pub fn area(&self) -> i64 {
        if self.is_empty() {
            0
        } else {
            i64::from(self.width) * i64::from(self.height)
        }
    }

    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= f64::from(self.x)
            && y >= f64::from(self.y)
            && x < f64::from(self.right())
            && y < f64::from(self.bottom())
    }

    /// The intersection, or `None` when it is empty.
    pub fn intersect(&self, other: &Rect) -> Option<Rect> {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let r = self.right().min(other.right());
        let b = self.bottom().min(other.bottom());
        (r > x && b > y).then(|| Rect::new(x, y, r - x, b - y))
    }

    pub fn union(&self, other: &Rect) -> Rect {
        if self.is_empty() {
            return *other;
        }
        if other.is_empty() {
            return *self;
        }
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let r = self.right().max(other.right());
        let b = self.bottom().max(other.bottom());
        Rect::new(x, y, r - x, b - y)
    }

    /// True when the rectangles overlap or share an edge.
    pub fn touches(&self, other: &Rect) -> bool {
        self.x <= other.right()
            && other.x <= self.right()
            && self.y <= other.bottom()
            && other.y <= self.bottom()
    }
}

/// Damage for one buffer: clipped to the buffer, merged when rectangles
/// touch, and collapsed to the bounding box once it holds more than
/// `max_rects` or when merging would not cost much extra area.
#[derive(Debug, Clone)]
pub struct Damage {
    bounds: Rect,
    rects: Vec<Rect>,
    max_rects: usize,
}

impl Damage {
    pub const DEFAULT_MAX_RECTS: usize = 16;

    pub fn new(width: u32, height: u32) -> Self {
        Self::with_limit(width, height, Self::DEFAULT_MAX_RECTS)
    }

    pub fn with_limit(width: u32, height: u32, max_rects: usize) -> Self {
        Self {
            bounds: Rect::new(0, 0, clamp_dim(width), clamp_dim(height)),
            rects: Vec::new(),
            max_rects: max_rects.max(1),
        }
    }

    pub fn bounds(&self) -> Rect {
        self.bounds
    }

    pub fn is_empty(&self) -> bool {
        self.rects.is_empty()
    }

    pub fn rects(&self) -> &[Rect] {
        &self.rects
    }

    pub fn clear(&mut self) {
        self.rects.clear();
    }

    pub fn is_full(&self) -> bool {
        self.rects.len() == 1 && self.rects[0] == self.bounds
    }

    pub fn add_full(&mut self) {
        self.rects.clear();
        if !self.bounds.is_empty() {
            self.rects.push(self.bounds);
        }
    }

    pub fn add(&mut self, rect: Rect) {
        let Some(mut rect) = rect.intersect(&self.bounds) else {
            return;
        };
        // Absorb every rectangle the new one touches, repeating because the
        // grown rectangle can reach ones it did not touch before.
        loop {
            let before = self.rects.len();
            self.rects.retain(|r| {
                let merged = rect.union(r);
                // Merge when touching, or when the union wastes less than a
                // quarter of its area: two nearby cells become one rectangle.
                if rect.touches(r) || (merged.area() - rect.area() - r.area()) * 4 < merged.area() {
                    rect = merged;
                    false
                } else {
                    true
                }
            });
            if self.rects.len() == before {
                break;
            }
        }
        self.rects.push(rect);
        if self.rects.len() > self.max_rects {
            let bbox = self.rects.iter().fold(Rect::default(), |a, r| a.union(r));
            self.rects.clear();
            self.rects.push(bbox);
        }
    }

    pub fn extend<'a>(&mut self, rects: impl IntoIterator<Item = &'a Rect>) {
        for rect in rects {
            self.add(*rect);
        }
    }
}

fn clamp_dim(v: u32) -> i32 {
    i32::try_from(v).unwrap_or(i32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clips_to_buffer_and_drops_outside() {
        let mut d = Damage::new(100, 50);
        d.add(Rect::new(-10, -10, 20, 20));
        assert_eq!(d.rects(), &[Rect::new(0, 0, 10, 10)]);
        d.add(Rect::new(200, 0, 10, 10));
        d.add(Rect::new(10, 10, 0, 5));
        assert_eq!(d.rects().len(), 1);
        d.add(Rect::new(90, 40, 50, 50));
        assert!(d.rects().contains(&Rect::new(90, 40, 10, 10)));
    }

    #[test]
    fn merges_touching_and_overlapping() {
        let mut d = Damage::new(1000, 1000);
        d.add(Rect::new(0, 0, 10, 10));
        d.add(Rect::new(10, 0, 10, 10)); // shares an edge
        assert_eq!(d.rects(), &[Rect::new(0, 0, 20, 10)]);
        d.add(Rect::new(15, 5, 10, 10)); // overlaps
        assert_eq!(d.rects(), &[Rect::new(0, 0, 25, 15)]);
    }

    #[test]
    fn keeps_distant_rects_apart_and_chains_merges() {
        let mut d = Damage::new(1000, 1000);
        d.add(Rect::new(0, 0, 10, 10));
        d.add(Rect::new(500, 500, 10, 10));
        assert_eq!(d.rects().len(), 2);
        // A bridge touching both collapses all three into one.
        d.add(Rect::new(5, 5, 500, 500));
        assert_eq!(d.rects(), &[Rect::new(0, 0, 510, 510)]);
    }

    #[test]
    fn collapses_to_bounding_box_past_limit() {
        let mut d = Damage::with_limit(1000, 1000, 3);
        for i in 0..4 {
            d.add(Rect::new(i * 100, i * 100, 5, 5));
        }
        assert_eq!(d.rects(), &[Rect::new(0, 0, 305, 305)]);
    }

    #[test]
    fn full_damage() {
        let mut d = Damage::new(64, 32);
        d.add(Rect::new(1, 1, 2, 2));
        d.add_full();
        assert!(d.is_full());
        d.add(Rect::new(3, 3, 3, 3));
        assert!(d.is_full());
    }

    #[test]
    fn rect_ops() {
        let a = Rect::new(0, 0, 10, 10);
        assert_eq!(a.intersect(&Rect::new(10, 0, 5, 5)), None);
        assert_eq!(
            a.intersect(&Rect::new(5, 5, 10, 10)),
            Some(Rect::new(5, 5, 5, 5))
        );
        assert!(a.contains(9.9, 0.0));
        assert!(!a.contains(10.0, 0.0));
        assert_eq!(Rect::default().union(&a), a);
    }
}
