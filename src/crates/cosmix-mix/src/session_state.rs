//! Owned, bounded stage-A state. No evaluator, transport or child waits here.
use crate::editor::Generation;
use cosmix_lib_bus::native_session::{DecimalU64, HexBytes, RecordRef, SessionRecord};
use cosmix_lib_client::session::boottime_ms;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

const REPLAY: usize = 64;
const MAX_CWD: usize = 4096;
static STATE: OnceLock<Mutex<Reducer>> = OnceLock::new();

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
    Idle,
    PromptPreparing,
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
    LineAbandoned,
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
    pub duration_ms: Option<DecimalU64>,
    /// Fixed diagnostics only; command source, output and errors are not retained.
    pub diagnostic: Option<&'static str>,
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
    pub prompt_binding_generation: DecimalU64,
    pub prompt_source: Option<Source>,
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
}

pub(crate) struct Reducer {
    origin: u64,
    snapshot: Snapshot,
    replay: VecDeque<Event>,
    next_command: u64,
    foreground_return: Phase,
    foreground_depth: u32,
    evaluation_started_ms: Option<u64>,
    pending_prompt: Option<(Generation, bool, Option<Source>)>,
}
impl Reducer {
    fn new() -> Self {
        Self {
            origin: boottime_ms().unwrap_or(0),
            snapshot: Snapshot {
                version: 1,
                source: None,
                sequence: DecimalU64(0),
                phase: Phase::Starting,
                cwd: None,
                cwd_truncated: false,
                cwd_observed_ms: DecimalU64(0),
                prompt_generation: DecimalU64(0),
                prompt_binding_generation: DecimalU64(0),
                prompt_source: None,
                continuation: false,
                command_id: None,
                transition_ms: DecimalU64(0),
            },
            replay: VecDeque::with_capacity(REPLAY),
            next_command: 0,
            foreground_return: Phase::Starting,
            foreground_depth: 0,
            evaluation_started_ms: None,
            pending_prompt: None,
        }
    }
    fn prepare_prompt(&mut self, continuation: bool) -> Option<Generation> {
        let session = self
            .snapshot
            .source
            .as_ref()
            .map_or(0, |s| s.record.binding_generation.0);
        // A reserved ticket the editor took but never activated is re-presented,
        // not re-minted. This counter only advances on activation, so a mint
        // would repeat the number anyway; stating the re-presentation makes the
        // pairing with the editor's unspent-generation rule explicit rather than
        // an accident of two counters happening to agree. An attachment change
        // is a genuinely new prompt and does mint.
        let prompt = match self.pending_prompt {
            Some((pending, _, _)) if pending.session == session => pending.prompt,
            _ => self.snapshot.prompt_generation.0.checked_add(1)?,
        };
        let generation = Generation { session, prompt };
        self.pending_prompt = Some((generation, continuation, self.snapshot.source.clone()));
        Some(generation)
    }
    fn activate_prompt(&mut self, generation: Generation) {
        if self
            .pending_prompt
            .as_ref()
            .is_none_or(|(expected, _, _)| *expected != generation)
        {
            return;
        }
        let (_, continuation, source) = self.pending_prompt.take().unwrap();
        self.snapshot.prompt_binding_generation = DecimalU64(generation.session);
        self.snapshot.prompt_source = source;
        self.commit(Transition::PromptReady { continuation });
        debug_assert_eq!(self.snapshot.prompt_generation.0, generation.prompt);
    }
    fn now(&self) -> u64 {
        boottime_ms()
            .unwrap_or(u64::MAX)
            .saturating_sub(self.origin)
    }
    fn commit(&mut self, mut transition: Transition) {
        let now = self.now();
        let s = &mut self.snapshot;
        let mut duration_ms = None;
        let mut diagnostic = None;
        // Exhaustion is terminal, never wrap a generation into apparent freshness.
        let Some(sequence) = s.sequence.0.checked_add(1) else {
            return;
        };
        match &mut transition {
            Transition::ShellReady => s.phase = Phase::Idle,
            Transition::PromptPreparing => s.phase = Phase::PromptPreparing,
            Transition::PromptReady { continuation } => {
                let Some(generation) = s.prompt_generation.0.checked_add(1) else {
                    return;
                };
                s.prompt_generation = DecimalU64(generation);
                s.continuation = *continuation;
                s.phase = Phase::PromptReady;
            }
            // A line is the shell's, not the prompt's: the window in which it is
            // classified and alias-expanded must never read `idle`, the one
            // phase an execution admission would accept. The id belongs to the
            // reducer, so it is minted here and the window reports
            // evaluating-with-id rather than an unidentified evaluation.
            Transition::LineAccepted => {
                let Some(id) = self.next_command.checked_add(1) else {
                    return;
                };
                self.next_command = id;
                s.command_id = Some(DecimalU64(id));
                s.phase = Phase::Evaluating;
            }
            // The line turned out to run nothing (empty, incomplete or a parse
            // error). Close its window truthfully instead of leaving a phantom
            // evaluation standing until the next prompt.
            Transition::LineAbandoned => s.phase = Phase::Idle,
            Transition::EvaluationAccepted => {
                // LineAccepted already minted this line's id; adopt it so one
                // accepted line is one command, not two.
                if s.command_id.is_none() {
                    let Some(id) = self.next_command.checked_add(1) else {
                        return;
                    };
                    self.next_command = id;
                    s.command_id = Some(DecimalU64(id));
                }
            }
            Transition::EvaluationStarted => {
                self.evaluation_started_ms = Some(now);
                s.phase = Phase::Evaluating;
            }
            Transition::EvaluationFinished => {
                duration_ms = self
                    .evaluation_started_ms
                    .take()
                    .map(|start| DecimalU64(now.saturating_sub(start)));
                s.phase = Phase::Idle;
            }
            Transition::ForegroundChanged { active } => {
                if *active {
                    debug_assert_eq!(self.foreground_depth, 0, "nested foreground bracket");
                    if self.foreground_depth == 0 {
                        self.foreground_return = s.phase;
                    }
                    self.foreground_depth = self.foreground_depth.saturating_add(1);
                    s.phase = Phase::ForegroundChild;
                } else {
                    debug_assert!(self.foreground_depth > 0, "unbalanced foreground bracket");
                    self.foreground_depth = self.foreground_depth.saturating_sub(1);
                    if self.foreground_depth == 0 {
                        s.phase = self.foreground_return;
                    }
                }
            }
            Transition::DirectoryChanged { cwd } => {
                s.cwd_truncated = cwd.as_ref().is_some_and(|v| v.len() > MAX_CWD);
                if s.cwd_truncated {
                    diagnostic = Some("cwd truncated");
                }
                if cwd.is_none() {
                    diagnostic = Some("cwd unavailable");
                }
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
            duration_ms,
            diagnostic,
            transition,
        });
        // Both events name the command they closed; only the snapshot that
        // follows them reports no command in flight.
        if matches!(
            self.replay.back().map(|e| &e.transition),
            Some(Transition::EvaluationFinished | Transition::LineAbandoned)
        ) {
            s.command_id = None;
        }
    }
    fn view(&self) -> View {
        let now = self.now();
        View {
            snapshot: self.snapshot.clone(),
            sampled_ms: DecimalU64(now),
            transition_age_ms: DecimalU64(now.saturating_sub(self.snapshot.transition_ms.0)),
            cwd_age_ms: DecimalU64(now.saturating_sub(self.snapshot.cwd_observed_ms.0)),
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
        state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .commit(transition);
    }
}
pub(crate) fn view() -> Option<View> {
    STATE.get().map(|state| {
        state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .view()
    })
}
/// Reserve the next value without publishing it. Only actual activation commits.
pub(crate) fn prepare_prompt(continuation: bool) -> Option<Generation> {
    STATE.get().and_then(|state| {
        state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .prepare_prompt(continuation)
    })
}
pub(crate) fn prompt_activated(generation: Generation) {
    if let Some(state) = STATE.get() {
        state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .activate_prompt(generation);
    }
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
    } else if enabled() {
        // Classified as running nothing: the window LineAccepted opened has to
        // close here, or a line that never executed keeps reporting evaluating.
        commit(Transition::LineAbandoned);
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
    fn prompt_ack_is_the_only_generation_commit() {
        let mut state = Reducer::new();
        assert_eq!(state.snapshot.phase, Phase::Starting);
        state.commit(Transition::ShellReady);
        assert_eq!(state.snapshot.phase, Phase::Idle);
        state.commit(Transition::PromptPreparing);
        assert_eq!(state.snapshot.phase, Phase::PromptPreparing);
        let ticket = state.prepare_prompt(false).unwrap();
        // Failed/suspended begin has no Editing ack and publishes nothing.
        assert_eq!(state.snapshot.prompt_generation, DecimalU64(0));
        let sequence = state.snapshot.sequence;
        state.activate_prompt(Generation {
            prompt: ticket.prompt + 1,
            ..ticket
        });
        assert_eq!(state.snapshot.sequence, sequence);
        state.activate_prompt(ticket);
        assert_eq!(state.snapshot.prompt_generation.0, ticket.prompt);
        assert_eq!(state.snapshot.phase, Phase::PromptReady);
        let sequence = state.snapshot.sequence;
        state.activate_prompt(ticket); // redraw/resume, not a new prompt
        assert_eq!(state.snapshot.sequence, sequence);
        // An accepted line is never idle: classification and alias expansion
        // run inside an identified evaluation, not in the one phase an
        // execution admission would accept.
        state.commit(Transition::LineAccepted);
        assert_eq!(state.snapshot.phase, Phase::Evaluating);
        let accepted = state.snapshot.command_id.unwrap();
        state.commit(Transition::LineAbandoned);
        assert_eq!(state.snapshot.phase, Phase::Idle);
        assert_eq!(state.replay.back().unwrap().command_id, Some(accepted));
        assert_eq!(state.snapshot.command_id, None);
        let continuation = state.prepare_prompt(true).unwrap();
        assert_eq!(state.snapshot.phase, Phase::Idle);
        state.activate_prompt(continuation);
        assert!(state.snapshot.continuation);
        assert_eq!(state.snapshot.prompt_generation.0, ticket.prompt + 1);
        state.commit(Transition::EvaluationAccepted);
        state.commit(Transition::EvaluationStarted);
        assert_eq!(state.snapshot.phase, Phase::Evaluating);
        assert!(state.snapshot.command_id.is_some());
        state.commit(Transition::EvaluationFinished);
        assert_eq!(state.snapshot.phase, Phase::Idle);
    }

    #[test]
    fn suspend_inclusive_age_is_not_capped_at_last_transition() {
        let mut state = Reducer::new();
        // Deterministically model a snapshot captured eight hours earlier on
        // the same suspend-inclusive clock. No host suspend is required.
        let eight_hours = 8 * 60 * 60 * 1000;
        state.origin = state.origin.saturating_sub(eight_hours);
        let view = state.view();
        let elapsed = boottime_ms().unwrap().saturating_sub(state.origin);
        assert!(view.cwd_age_ms.0 <= elapsed);
        assert_eq!(view.cwd_age_ms, view.transition_age_ms);
        assert!(view.cwd_age_ms.0 >= boottime_ms().unwrap().min(eight_hours).saturating_sub(100));
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "unbalanced foreground bracket")]
    fn foreground_underflow_is_detected() {
        Reducer::new().commit(Transition::ForegroundChanged { active: false });
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "nested foreground bracket")]
    fn nested_foreground_is_detected() {
        let mut state = Reducer::new();
        state.commit(Transition::ForegroundChanged { active: true });
        state.commit(Transition::ForegroundChanged { active: true });
    }
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
        assert_eq!(state.replay.front().unwrap().sequence, DecimalU64(37));
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

    #[test]
    fn an_unactivated_ticket_is_represented_not_reminted() {
        let mut state = Reducer::new();
        let ticket = state.prepare_prompt(false).unwrap();
        // The editor has taken this prompt number; a Begin that never reached
        // Editing leaves it taken. Offering a different number would present
        // one the editor refuses, so the reserved ticket is offered again.
        assert!(state.prepare_prompt(true).unwrap() == ticket);
        assert_eq!(state.snapshot.prompt_generation, DecimalU64(0));
        state.activate_prompt(ticket);
        assert_eq!(state.snapshot.prompt_generation.0, ticket.prompt);
        assert!(state.snapshot.continuation);
        let sequence = state.snapshot.sequence;
        state.activate_prompt(ticket);
        assert_eq!(
            state.snapshot.sequence, sequence,
            "one activation, one commit"
        );
        // Taken up: the next prompt is genuinely a new one.
        assert_eq!(
            state.prepare_prompt(false).unwrap().prompt,
            ticket.prompt + 1
        );
    }

    #[test]
    fn an_accepted_line_keeps_one_command_identity_through_evaluation() {
        let mut state = Reducer::new();
        state.commit(Transition::LineAccepted);
        let id = state.snapshot.command_id.unwrap();
        // The reducer mints at LineAccepted; the evaluation adopts that id
        // rather than opening a second command for the same line.
        state.commit(Transition::EvaluationAccepted);
        assert_eq!(state.snapshot.command_id, Some(id));
        state.commit(Transition::EvaluationStarted);
        assert_eq!(state.snapshot.phase, Phase::Evaluating);
        state.commit(Transition::EvaluationFinished);
        assert_eq!(state.snapshot.phase, Phase::Idle);
        assert_eq!(state.snapshot.command_id, None);
        state.commit(Transition::LineAccepted);
        assert!(state.snapshot.command_id.unwrap().0 > id.0);
    }

    #[test]
    fn attachment_recovery_keeps_sequence_and_historical_provenance() {
        let mut state = Reducer::new();
        let mut source = Source {
            broker_epoch: HexBytes([1; 16]),
            record: RecordRef {
                record_id: HexBytes([2; 16]),
                incarnation: HexBytes([3; 16]),
                binding_generation: DecimalU64(1),
            },
            instance_id: HexBytes([4; 16]),
            pane_id: Some(DecimalU64(1)),
            pane_generation: Some(DecimalU64(1)),
        };
        state.commit(Transition::AttachmentChanged {
            source: Some(source.clone()),
        });
        state.commit(Transition::PromptReady {
            continuation: false,
        });
        let original = state.snapshot.clone();
        state.commit(Transition::AttachmentChanged { source: None });
        source.record.binding_generation = DecimalU64(2);
        source.pane_generation = Some(DecimalU64(2));
        state.commit(Transition::AttachmentChanged {
            source: Some(source.clone()),
        });
        assert!(state.snapshot.sequence.0 > original.sequence.0);
        assert_eq!(state.snapshot.prompt_generation, original.prompt_generation);
        assert_eq!(state.replay[1].source, original.source);
        assert_eq!(state.snapshot.source, Some(source));
    }
}
