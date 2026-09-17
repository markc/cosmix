//! text-input-v3 batch and serial bookkeeping. Pure; the runtime sends the
//! requests and feeds the events in.
//!
//! The `done` serial is the number of `commit` requests the compositor had
//! seen. A batch is applied when it belongs to the current focus generation
//! and its serial lies between the commit that enabled this focus and the
//! latest commit: a later state commit (a cursor rectangle) must not
//! invalidate input already in flight, but a batch meant for an earlier
//! focus must be dropped.

use crate::geom::Rect;
pub use wayland_protocols::wp::text_input::zv3::client::zwp_text_input_v3::{
    ContentHint, ContentPurpose,
};

/// What the app asks of the input method.
#[derive(Debug, Clone, PartialEq)]
pub struct ImeState {
    pub surface: crate::SurfaceId,
    /// App-chosen owner of the input (a grid, a text field). Changing it
    /// restarts the input method, so input in flight for the previous
    /// owner is dropped, and [`crate::Event::Ime`] carries the owner each
    /// result belongs to.
    pub target: u64,
    /// Caret rectangle in logical surface coordinates.
    pub cursor: Rect,
    pub hint: ContentHint,
    pub purpose: ContentPurpose,
    /// Text around the caret, with the cursor and anchor byte offsets.
    pub surrounding: Option<(String, i32, i32)>,
}

impl ImeState {
    pub fn new(surface: crate::SurfaceId, cursor: Rect) -> Self {
        Self {
            surface,
            target: 0,
            cursor,
            hint: ContentHint::None,
            purpose: ContentPurpose::Normal,
            surrounding: None,
        }
    }
}

/// Input method results, in the order the protocol says to apply them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImeEvent {
    /// The input method is active for the surface; `false` when it left.
    Focus { active: bool },
    /// Delete bytes around the caret (before the commit is inserted).
    DeleteSurrounding { before: u32, after: u32 },
    /// Insert text at the caret.
    Commit(String),
    /// Replace the preedit. Empty text clears it. The cursor is a byte range
    /// within the text, when the input method gave one.
    Preedit {
        text: String,
        cursor: Option<(usize, usize)>,
    },
}

#[derive(Debug, Default, Clone)]
pub struct ImeSerials {
    commits: u32,
    focus_serial: u32,
    generation: u64,
    batch_generation: Option<u64>,
    enabled: bool,
    preedit: Option<(String, Option<(usize, usize)>)>,
    commit: Option<String>,
    delete: Option<(u32, u32)>,
    showing_preedit: bool,
}

impl ImeSerials {
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn commits(&self) -> u32 {
        self.commits
    }

    /// Call after sending an `enable` followed by its `commit`.
    pub fn enabled_and_committed(&mut self) {
        self.new_generation();
        self.commits = self.commits.wrapping_add(1);
        self.focus_serial = self.commits;
        self.enabled = true;
    }

    /// Call after sending a `disable` followed by its `commit`.
    pub fn disabled_and_committed(&mut self) -> Vec<ImeEvent> {
        self.commits = self.commits.wrapping_add(1);
        self.drop_focus()
    }

    /// Call after any other `commit` (state updates while enabled).
    pub fn committed(&mut self) {
        self.commits = self.commits.wrapping_add(1);
    }

    /// The text input left the surface, or was destroyed.
    pub fn leave(&mut self) -> Vec<ImeEvent> {
        self.drop_focus()
    }

    /// A new text-input object: the serial count restarts.
    pub fn reset(&mut self) {
        self.drop_focus();
        self.commits = 0;
        self.focus_serial = 0;
    }

    fn drop_focus(&mut self) -> Vec<ImeEvent> {
        let was = std::mem::replace(&mut self.enabled, false);
        self.new_generation();
        let mut out = Vec::new();
        if std::mem::take(&mut self.showing_preedit) {
            out.push(ImeEvent::Preedit {
                text: String::new(),
                cursor: None,
            });
        }
        if was {
            out.push(ImeEvent::Focus { active: false });
        }
        out
    }

    fn new_generation(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.batch_generation = None;
        self.preedit = None;
        self.commit = None;
        self.delete = None;
    }

