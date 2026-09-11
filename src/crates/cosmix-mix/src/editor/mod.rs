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
        }
    }
    pub fn state(&self) -> State {
        self.state
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
    pub fn command(&mut self, command: Command) -> Result<Effect, ProtocolError> {
        match command {
            Command::BeginPrompt {
                generation,
                profile,
            } => {
                if generation.session != self.session
                    || self
                        .generation
                        .is_some_and(|g| generation.prompt <= g.prompt)
                {
                    return Err(ProtocolError::StaleGeneration);
                }
                if self.state != State::Idle {
                    return Err(ProtocolError::InvalidState);
                }
                if self.serial == u64::MAX {
                    return Err(ProtocolError::Exhausted);
                }
                self.generation = Some(generation);
                self.profile = Some(profile);
                self.buffer = Buffer::default();
                self.interaction = Interaction::default();
                self.modes(State::Activating, ModeAction::EnterEditing)
            }
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
                let effect = self.modes(State::RestoringForSuspend, ModeAction::Restore)?;
                self.reserved = true;
                Ok(effect)
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
    /// Admission owner consumes a suspended reservation after its final
    /// identity/deadline/revision checks. No speculative next prompt.
    /// Only a suspended reservation may be consumed here; human-line return
    /// requires a separate terminal-restored completion seam in the input lane.
    pub fn consume_reservation(
        &mut self,
        generation: Generation,
        revision: u64,
    ) -> Result<(), ProtocolError> {
        self.check(generation)?;
        if revision != self.revision {
            return Err(ProtocolError::StaleRevision);
        }
        if self.state != State::Suspended || !self.reserved {
            return Err(ProtocolError::InvalidState);
        }
        self.state = State::Idle;
        self.reserved = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
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
    fn suspend(e: &mut Editor) -> Effect {
        e.command(Command::SuspendRequested {
            generation: G,
            edit_revision: e.edit_revision(),
        })
        .unwrap()
    }
    #[test]
    fn restore_before_ack_and_preserve_undo() {
        let mut e = editing(PromptProfile::Primary("> ".into()));
        e.edit(|b| b.insert("draft")).unwrap();
        e.edit(|b| b.kill(0..5, false)).unwrap();
        let before = e.buffer().clone();
        let rev = e.edit_revision();
        let t = token(suspend(&mut e));
        assert_eq!(e.state(), State::RestoringForSuspend);
        assert_eq!(e.edit(|b| b.insert("x")), Err(ProtocolError::InvalidState));
        assert_eq!(
            e.modes_completed(t, true),
            Ok(Reply::Suspended {
                generation: G,
                edit_revision: rev
            })
        );
        assert_eq!(e.buffer(), &before);
        let t = token(
            e.command(Command::Resume {
                generation: G,
                edit_revision: rev,
            })
            .unwrap(),
        );
        e.modes_completed(t, true).unwrap();
        assert_eq!(e.buffer(), &before);
        e.edit(Buffer::undo).unwrap();
        assert_eq!(e.buffer().text(), "draft");
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
        let t = token(suspend(&mut e));
        e.modes_completed(t, true).unwrap();
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
        let old = token(suspend(&mut e));
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
                let t = token(suspend(&mut e));
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
            assert_eq!(
                matches!(effect, Effect::Modes { .. }),
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
        let mut e = editing(PromptProfile::Primary(String::new()));
        e.serial = u64::MAX;
        let before = e.clone();
        assert_eq!(
            e.command(Command::SuspendRequested {
                generation: G,
                edit_revision: 0
            }),
            Err(ProtocolError::Exhausted)
        );
        assert_eq!(e, before);
        e.revision = u64::MAX;
        let before = e.clone();
        assert_eq!(e.edit(|b| b.insert("x")), Err(ProtocolError::Exhausted));
        assert_eq!(e, before);
    }
}
