use crate::{
    ActionId, BindingLayer, BindingScope, Chord, EffectiveBinding, FocusContext, KeyStroke, Keymap,
    RawInput, RawInputState, RepeatPolicy, Tick,
};

/// Mutable chord progress owned by an input adapter.
///
/// State retains the accepted prefix, deadline, and a non-executable comparison
/// stamp of the admitting context/candidates—never a cached fallback action.
/// Every fallback is recomputed from the current keymap and current focus
/// context, while the stamp prevents an old prefix being reinterpreted as a
/// newly introduced binding after modal, focus, or hot-reload changes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResolveState {
    sequence: Vec<KeyStroke>,
    deadline: Option<Tick>,
    stamp: Option<PendingStamp>,
}

impl ResolveState {
    /// Whether the resolver is waiting for another chord stroke.
    pub fn is_pending(&self) -> bool {
        !self.sequence.is_empty()
    }

    /// Current accepted prefix.
    pub fn sequence(&self) -> &[KeyStroke] {
        &self.sequence
    }

    /// Current inter-stroke deadline.
    pub fn deadline(&self) -> Option<Tick> {
        self.deadline
    }

    /// Discard a partial chord without resolving its shorter fallback.
    pub fn cancel(&mut self) {
        self.sequence.clear();
        self.deadline = None;
        self.stamp = None;
    }
}

/// Why otherwise matching input was kept by its focus owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SuppressionReason {
    /// An editable owns the stroke and the binding did not opt in.
    EditableFocus,
    /// A modal owns all input but had no matching modal-scoped binding.
    ModalCapture(String),
    /// A programmatically built focus context violated the scope grammar.
    InvalidFocusContext(String),
}

/// Resolution state of the current input or timeout poll.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolveOutcome {
    /// The current operation completed an unambiguous action.
    Complete,
    /// A longer chord can still win before this deadline.
    Pending {
        /// Inter-stroke deadline supplied from the keymap timeout.
        deadline: Tick,
    },
    /// No binding matched the current operation.
    NoMatch,
    /// Input was deliberately retained by the focus owner.
    Suppressed(SuppressionReason),
    /// Releases do not resolve actions.
    IgnoredRelease,
    /// Matching bindings reject operating-system repeat presses.
    IgnoredRepeat,
}

/// Non-fatal issue observed alongside the current resolution outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolveDiagnostic {
    /// Equal-priority exact bindings name different actions; none fired.
    BindingConflict {
        /// Prefix that completed the conflicting bindings.
        chord: Chord,
        /// Stable sorted conflicting action ids.
        actions: Vec<ActionId>,
    },
    /// Programmatic raw input used a key outside the stable vocabulary.
    InvalidInput(String),
    /// Focus or relevant keymap data changed while a chord was pending.
    PendingInvalidated {
        /// State change that made the old prefix unsafe to retain.
        reason: PendingInvalidation,
    },
}

/// Why a partial chord was dropped before timeout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PendingInvalidation {
    /// Editable/modal/tag focus context changed.
    FocusContextChanged,
    /// The effective candidate bindings or timeout changed.
    KeymapChanged,
}

/// Actions emitted, current disposition, and independent diagnostics.
///
/// Earlier timeout/fallback diagnostics never replace `outcome`: if an
/// expired prefix conflicts and the new stroke begins another chord, the
/// result carries both the conflict in `diagnostics` and `Pending` as the
/// current outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    /// Unambiguous actions to enqueue in order.
    pub actions: Vec<ActionId>,
    /// Disposition of the current input or timeout poll.
    pub outcome: ResolveOutcome,
    /// Issues that did not prevent an independent current outcome.
    pub diagnostics: Vec<ResolveDiagnostic>,
}

impl Resolved {
    fn new(outcome: ResolveOutcome) -> Self {
        Self {
            actions: Vec::new(),
            outcome,
            diagnostics: Vec::new(),
        }
    }

    fn append_prior(&mut self, mut prior: Self) {
        prior.actions.append(&mut self.actions);
        self.actions = prior.actions;
        prior.diagnostics.append(&mut self.diagnostics);
        self.diagnostics = prior.diagnostics;
    }
}

