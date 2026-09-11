//! Owned, bounded stage-A state. No evaluator, transport or child waits here.
use cosmix_lib_bus::native_session::{DecimalU64, HexBytes, RecordRef, SessionRecord};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

const REPLAY: usize = 64;
const MAX_CWD: usize = 4096;
static STATE: OnceLock<Mutex<Reducer>> = OnceLock::new();

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Source {
    pub broker_epoch: HexBytes<16>,
    pub record: RecordRef,
    pub instance_id: HexBytes<16>,
    pub pane_id: Option<DecimalU64>,
    pub pane_generation: Option<DecimalU64>,
}
impl From<&SessionRecord> for Source {
    fn from(record: &SessionRecord) -> Self {
        Self {
            broker_epoch: record.broker_epoch,
            record: record.reference(),
            instance_id: record.instance_id,
            pane_id: record.pane_id,
            pane_generation: record.pane_generation,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Phase {
    Starting,
    PromptReady,
    Evaluating,
    ForegroundChild,
    Exiting,
}

/// Internal taxonomy is ready for later publication; stage A publishes no events.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Transition {
    ShellReady,
    PromptPreparing,
    PromptReady { continuation: bool },
    LineAccepted,
    EvaluationAccepted,
    EvaluationStarted,
    EvaluationFinished,
    ForegroundChanged { active: bool },
    DirectoryChanged { cwd: Option<String> },
    AttachmentChanged { source: Option<Source> },
    ShellExit,
    ShellReplacement,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Event {
    pub source: Option<Source>,
    pub sequence: DecimalU64,
    pub command_id: Option<DecimalU64>,
    pub timestamp_ms: DecimalU64,
    pub transition: Transition,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Snapshot {
    pub version: u8,
    pub source: Option<Source>,
    pub sequence: DecimalU64,
    pub phase: Phase,
    pub cwd: Option<String>,
    pub cwd_truncated: bool,
    pub cwd_observed_ms: DecimalU64,
    pub prompt_generation: DecimalU64,
    pub continuation: bool,
    pub command_id: Option<DecimalU64>,
    pub transition_ms: DecimalU64,
}

#[derive(Serialize)]
pub(crate) struct View {
    pub snapshot: Snapshot,
    pub sampled_ms: DecimalU64,
    pub transition_age_ms: DecimalU64,
    pub cwd_age_ms: DecimalU64,
    pub oldest_retained_sequence: DecimalU64,
    /// A consumer behind the retained ring must discard replay and resnapshot.
    pub gap: bool,
}

pub(crate) struct Reducer {
    origin: Instant,
    snapshot: Snapshot,
    replay: VecDeque<Event>,
    next_command: u64,
    foreground_return: Phase,
}
impl Reducer {
    fn new() -> Self {
        Self {
            origin: Instant::now(),
            snapshot: Snapshot {
                version: 1,
                source: None,
                sequence: DecimalU64(0),
                phase: Phase::Starting,
                cwd: None,
                cwd_truncated: false,
                cwd_observed_ms: DecimalU64(0),
                prompt_generation: DecimalU64(0),
                continuation: false,
                command_id: None,
                transition_ms: DecimalU64(0),
            },
            replay: VecDeque::with_capacity(REPLAY),
            next_command: 0,
            foreground_return: Phase::Starting,
        }
    }
    fn now(&self) -> u64 {
        self.origin.elapsed().as_millis().min(u64::MAX as u128) as u64
    }
    fn commit(&mut self, mut transition: Transition) {
        let now = self.now();
        let s = &mut self.snapshot;
        // Exhaustion is terminal, never wrap a generation into apparent freshness.
        let Some(sequence) = s.sequence.0.checked_add(1) else {
            return;
        };
        match &mut transition {
            Transition::ShellReady => s.phase = Phase::Starting,
            Transition::PromptPreparing => s.phase = Phase::Evaluating,
            Transition::PromptReady { continuation } => {
                let Some(generation) = s.prompt_generation.0.checked_add(1) else {
                    return;
                };
                s.prompt_generation = DecimalU64(generation);
                s.continuation = *continuation;
                s.phase = Phase::PromptReady;
            }
            Transition::LineAccepted => s.phase = Phase::Starting,
            Transition::EvaluationAccepted => {
                let Some(id) = self.next_command.checked_add(1) else {
                    return;
                };
                self.next_command = id;
                s.command_id = Some(DecimalU64(id));
            }
            Transition::EvaluationStarted => s.phase = Phase::Evaluating,
            Transition::EvaluationFinished => s.phase = Phase::Starting,
            Transition::ForegroundChanged { active } => {
                if *active {
                    self.foreground_return = s.phase;
                    s.phase = Phase::ForegroundChild;
                } else {
                    s.phase = self.foreground_return;
                }
            }
            Transition::DirectoryChanged { cwd } => {
                s.cwd_truncated = cwd.as_ref().is_some_and(|v| v.len() > MAX_CWD);
                if let Some(value) = cwd {
                    let mut end = value.len().min(MAX_CWD);
                    while !value.is_char_boundary(end) {
                        end -= 1;
                    }
                    value.truncate(end);
                }
                s.cwd.clone_from(cwd);
                s.cwd_observed_ms = DecimalU64(now);
            }
            Transition::AttachmentChanged { source } => s.source.clone_from(source),
            Transition::ShellExit | Transition::ShellReplacement => s.phase = Phase::Exiting,
        }
        s.sequence = DecimalU64(sequence);
        s.transition_ms = DecimalU64(now);
        if self.replay.len() == REPLAY {
            self.replay.pop_front();
        }
        self.replay.push_back(Event {
            source: s.source.clone(),
            sequence: s.sequence,
            command_id: s.command_id,
            timestamp_ms: s.transition_ms,
            transition,
        });
        if matches!(
            self.replay.back().map(|e| &e.transition),
            Some(Transition::EvaluationFinished)
        ) {
            s.command_id = None;
        }
    }
    fn view(&self, after: Option<DecimalU64>) -> View {
        let now = self.now();
        let oldest = self
            .replay
            .front()
            .map_or(self.snapshot.sequence, |e| e.sequence);
        View {
            snapshot: self.snapshot.clone(),
            sampled_ms: DecimalU64(now),
            transition_age_ms: DecimalU64(now.saturating_sub(self.snapshot.transition_ms.0)),
            cwd_age_ms: DecimalU64(now.saturating_sub(self.snapshot.cwd_observed_ms.0)),
            oldest_retained_sequence: oldest,
            gap: after.is_some_and(|v| {
                v.0.saturating_add(1) < oldest.0 || v.0 > self.snapshot.sequence.0
            }),
        }
    }
}

pub(crate) fn enable() {
    let _ = STATE.set(Mutex::new(Reducer::new()));
}
pub(crate) fn enabled() -> bool {
    STATE.get().is_some()
}
pub(crate) fn commit(transition: Transition) {
    if let Some(state) = STATE.get() {
        state.lock().unwrap().commit(transition);
    }
}
pub(crate) fn view(after: Option<DecimalU64>) -> Option<View> {
    STATE.get().map(|state| state.lock().unwrap().view(after))
}
pub(crate) fn observe_directory() {
    if enabled() {
        commit(Transition::DirectoryChanged {
            cwd: std::env::current_dir()
                .ok()
                .map(|p| p.to_string_lossy().into_owned()),
        });
    }
}
pub(crate) struct Evaluation(bool);
pub(crate) fn evaluation(executing: bool) -> Evaluation {
    let active = executing && enabled();
    if active {
        commit(Transition::EvaluationAccepted);
        commit(Transition::EvaluationStarted);
    }
    Evaluation(active)
}
impl Drop for Evaluation {
    fn drop(&mut self) {
        if self.0 {
            commit(Transition::EvaluationFinished);
        }
    }
}
pub(crate) struct ShellLifetime;
impl Drop for ShellLifetime {
    fn drop(&mut self) {
        commit(Transition::ShellExit);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_replay_gap_and_atomic_snapshot() {
        fn send<T: Send>() {}
        send::<Reducer>();
        send::<Transition>();
        send::<Snapshot>();
        let mut state = Reducer::new();
        for _ in 0..100 {
            state.commit(Transition::PromptReady {
                continuation: false,
            });
        }
        assert_eq!(state.replay.len(), REPLAY);
        assert!(state.view(Some(DecimalU64(0))).gap);
        assert!(!state.view(Some(DecimalU64(99))).gap);
        assert_eq!(
            state.snapshot.sequence,
            state.replay.back().unwrap().sequence
        );
        assert_eq!(state.snapshot.prompt_generation, DecimalU64(100));
    }
    #[test]
    fn phases_command_identity_and_bounded_cwd() {
        let mut state = Reducer::new();
        state.commit(Transition::EvaluationAccepted);
        state.commit(Transition::EvaluationStarted);
        let id = state.snapshot.command_id;
        state.commit(Transition::ForegroundChanged { active: true });
        assert_eq!(state.snapshot.phase, Phase::ForegroundChild);
        assert_eq!(state.snapshot.command_id, id);
        state.commit(Transition::ForegroundChanged { active: false });
        assert_eq!(state.snapshot.phase, Phase::Evaluating);
        state.commit(Transition::DirectoryChanged {
            cwd: Some("é".repeat(MAX_CWD)),
        });
        assert_eq!(state.snapshot.cwd.as_ref().unwrap().len(), MAX_CWD);
        assert!(state.snapshot.cwd_truncated);
        state.commit(Transition::EvaluationFinished);
        assert_eq!(state.replay.back().unwrap().command_id, id);
        assert_eq!(state.snapshot.command_id, None);
        state.commit(Transition::EvaluationAccepted);
        assert!(state.snapshot.command_id.unwrap().0 > id.unwrap().0);
    }
}
