//! Deterministic, bounded flock dynamics in output-local logical pixels.
//! No renderer, window system, clock, thread or Bus dependency lives here.

use std::collections::HashMap;
use std::ops::{Add, AddAssign, Mul, Sub};

pub const MAX_BIRDS: usize = 1024;
pub const MAX_OBSTACLES: usize = 512;
pub const STEP: f32 = 1.0 / 30.0;
const NEIGHBOURS: usize = 48;
const RADIUS: f32 = 80.0;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}
impl Point {
    pub const ZERO: Self = Self { x: 0.0, y: 0.0 };
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
    pub fn finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite()
    }
    pub fn length(self) -> f32 {
        self.x.hypot(self.y)
    }
    pub fn unit(self) -> Self {
        let n = self.length();
        if n > 0.0001 {
            self * (1.0 / n)
        } else {
            Self::ZERO
        }
    }
    fn limited(self, max: f32) -> Self {
        if self.length() > max {
            self.unit() * max
        } else {
            self
        }
    }
}
impl Add for Point {
    type Output = Self;
    fn add(self, b: Self) -> Self {
        Self::new(self.x + b.x, self.y + b.y)
    }
}
impl AddAssign for Point {
    fn add_assign(&mut self, b: Self) {
        *self = *self + b;
    }
}
impl Sub for Point {
    type Output = Self;
    fn sub(self, b: Self) -> Self {
        Self::new(self.x - b.x, self.y - b.y)
    }
}
impl Mul<f32> for Point {
    type Output = Self;
    fn mul(self, k: f32) -> Self {
        Self::new(self.x * k, self.y * k)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub min: Point,
    pub max: Point,
}
impl Rect {
    pub fn new(x: f32, y: f32, width: f32, height: f32) -> Option<Self> {
        let r = Self {
            min: Point::new(x, y),
            max: Point::new(x + width, y + height),
        };
        (r.min.finite() && r.max.finite() && width > 0.0 && height > 0.0).then_some(r)
    }
    pub fn contains(self, p: Point) -> bool {
        p.x >= self.min.x && p.y >= self.min.y && p.x <= self.max.x && p.y <= self.max.y
    }
    pub fn expanded(self, margin: f32) -> Self {
        Self {
            min: self.min - Point::new(margin, margin),
            max: self.max + Point::new(margin, margin),
        }
    }
    fn closest(self, p: Point) -> Point {
        Point::new(
            p.x.clamp(self.min.x, self.max.x),
            p.y.clamp(self.min.y, self.max.y),
        )
    }
    /// Exact nearest-edge escape, including a deterministic centre tie-break.
    pub fn escape(self, p: Point) -> Point {
        if !self.contains(p) {
            return p;
        }
        let candidates = [
            (p.x - self.min.x, Point::new(self.min.x - 1.0, p.y)),
            (self.max.x - p.x, Point::new(self.max.x + 1.0, p.y)),
            (p.y - self.min.y, Point::new(p.x, self.min.y - 1.0)),
            (self.max.y - p.y, Point::new(p.x, self.max.y + 1.0)),
        ];
        candidates
            .into_iter()
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .unwrap()
            .1
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bird {
    pub position: Point,
    pub velocity: Point,
    pub visible: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Settings {
    pub count: usize,
    pub speed: f32,
    pub pointer_radius: f32,
    pub window_margin: f32,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            count: 192,
            speed: 80.0,
            pointer_radius: 110.0,
            window_margin: 12.0,
        }
    }
}
impl Settings {
    pub fn valid(self) -> bool {
        self.count <= MAX_BIRDS
            && self.speed.is_finite()
            && (5.0..=300.0).contains(&self.speed)
            && self.pointer_radius.is_finite()
            && (0.0..=600.0).contains(&self.pointer_radius)
            && self.window_margin.is_finite()
            && (0.0..=100.0).contains(&self.window_margin)
    }
}

#[derive(Debug)]
pub struct Flock {
    bounds: Rect,
    settings: Settings,
    birds: Vec<Bird>,
    obstacles: Vec<Rect>,
    pointer: Option<Point>,
    rng: u64,
    accumulator: f32,
    pub ticks: u64,
}
impl Flock {
    pub fn new(
        width: f32,
        height: f32,
        seed: u64,
        settings: Settings,
    ) -> Result<Self, &'static str> {
        if !settings.valid() {
            return Err("invalid flock settings");
        }
        let bounds = Self::bounds(width, height)?;
        let mut flock = Self {
            bounds,
            settings,
            birds: Vec::new(),
            obstacles: Vec::new(),
            pointer: None,
            rng: seed.max(1),
            accumulator: 0.0,
            ticks: 0,
        };
        flock.populate();
        Ok(flock)
    }
    fn bounds(width: f32, height: f32) -> Result<Rect, &'static str> {
        if !(1.0..=32768.0).contains(&width) || !(1.0..=32768.0).contains(&height) {
            return Err("invalid output size");
        }
        Rect::new(0.0, 0.0, width, height).ok_or("invalid output size")
    }
    fn random(&mut self) -> f32 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        ((self.rng >> 40) as u32) as f32 / 16777216.0
    }
    fn populate(&mut self) {
        self.birds.truncate(self.settings.count);
        while self.birds.len() < self.settings.count {
            let position = Point::new(
                self.random() * self.bounds.max.x,
                self.random() * self.bounds.max.y,
            );
            let angle = self.random() * std::f32::consts::TAU;
            self.birds.push(Bird {
                position,
                velocity: Point::new(angle.cos(), angle.sin()) * self.settings.speed,
                visible: true,
            });
        }
    }
    pub fn birds(&self) -> &[Bird] {
        &self.birds
    }
    pub fn settings(&self) -> Settings {
        self.settings
    }
    pub fn configure(&mut self, settings: Settings) -> Result<(), &'static str> {
        if !settings.valid() {
            return Err("invalid flock settings");
        }
        let visibility_changed = self.settings.window_margin != settings.window_margin
            || self.settings.count != settings.count;
        self.settings = settings;
        self.populate();
        if visibility_changed {
            self.refresh_visibility();
        }
        Ok(())
    }
    pub fn resize(&mut self, width: f32, height: f32) -> Result<(), &'static str> {
        self.bounds = Self::bounds(width, height)?;
        for bird in &mut self.birds {
            bird.position = self.bounds.closest(bird.position);
        }
        self.refresh_visibility();
        Ok(())
    }
    pub fn set_pointer(&mut self, pointer: Option<Point>) {
        self.pointer = pointer.filter(|p| p.finite() && self.bounds.contains(*p));
    }
    pub fn set_obstacles(&mut self, obstacles: &[Rect]) -> Result<(), &'static str> {
        if obstacles.len() > MAX_OBSTACLES
            || obstacles.iter().any(|r| {
                !r.min.finite() || !r.max.finite() || r.min.x >= r.max.x || r.min.y >= r.max.y
            })
        {
            return Err("invalid scene obstacles");
        }
        if self.obstacles != obstacles {
            self.obstacles.clear();
            self.obstacles.extend_from_slice(obstacles);
            self.refresh_visibility();
        }
        Ok(())
    }
    /// Geometry is observable even when no fixed simulation step is due.
    fn refresh_visibility(&mut self) {
        for bird in &mut self.birds {
            bird.visible = !self.obstacles.iter().any(|rect| {
                rect.expanded(self.settings.window_margin)
                    .contains(bird.position)
            });
        }
    }
    /// At most four fixed steps, irrespective of pause/stall duration.
    pub fn advance(&mut self, seconds: f32) -> usize {
        self.advance_with_budget(seconds, 4)
    }
    /// Rendering may deliberately be slower than the fixed simulation. Allow
    /// the expected interval's work (at most 32 steps), while an unexpected
    /// stall still uses the ordinary four-step catch-up bound.
    pub fn advance_scheduled(&mut self, seconds: f32, fps: u32) -> usize {
        if !(1..=60).contains(&fps) {
            return 0;
        }
        let interval = 1.0 / fps as f32;
        let budget = if seconds <= interval + STEP * 2.0 {
            ((interval / STEP).ceil() as usize + 2).clamp(4, 32)
        } else {
            4
        };
        self.advance_with_budget(seconds, budget)
    }
    fn advance_with_budget(&mut self, seconds: f32, budget: usize) -> usize {
        if !seconds.is_finite() || seconds <= 0.0 {
            return 0;
        }
        let limit = STEP * budget as f32;
        self.accumulator = (self.accumulator + seconds.min(limit)).min(limit);
        let mut count = 0;
        while self.accumulator >= STEP && count < budget {
            self.step();
            self.accumulator -= STEP;
            count += 1;
        }
        count
    }
    fn step(&mut self) {
        let old = self.birds.clone();
        let mut grid: HashMap<(i32, i32), Vec<usize>> = HashMap::new();
        for (i, bird) in old.iter().enumerate() {
            grid.entry(cell(bird.position)).or_default().push(i);
        }
        for (i, bird) in self.birds.iter_mut().enumerate() {
            let p = old[i].position;
            let v = old[i].velocity;
            let (cx, cy) = cell(p);
            let mut separation = Point::ZERO;
            let mut alignment = Point::ZERO;
            let mut cohesion = Point::ZERO;
            let mut n = 0;
            'cells: for y in cy - 1..=cy + 1 {
                for x in cx - 1..=cx + 1 {
                    if let Some(indices) = grid.get(&(x, y)) {
                        for &j in indices.iter().take(NEIGHBOURS + 1) {
                            if i == j {
                                continue;
                            }
                            let delta = p - old[j].position;
                            let distance = delta.length();
                            if distance < RADIUS {
                                separation += if distance > 0.001 {
                                    delta * (1.0 / (distance * distance).max(1.0))
                                } else {
                                    Point::new(if i < j { -1.0 } else { 1.0 }, 0.0)
                                };
                                alignment += old[j].velocity;
                                cohesion += old[j].position;
                                n += 1;
                                if n >= NEIGHBOURS {
                                    break 'cells;
                                }
                            }
                        }
                    }
                }
            }
            let mut force = Point::new(10.0, 3.0);
            if n > 0 {
                force += separation * 230.0
                    + (alignment * (1.0 / n as f32) - v) * 0.45
                    + (cohesion * (1.0 / n as f32) - p) * 0.3;
            }
            if let Some(pointer) = self.pointer {
                let away = p - pointer;
                let distance = away.length();
                if distance < self.settings.pointer_radius {
                    let direction = if distance > 0.001 {
                        away.unit()
                    } else {
                        Point::new(1.0, 0.0)
                    };
                    force += direction * ((self.settings.pointer_radius - distance) * 5.0);
                }
            }
            let predict = p + v * 0.35;
            for obstacle in &self.obstacles {
                let rect = obstacle.expanded(self.settings.window_margin);
                let away = predict - rect.closest(predict);
                if rect.contains(predict) {
                    force += (rect.escape(predict) - predict).unit() * 600.0;
                } else if away.length() < 35.0 {
                    force += away.unit() * (35.0 - away.length()) * 12.0;
                }
            }
            let edge = 45.0;
            if p.x < edge {
                force.x += (edge - p.x) * 4.0;
            }
            if p.y < edge {
                force.y += (edge - p.y) * 4.0;
            }
            if p.x > self.bounds.max.x - edge {
                force.x -= (p.x - self.bounds.max.x + edge) * 4.0;
            }
            if p.y > self.bounds.max.y - edge {
                force.y -= (p.y - self.bounds.max.y + edge) * 4.0;
            }
            bird.velocity = (v + force.limited(500.0) * STEP).limited(self.settings.speed);
            bird.position = self.bounds.closest(p + bird.velocity * STEP);
            // Bound moving-window push-out even with overlapping/fullscreen obstacles.
            for _ in 0..3 {
                let Some(r) = self
                    .obstacles
                    .iter()
                    .map(|r| r.expanded(self.settings.window_margin))
                    .find(|r| r.contains(bird.position))
                else {
                    break;
                };
                bird.position = self.bounds.closest(r.escape(bird.position));
            }
            bird.visible = !self.obstacles.iter().any(|r| {
                r.expanded(self.settings.window_margin)
                    .contains(bird.position)
            });
            if !bird.visible {
                bird.velocity = Point::ZERO;
            }
        }
        self.ticks = self.ticks.saturating_add(1);
    }
}
fn cell(p: Point) -> (i32, i32) {
    ((p.x / RADIUS).floor() as i32, (p.y / RADIUS).floor() as i32)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn low_render_rates_preserve_fixed_simulation_time_without_unbounded_catchup() {
        for fps in [1, 2, 5, 7, 15, 30, 60] {
            let mut f = Flock::new(
                800.0,
                600.0,
                42,
                Settings {
                    count: 4,
                    ..Settings::default()
                },
            )
            .unwrap();
            let mut steps = 0;
            for _ in 0..(fps * 10) {
                steps += f.advance_scheduled(1.0 / fps as f32, fps);
            }
            assert!((299..=300).contains(&steps), "fps={fps}, steps={steps}");
            assert!(f.advance_scheduled(100.0, fps) <= 4);
            assert_eq!(f.advance_scheduled(f32::NAN, fps), 0);
        }
    }

    #[test]
    fn deterministic_and_bounded_after_stalls() {
        let mut a = Flock::new(1280.0, 720.0, 42, Settings::default()).unwrap();
        let mut b = Flock::new(1280.0, 720.0, 42, Settings::default()).unwrap();
        for _ in 0..200 {
            assert!(a.advance(10.0) <= 4);
            b.advance(10.0);
        }
        assert_eq!(a.birds(), b.birds());
        for bird in a.birds() {
            assert!(bird.position.finite() && bird.velocity.finite());
            assert!(a.bounds.contains(bird.position));
            assert!(bird.velocity.length() <= 80.001);
        }
    }
    #[test]
    fn centre_escape_is_finite_and_uses_nearest_edge() {
        let rect = Rect::new(10.0, 10.0, 100.0, 20.0).unwrap();
        let escaped = rect.escape(Point::new(60.0, 20.0));
        assert!(!rect.contains(escaped));
        assert_eq!(escaped.x, 60.0);
        assert!(escaped.finite());
    }
    #[test]
    fn fullscreen_parks_then_removed_obstacle_releases_birds() {
        let mut f = Flock::new(320.0, 200.0, 1, Settings::default()).unwrap();
        f.set_obstacles(&[Rect::new(0.0, 0.0, 320.0, 200.0).unwrap()])
            .unwrap();
        f.advance(STEP);
        assert!(
            f.birds()
                .iter()
                .all(|b| !b.visible && b.velocity == Point::ZERO)
        );
        f.set_obstacles(&[]).unwrap();
        f.advance(STEP);
        assert!(f.birds().iter().all(|b| b.visible && b.position.finite()));
    }
    #[test]
    fn obstacle_and_margin_changes_refresh_visibility_without_a_step() {
        let mut f = Flock::new(
            320.0,
            200.0,
            1,
            Settings {
                count: 1,
                window_margin: 0.0,
                ..Settings::default()
            },
        )
        .unwrap();
        f.birds[0].position = Point::new(100.0, 100.0);
        let velocity = f.birds[0].velocity;
        f.set_obstacles(&[Rect::new(90.0, 90.0, 20.0, 20.0).unwrap()])
            .unwrap();
        assert_eq!(f.advance_scheduled(0.0, 30), 0);
        assert!(!f.birds[0].visible);
        assert_eq!(f.birds[0].position, Point::new(100.0, 100.0));
        assert_eq!(f.birds[0].velocity, velocity);
        f.set_obstacles(&[Rect::new(110.0, 90.0, 20.0, 20.0).unwrap()])
            .unwrap();
        assert!(f.birds[0].visible);
        f.configure(Settings {
            window_margin: 12.0,
            ..f.settings()
        })
        .unwrap();
        assert!(!f.birds[0].visible);
        f.set_obstacles(&[]).unwrap();
        assert!(f.birds[0].visible);
        assert_eq!(f.ticks, 0);
    }
    #[test]
    fn coincident_pointer_and_birds_stay_finite() {
        let mut f = Flock::new(640.0, 480.0, 0, Settings::default()).unwrap();
        for b in &mut f.birds {
            b.position = Point::new(320.0, 240.0);
            b.velocity = Point::ZERO;
        }
        f.set_pointer(Some(Point::new(320.0, 240.0)));
        f.advance(STEP);
        assert!(
            f.birds()
                .iter()
                .all(|b| b.position.finite() && b.velocity.finite())
        );
    }
    #[test]
    fn invalid_updates_are_atomic() {
        let mut f = Flock::new(640.0, 480.0, 1, Settings::default()).unwrap();
        let initial = f.settings();
        assert!(
            f.configure(Settings {
                speed: f32::NAN,
                ..initial
            })
            .is_err()
        );
        assert_eq!(f.settings(), initial);
        let original = f.bounds;
        assert!(f.resize(f32::INFINITY, 10.0).is_err());
        assert_eq!(f.bounds, original);
        assert_eq!(f.advance(f32::NAN), 0);
        assert_eq!(f.advance(-1.0), 0);
        assert!(f.set_obstacles(&vec![original; MAX_OBSTACLES + 1]).is_err());
        assert!(f.obstacles.is_empty());
    }
}
