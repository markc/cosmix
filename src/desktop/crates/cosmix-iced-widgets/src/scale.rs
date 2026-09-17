//! The gain scale shared by `Fader` and `LevelMeter`, so a meter beside a
//! fader lines up with it.

/// Top of the scale.
pub const MAX_DB: f32 = 6.0;
/// Lowest finite level shown. Anything below reads as silence (position 0).
pub const FLOOR_DB: f32 = -60.0;

// (position, dB), strictly increasing in both. Position 0 is -inf.
const POINTS: [(f32, f32); 6] = [
    (0.0, FLOOR_DB),
    (0.15, -40.0),
    (0.35, -20.0),
    (0.6, -6.0),
    (0.8, 0.0),
    (1.0, MAX_DB),
];

/// Maps a gain in dB to a 0..=1 travel position. -inf, NaN and anything at or
/// below `FLOOR_DB` give 0.
pub fn db_to_position(db: f32) -> f32 {
    if db.is_nan() || db <= FLOOR_DB {
        return 0.0;
    }
    if db >= MAX_DB {
        return 1.0;
    }
    let upper = POINTS.iter().position(|(_, d)| db <= *d).unwrap_or(5);
    let (p0, d0) = POINTS[upper - 1];
    let (p1, d1) = POINTS[upper];
    if db == d1 {
        return p1;
    }
    p0 + (db - d0) / (d1 - d0) * (p1 - p0)
}

/// Inverse of `db_to_position`. Position 0 (or less) is `f32::NEG_INFINITY`.
pub fn position_to_db(position: f32) -> f32 {
    if position.is_nan() || position <= 0.0 {
        return f32::NEG_INFINITY;
    }
    if position >= 1.0 {
        return MAX_DB;
    }
    let upper = POINTS.iter().position(|(p, _)| position <= *p).unwrap_or(5);
    let (p0, d0) = POINTS[upper - 1];
    let (p1, d1) = POINTS[upper];
    if position == p1 {
        return d1;
    }
    d0 + (position - p0) / (p1 - p0) * (d1 - d0)
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
}
