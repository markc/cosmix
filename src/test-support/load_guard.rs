//! Shared load guard for timing-sensitive integration tests.

/// Skip timing assertions when one-minute load per available CPU reaches this value.
pub const LOAD_THRESHOLD: f64 = 0.5;

/// Scheduler load sampled once at the start of a test.
#[derive(Clone, Copy, Debug)]
pub struct LoadSample {
    /// One-minute load average from `/proc/loadavg`.
    pub load1: f64,
    /// CPU parallelism available to the test process.
    pub parallelism: usize,
}

impl LoadSample {
    /// One-minute load normalised by the parallelism available to the process.
    pub fn load_per_cpu(self) -> f64 {
        self.load1 / self.parallelism.max(1) as f64
    }
}

/// Read the scheduler load once. Failure to sample leaves timing checks enabled.
pub fn read_load_sample() -> LoadSample {
    let load1 = std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|s| {
            s.split_whitespace()
                .next()
                .and_then(|value| value.parse().ok())
        })
        .unwrap_or(0.0);
    let parallelism = std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(1);

    LoadSample { load1, parallelism }
}

/// Whether a wall-clock timing assertion is meaningful for this load sample.
pub fn should_assert_timing(load: LoadSample) -> bool {
    load.load_per_cpu() < LOAD_THRESHOLD
}
