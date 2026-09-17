//! The gain scale shared by `Fader` and `LevelMeter`, so a meter beside a
//! fader lines up with it. A host that wants its own curve (to match another
//! mixer's taper) passes a `Taper`; everything else — the unity tick, the
//! meter's colour zones, the thumb — follows from it.

/// Top of the default scale.
pub const MAX_DB: f32 = 6.0;
/// Lowest finite level on the default scale. Anything at or below it reads as
/// silence (position 0).
pub const FLOOR_DB: f32 = -60.0;

/// The default taper's breakpoints: (position, dB), increasing in both.
pub const DEFAULT_POINTS: [(f32, f32); 6] = [
    (0.0, FLOOR_DB),
    (0.15, -40.0),
    (0.35, -20.0),
    (0.6, -6.0),
    (0.8, 0.0),
    (1.0, MAX_DB),
];

/// A gain taper: a piecewise-linear map between a 0..=1 travel position and
/// dB, given by breakpoints that increase in both coordinates.
///
/// The first point is the floor (position 0, read as silence) and the last is
/// the top. Fewer than two points, or points that do not increase, fall back
/// to the default taper rather than producing a nonsense scale.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Taper<'a> {
    points: &'a [(f32, f32)],
}

impl Default for Taper<'_> {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl<'a> Taper<'a> {
    /// The scale `Fader` and `LevelMeter` use unless a host overrides it:
    /// -60 dB at the bottom, 0 dB at 0.8 of the travel, +6 dB at the top.
    pub const DEFAULT: Taper<'static> = Taper {
        points: &DEFAULT_POINTS,
    };

    /// A taper from `points`, or the default if they are unusable.
    pub fn new(points: &'a [(f32, f32)]) -> Self {
        if is_valid(points) {
            Self { points }
        } else {
            Self::DEFAULT
        }
    }

    /// The breakpoints in use.
    pub fn points(&self) -> &'a [(f32, f32)] {
        self.points
    }

    /// The dB of the bottom of the travel; anything at or below reads as
    /// silence.
    pub fn floor_db(&self) -> f32 {
        self.points[0].1
    }

    /// The dB at the top of the travel.
    pub fn max_db(&self) -> f32 {
        self.points[self.points.len() - 1].1
    }

    /// Maps a gain in dB to a 0..=1 travel position. -inf, NaN and anything at
    /// or below the floor give 0.
    pub fn position(&self, db: f32) -> f32 {
        if db.is_nan() || db <= self.floor_db() {
            return 0.0;
        }
        if db >= self.max_db() {
            return 1.0;
        }
        let upper = self
            .points
            .iter()
            .position(|(_, point_db)| db <= *point_db)
            .unwrap_or(self.points.len() - 1);
        let (p0, d0) = self.points[upper - 1];
        let (p1, d1) = self.points[upper];
        if db == d1 {
            return p1;
        }
        p0 + (db - d0) / (d1 - d0) * (p1 - p0)
    }

    /// Inverse of `position`. Position 0 (or less) is `f32::NEG_INFINITY`.
    pub fn db(&self, position: f32) -> f32 {
        if position.is_nan() || position <= 0.0 {
            return f32::NEG_INFINITY;
        }
        if position >= 1.0 {
            return self.max_db();
        }
        let upper = self
            .points
            .iter()
            .position(|(point_position, _)| position <= *point_position)
            .unwrap_or(self.points.len() - 1);
        let (p0, d0) = self.points[upper - 1];
        let (p1, d1) = self.points[upper];
        if position == p1 {
            return d1;
        }
        d0 + (position - p0) / (p1 - p0) * (d1 - d0)
    }
}

fn is_valid(points: &[(f32, f32)]) -> bool {
    points.len() >= 2
        && points.iter().all(|(p, d)| p.is_finite() && d.is_finite())
        && points
            .windows(2)
            .all(|pair| pair[0].0 < pair[1].0 && pair[0].1 < pair[1].1)
}

/// `Taper::DEFAULT.position`, kept as a free function for callers on the
/// default scale.
pub fn db_to_position(db: f32) -> f32 {
    Taper::DEFAULT.position(db)
}

