//! Snapshot-based undo/redo history for the song document.
//!
//! A snapshot is a full clone of the song — cheap at document-model scale
//! (a note is a few dozen bytes). Unlike the miditui original, snapshots
//! carry NO UI selection state; restoring selection after undo is the
//! front-end's own concern.

use crate::song::Song;

/// Maximum number of undo/redo states to keep.
const MAX_HISTORY_SIZE: usize = 8;

/// A snapshot of the song at a point in time.
#[derive(Debug, Clone)]
pub struct StateSnapshot {
    /// The complete song state (tracks, notes, tempo, etc.).
    pub song: Song,

    /// A brief description of what operation created this snapshot.
    /// Used for status messages when undoing/redoing.
    pub description: String,
}

impl StateSnapshot {
    /// Creates a new snapshot from the current song state.
    pub fn new(song: &Song, description: impl Into<String>) -> Self {
        Self {
            song: song.clone(),
            description: description.into(),
        }
    }
}

/// Manages undo/redo history using a snapshot-based approach.
///
/// The manager maintains two stacks:
/// - `undo_stack`: Past states that can be reverted to
/// - `redo_stack`: Future states that can be restored after undoing
///
/// When a new action is performed, the current state is pushed to the
/// undo stack and the redo stack is cleared (branching creates a new timeline).
#[derive(Debug, Default)]
pub struct HistoryManager {
    /// Stack of states to undo to (most recent last).
    undo_stack: Vec<StateSnapshot>,

    /// Stack of states to redo to (most recent last).
    redo_stack: Vec<StateSnapshot>,
}

impl HistoryManager {
    /// Creates a new empty history manager.
    pub fn new() -> Self {
        Self {
            undo_stack: Vec::with_capacity(MAX_HISTORY_SIZE),
            redo_stack: Vec::with_capacity(MAX_HISTORY_SIZE),
        }
    }

    /// Records a snapshot before an operation.
    ///
    /// Call this BEFORE making any changes to capture the current state.
    /// The redo stack is cleared since we're starting a new branch of history.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Before placing a note:
    /// history.push_undo(StateSnapshot::new(&song, "Place note"));
    /// // Now make the change:
    /// track.create_note(...);
    /// ```
    pub fn push_undo(&mut self, snapshot: StateSnapshot) {
        // Clear redo stack - we're branching to a new timeline
        self.redo_stack.clear();

        self.push_undo_preserve_redo(snapshot);
    }

    /// Pushes a state to the undo stack WITHOUT clearing the redo stack.
    ///
    /// This is used during redo operations. When the user redoes, the
    /// current state must go to undo for potential future undos, but the
    /// remaining redo states must NOT be cleared.
    pub fn push_undo_preserve_redo(&mut self, snapshot: StateSnapshot) {
        // Add to undo stack without clearing redo
        self.undo_stack.push(snapshot);

        // Enforce maximum history size by removing oldest entries
        while self.undo_stack.len() > MAX_HISTORY_SIZE {
            self.undo_stack.remove(0);
        }
    }

    /// Pops the most recent undo state.
    ///
    /// This should be called to get the state to restore to.
    /// The caller should push the CURRENT state to redo before applying
    /// the returned snapshot.
    pub fn pop_undo(&mut self) -> Option<StateSnapshot> {
        self.undo_stack.pop()
    }

    /// Pushes a state to the redo stack.
    ///
    /// Called when undoing to save the current state for potential redo.
    pub fn push_redo(&mut self, snapshot: StateSnapshot) {
        self.redo_stack.push(snapshot);

        // Enforce maximum history size
        while self.redo_stack.len() > MAX_HISTORY_SIZE {
            self.redo_stack.remove(0);
        }
    }

    /// Pops the most recent redo state.
    ///
    /// The caller should push the CURRENT state to undo (via
    /// `push_undo_preserve_redo`) before applying the returned snapshot.
    pub fn pop_redo(&mut self) -> Option<StateSnapshot> {
        self.redo_stack.pop()
    }

    /// Returns true if there are states available to undo to.
    pub fn can_undo(&self) -> bool {
        !self.undo_stack.is_empty()
    }

    /// Returns true if there are states available to redo to.
    pub fn can_redo(&self) -> bool {
        !self.redo_stack.is_empty()
    }