    pub fn preedit(&mut self, text: Option<String>, begin: i32, end: i32) {
        let text = text.unwrap_or_default();
        let cursor = (begin >= 0
            && end >= 0
            && begin as usize <= text.len()
            && end as usize <= text.len()
            && text.is_char_boundary(begin as usize)
            && text.is_char_boundary(end as usize))
        .then_some((begin as usize, end as usize));
        self.batch_generation.get_or_insert(self.generation);
        self.preedit = Some((text, cursor));
    }

    pub fn commit_string(&mut self, text: Option<String>) {
        self.batch_generation.get_or_insert(self.generation);
        self.commit = text;
    }

    pub fn delete_surrounding(&mut self, before: u32, after: u32) {
        self.batch_generation.get_or_insert(self.generation);
        self.delete = Some((before, after));
    }

    /// Apply or drop the pending batch.
    pub fn done(&mut self, serial: u32) -> Vec<ImeEvent> {
        let preedit = self.preedit.take();
        let commit = self.commit.take();
        let delete = self.delete.take();
        // An empty batch (only `done`) belongs to whoever holds focus now;
        // the serial check still rejects one sent before this focus.
        let generation = self.batch_generation.take().unwrap_or(self.generation);
        let in_window =
            serial.wrapping_sub(self.focus_serial) <= self.commits.wrapping_sub(self.focus_serial);
        if !self.enabled || generation != self.generation || !in_window {
            return Vec::new();
        }
        let mut out = Vec::new();
        if let Some((before, after)) = delete {
            out.push(ImeEvent::DeleteSurrounding { before, after });
        }
        if let Some(text) = commit.filter(|t| !t.is_empty()) {
            out.push(ImeEvent::Commit(text));
        }
        // A batch without preedit_string clears the preedit.
        let (text, cursor) = preedit.unwrap_or_default();
        if !text.is_empty() || self.showing_preedit {
            self.showing_preedit = !text.is_empty();
            out.push(ImeEvent::Preedit { text, cursor });
        }
        out
    }
}

/// What `sync` must send to bring the input method from `sent` to `want`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImePlan {
    Nothing,
    /// `disable` + `commit`.
    Disable,
    /// `enable`, the full state, `commit`.
    Enable,
    /// Disable + commit, then enable + commit: a new owner (surface or
    /// target) starts a new generation.
    Restart,
    /// The changed state + `commit`.
    Update,
}

