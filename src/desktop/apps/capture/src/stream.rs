//! One capture producer and a three-frame FIFO. A small initial reserve
//! absorbs presentation jitter without discarding adjacent fresh frames.
use crate::{
    Status,
    wayland::{Capture, Pixels},
};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

const QUEUE_CAPACITY: usize = 3;
const PREROLL_FRAMES: usize = 2;
pub struct Sample {
    pub pixels: Pixels,
    pub sequence: u128,
}
impl Sample {
    pub fn newer_than(&self, sequence: u128) -> bool {
        self.sequence > sequence
    }
}
#[derive(Default)]
struct Slot {
    frames: VecDeque<Sample>,
    error: Option<String>,
}
impl Slot {
    fn push(&mut self, frame: Sample) -> bool {
        let dropped = self.frames.len() == QUEUE_CAPACITY;
        if dropped {
            self.frames.pop_front();
        }
        self.frames.push_back(frame);
        dropped
    }
}
pub struct Stream {
    slot: Arc<Mutex<Slot>>,
    ready: Arc<Condvar>,
    cancel: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl Stream {
    pub fn start(
        mut capture: Capture,
        fps: u32,
        depth: usize,
        status: Arc<Mutex<Status>>,
        cancel: Arc<AtomicBool>,
    ) -> Result<Self, String> {
        let slot = Arc::new(Mutex::new(Slot::default()));
        let ready = Arc::new(Condvar::new());
        let notify = ready.clone();
        let (out, stop) = (slot.clone(), cancel.clone());
        let worker = std::thread::Builder::new()
            .name("cosmix-capture-wayland".into())
            .spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let result = capture.stream_frame(fps, depth);
                    {
                        let mut state = status.lock().unwrap();
                        state.refused_frames = capture.refused_frames;
                        state.repeated_frames = capture.repeated_frames;
                    }
                    match result {
                        Ok((pixels, sequence)) => {
                            let dropped = out.lock().unwrap().push(Sample { pixels, sequence });
                            notify.notify_one();
                            let mut state = status.lock().unwrap();
                            state.acquired_frames += 1;
                            state.dropped_frames += u64::from(dropped);
                            state.repeated_frames = capture.repeated_frames;
                            state.observe_capture(capture.timing);
                            state.capture_ms += capture.timing.wait_ms
                                + capture.timing.read_ms
                                + capture.timing.normalise_ms;
                        }
                        Err(error) => {
                            if !stop.load(Ordering::Relaxed) {
                                let mut slot = out.lock().unwrap();
                                slot.frames.clear();
                                slot.error = Some(error);
                                notify.notify_one();
                            }
                            break;
                        }
                    }
                }
            })
            .map_err(|e| format!("start capture producer: {e}"))?;
        Ok(Self {
            slot,
            ready,
            cancel,
            worker: Some(worker),
        })
    }
    /// The first image remains with the encoder. Queue two following images
    /// before starting the movie clock, retaining their chronological order.
    pub fn preroll(&self) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut slot = self.slot.lock().unwrap();
        loop {
            if let Some(error) = &slot.error {
                return Err(error.clone());
            }
            if self.cancel.load(Ordering::Relaxed) {
                return Err("capture cancelled during preroll".into());
            }
            if slot.frames.len() >= PREROLL_FRAMES {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err("capture preroll deadline elapsed".into());
            }
            slot = self
                .ready
                .wait_timeout(slot, Duration::from_millis(20))
                .unwrap()
                .0;
        }
    }
    pub fn next(&self) -> Result<Option<Sample>, String> {
        let mut slot = self.slot.lock().unwrap();
        self.check_slot(&slot)?;
        Ok(slot.frames.pop_front())
    }
    pub fn check(&self) -> Result<(), String> {
        self.check_slot(&self.slot.lock().unwrap())
    }
    fn check_slot(&self, slot: &Slot) -> Result<(), String> {
        if let Some(error) = &slot.error {
            return Err(error.clone());
        }
        if self
            .worker
            .as_ref()
            .is_some_and(|worker| worker.is_finished())
            && !self.cancel.load(Ordering::Relaxed)
        {
            return Err("capture producer stopped unexpectedly".into());
        }
        Ok(())
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        // No slot/status mutex is held during join. The producer checks stop
        // between dispatches and polls for at most 20ms at a time.
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn static_responses_complete_preroll_without_inflating_fresh_count() {
        let mut slot = Slot::default();
        for cursor_pixel in [7, 8] {
            let mut sample = frame(cursor_pixel);
            sample.sequence = 0;
            slot.push(sample);
        }
        let stream = Stream {
            slot: Arc::new(Mutex::new(slot)),
            ready: Arc::new(Condvar::new()),
            cancel: Arc::new(AtomicBool::new(false)),
            worker: None,
        };
        stream.preroll().unwrap();
        let mut fresh_frames = 1; // Initial image already encoded.
        for cursor_pixel in [7, 8] {
            let sample = stream.next().unwrap().unwrap();
            fresh_frames += u64::from(sample.newer_than(0));
            // A repeated timestamp must still carry the newly read overlay.
            assert_eq!(sample.pixels.rgba[0], cursor_pixel);
        }
        assert_eq!(fresh_frames, 1);
    }
    #[test]
    fn dropped_first_occurrence_still_counts_retained_presentation_once() {
        let mut slot = Slot::default();
        for cursor_pixel in 1..=4 {
            let mut sample = frame(cursor_pixel);
            sample.sequence = 1;
            slot.push(sample);
        }
        let mut previous_sequence = 0;
        let mut fresh = 0;
        while let Some(sample) = slot.frames.pop_front() {
            fresh += u64::from(sample.newer_than(previous_sequence));
            previous_sequence = sample.sequence;
        }
        assert_eq!(fresh, 1);
    }
    fn frame(id: u8) -> Sample {
        Sample {
            sequence: u128::from(id),
            pixels: Pixels {
                width: 1,
                height: 1,
                rgba: vec![id, 0, 0, 255],
            },
        }
    }
    #[test]
    fn burst_arrivals_preserve_adjacent_frames_instead_of_latest_only() {
        let mut slot = Slot::default();
        assert!(!slot.push(frame(1)));
        assert!(!slot.push(frame(2)));
        assert!(slot.frames.len() >= PREROLL_FRAMES);
        assert_eq!(slot.frames.pop_front().unwrap().pixels.rgba[0], 1);
        assert!(!slot.push(frame(3)));
        assert!(!slot.push(frame(4)));
        for expected in [2, 3, 4] {
            assert_eq!(slot.frames.pop_front().unwrap().pixels.rgba[0], expected);
        }
        assert!(slot.frames.is_empty());
    }
    #[test]
    fn overload_drops_only_oldest_and_never_exceeds_bound() {
        let mut slot = Slot::default();
        for id in 0..3 {
            assert!(!slot.push(frame(id)));
        }
        assert!(slot.push(frame(3)));
        assert_eq!(slot.frames.len(), QUEUE_CAPACITY);
        for expected in [1, 2, 3] {
            assert_eq!(slot.frames.pop_front().unwrap().pixels.rgba[0], expected);
        }
    }
}