    /// Returns the number of undo states available.
    pub fn undo_count(&self) -> usize {
        self.undo_stack.len()
    }

    /// Returns the number of redo states available.
    pub fn redo_count(&self) -> usize {
        self.redo_stack.len()
    }

    /// Clears all history.
    ///
    /// Called when:
    /// - Loading a new song
    /// - Creating a new song
    /// - Encountering an invalid state that can't be recovered
    pub fn clear(&mut self) {
        self.undo_stack.clear();
        self.redo_stack.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_history_push_and_pop() {
        let mut history = HistoryManager::new();

        let song = Song::with_default_track("Test");
        let snapshot = StateSnapshot::new(&song, "Test action");

        history.push_undo(snapshot);

        assert!(history.can_undo());
        assert!(!history.can_redo());
        assert_eq!(history.undo_count(), 1);

        let restored = history.pop_undo().unwrap();
        assert_eq!(restored.description, "Test action");
        assert!(!history.can_undo());
    }

    #[test]
    fn test_history_max_size() {
        let mut history = HistoryManager::new();

        let song = Song::with_default_track("Test");

        // Push more than MAX_HISTORY_SIZE entries
        for i in 0..MAX_HISTORY_SIZE + 5 {
            let snapshot = StateSnapshot::new(&song, format!("Action {i}"));
            history.push_undo(snapshot);
        }

        // Should only keep MAX_HISTORY_SIZE entries
        assert_eq!(history.undo_count(), MAX_HISTORY_SIZE);

        // The oldest entries should have been removed
        // Most recent should still be there
        let last = history.pop_undo().unwrap();
        assert_eq!(last.description, format!("Action {}", MAX_HISTORY_SIZE + 4));
    }

    #[test]
    fn test_redo_cleared_on_new_action() {
        let mut history = HistoryManager::new();

        let song = Song::with_default_track("Test");

        // Create an undo state
        history.push_undo(StateSnapshot::new(&song, "Action 1"));

        // Pop it and push to redo (simulating an undo operation)
        let undone = history.pop_undo().unwrap();
        history.push_redo(undone);

        assert!(history.can_redo());

        // New action should clear redo stack
        history.push_undo(StateSnapshot::new(&song, "Action 2"));

        assert!(!history.can_redo());
    }

    #[test]
    fn test_multi_level_undo_redo() {
        // Test that if user undoes 4 changes, they can redo those same 4 changes
        let mut history = HistoryManager::new();
        let song = Song::with_default_track("Test");

        // Simulate 4 user actions
        for i in 0..4 {
            history.push_undo(StateSnapshot::new(&song, format!("Action {i}")));
        }

        assert_eq!(history.undo_count(), 4);
        assert_eq!(history.redo_count(), 0);

        // Undo all 4 actions
        for _ in 0..4 {
            let undone = history.pop_undo().unwrap();
            history.push_redo(undone);
        }

        assert_eq!(history.undo_count(), 0);
        assert_eq!(history.redo_count(), 4);

        // Now redo all 4 actions using push_undo_preserve_redo
        for _ in 0..4 {
            let redone = history.pop_redo().unwrap();
            // This is the key: use push_undo_preserve_redo, NOT push_undo
            history.push_undo_preserve_redo(redone);
        }

        // Should have all 4 back in undo stack, redo should be empty
        assert_eq!(history.undo_count(), 4);
        assert_eq!(history.redo_count(), 0);
    }

    #[test]
    fn test_new_action_clears_redo_after_undo() {
        // Test that a new action after undo clears the redo stack
        let mut history = HistoryManager::new();
        let song = Song::with_default_track("Test");

        // Make 3 actions
        for i in 0..3 {
            history.push_undo(StateSnapshot::new(&song, format!("Action {i}")));
        }

        // Undo 2 of them
        for _ in 0..2 {
            let undone = history.pop_undo().unwrap();
            history.push_redo(undone);
        }

        assert_eq!(history.undo_count(), 1);
        assert_eq!(history.redo_count(), 2);

        // Make a NEW action (this should clear redo stack - branching timeline)
        history.push_undo(StateSnapshot::new(&song, "New action after undo"));

        // Redo stack should be cleared, undo should have 2 items
        assert_eq!(history.undo_count(), 2);
        assert_eq!(history.redo_count(), 0);
    }
}
