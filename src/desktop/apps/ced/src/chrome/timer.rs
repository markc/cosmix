//! The chrome's one-shot timers (session save, lint and find debounces,
//! change-marker clearing, status-message expiry). One thread waits on a
//! channel with the nearest deadline as its timeout — it wakes only when a
//! timer is armed or due, never on a period (no polling). Re-arming a key
//! replaces its deadline, which is what a debounce is.
//!
//! The Controller's own timers (request deadlines, `ced.wait`, backoff) stay
//! on the Bus thread (`Effect::Timer`); these are the app's.

use std::collections::HashMap;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use cosmix_edit_client::types::TabId;

/// What a timer is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TimerKey {
    SessionSave,
    Lint(TabId),
    FindHighlight,
    ClearMarkers(TabId),
    StatusExpiry,
}

enum Command {
    Arm(TimerKey, Instant),
    Cancel(TimerKey),
}

/// Handle to the timer thread.
#[derive(Clone)]
pub struct Timers {
    tx: mpsc::Sender<Command>,
}

impl Timers {
    /// Start the thread; fired keys arrive on the returned stream.
    pub fn start() -> (Timers, iced::futures::channel::mpsc::UnboundedReceiver<TimerKey>) {
        let (tx, rx) = mpsc::channel();
        let (fire_tx, fire_rx) = iced::futures::channel::mpsc::unbounded();
        let _ = std::thread::Builder::new().name("ced-timers".into()).spawn(move || run(rx, fire_tx));
        (Timers { tx }, fire_rx)
    }

    /// Fire `key` after `ms`, replacing any pending deadline for it.
    pub fn arm(&self, key: TimerKey, ms: u64) {
        let _ = self.tx.send(Command::Arm(key, Instant::now() + Duration::from_millis(ms)));
    }

    pub fn cancel(&self, key: TimerKey) {
        let _ = self.tx.send(Command::Cancel(key));
    }
}

fn run(rx: mpsc::Receiver<Command>, fire: iced::futures::channel::mpsc::UnboundedSender<TimerKey>) {
    let mut pending: HashMap<TimerKey, Instant> = HashMap::new();
    loop {
        let next = pending.values().min().copied();
        let command = match next {
            None => match rx.recv() {
                Ok(c) => Some(c),
                Err(_) => return,
            },
            Some(at) => match rx.recv_timeout(at.saturating_duration_since(Instant::now())) {
                Ok(c) => Some(c),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            },
        };
        match command {
            Some(Command::Arm(key, at)) => {
                pending.insert(key, at);
            }
            Some(Command::Cancel(key)) => {
                pending.remove(&key);
            }
            None => {
                let now = Instant::now();
                let due: Vec<TimerKey> = pending.iter().filter(|(_, at)| **at <= now).map(|(k, _)| *k).collect();
                for key in due {
                    pending.remove(&key);
                    if fire.unbounded_send(key).is_err() {
                        return;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::futures::StreamExt;

    #[test]
    fn rearming_debounces_and_cancel_removes() {
        let (timers, mut fired) = Timers::start();
        timers.arm(TimerKey::SessionSave, 40);
        timers.arm(TimerKey::SessionSave, 60);
        timers.arm(TimerKey::Lint(7), 10);
        timers.arm(TimerKey::FindHighlight, 20);
        timers.cancel(TimerKey::FindHighlight);
        let first = iced::futures::executor::block_on(fired.next());
        let second = iced::futures::executor::block_on(fired.next());
        assert_eq!((first, second), (Some(TimerKey::Lint(7)), Some(TimerKey::SessionSave)));
        timers.arm(TimerKey::StatusExpiry, 5);
        assert_eq!(iced::futures::executor::block_on(fired.next()), Some(TimerKey::StatusExpiry), "the cancelled key never fired");
    }
}
