//! Key repeat arming logic. Pure; the runtime owns the calloop timer and
//! does what the returned [`TimerAction`] says.

use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepeatRate {
    /// `rate` repeats per second after `delay` milliseconds.
    Repeat {
        rate: u32,
        delay: u32,
    },
    Disabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimerAction {
    /// (Re)arm the timer to fire after the duration.
    Arm(Duration),
    /// Remove the timer.
    Disarm,
    /// Leave the timer as it is.
    Keep,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repeat {
    rate: RepeatRate,
    held: Option<u32>,
}

impl Default for Repeat {
    fn default() -> Self {
        // wl_keyboard < v4 never sends repeat_info; these are X's defaults.
        Self {
            rate: RepeatRate::Repeat {
                rate: 25,
                delay: 600,
            },
            held: None,
        }
    }
}

impl Repeat {
    pub fn held(&self) -> Option<u32> {
        self.held
    }

    pub fn is_armed(&self) -> bool {
        self.held.is_some()
    }

    pub fn set_rate(&mut self, rate: RepeatRate) -> TimerAction {
        self.rate = match rate {
            RepeatRate::Repeat { rate: 0, .. } => RepeatRate::Disabled,
            other => other,
        };
        match (self.rate, self.held) {
            (RepeatRate::Disabled, Some(_)) => {
                self.held = None;
                TimerAction::Disarm
            }
            _ => TimerAction::Keep,
        }
    }

    /// A key went down. `repeats` is the keymap's answer for this key.
    pub fn press(&mut self, raw: u32, repeats: bool) -> TimerAction {
        match (self.rate, repeats) {
            (RepeatRate::Repeat { delay, .. }, true) => {
                self.held = Some(raw);
                TimerAction::Arm(Duration::from_millis(u64::from(delay)))
            }
            // A non-repeating key (a modifier) still ends an earlier repeat,
            // matching what other clients do.
            _ if self.held.take().is_some() => TimerAction::Disarm,
            _ => TimerAction::Keep,
        }
    }

    pub fn release(&mut self, raw: u32) -> TimerAction {
        if self.held == Some(raw) {
            self.held = None;
            TimerAction::Disarm
        } else {
            TimerAction::Keep
        }
    }

    /// Focus left the surface: all keys count as released.
    pub fn leave(&mut self) -> TimerAction {
        if self.held.take().is_some() {
            TimerAction::Disarm
        } else {
            TimerAction::Keep
        }
    }

    /// The timer fired. Returns the key to repeat and the next interval, or
    /// `None` when the timer should be dropped.
    pub fn fire(&mut self) -> Option<(u32, Duration)> {
        match (self.rate, self.held) {
            (RepeatRate::Repeat { rate, .. }, Some(raw)) if rate > 0 => {
                Some((raw, Duration::from_micros(1_000_000 / u64::from(rate))))
            }
            _ => {
                self.held = None;
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rate() -> Repeat {
        let mut r = Repeat::default();
        r.set_rate(RepeatRate::Repeat {
            rate: 40,
            delay: 300,
        });
        r
    }

    #[test]
    fn idle_is_unarmed() {
        let r = rate();
        assert!(!r.is_armed());
    }

    #[test]
    fn press_arms_release_disarms() {
        let mut r = rate();
        assert_eq!(
            r.press(30, true),
            TimerAction::Arm(Duration::from_millis(300))
        );
        assert_eq!(r.fire(), Some((30, Duration::from_millis(25))));
        assert_eq!(r.release(31), TimerAction::Keep);
        assert!(r.is_armed());
        assert_eq!(r.release(30), TimerAction::Disarm);
        assert!(!r.is_armed());
        assert_eq!(r.fire(), None);
    }

    #[test]
    fn second_key_takes_over() {
        let mut r = rate();
        r.press(30, true);
        assert_eq!(
            r.press(31, true),
            TimerAction::Arm(Duration::from_millis(300))
        );
        // Releasing the first key does not stop the second's repeat.
        assert_eq!(r.release(30), TimerAction::Keep);
        assert_eq!(r.held(), Some(31));
    }

    #[test]
    fn non_repeating_key_stops_repeat() {
        let mut r = rate();
        assert_eq!(r.press(42, false), TimerAction::Keep);
        r.press(30, true);
        assert_eq!(r.press(42, false), TimerAction::Disarm);
        assert!(!r.is_armed());
    }

    #[test]
    fn leave_and_disable() {
        let mut r = rate();
        r.press(30, true);
        assert_eq!(r.leave(), TimerAction::Disarm);
        assert_eq!(r.leave(), TimerAction::Keep);
        r.press(30, true);
        assert_eq!(r.set_rate(RepeatRate::Disabled), TimerAction::Disarm);
        assert_eq!(r.press(30, true), TimerAction::Keep);
        assert!(!r.is_armed());
        // rate 0 means disabled in wl_keyboard.repeat_info.
        let mut r = rate();
        r.press(30, true);
        assert_eq!(
            r.set_rate(RepeatRate::Repeat { rate: 0, delay: 1 }),
            TimerAction::Disarm
        );
    }
}