/// Resolve one raw input event against focus, layered bindings and chord state.
///
/// Precedence applies only once bindings complete the same chord:
///
/// 1. a captured modal is exclusive; otherwise matching focus tags beat global;
/// 2. custom exact bindings beat default exact bindings;
/// 3. longer chords across any layer preserve shorter completed fallbacks;
/// 4. equal remaining exact bindings report [`ResolveDiagnostic::BindingConflict`].
///
/// A shared prefix is not a conflict. For example, default `Ctrl+K` remains the
/// timeout fallback behind custom `Ctrl+K, Ctrl+C`, and the inverse does too.
/// The supplied [`Tick`] drives timeout state; this function reads no clock.
pub fn resolve(
    input: RawInput,
    context: &FocusContext,
    keymap: &Keymap,
    state: &mut ResolveState,
    now: Tick,
) -> Resolved {
    if input.state == RawInputState::Released {
        return Resolved::new(ResolveOutcome::IgnoredRelease);
    }
    if let Err(error) = input.key.validate() {
        state.cancel();
        let mut result = Resolved::new(ResolveOutcome::NoMatch);
        result
            .diagnostics
            .push(ResolveDiagnostic::InvalidInput(error.to_string()));
        return result;
    }
    if let Err(error) = context.validate() {
        state.cancel();
        return Resolved::new(ResolveOutcome::Suppressed(
            SuppressionReason::InvalidFocusContext(error.to_string()),
        ));
    }

    let mut prior = invalidate_stale_pending(context, keymap, state);
    let expired = state
        .deadline
        .filter(|deadline| now >= *deadline)
        .map(|_| resolve_timeout(context, keymap, state, now));
    if let Some(mut expired) = expired {
        if let Some(prior) = prior.take() {
            expired.append_prior(prior);
        }
        prior = Some(expired);
    }

    // An OS repeat of the last accepted prefix key is not a deliberate next
    // chord stroke. `Allow` applies only once a complete binding is resolving.
    let mut current = if input.repeat
        && state
            .sequence
            .last()
            .is_some_and(|last| *last == input.stroke())
    {
        Resolved::new(ResolveOutcome::IgnoredRepeat)
    } else {
        resolve_stroke(input, context, keymap, state, now, true)
    };
    if let Some(prior) = prior {
        current.append_prior(prior);
    }
    current
}

/// Revalidate and resolve or retain a partial chord against current state.
///
/// Both context and keymap are mandatory: changing modal ownership, moving
/// focus into an editable, deleting a binding, or replacing a custom layer
/// immediately invalidates a stale prefix. Before the deadline this function
/// returns `Pending` only if a current longer candidate still exists.
pub fn resolve_timeout(
    context: &FocusContext,
    keymap: &Keymap,
    state: &mut ResolveState,
    now: Tick,
) -> Resolved {
    let Some(deadline) = state.deadline else {
        return Resolved::new(ResolveOutcome::NoMatch);
    };
    if let Err(error) = context.validate() {
        state.cancel();
        return Resolved::new(ResolveOutcome::Suppressed(
            SuppressionReason::InvalidFocusContext(error.to_string()),
        ));
    }
    if let Some(invalidated) = invalidate_stale_pending(context, keymap, state) {
        return invalidated;
    }

    let repeated = state.stamp.as_ref().is_some_and(|stamp| stamp.repeated);
    let evaluation = evaluate_prefix(&state.sequence, context, keymap, repeated);
    let Eligible::Bindings(candidates) = evaluation.eligible else {
        state.cancel();
        return Resolved::new(evaluation.eligible.outcome());
    };

    let exact = exact_bindings(&candidates, state.sequence.len());
    let has_longer = candidates
        .iter()
        .any(|binding| binding.chord.strokes().len() > state.sequence.len());
    if now < deadline && has_longer {
        return Resolved::new(ResolveOutcome::Pending { deadline });
    }

    let sequence = state.sequence.clone();
    state.cancel();
    complete_exact(exact, &sequence)
}

