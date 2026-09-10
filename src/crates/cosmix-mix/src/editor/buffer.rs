//! Owned UTF-8 editing. Cursors and ranges are byte offsets at extended
//! grapheme boundaries; every edit resegments, including across the insertion.
use std::collections::VecDeque;
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Snapshot {
    text: String,
    cursor: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub bytes: usize,
    pub undo_entries: usize,
    pub kill_entries: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self { bytes: 1024 * 1024, undo_entries: 100, kill_entries: 60 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditError { Limit, InvalidRange, RevisionExhausted }

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Buffer {
    current: Snapshot,
    undo: VecDeque<Snapshot>,
    redo: VecDeque<Snapshot>,
    kills: VecDeque<String>,
    last_kill: bool,
    yank: Option<(Range<usize>, usize)>,
    revision: u64,
    limits: Limits,
}

impl Default for Buffer {
    fn default() -> Self { Self::new(Limits::default()) }
}

impl Buffer {
    pub fn new(limits: Limits) -> Self {
        Self { current: Snapshot { text: String::new(), cursor: 0 },
            undo: VecDeque::new(), redo: VecDeque::new(), kills: VecDeque::new(),
            last_kill: false, yank: None, revision: 0, limits }
    }
    pub fn text(&self) -> &str { &self.current.text }
    pub fn cursor(&self) -> usize { self.current.cursor }
    /// Monotonic across edits, undo/redo and actual cursor moves; never rolled
    /// back with a snapshot. No-ops do not invalidate completion results.
    pub fn revision(&self) -> u64 { self.revision }
    pub fn display_width(&self) -> usize { UnicodeWidthStr::width(self.text()) }
    pub fn boundaries(&self) -> impl DoubleEndedIterator<Item = usize> + '_ {
        self.text().grapheme_indices(true).map(|(i, _)| i).chain(std::iter::once(self.text().len()))
    }
    pub fn is_boundary(&self, byte: usize) -> bool { self.boundaries().any(|i| i == byte) }
    fn next_revision(&self) -> Result<u64, EditError> {
        self.revision.checked_add(1).ok_or(EditError::RevisionExhausted)
    }
    fn break_chain(&mut self) { self.last_kill = false; self.yank = None; }
    fn retain(stack: &mut VecDeque<Snapshot>, item: Snapshot, limit: usize) {
        if limit == 0 { return; }
        if stack.len() == limit { stack.pop_front(); }
        stack.push_back(item);
    }
    pub fn move_to(&mut self, byte: usize) -> Result<bool, EditError> {
        if !self.is_boundary(byte) { return Err(EditError::InvalidRange); }
        self.break_chain();
        if byte == self.cursor() { return Ok(false); }
        self.revision = self.next_revision()?;
        self.current.cursor = byte;
        Ok(true)
    }
    pub fn left(&mut self) -> Result<bool, EditError> {
        let pos = self.boundaries().take_while(|&i| i < self.cursor()).last().unwrap_or(0);
        self.move_to(pos)
    }
    pub fn right(&mut self) -> Result<bool, EditError> {
        let pos = self.boundaries().find(|&i| i > self.cursor()).unwrap_or(self.text().len());
        self.move_to(pos)
    }
    /// Emacs-style words: alphanumeric grapheme runs, skipping punctuation.
    pub fn word_left(&mut self) -> Result<bool, EditError> {
        let mut pos = self.cursor();
        let mut in_word = false;
        for (i, g) in self.text()[..self.cursor()].grapheme_indices(true).rev() {
            let word = g.chars().any(char::is_alphanumeric);
            if in_word && !word { break; }
            in_word |= word;
            pos = i;
        }
        self.move_to(pos)
    }
    pub fn word_right(&mut self) -> Result<bool, EditError> {
        let mut pos = self.cursor();
        let mut in_word = false;
        for (i, g) in self.text()[self.cursor()..].grapheme_indices(true) {
            let word = g.chars().any(char::is_alphanumeric);
            if in_word && !word { break; }
            in_word |= word;
            pos = self.cursor() + i + g.len();
        }
        self.move_to(pos)
    }
    pub fn replace(&mut self, range: Range<usize>, text: &str) -> Result<bool, EditError> {
        if range.start > range.end || !self.is_boundary(range.start) || !self.is_boundary(range.end) {
            return Err(EditError::InvalidRange);
        }
        let size = self.text().len() - range.len();
        if text.len() > self.limits.bytes.saturating_sub(size) { return Err(EditError::Limit); }
        if &self.text()[range.clone()] == text { self.break_chain(); return Ok(false); }
        let revision = self.next_revision()?;
        let old = self.current.clone();
        let end = range.start + text.len();
        self.current.text.replace_range(range, text);
        // Inserted combining/ZWJ characters can merge with either neighbour.
        self.current.cursor = self.boundaries().find(|&i| i >= end).unwrap();
        Self::retain(&mut self.undo, old, self.limits.undo_entries);
        self.redo.clear();
        self.revision = revision;
        self.break_chain();
        Ok(true)
    }
    pub fn insert(&mut self, text: &str) -> Result<bool, EditError> { self.replace(self.cursor()..self.cursor(), text) }
    /// A complete bracketed paste is committed once, regardless of newlines.
    /// The input layer must collect it under the same byte limit first.
    pub fn paste(&mut self, text: &str) -> Result<bool, EditError> { self.insert(text) }
    pub fn backspace(&mut self) -> Result<bool, EditError> {
        let start = self.boundaries().take_while(|&i| i < self.cursor()).last().unwrap_or(0);
        self.replace(start..self.cursor(), "")
    }
    pub fn delete(&mut self) -> Result<bool, EditError> {
        let end = self.boundaries().find(|&i| i > self.cursor()).unwrap_or(self.cursor());
        self.replace(self.cursor()..end, "")
    }
    /// Adjacent kills coalesce; backward kills prepend, forward kills append.
    pub fn kill(&mut self, range: Range<usize>, backward: bool) -> Result<bool, EditError> {
        if range.start > range.end || !self.is_boundary(range.start) || !self.is_boundary(range.end) {
            return Err(EditError::InvalidRange);
        }
        if range.is_empty() { self.break_chain(); return Ok(false); }
        let killed = self.text()[range.clone()].to_owned();
        let coalesce = self.last_kill && self.kills.front().is_some_and(|s| s.len() + killed.len() <= self.limits.bytes);
        self.replace(range, "")?;
        if self.limits.kill_entries > 0 {
            if coalesce {
                let first = self.kills.front_mut().unwrap();
                if backward { first.insert_str(0, &killed); } else { first.push_str(&killed); }
            } else {
                self.kills.push_front(killed);
                self.kills.truncate(self.limits.kill_entries);
            }
        }
        self.last_kill = true;
        Ok(true)
    }
    pub fn yank(&mut self) -> Result<bool, EditError> {
        let Some(text) = self.kills.front().cloned() else { return Ok(false); };
        let start = self.cursor();
        let changed = self.insert(&text)?;
        // Yank-pop is unsafe if the insertion merged across either boundary.
        if self.is_boundary(start) && self.is_boundary(start + text.len()) {
            self.yank = Some((start..start + text.len(), 0));
        }
        Ok(changed)
    }
    pub fn yank_pop(&mut self) -> Result<bool, EditError> {
        let Some((range, index)) = self.yank.clone() else { return Ok(false); };
        let index = (index + 1) % self.kills.len();
        let text = self.kills[index].clone();
        let start = range.start;
        let changed = self.replace(range, &text)?;
        if self.is_boundary(start) && self.is_boundary(start + text.len()) {
            self.yank = Some((start..start + text.len(), index));
        }
        Ok(changed)
    }
    pub fn undo(&mut self) -> Result<bool, EditError> { self.restore(false) }
    pub fn redo(&mut self) -> Result<bool, EditError> { self.restore(true) }
    fn restore(&mut self, redo: bool) -> Result<bool, EditError> {
        self.break_chain();
        if (if redo { &self.redo } else { &self.undo }).is_empty() { return Ok(false); }
        let revision = self.next_revision()?;
        let (source, target) = if redo { (&mut self.redo, &mut self.undo) } else { (&mut self.undo, &mut self.redo) };
        let snapshot = source.pop_back().unwrap();
        Self::retain(target, std::mem::replace(&mut self.current, snapshot), self.limits.undo_entries);
        self.revision = revision;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unicode_insert_move_delete() {
        for (text, width) in [("e\u{301}", 1), ("界", 2), ("👩‍👩‍👧‍👦", 2)] {
            let mut b = Buffer::default();
            b.insert(text).unwrap();
            assert_eq!(b.display_width(), width);
            assert_eq!(b.boundaries().collect::<Vec<_>>(), [0, text.len()]);
            b.left().unwrap(); assert_eq!(b.cursor(), 0);
            b.right().unwrap(); assert_eq!(b.cursor(), text.len());
            b.backspace().unwrap(); assert_eq!(b.text(), "");
            b.undo().unwrap(); b.left().unwrap(); b.delete().unwrap();
            assert_eq!(b.text(), "");
        }
    }
    #[test]
    fn insertion_resegments_both_sides() {
        let mut b = Buffer::default();
        b.insert("e").unwrap(); b.insert("\u{301}").unwrap();
        assert_eq!(b.boundaries().count(), 2);
        b.backspace().unwrap(); assert_eq!(b.text(), "");
        b.insert("👩👩").unwrap(); b.left().unwrap(); b.insert("‍").unwrap();
        assert_eq!(b.text(), "👩‍👩"); assert_eq!(b.cursor(), b.text().len());
        b.backspace().unwrap(); assert_eq!(b.text(), "");
    }
    #[test]
    fn empty_edges_and_invalid_offsets() {
        let mut b = Buffer::default();
        assert!(!b.left().unwrap()); assert!(!b.right().unwrap());
        assert!(!b.backspace().unwrap()); assert!(!b.delete().unwrap());
        assert!(!b.undo().unwrap()); assert!(!b.redo().unwrap());
        assert!(!b.yank().unwrap()); assert!(!b.yank_pop().unwrap());
        assert_eq!(b.revision(), 0);
        b.insert("界").unwrap();
        assert_eq!(b.move_to(1), Err(EditError::InvalidRange));
        assert_eq!(b.replace(0..1, "x"), Err(EditError::InvalidRange));
    }
    #[test]
    fn paste_undo_redo_and_branch() {
        let mut b = Buffer::default();
        b.insert("a").unwrap(); b.paste("b\n界👩‍👩‍👧‍👦").unwrap();
        let text = b.text().to_owned(); let revision = b.revision();
        b.undo().unwrap(); assert_eq!(b.text(), "a");
        b.undo().unwrap(); assert_eq!(b.text(), "");
        b.redo().unwrap(); b.redo().unwrap(); assert_eq!(b.text(), text);
        assert!(b.revision() > revision);
        b.undo().unwrap(); b.insert("c").unwrap(); assert!(!b.redo().unwrap());
    }
    #[test]
    fn words_skip_punctuation_without_splitting_clusters() {
        let mut b = Buffer::default(); b.insert("e\u{301}cho, 世界!").unwrap();
        b.word_left().unwrap(); assert_eq!(&b.text()[b.cursor()..], "世界!");
        b.word_left().unwrap(); assert_eq!(b.cursor(), 0);
        b.word_right().unwrap(); assert_eq!(&b.text()[b.cursor()..], ", 世界!");
        b.word_right().unwrap(); assert_eq!(&b.text()[b.cursor()..], "!");
    }
    #[test]
    fn kills_coalesce_rotate_and_survive_undo() {
        let mut b = Buffer::default(); b.insert("abc def").unwrap();
        b.kill(4..7, true).unwrap(); b.kill(3..4, true).unwrap();
        b.yank().unwrap(); assert_eq!(b.text(), "abc def");
        b.kill(0..3, false).unwrap(); b.yank().unwrap();
        assert_eq!(b.text(), "abc def");
        b.yank_pop().unwrap(); assert_eq!(b.text(), " def def");
        b.undo().unwrap(); assert_eq!(b.text(), "abc def");
        assert!(!b.yank_pop().unwrap());
        b.move_to(0).unwrap(); b.kill(0..3, false).unwrap();
        b.kill(0..1, false).unwrap(); b.yank().unwrap();
        assert_eq!(b.text(), "abc def");
    }
    #[test]
    fn bounded_stacks_and_atomic_limit_failure() {
        let mut b = Buffer::new(Limits { bytes: 4, undo_entries: 1, kill_entries: 1 });
        b.insert("ab").unwrap(); b.insert("cd").unwrap(); let old = b.clone();
        assert_eq!(b.paste("x"), Err(EditError::Limit)); assert_eq!(b, old);
        b.undo().unwrap(); assert_eq!(b.text(), "ab"); assert!(!b.undo().unwrap());
        b.revision = u64::MAX;
        assert_eq!(b.insert("x"), Err(EditError::RevisionExhausted));
        assert_eq!(b.text(), "ab");
    }
}
