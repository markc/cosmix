//! Startup timing: process start to first committed frame.

use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
pub struct Clock {
    main_entry: Instant,
    /// Process age when `main` began (kernel start time has 1-tick resolution).
    age_at_main: Option<Duration>,
}

impl Clock {
    pub fn start() -> Self {
        Self {
            main_entry: Instant::now(),
            age_at_main: process_age(),
        }
    }

    /// Milliseconds from process start (or `main`, if the start time is
    /// unreadable) to `at`.
    pub fn startup_ms(&self, at: Instant) -> f64 {
        let since_main = at.saturating_duration_since(self.main_entry);
        (self.age_at_main.unwrap_or_default() + since_main).as_secs_f64() * 1000.0
    }

    pub fn main_to_ms(&self, at: Instant) -> f64 {
        at.saturating_duration_since(self.main_entry).as_secs_f64() * 1000.0
    }
}

fn process_age() -> Option<Duration> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    // Field 22 (starttime); skip past the parenthesised command name.
    let rest = &stat[stat.rfind(')')? + 2..];
    let ticks: u64 = rest.split_whitespace().nth(19)?.parse().ok()?;
    let hz = rustix::param::clock_ticks_per_second().max(1);
    let start = Duration::from_nanos(ticks.checked_mul(1_000_000_000)? / hz);
    let now = rustix::time::clock_gettime(rustix::time::ClockId::Boottime);
    let now = Duration::new(now.tv_sec as u64, now.tv_nsec as u32);
    now.checked_sub(start)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_age_is_plausible() {
        let age = process_age().expect("readable /proc/self/stat");
        assert!(age < Duration::from_secs(24 * 3600));
    }
}
