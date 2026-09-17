use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

#[derive(Default)]
pub struct Samples {
    values: VecDeque<u64>,
    count: u64,
}
impl Samples {
    pub fn add(&mut self, elapsed: Duration) {
        self.count += 1;
        if self.values.len() == 2048 {
            self.values.pop_front();
        }
        self.values
            .push_back(elapsed.as_micros().min(u64::MAX as u128) as u64);
    }
    fn summary(&self, name: &str) -> String {
        let mut v: Vec<_> = self.values.iter().copied().collect();
        v.sort_unstable();
        if v.is_empty() {
            return format!("{name}:n=0");
        }
        format!(
            "{name}:n={} window={} p50={}us p95={}us max={}us",
            self.count,
            v.len(),
            v[(v.len() - 1) / 2],
            v[(v.len() - 1) * 95 / 100],
            v[v.len() - 1]
        )
    }
}
#[derive(Default)]
pub struct Metrics {
    pub key_write: Samples,
    pub read_vt_bound: Samples,
    pub vt_rgba: Samples,
    pub rgba_upload: Samples,
    pub frame: Samples,
    pub frames: u64,
    pub reads: u64,
    pub bytes_read: u64,
    pub bytes_written: u64,
    pub input_written: u64,
    pub reply_written: u64,
    pub wakes: u64,
    pub uploads: u64,
    pub last_read: Option<Instant>,
    pub vt_updated: Option<Instant>,
}
impl Metrics {
    pub fn summary(&self) -> String {
        format!(
            "DIAGNOSTIC process-side only; frames={} reads={} bytes_read={} bytes_written={} input_written={} reply_written={} wakes={} uploads={}\n{}\n{}\n{}\n{}\n{}",
            self.frames,
            self.reads,
            self.bytes_read,
            self.bytes_written,
            self.input_written,
            self.reply_written,
            self.wakes,
            self.uploads,
            self.key_write.summary("key_to_actual_PTY_write"),
            self.read_vt_bound
                .summary("PTY_last_read_to_damage_notification_upper_bound"),
            self.vt_rgba.summary("VT_notification_to_RGBA"),
            self.rgba_upload.summary("RGBA_to_Assets_replaced"),
            self.frame.summary("process_frame_interval")
        )
    }
    pub fn parsed_boundary(&mut self) {
        if let Some(read) = self.last_read.take() {
            self.read_vt_bound.add(read.elapsed());
            self.vt_updated = Some(Instant::now());
        }
    }
}