pub fn plan(enabled: bool, sent: Option<&ImeState>, want: Option<&ImeState>) -> ImePlan {
    match (want, enabled) {
        (None, false) => ImePlan::Nothing,
        (None, true) => ImePlan::Disable,
        (Some(_), false) => ImePlan::Enable,
        (Some(want), true) => match sent {
            Some(sent) if sent.surface != want.surface || sent.target != want.target => {
                ImePlan::Restart
            }
            Some(sent) if sent == want => ImePlan::Nothing,
            _ => ImePlan::Update,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled_at(start: u32) -> ImeSerials {
        let mut s = ImeSerials {
            commits: start.wrapping_sub(1),
            ..Default::default()
        };
        s.enabled_and_committed();
        assert_eq!(s.commits(), start);
        s
    }

    #[test]
    fn accepts_lagging_serial_within_focus() {
        for start in [1, u32::MAX, 0] {
            let mut s = enabled_at(start);
            s.commit_string(Some("typed".into()));
            s.committed(); // cursor rectangle update
            assert_eq!(s.done(start), vec![ImeEvent::Commit("typed".into())]);
            s.preedit(Some("compose".into()), 0, 7);
            assert_eq!(
                s.done(start),
                vec![ImeEvent::Preedit {
                    text: "compose".into(),
                    cursor: Some((0, 7))
                }]
            );
            // A serial from the future is not ours.
            s.commit_string(Some("future".into()));
            assert!(s.done(start.wrapping_add(2)).is_empty());
        }
    }

    #[test]
    fn drops_batches_from_previous_focus() {
        let mut s = enabled_at(1);
        s.commit_string(Some("old".into()));
        let generation = s.generation();
        s.disabled_and_committed();
        s.enabled_and_committed();
        assert_ne!(s.generation(), generation);
        assert_eq!(s.commits(), 3);
        // The pending batch was discarded with the old generation.
        assert!(s.done(1).is_empty());
        // A whole old batch arriving after the new enable is rejected by serial.
        s.preedit(Some("stale".into()), 0, 0);
        assert!(s.done(1).is_empty());
        assert!(s.done(2).is_empty());
        s.commit_string(Some("new".into()));
        assert_eq!(s.done(3), vec![ImeEvent::Commit("new".into())]);
    }

    #[test]
    fn preedit_clears_and_ordering() {
        let mut s = enabled_at(5);
        s.preedit(Some("ka".into()), 2, 2);
        assert_eq!(s.done(5).len(), 1);
        // Commit with no preedit: delete, commit, then clear the preedit.
        s.delete_surrounding(1, 0);
        s.commit_string(Some("か".into()));
        assert_eq!(
            s.done(5),
            vec![
                ImeEvent::DeleteSurrounding {
                    before: 1,
                    after: 0
                },
                ImeEvent::Commit("か".into()),
                ImeEvent::Preedit {
                    text: String::new(),
                    cursor: None
                },
            ]
        );
        // Nothing showing and nothing new: empty done emits nothing.
        assert!(s.done(5).is_empty());
    }

    #[test]
    fn invalid_cursor_is_dropped() {
        let mut s = enabled_at(1);
        s.preedit(Some("か".into()), 1, 1); // not a char boundary
        assert_eq!(
            s.done(1),
            vec![ImeEvent::Preedit {
                text: "か".into(),
                cursor: None
            }]
        );
        s.preedit(Some("ab".into()), -1, 1);
        assert!(matches!(
            &s.done(1)[..],
            [ImeEvent::Preedit { cursor: None, .. }]
        ));
    }

    #[test]
    fn plan_restarts_on_new_target() {
        let surface = crate::SurfaceId::from_raw(1);
        let grid = ImeState {
            target: 1,
            ..ImeState::new(surface, Rect::new(0, 0, 1, 10))
        };
        let field = ImeState {
            target: 2,
            ..grid.clone()
        };
        let moved = ImeState {
            cursor: Rect::new(5, 0, 1, 10),
            ..grid.clone()
        };
        assert_eq!(plan(false, None, None), ImePlan::Nothing);
        assert_eq!(plan(false, None, Some(&grid)), ImePlan::Enable);
        assert_eq!(plan(true, Some(&grid), Some(&grid)), ImePlan::Nothing);
        assert_eq!(plan(true, Some(&grid), Some(&moved)), ImePlan::Update);
        assert_eq!(plan(true, Some(&grid), Some(&field)), ImePlan::Restart);
        assert_eq!(plan(true, Some(&grid), None), ImePlan::Disable);
    }

    #[test]
    fn handover_drops_batch_for_old_target() {
        // The grid owns the input method and shows a preedit.
        let mut s = enabled_at(1);
        s.preedit(Some("ka".into()), 2, 2);
        assert_eq!(s.done(1).len(), 1);
        // A batch for the grid is in flight when the search field takes
        // over: Restart = disable + commit, enable + commit.
        s.commit_string(Some("grid text".into()));
        let cleared = s.disabled_and_committed();
        assert_eq!(
            cleared,
            vec![
                ImeEvent::Preedit {
                    text: String::new(),
                    cursor: None
                },
                ImeEvent::Focus { active: false }
            ],
            "the old owner's preedit is cleared"
        );
        s.enabled_and_committed();
        assert_eq!(s.commits(), 3);
        // The compositor answers the grid's batch with its old serial.
        assert!(s.done(1).is_empty());
        // Even a whole batch re-sent under the disable commit is dropped.
        s.commit_string(Some("grid text".into()));
        assert!(s.done(2).is_empty());
        // Input for the field, after its enable, is delivered.
        s.commit_string(Some("field".into()));
        assert_eq!(s.done(3), vec![ImeEvent::Commit("field".into())]);
    }

    #[test]
    fn leave_clears_preedit_and_rejects_until_reenabled() {
        let mut s = enabled_at(1);
        s.preedit(Some("x".into()), 1, 1);
        s.done(1);
        assert_eq!(
            s.leave(),
            vec![
                ImeEvent::Preedit {
                    text: String::new(),
                    cursor: None
                },
                ImeEvent::Focus { active: false }
            ]
        );
        s.commit_string(Some("late".into()));
        assert!(s.done(1).is_empty());
        s.reset();
        s.enabled_and_committed();
        assert_eq!(s.commits(), 1);
        s.commit_string(Some("ok".into()));
        assert_eq!(s.done(1), vec![ImeEvent::Commit("ok".into())]);
    }
}