fn resolve_stroke(
    input: RawInput,
    context: &FocusContext,
    keymap: &Keymap,
    state: &mut ResolveState,
    now: Tick,
    may_reprocess: bool,
) -> Resolved {
    let previous = state.sequence.clone();
    let prior_repeat = state.stamp.as_ref().is_some_and(|stamp| stamp.repeated);
    let admitting_repeat = input.repeat || prior_repeat;
    state.sequence.push(input.stroke());
    let evaluation = evaluate_prefix(&state.sequence, context, keymap, admitting_repeat);

    let Eligible::Bindings(candidates) = evaluation.eligible else {
        let outcome = evaluation.eligible.outcome();
        if outcome == ResolveOutcome::IgnoredRepeat {
            state.sequence = previous;
            return Resolved::new(outcome);
        }

        let mut fallback = complete_fallback(&previous, context, keymap, prior_repeat);
        state.cancel();
        if may_reprocess {
            let mut retried = resolve_stroke(input, context, keymap, state, now, false);
            retried.append_prior(fallback);
            return retried;
        }
        if fallback.actions.is_empty() && fallback.diagnostics.is_empty() {
            fallback.outcome = outcome;
        }
        return fallback;
    };

    let exact = exact_bindings(&candidates, state.sequence.len());
    let has_longer = candidates
        .iter()
        .any(|binding| binding.chord.strokes().len() > state.sequence.len());
    if has_longer {
        let deadline = now.saturating_add(keymap.chord_timeout_ms);
        state.deadline = Some(deadline);
        state.stamp = Some(PendingStamp::new(
            context,
            keymap,
            &state.sequence,
            &candidates,
            admitting_repeat,
        ));
        return Resolved::new(ResolveOutcome::Pending { deadline });
    }

    let sequence = state.sequence.clone();
    state.cancel();
    complete_exact(exact, &sequence)
}

fn complete_fallback(
    sequence: &[KeyStroke],
    context: &FocusContext,
    keymap: &Keymap,
    repeated: bool,
) -> Resolved {
    if sequence.is_empty() {
        return Resolved::new(ResolveOutcome::NoMatch);
    }
    let evaluation = evaluate_prefix(sequence, context, keymap, repeated);
    let Eligible::Bindings(candidates) = evaluation.eligible else {
        return Resolved::new(evaluation.eligible.outcome());
    };
    complete_exact(exact_bindings(&candidates, sequence.len()), sequence)
}

fn complete_exact(bindings: Vec<EffectiveBinding<'_>>, sequence: &[KeyStroke]) -> Resolved {
    if bindings.is_empty() {
        return Resolved::new(ResolveOutcome::NoMatch);
    }
    let maximum_scope = bindings
        .iter()
        .map(|binding| binding.scope.rank())
        .max()
        .expect("exact bindings are non-empty");
    let maximum_layer = bindings
        .iter()
        .filter(|binding| binding.scope.rank() == maximum_scope)
        .map(|binding| binding.layer.rank())
        .max()
        .expect("maximum scope has a binding");
    let mut actions: Vec<_> = bindings
        .iter()
        .filter(|binding| {
            binding.scope.rank() == maximum_scope && binding.layer.rank() == maximum_layer
        })
        .map(|binding| binding.action)
        .collect();
    actions.sort_unstable();
    actions.dedup();
    match actions.as_slice() {
        [action] => Resolved {
            actions: vec![*action],
            outcome: ResolveOutcome::Complete,
            diagnostics: Vec::new(),
        },
        [] => Resolved::new(ResolveOutcome::NoMatch),
        _ => Resolved {
            actions: Vec::new(),
            outcome: ResolveOutcome::NoMatch,
            diagnostics: vec![ResolveDiagnostic::BindingConflict {
                chord: Chord::new(sequence.to_vec()).expect("resolved sequence is non-empty"),
                actions,
            }],
        },
    }
}

fn exact_bindings<'a>(
    candidates: &[EffectiveBinding<'a>],
    length: usize,
) -> Vec<EffectiveBinding<'a>> {
    candidates
        .iter()
        .copied()
        .filter(|binding| binding.chord.strokes().len() == length)
        .collect()
}

struct Evaluation<'a> {
    eligible: Eligible<'a>,
}

enum Eligible<'a> {
    Bindings(Vec<EffectiveBinding<'a>>),
    NoMatch,
    Suppressed(SuppressionReason),
    IgnoredRepeat,
}

impl Eligible<'_> {
    fn outcome(&self) -> ResolveOutcome {
        match self {
            Self::Bindings(_) => unreachable!("bindings do not have an empty outcome"),
            Self::NoMatch => ResolveOutcome::NoMatch,
            Self::Suppressed(reason) => ResolveOutcome::Suppressed(reason.clone()),
            Self::IgnoredRepeat => ResolveOutcome::IgnoredRepeat,
        }
    }
}

