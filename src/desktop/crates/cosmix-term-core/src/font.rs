//! Runtime font sizing: foot's `font-increase` / `font-decrease` /
//! `font-reset`, as state a frontend owns and a raster is rebuilt from.
//!
//! Toolkit-free on purpose. The chords that drive it are the frontend's
//! business (they depend on how the toolkit reports keys and wheels); the
//! arithmetic is not, and both frontends must agree on it.
//!
//! foot steps by **0.5 pt**. The terminal's configured size is in logical
//! pixels (`font_px`), so the step is 0.5 pt at the 96 dpi that logical
//! pixels are defined against: 0.5 × 96 / 72 = ⅔ px.
//!
//! The current size is held as a whole number of steps from the configured
//! size, never as a running float. Stepping up fifty times and down fifty
//! times therefore lands exactly on the configured size, and `reset` is exact
//! by construction rather than by rounding.

use crate::config::valid_font;

/// One `font-increase` / `font-decrease`: 0.5 pt, in logical pixels.
pub const STEP_PX: f32 = 0.5 * 96.0 / 72.0;

/// The configured size and how far the user has zoomed from it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FontSize {
    configured: f32,
    steps: i32,
}

impl FontSize {
    /// `configured` is the startup size, already validated by the config
    /// loader; it is what `reset` returns to.
    pub fn new(configured: f32) -> Self {
        Self {
            configured,
            steps: 0,
        }
    }

    /// The size to rasterise at now, in logical pixels.
    pub fn current(&self) -> f32 {
        self.configured + self.steps as f32 * STEP_PX
    }

    pub fn configured(&self) -> f32 {
        self.configured
    }

    /// One step larger. False, and no change, when the next step would leave
    /// the range the config accepts — the same bounds a hand-written
    /// `font_px` is held to, so zoom cannot reach a size the file could not.
    pub fn increase(&mut self) -> bool {
        self.step(1)
    }

    /// One step smaller; false at the lower bound.
    pub fn decrease(&mut self) -> bool {
        self.step(-1)
    }

    /// Back to the configured size. False if already there.
    pub fn reset(&mut self) -> bool {
        std::mem::replace(&mut self.steps, 0) != 0
    }

    /// Apply `steps` single steps in one direction (a wheel that reports
    /// several notches at once), stopping at the bound. Returns whether the
    /// size changed at all.
    pub fn step_by(&mut self, steps: i32) -> bool {
        let mut changed = false;
        for _ in 0..steps.unsigned_abs() {
            if !self.step(steps.signum()) {
                break;
            }
            changed = true;
        }
        changed
    }

    fn step(&mut self, direction: i32) -> bool {
        let Some(next) = self.steps.checked_add(direction) else {
            return false;
        };
        if !valid_font(self.configured + next as f32 * STEP_PX) {
            return false;
        }
        self.steps = next;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_preserves_fractional_config_and_design_default() {
        for configured in [21.333, crate::config::Config::default().font_px] {
            let mut size = FontSize::new(configured);
            assert!(size.step_by(3));
            assert!(size.reset());
            assert_eq!(size.current(), configured);
        }
    }

    #[test]
    fn a_step_is_half_a_point_at_96_dpi() {
        let mut size = FontSize::new(13.0);
        assert!(size.increase());
        assert!((size.current() - (13.0 + 2.0 / 3.0)).abs() < 1e-5);
        assert!(size.decrease());
        assert!(size.decrease());
        assert!((size.current() - (13.0 - 2.0 / 3.0)).abs() < 1e-5);
    }

    /// Held as steps, not as a float that accumulates: many round trips must
    /// land on the configured size EXACTLY, or `reset` and "back where I
    /// started" would disagree by a rounding error and re-rasterise.
    #[test]
    fn round_trips_are_exact_and_reset_returns_to_the_configured_size() {
        let mut size = FontSize::new(13.0);
        for _ in 0..20 {
            assert!(size.increase());
        }
        for _ in 0..20 {
            assert!(size.decrease());
        }
        assert_eq!(size.current(), 13.0);
        assert!(!size.reset(), "already at the configured size");

        size.step_by(7);
        assert_ne!(size.current(), 13.0);
        assert!(size.reset());
        assert_eq!(size.current(), 13.0);
        assert_eq!(size.configured(), 13.0);
    }

    /// The bounds are the config's own: zoom must not reach a size that a
    /// hand-written `font_px` would be refused for.
    #[test]
    fn stepping_stops_inside_the_configured_range() {
        let mut size = FontSize::new(47.5);
        assert!(!size.increase(), "47.5 + 2/3 is past 48");
        assert_eq!(size.current(), 47.5);

        let mut size = FontSize::new(6.5);
        assert!(!size.decrease(), "6.5 - 2/3 is below 6");
        assert_eq!(size.current(), 6.5);

        // A multi-notch wheel stops AT the bound, not before it, and still
        // reports the steps it did take.
        let mut size = FontSize::new(13.0);
        assert!(size.step_by(-100));
        assert!(valid_font(size.current()));
        assert!(!valid_font(size.current() - STEP_PX));
        assert!(!size.step_by(-1));
        assert!(!size.step_by(0));
    }
}