/// `Taper::DEFAULT.db`.
pub fn position_to_db(position: f32) -> f32 {
    Taper::DEFAULT.db(position)
}

/// Formats a gain for a label: `-inf` or one decimal place.
pub fn format_db(db: f32) -> String {
    if db == f32::NEG_INFINITY || db <= FLOOR_DB {
        "-inf".into()
    } else {
        format!("{db:.1}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchors_and_limits() {
        assert_eq!(db_to_position(f32::NEG_INFINITY), 0.0);
        assert_eq!(db_to_position(f32::NAN), 0.0);
        assert_eq!(db_to_position(-90.0), 0.0);
        assert_eq!(db_to_position(0.0), 0.8);
        assert_eq!(db_to_position(12.0), 1.0);
        assert_eq!(position_to_db(0.0), f32::NEG_INFINITY);
        assert_eq!(position_to_db(-1.0), f32::NEG_INFINITY);
        assert_eq!(position_to_db(0.8), 0.0);
        assert_eq!(position_to_db(2.0), MAX_DB);
        assert_eq!(format_db(f32::NEG_INFINITY), "-inf");
        assert_eq!(format_db(-6.04), "-6.0");
        assert_eq!(Taper::default(), Taper::DEFAULT);
        assert_eq!(Taper::DEFAULT.floor_db(), FLOOR_DB);
        assert_eq!(Taper::DEFAULT.max_db(), MAX_DB);
    }

    #[test]
    fn mapping_is_monotonic_and_round_trips() {
        let mut previous = 0.0;
        for step in 1..=660 {
            let db = FLOOR_DB + step as f32 * 0.1;
            let position = db_to_position(db);
            assert!(position > previous, "not increasing at {db}");
            previous = position;
            assert!((position_to_db(position) - db).abs() < 1e-3, "{db}");
        }
    }

    #[test]
    fn a_host_taper_replaces_the_curve_and_round_trips() {
        // A linear -80..0 dB scale, as a console with no headroom might use.
        let points = [(0.0, -80.0), (1.0, 0.0)];
        let taper = Taper::new(&points);
        assert_eq!(taper.floor_db(), -80.0);
        assert_eq!(taper.max_db(), 0.0);
        assert_eq!(taper.position(-40.0), 0.5);
        assert_eq!(taper.db(0.5), -40.0);
        assert_eq!(taper.position(-100.0), 0.0);
        assert_eq!(taper.position(3.0), 1.0);
        assert_eq!(taper.db(0.0), f32::NEG_INFINITY);
        // -40 dB sits mid-travel here and near a third on the default taper.
        assert!(taper.position(-40.0) > db_to_position(-40.0));
        for step in 0..=800 {
            let db = -80.0 + step as f32 * 0.1;
            let position = taper.position(db);
            assert!((0.0..=1.0).contains(&position));
            if db > -80.0 {
                assert!((taper.db(position) - db).abs() < 1e-3, "{db}");
            }
        }
        // Several segments, and exact breakpoints come back exactly.
        let curve = [(0.0, -60.0), (0.5, -12.0), (0.75, 0.0), (1.0, 12.0)];
        let taper = Taper::new(&curve);
        for (position, db) in curve {
            assert_eq!(taper.position(db), position);
            if position > 0.0 {
                assert_eq!(taper.db(position), db);
            }
        }
        assert_eq!(taper.db(0.875), 6.0);
    }

    #[test]
    fn an_unusable_taper_falls_back_to_the_default() {
        for points in [
            &[][..],
            &[(0.0, -60.0)][..],
            &[(0.0, -60.0), (0.0, 0.0)][..], // position not increasing
            &[(0.0, -60.0), (1.0, -70.0)][..], // dB not increasing
            &[(0.0, -60.0), (f32::NAN, 0.0)][..], // not finite
            &[(0.0, -60.0), (1.0, f32::INFINITY)][..], // not finite
        ] {
            assert_eq!(Taper::new(points), Taper::DEFAULT, "{points:?}");
        }
    }
}
