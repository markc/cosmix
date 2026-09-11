//! Pure owned-editor core. No tty, evaluator, Bus, or process-group ownership.
//!
//! The input owner must drain already-observed human activity before
//! processing control requests. Effects are instructions, not acknowledgements:
//! mode restoration must succeed before `Suspended`/`RestoredAndStopped`.
//! The admission owner still rechecks identity, deadline and revision before
//! consuming a prompt. A suspension acknowledgement alone is not permission.
pub mod buffer;
pub mod history;
pub mod input;
pub mod render;
pub mod runtime;
pub mod signals;
pub mod terminal;

use buffer::{Buffer, EditError};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Generation {
    /// Opaque attachment generation supplied by the session owner, not identity.
    pub session: u64,
    /// Zero identifies startup; BeginPrompt must use a strictly newer counter.
    pub prompt: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PromptProfile {
    /// Already evaluated by the evaluator owner.
    Primary(String),
    Continuation,
    /// Fixed prompt; no evaluator callbacks or fresh completion snapshots.
    Restricted,
}

impl PromptProfile {
    pub fn text(&self) -> &str {
        match self {
            Self::Primary(text) => text,
            Self::Continuation => "  > ",
            Self::Restricted => "jobs> ",
        }
    }
    pub fn allows_completion(&self) -> bool {
        !matches!(self, Self::Restricted)
    }
    pub fn allows_command(&self, command: &str) -> bool {
        !matches!(self, Self::Restricted)
            || matches!(command, "jobs" | "fg" | "bg" | "cancel" | "exit")
    }
}

/// Owned interaction state survives suspension. Partial decoder bytes count as
/// activity even when no grapheme has reached the buffer yet.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Interaction {
    pub paste: bool,
    pub search: Option<String>,
    pub completion: bool,
    pub decoder_pending: bool,
}