fn evaluate_prefix<'a>(
    prefix: &[KeyStroke],
    context: &FocusContext,
    keymap: &'a Keymap,
    repeated: bool,
) -> Evaluation<'a> {
    let matching: Vec<_> = keymap
        .effective_bindings()
        .filter(|binding| starts_with(binding.chord.strokes(), prefix))
        .collect();
    if matching.is_empty() {
        return Evaluation {
            eligible: context
                .modal_scope
                .as_ref()
                .map_or(Eligible::NoMatch, |modal| {
                    Eligible::Suppressed(SuppressionReason::ModalCapture(modal.clone()))
                }),
        };
    }

    let admitted: Vec<_> = matching
        .into_iter()
        .filter(|binding| context.admits(binding.scope))
        .collect();
    if admitted.is_empty() {
        return Evaluation {
            eligible: context
                .modal_scope
                .as_ref()
                .map_or(Eligible::NoMatch, |modal| {
                    Eligible::Suppressed(SuppressionReason::ModalCapture(modal.clone()))
                }),
        };
    }

    let focus_eligible: Vec<_> = admitted
        .into_iter()
        .filter(|binding| !context.focused_editable || binding.allow_in_editable)
        .collect();
    if focus_eligible.is_empty() {
        return Evaluation {
            eligible: Eligible::Suppressed(SuppressionReason::EditableFocus),
        };
    }

    let repeat_eligible: Vec<_> = focus_eligible
        .into_iter()
        .filter(|binding| !repeated || binding.repeat == RepeatPolicy::Allow)
        .collect();
    Evaluation {
        eligible: if repeat_eligible.is_empty() {
            Eligible::IgnoredRepeat
        } else {
            Eligible::Bindings(repeat_eligible)
        },
    }
}

fn starts_with(chord: &[KeyStroke], prefix: &[KeyStroke]) -> bool {
    chord.len() >= prefix.len() && chord[..prefix.len()] == *prefix
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingStamp {
    context: FocusContext,
    repeated: bool,
    timeout_ms: u64,
    candidates: Vec<BindingStamp>,
}

impl PendingStamp {
    fn new(
        context: &FocusContext,
        keymap: &Keymap,
        prefix: &[KeyStroke],
        candidates: &[EffectiveBinding<'_>],
        repeated: bool,
    ) -> Self {
        debug_assert_eq!(
            canonical_candidates(candidates),
            eligible_candidate_stamps(prefix, context, keymap, repeated)
        );
        Self {
            context: context.clone(),
            repeated,
            timeout_ms: keymap.chord_timeout_ms,
            candidates: canonical_candidates(candidates),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct BindingStamp {
    action: ActionId,
    chord: Chord,
    scope: BindingScope,
    repeat: RepeatPolicy,
    allow_in_editable: bool,
    layer: BindingLayer,
}

fn canonical_candidates(candidates: &[EffectiveBinding<'_>]) -> Vec<BindingStamp> {
    let mut stamps: Vec<_> = candidates.iter().map(BindingStamp::from).collect();
    stamps.sort_unstable();
    stamps.dedup();
    stamps
}

fn eligible_candidate_stamps(
    prefix: &[KeyStroke],
    context: &FocusContext,
    keymap: &Keymap,
    repeated: bool,
) -> Vec<BindingStamp> {
    match evaluate_prefix(prefix, context, keymap, repeated).eligible {
        Eligible::Bindings(candidates) => canonical_candidates(&candidates),
        Eligible::NoMatch | Eligible::Suppressed(_) | Eligible::IgnoredRepeat => Vec::new(),
    }
}

impl From<&EffectiveBinding<'_>> for BindingStamp {
    fn from(binding: &EffectiveBinding<'_>) -> Self {
        Self {
            action: binding.action,
            chord: binding.chord.clone(),
            scope: binding.scope.clone(),
            repeat: binding.repeat,
            allow_in_editable: binding.allow_in_editable,
            layer: binding.layer,
        }
    }
}

fn invalidate_stale_pending(
    context: &FocusContext,
    keymap: &Keymap,
    state: &mut ResolveState,
) -> Option<Resolved> {
    let stamp = state.stamp.as_ref()?;
    let old_context_candidates =
        eligible_candidate_stamps(&state.sequence, &stamp.context, keymap, stamp.repeated);
    let reason = if stamp.timeout_ms != keymap.chord_timeout_ms
        || stamp.candidates != old_context_candidates
    {
        Some(PendingInvalidation::KeymapChanged)
    } else {
        let new_context_candidates =
            eligible_candidate_stamps(&state.sequence, context, keymap, stamp.repeated);
        (old_context_candidates != new_context_candidates)
            .then_some(PendingInvalidation::FocusContextChanged)
    }?;
    state.cancel();
    let mut resolved = Resolved::new(ResolveOutcome::NoMatch);
    resolved
        .diagnostics
        .push(ResolveDiagnostic::PendingInvalidated { reason });
    Some(resolved)
}