impl Interaction {
    fn idle(&self) -> bool {
        !self.paste && self.search.is_none() && !self.completion && !self.decoder_pending
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Idle,
    Activating,
    Editing,
    RestoringForSuspend,
    Suspended,
    RestoringForShutdown,
    Stopped,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    BeginPrompt {
        generation: Generation,
        profile: PromptProfile,
    },
    SuspendRequested {
        generation: Generation,
        edit_revision: u64,
    },
    Resume {
        generation: Generation,
        edit_revision: u64,
    },
    Shutdown {
        generation: Generation,
    },
}

/// Serial distinguishes two mode operations within the same prompt generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModeToken {
    pub generation: Generation,
    pub serial: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModeAction {
    EnterEditing,
    Restore,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    Editing {
        generation: Generation,
        edit_revision: u64,
    },
    Busy {
        generation: Generation,
        edit_revision: u64,
    },
    Suspended {
        generation: Generation,
        edit_revision: u64,
    },
    /// An execution reservation stands. The editor is STILL editing, still in
    /// raw mode and still reading: this is a promise not to accept a second
    /// admission, not a surrender of the terminal.
    Reserved {
        generation: Generation,
        edit_revision: u64,
    },
    RestoredAndStopped {
        generation: Generation,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    Modes {
        token: ModeToken,
        action: ModeAction,
    },
    Reply(Reply),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProtocolError {
    StaleGeneration,
    StaleRevision,
    InvalidState,
    StaleModeCompletion,
    ModeFailure,
    Exhausted,
    Edit(EditError),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Editor {
    session: u64,
    generation: Option<Generation>,
    profile: Option<PromptProfile>,
    state: State,
    buffer: Buffer,
    interaction: Interaction,
    revision: u64,
    serial: u64,
    pending: Option<ModeToken>,
    reserved: bool,
    /// Whether the current generation's Begin ever reached Editing.
    activated: bool,
}

impl Editor {
    /// Local lifecycle suspension preserves drafts. It does not reserve an
    /// empty prompt for evaluation; only SuspendRequested can do that.
    pub fn pause(
        &mut self,
        generation: Generation,
        revision: u64,
    ) -> Result<Effect, ProtocolError> {
        self.check(generation)?;
        if revision != self.revision {
            return Err(ProtocolError::StaleRevision);
        }
        if self.state != State::Editing {
            return Err(ProtocolError::InvalidState);
        }
        let effect = self.modes(State::RestoringForSuspend, ModeAction::Restore)?;
        self.reserved = false;
        Ok(effect)
    }
    /// The input owner has restored modes before returning a human line.
    pub fn finish_line(&mut self) -> Result<(), ProtocolError> {
        if self.state != State::Editing {
            return Err(ProtocolError::InvalidState);
        }
        self.state = State::Idle;
        Ok(())
    }
    pub fn new(session: u64) -> Self {
        Self {
            session,
            generation: Some(Generation { session, prompt: 0 }),
            profile: None,
            state: State::Idle,
            buffer: Buffer::default(),
            interaction: Interaction::default(),
            revision: 0,
            serial: 0,
            pending: None,
            reserved: false,
            // The zero sentinel is not a prompt anyone may present.
            activated: true,
        }
    }
    /// A prompt number is spent once its Begin reached Editing. Until then the
    /// owner may present it again: the reducer reserved that number and the
    /// editor took it, so any other number it could offer would be refused.
    fn spent(&self, generation: Generation) -> bool {
        self.generation.is_some_and(|current| {
            generation.prompt < current.prompt
                || (generation.prompt == current.prompt && self.activated)
        })
    }
    pub fn state(&self) -> State {
        self.state
    }
    /// Whether a granted execution reservation is standing. A local lifecycle
    /// `pause` never sets this, so suspension alone is not a permit.
    pub fn reserved(&self) -> bool {
        self.reserved
    }
    /// Would [`Self::consume_reservation`] succeed right now? The admission
    /// owner writes its visible echo between this question and the consume;
    /// both run on the thread that owns every field read here, so a true answer
    /// stays true across that write and the announcement can never be separated
    /// from the execution it announces.
    pub fn admissible(&self, generation: Generation, revision: u64) -> bool {
        self.generation == Some(generation)
            && revision == self.revision
            && self.state == State::Editing
            && self.reserved
    }
    pub fn buffer(&self) -> &Buffer {
        &self.buffer
    }
    pub fn interaction(&self) -> &Interaction {
        &self.interaction
    }
    /// Admission/completion revision advances for EVERY observed human action,
    /// even a no-op or rejected edit. Resize and mode transitions do not advance
    /// it. It never resets on undo, resume, or a new prompt.
    pub fn edit_revision(&self) -> u64 {
        self.revision
    }
    fn activity(&mut self) -> Result<(), ProtocolError> {
        if self.state != State::Editing {
            return Err(ProtocolError::InvalidState);
        }
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or(ProtocolError::Exhausted)?;
        // Human-first, enforced at the one place every observed human action
        // passes through. A byte — even one that is only part of an escape
        // sequence, even one that edits nothing — ends any standing
        // reservation. The admission owner then finds nothing to consume and
        // is refused, which is the whole of the rule: the person at the
        // keyboard wins ties.
        self.reserved = false;
        Ok(())
    }
    pub fn edit(
        &mut self,
        edit: impl FnOnce(&mut Buffer) -> Result<bool, EditError>,
    ) -> Result<bool, ProtocolError> {
        self.activity()?;
        edit(&mut self.buffer).map_err(ProtocolError::Edit)
    }
    pub fn set_interaction(&mut self, interaction: Interaction) -> Result<(), ProtocolError> {
        self.activity()?;
        self.interaction = interaction;
        Ok(())
    }
    fn check(&self, generation: Generation) -> Result<(), ProtocolError> {
        if self.generation != Some(generation) {
            Err(ProtocolError::StaleGeneration)
        } else {
            Ok(())
        }
    }
    fn modes(&mut self, state: State, action: ModeAction) -> Result<Effect, ProtocolError> {
        let serial = self.serial.checked_add(1).ok_or(ProtocolError::Exhausted)?;
        let token = ModeToken {
            generation: self.generation.ok_or(ProtocolError::InvalidState)?,
            serial,
        };
        self.serial = serial;
        self.pending = Some(token);
        self.state = state;
        Ok(Effect::Modes { token, action })
    }
    /// Only the local Begin path may move to a new attachment between prompts.
    /// All in-flight control commands still require an exact generation match.
    /// A Begin that took a prompt number without reaching Editing leaves the
    /// owner free to present that same number again; anything spent is stale.
    /// In-flight control commands are unaffected: `check` still demands an
    /// exact match.
    pub(crate) fn bind_prompt_session(
        &mut self,
        generation: Generation,
    ) -> Result<(), ProtocolError> {
        if self.state != State::Idle {
            return Err(ProtocolError::InvalidState);
        }
        if self.spent(generation) {
            return Err(ProtocolError::StaleGeneration);
        }
        self.session = generation.session;
        Ok(())
    }
    pub fn command(&mut self, command: Command) -> Result<Effect, ProtocolError> {
        match command {
            Command::BeginPrompt {
                generation,
                profile,
            } => {
                if generation.session != self.session || self.spent(generation) {
                    return Err(ProtocolError::StaleGeneration);
                }
                if self.state != State::Idle {
                    return Err(ProtocolError::InvalidState);
                }
                if self.serial == u64::MAX {
                    return Err(ProtocolError::Exhausted);
                }
                let bytes = render::MAX_LAYOUT_BYTES
                    .checked_sub(profile.text().len())
                    .ok_or(ProtocolError::Edit(EditError::Limit))?;
                self.generation = Some(generation);
                self.activated = false;
                self.profile = Some(profile);
                // Reserve prompt bytes up front so a successful edit can never
                // exceed the renderer's combined input bound later.
                self.buffer = Buffer::new(buffer::Limits {
                    bytes,
                    ..Default::default()
                });
                self.interaction = Interaction::default();
                self.modes(State::Activating, ModeAction::EnterEditing)
            }
            // A reservation changes NO terminal state. §8 puts the cooked-mode
            // restore at step 6, the commit — and that placement is what makes
            // human-first hold: while the reservation stands the editor is
            // still in raw mode reading byte by byte, so a half-typed character
            // is seen immediately, lands in the draft as ordinary typing, and
            // clears the reservation. Restoring here instead handed the window
            // to the canonical line discipline, where kernel echo painted the
            // keystroke into the announcement and the bytes then became the
            // admitted execution's stdin.
            Command::SuspendRequested {
                generation,
                edit_revision,
            } => {
                self.check(generation)?;
                if edit_revision != self.revision {
                    return Err(ProtocolError::StaleRevision);
                }
                if self.state != State::Editing
                    || !matches!(self.profile, Some(PromptProfile::Primary(_)))
                    || !self.buffer.text().is_empty()
                    || !self.interaction.idle()
                {
                    return Ok(Effect::Reply(Reply::Busy {
                        generation,
                        edit_revision: self.revision,
                    }));
                }
                self.reserved = true;
                Ok(Effect::Reply(Reply::Reserved {
                    generation,
                    edit_revision: self.revision,
                }))
            }
            Command::Resume {
                generation,
                edit_revision,
            } => {
                self.check(generation)?;
                if edit_revision != self.revision {
                    return Err(ProtocolError::StaleRevision);
                }
                if self.state != State::Suspended {
                    return Err(ProtocolError::InvalidState);
                }
                // Returning the prompt to the human ends any reservation over
                // it. An admission owner that comes back after a release —
                // late, cancelled, or timed out — finds nothing to consume.
                self.reserved = false;
                self.modes(State::Activating, ModeAction::EnterEditing)
            }
            Command::Shutdown { generation } => {
                self.check(generation)?;
                if self.state == State::Stopped {
                    return Err(ProtocolError::InvalidState);
                }
                // Replaces any pending operation. Its late completion is stale.
                self.modes(State::RestoringForShutdown, ModeAction::Restore)
            }
        }
    }
    /// Foreground ownership was lost before raw entry. No modes changed and no
    /// admission reservation is granted; a later foreground wake may Resume.
    pub fn modes_waiting(&mut self, token: ModeToken) -> Result<Reply, ProtocolError> {
        if self.pending != Some(token) {
            return Err(ProtocolError::StaleModeCompletion);
        }
        if self.state != State::Activating {
            return Err(ProtocolError::InvalidState);
        }
        self.pending = None;
        self.reserved = false;
        self.state = State::Suspended;
        Ok(Reply::Suspended {
            generation: token.generation,
            edit_revision: self.revision,
        })
    }
    /// Called ONLY after the terminal owner has completed the named operation.
    /// Failure is fail-closed; shutdown can retry restoration afterwards.
    pub fn modes_completed(
        &mut self,
        token: ModeToken,
        success: bool,
    ) -> Result<Reply, ProtocolError> {
        if self.pending != Some(token) {
            return Err(ProtocolError::StaleModeCompletion);
        }
        self.pending = None;
        if !success {
            self.state = State::Failed;
            return Err(ProtocolError::ModeFailure);
        }
        let generation = token.generation;
        let edit_revision = self.revision;
        let reply = match self.state {
            State::Activating => {
                self.state = State::Editing;
                // Reaching Editing is what spends the prompt number.
                self.activated = true;
                Reply::Editing {
                    generation,
                    edit_revision,
                }
            }
            State::RestoringForSuspend => {
                self.state = State::Suspended;
                Reply::Suspended {
                    generation,
                    edit_revision,
                }
            }
            State::RestoringForShutdown => {
                self.state = State::Stopped;
                Reply::RestoredAndStopped { generation }
            }
            _ => return Err(ProtocolError::InvalidState),
        };
        Ok(reply)
    }
    /// Step 5: the admission owner consumes the reservation after its final
    /// identity/deadline/revision checks, and the prompt ends here.
    ///
    /// The caller has already restored the terminal at this point, exactly as
    /// the human-line path does in `finish_line`. Both routes off a prompt
    /// therefore look the same to the editor: modes restored, then Idle.
    pub fn consume_reservation(
        &mut self,
        generation: Generation,
        revision: u64,
    ) -> Result<(), ProtocolError> {
        self.check(generation)?;
        if revision != self.revision {
            return Err(ProtocolError::StaleRevision);
        }
        if self.state != State::Editing || !self.reserved {
            return Err(ProtocolError::InvalidState);
        }
        self.state = State::Idle;
        self.reserved = false;
        Ok(())
    }
    /// Drop a reservation without executing anything, leaving the prompt and
    /// its draft exactly as they were. Nothing to undo: a reservation never
    /// changed the terminal in the first place.
    pub fn release_reservation(
        &mut self,
        generation: Generation,
        revision: u64,
    ) -> Result<(), ProtocolError> {
        self.check(generation)?;
        if revision != self.revision {
            return Err(ProtocolError::StaleRevision);
        }
        self.reserved = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn paste_cannot_exceed_combined_prompt_render_budget() {
        let mut e = editing(super::PromptProfile::Primary("p".repeat(2048)));
        let draft = "x".repeat(super::render::MAX_LAYOUT_BYTES - 2048 - 8);
        e.edit(|b| b.insert(&draft)).unwrap();
        let before = e.buffer().clone();
        assert_eq!(
            e.edit(|b| b.paste("0123456789012345")),
            Err(super::ProtocolError::Edit(super::buffer::EditError::Limit))
        );
        assert_eq!(e.buffer(), &before);
        assert_eq!(e.state(), super::State::Editing);
    }
    use super::*;
    const G: Generation = Generation {
        session: 7,
        prompt: 1,
    };
    fn token(effect: Effect) -> ModeToken {
        match effect {
            Effect::Modes { token, .. } => token,
            _ => panic!("expected mode operation"),
        }
    }
    fn editing(profile: PromptProfile) -> Editor {
        let mut e = Editor::new(7);
        let t = token(
            e.command(Command::BeginPrompt {
                generation: G,
                profile,
            })
            .unwrap(),
        );
        assert_eq!(e.state(), State::Activating);
        e.modes_completed(t, true).unwrap();
        e
    }
    #[test]
    fn a_prompt_number_is_spent_only_by_reaching_editing() {
        let mut e = Editor::new(7);
        e.bind_prompt_session(G).unwrap();
        let t = token(
            e.command(Command::BeginPrompt {
                generation: G,
                profile: PromptProfile::Primary("> ".into()),
            })
            .unwrap(),
        );
        // Foreground was lost before raw entry: the number is taken, but
        // nothing was reserved and no activation was published, so the reducer
        // may still present that same reserved ticket.
        assert!(matches!(
            e.modes_waiting(t).unwrap(),
            Reply::Suspended { .. }
        ));
        assert!(!e.spent(G));
        assert_eq!(e.bind_prompt_session(G), Err(ProtocolError::InvalidState));
        let revision = e.edit_revision();
        let t = token(
            e.command(Command::Resume {
                generation: G,
                edit_revision: revision,
            })
            .unwrap(),
        );
        // The foreground wake activates that same generation, once.
        assert_eq!(
            e.modes_completed(t, true),
            Ok(Reply::Editing {
                generation: G,
                edit_revision: revision
            })
        );
        e.finish_line().unwrap();
        assert!(e.spent(G));
        assert_eq!(
            e.bind_prompt_session(G),
            Err(ProtocolError::StaleGeneration)
        );
        assert_eq!(e.bind_prompt_session(Generation { prompt: 2, ..G }), Ok(()));
    }

    #[test]
    fn foreground_loss_during_activation_preserves_draft_and_can_resume() {
        let mut e = editing(PromptProfile::Primary("test> ".into()));
        e.edit(|b| b.insert("draft")).unwrap();
        let revision = e.edit_revision();
        let t = token(e.pause(G, revision).unwrap());
        e.modes_completed(t, true).unwrap();
        let t = token(
            e.command(Command::Resume {
                generation: G,
                edit_revision: revision,
            })
            .unwrap(),
        );
        assert!(matches!(
            e.modes_waiting(t).unwrap(),
            Reply::Suspended { .. }
        ));
        assert_eq!(e.buffer().text(), "draft");
        assert_eq!(e.edit_revision(), revision);
        assert!(e.consume_reservation(G, revision).is_err());
        let t = token(
            e.command(Command::Resume {
                generation: G,
                edit_revision: revision,
            })
            .unwrap(),
        );
        e.modes_completed(t, true).unwrap();
        assert_eq!(e.state(), State::Editing);
        assert_eq!(e.buffer().text(), "draft");
    }
    fn suspend(e: &mut Editor) -> Effect {
        e.command(Command::SuspendRequested {
            generation: G,
            edit_revision: e.edit_revision(),
        })
        .unwrap()
    }
    /// §8 step 6 puts the cooked-mode restore at the COMMIT. A reservation
    /// therefore changes nothing at all: the editor keeps editing, keeps its
    /// raw mode and keeps reading, which is what leaves the human able to
    /// out-race it with a single keystroke.
    #[test]
    fn a_reservation_changes_no_terminal_state_and_keeps_editing() {
        let mut e = editing(PromptProfile::Primary("> ".into()));
        let revision = e.edit_revision();
        let before = e.clone();
        let reply = suspend(&mut e);
        assert_eq!(
            reply,
            Effect::Reply(Reply::Reserved {
                generation: G,
                edit_revision: revision
            }),
            "a reservation must not produce a mode operation"
        );
        assert_eq!(e.state(), State::Editing, "the editor stopped editing");
        assert!(e.reserved());
        // Everything except the promise itself is untouched.
        assert_eq!(e.buffer(), before.buffer());
        assert_eq!(e.edit_revision(), revision);
        // And it can still be typed into, because it was never taken away.
        assert!(e.edit(|b| b.insert("x")).is_ok());
    }

    /// Human-first, at the one place every observed human action passes
    /// through. A byte that edits nothing still ends the reservation.
    #[test]
    fn any_human_activity_ends_a_standing_reservation() {
        type Act = fn(&mut Editor) -> Result<(), ProtocolError>;
        for (name, act) in [
            ("insert", (|e| e.edit(|b| b.insert("x")).map(|_| ())) as Act),
            // A no-op edit: backspace at position zero changes no text.
            ("no-op edit", |e| e.edit(Buffer::backspace).map(|_| ())),
            // A partial escape sequence, which reaches the buffer as nothing
            // at all but is unmistakably a person at the keyboard.
            ("partial key", |e| {
                e.set_interaction(Interaction {
                    decoder_pending: true,
                    ..Default::default()
                })
            }),
        ] {
            let mut e = editing(PromptProfile::Primary("> ".into()));
            let revision = e.edit_revision();
            assert!(matches!(
                suspend(&mut e),
                Effect::Reply(Reply::Reserved { .. })
            ));
            assert!(e.reserved(), "{name}");
            act(&mut e).unwrap();
            assert!(!e.reserved(), "{name}: the reservation survived a keystroke");
            assert!(
                !e.admissible(G, revision),
                "{name}: an admission could still commit"
            );
        }
    }

    #[test]
    fn a_released_reservation_leaves_the_prompt_exactly_as_it_was() {
        let mut e = editing(PromptProfile::Primary("> ".into()));
        let revision = e.edit_revision();
        suspend(&mut e);
        let reserved = e.clone();
        e.release_reservation(G, revision).unwrap();
        assert!(!e.reserved());
        assert_eq!(e.state(), State::Editing);
        assert_eq!(e.buffer(), reserved.buffer());
        assert_eq!(e.edit_revision(), revision);
    }
    #[test]
    fn admission_blockers() {
        for profile in [PromptProfile::Continuation, PromptProfile::Restricted] {
            let mut e = editing(profile);
            assert!(matches!(suspend(&mut e), Effect::Reply(Reply::Busy { .. })));
        }
        for interaction in [
            Interaction {
                paste: true,
                ..Default::default()
            },
            Interaction {
                search: Some(String::new()),
                ..Default::default()
            },
            Interaction {
                completion: true,
                ..Default::default()
            },
            Interaction {
                decoder_pending: true,
                ..Default::default()
            },
        ] {
            let mut e = editing(PromptProfile::Primary(String::new()));
            e.set_interaction(interaction.clone()).unwrap();
            let before = e.clone();
            assert!(matches!(suspend(&mut e), Effect::Reply(Reply::Busy { .. })));
            assert_eq!(e, before);
        }
        let mut e = editing(PromptProfile::Primary(String::new()));
        e.edit(|b| b.insert(" ")).unwrap();
        assert!(matches!(suspend(&mut e), Effect::Reply(Reply::Busy { .. })));
    }
    #[test]
    fn stale_human_activity_and_generations() {
        let mut e = editing(PromptProfile::Primary(String::new()));
        e.edit(Buffer::backspace).unwrap();
        assert_eq!(
            e.command(Command::SuspendRequested {
                generation: G,
                edit_revision: 0
            }),
            Err(ProtocolError::StaleRevision)
        );
        for generation in [
            Generation { session: 8, ..G },
            Generation { prompt: 0, ..G },
        ] {
            assert_eq!(
                e.command(Command::Shutdown { generation }),
                Err(ProtocolError::StaleGeneration)
            );
        }
        suspend(&mut e);
        e.consume_reservation(G, 1).unwrap();
        assert_eq!(
            e.command(Command::BeginPrompt {
                generation: G,
                profile: PromptProfile::Continuation
            }),
            Err(ProtocolError::StaleGeneration)
        );
        let next = Generation { prompt: 2, ..G };
        e.command(Command::BeginPrompt {
            generation: next,
            profile: PromptProfile::Continuation,
        })
        .unwrap();
        assert_eq!(e.edit_revision(), 1);
    }
    #[test]
    fn failure_and_shutdown_supersede_pending_operations() {
        let mut e = editing(PromptProfile::Primary(String::new()));
        // The lifecycle pause is the mode-moving operation now; a reservation
        // deliberately has no modes to fail.
        let old = token(e.pause(G, e.edit_revision()).unwrap());
        assert_eq!(
            e.modes_completed(old, false),
            Err(ProtocolError::ModeFailure)
        );
        assert_eq!(e.state(), State::Failed);
        assert_eq!(
            e.command(Command::Resume {
                generation: G,
                edit_revision: 0
            }),
            Err(ProtocolError::InvalidState)
        );
        let t = token(e.command(Command::Shutdown { generation: G }).unwrap());
        assert_eq!(
            e.modes_completed(old, true),
            Err(ProtocolError::StaleModeCompletion)
        );
        assert_eq!(
            e.modes_completed(t, true),
            Ok(Reply::RestoredAndStopped { generation: G })
        );
        assert_eq!(
            e.modes_completed(t, true),
            Err(ProtocolError::StaleModeCompletion)
        );
    }
    #[test]
    fn shutdown_in_each_live_phase() {
        for phase in 0..5 {
            let mut e = Editor::new(7);
            let begin = token(
                e.command(Command::BeginPrompt {
                    generation: G,
                    profile: PromptProfile::Primary(String::new()),
                })
                .unwrap(),
            );
            if phase > 0 {
                e.modes_completed(begin, true).unwrap();
            }
            if phase > 1 {
                let t = token(e.pause(G, e.edit_revision()).unwrap());
                if phase > 2 {
                    e.modes_completed(t, true).unwrap();
                }
            }
            if phase > 3 {
                e.command(Command::Resume {
                    generation: G,
                    edit_revision: 0,
                })
                .unwrap();
            }
            let t = token(e.command(Command::Shutdown { generation: G }).unwrap());
            e.modes_completed(t, true).unwrap();
            assert_eq!(e.state(), State::Stopped);
        }
    }
    #[test]
    fn profiles_are_owned_and_restricted() {
        assert_eq!(PromptProfile::Continuation.text(), "  > ");
        let p = PromptProfile::Restricted;
        assert_eq!(p.text(), "jobs> ");
        assert!(!p.allows_completion());
        assert!(p.allows_command("fg"));
        assert!(!p.allows_command("print"));
        assert!(PromptProfile::Primary(String::new()).allows_completion());
    }

    #[test]
    fn startup_shutdown_and_mode_entry_failure() {
        let mut e = Editor::new(7);
        let t = token(
            e.command(Command::Shutdown {
                generation: Generation {
                    session: 7,
                    prompt: 0,
                },
            })
            .unwrap(),
        );
        assert_eq!(e.state(), State::RestoringForShutdown);
        assert!(matches!(
            e.modes_completed(t, true),
            Ok(Reply::RestoredAndStopped { .. })
        ));
        let mut e = Editor::new(7);
        let t = token(
            e.command(Command::BeginPrompt {
                generation: G,
                profile: PromptProfile::Continuation,
            })
            .unwrap(),
        );
        assert_eq!(e.modes_completed(t, false), Err(ProtocolError::ModeFailure));
        assert_eq!(e.edit(Buffer::backspace), Err(ProtocolError::InvalidState));
    }

    #[test]
    fn lifecycle_pause_is_not_an_evaluation_reservation() {
        let mut e = editing(PromptProfile::Primary(String::new()));
        e.edit(|b| b.insert("draft")).unwrap();
        let revision = e.edit_revision();
        let t = token(e.pause(G, revision).unwrap());
        e.modes_completed(t, true).unwrap();
        assert_eq!(
            e.consume_reservation(G, revision),
            Err(ProtocolError::InvalidState)
        );
        let t = token(
            e.command(Command::Resume {
                generation: G,
                edit_revision: revision,
            })
            .unwrap(),
        );
        e.modes_completed(t, true).unwrap();
        assert_eq!(e.buffer().text(), "draft");
    }

    #[test]
    fn command_state_matrix_and_exhaustion() {
        for state in [
            State::Idle,
            State::Activating,
            State::Editing,
            State::RestoringForSuspend,
            State::Suspended,
            State::RestoringForShutdown,
            State::Stopped,
            State::Failed,
        ] {
            let mut base = editing(PromptProfile::Primary(String::new()));
            base.state = state;
            let mut e = base.clone();
            let begin = e.command(Command::BeginPrompt {
                generation: Generation { prompt: 2, ..G },
                profile: PromptProfile::Continuation,
            });
            assert_eq!(begin.is_ok(), state == State::Idle);
            if begin.is_err() {
                assert_eq!(e, base);
            }
            let mut e = base.clone();
            let resume = e.command(Command::Resume {
                generation: G,
                edit_revision: 0,
            });
            assert_eq!(resume.is_ok(), state == State::Suspended);
            if resume.is_err() {
                assert_eq!(e, base);
            }
            let mut e = base.clone();
            let effect = suspend(&mut e);
            // A reservation is granted only while EDITING, and it is never a
            // mode operation — it changes nothing but the promise.
            assert_eq!(
                matches!(effect, Effect::Reply(Reply::Reserved { .. })),
                state == State::Editing
            );
            if state != State::Editing {
                assert_eq!(e, base);
            }
            let mut e = base.clone();
            assert_eq!(
                e.command(Command::Shutdown { generation: G }).is_ok(),
                state != State::Stopped
            );
        }
        // Serial exhaustion belongs to the mode-moving operations now; a
        // reservation allocates no mode token, so it cannot exhaust one.
        let mut e = editing(PromptProfile::Primary(String::new()));
        e.serial = u64::MAX;
        let before = e.clone();
        assert_eq!(e.pause(G, 0), Err(ProtocolError::Exhausted));
        assert_eq!(e, before);
        e.revision = u64::MAX;
        let before = e.clone();
        assert_eq!(e.edit(|b| b.insert("x")), Err(ProtocolError::Exhausted));
        assert_eq!(e, before);
    }
}
