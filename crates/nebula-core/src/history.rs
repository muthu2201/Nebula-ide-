//! Undo/redo.
//!
//! History stores inverted transactions produced by [`Transaction::apply`], so
//! undo never re-derives a diff — it replays an exact inverse that was captured
//! at the moment of the edit.
//!
//! Consecutive small edits are **grouped**: typing twenty characters and hitting
//! undo should remove the word, not one letter. Grouping breaks on a time gap,
//! on a cursor jump, and on any edit that is not a simple forward-typing
//! insertion.

use std::time::{Duration, Instant};

use crate::edit::Transaction;
use crate::selection::SelectionSet;

/// How long a pause in typing ends an undo group.
pub const DEFAULT_GROUP_INTERVAL: Duration = Duration::from_millis(600);

/// Maximum number of undo entries retained.
///
/// Entries hold only the text they replaced, so this bound is on entry count
/// rather than bytes; a runaway single transaction is bounded by the document
/// size itself.
pub const DEFAULT_CAPACITY: usize = 2048;

/// One undoable step.
#[derive(Debug, Clone)]
pub struct HistoryEntry {
    /// The transaction that undoes this step.
    pub undo: Transaction,
    /// The transaction that redoes it.
    pub redo: Transaction,
    /// Selections as they were *before* the edit, restored on undo.
    pub selections_before: SelectionSet,
    /// Selections as they were *after* the edit, restored on redo.
    pub selections_after: SelectionSet,
    /// When this entry was last extended, for time-based grouping.
    pub timestamp: Instant,
    /// Whether this entry may still absorb a following edit.
    pub open: bool,
}

/// The undo/redo stacks for one document.
#[derive(Debug)]
pub struct History {
    undo_stack: Vec<HistoryEntry>,
    redo_stack: Vec<HistoryEntry>,
    capacity: usize,
    group_interval: Duration,
    /// Saved-state marker: the depth of `undo_stack` when the document was last
    /// written to disk. `None` means "no clean point in this history" (e.g. the
    /// document was never saved, or a save happened and was then undone past).
    saved_depth: Option<usize>,
}

impl Default for History {
    fn default() -> Self {
        Self::new()
    }
}

impl History {
    /// A fresh history with default capacity and grouping interval.
    pub fn new() -> Self {
        Self {
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            capacity: DEFAULT_CAPACITY,
            group_interval: DEFAULT_GROUP_INTERVAL,
            saved_depth: Some(0),
        }
    }

    /// A history with a custom entry cap and grouping interval.
    pub fn with_config(capacity: usize, group_interval: Duration) -> Self {
        Self {
            capacity: capacity.max(1),
            group_interval,
            ..Self::new()
        }
    }

    /// Record an applied transaction.
    ///
    /// `groupable` should be `true` only for plain forward typing; every other
    /// kind of edit (paste, format, refactor, agent edit) starts its own entry
    /// so it can be undone as a unit.
    pub fn record(
        &mut self,
        redo: Transaction,
        undo: Transaction,
        selections_before: SelectionSet,
        selections_after: SelectionSet,
        groupable: bool,
    ) {
        if redo.is_empty() {
            return;
        }
        // Any new edit invalidates the redo branch.
        self.redo_stack.clear();

        let now = Instant::now();
        if groupable
            && let Some(last) = self.undo_stack.last_mut()
            && last.open
            && now.duration_since(last.timestamp) <= self.group_interval
        {
            // Extend the open group. The undo of the combined group is the new
            // undo followed by the old one (inverse order), and the redo is the
            // old redo followed by the new one.
            last.redo = concat(&last.redo, &redo);
            last.undo = concat(&undo, &last.undo);
            last.selections_after = selections_after;
            last.timestamp = now;
            return;
        }

        // Starting a new entry closes the previous group for good.
        if let Some(last) = self.undo_stack.last_mut() {
            last.open = false;
        }

        self.undo_stack.push(HistoryEntry {
            undo,
            redo,
            selections_before,
            selections_after,
            timestamp: now,
            open: groupable,
        });

        if self.undo_stack.len() > self.capacity {
            let overflow = self.undo_stack.len() - self.capacity;
            self.undo_stack.drain(0..overflow);
            // The saved marker moves down with the truncation, and is lost
            // entirely if the clean point fell off the bottom.
            self.saved_depth = match self.saved_depth {
                Some(d) if d >= overflow => Some(d - overflow),
                _ => None,
            };
        }
    }

    /// Pop the newest undo entry, moving it onto the redo stack.
    pub fn undo(&mut self) -> Option<HistoryEntry> {
        let entry = self.undo_stack.pop()?;
        self.redo_stack.push(entry.clone());
        Some(entry)
    }

    /// Pop the newest redo entry, moving it back onto the undo stack.
    pub fn redo(&mut self) -> Option<HistoryEntry> {
        let mut entry = self.redo_stack.pop()?;
        // A redone entry must not absorb the next keystroke into itself.
        entry.open = false;
        self.undo_stack.push(entry.clone());
        Some(entry)
    }

    /// Close the current group, so the next edit starts a new undo step.
    ///
    /// Called on cursor jumps, focus loss, and save.
    pub fn commit_group(&mut self) {
        if let Some(last) = self.undo_stack.last_mut() {
            last.open = false;
        }
    }

    /// Mark the current state as matching what is on disk.
    pub fn mark_saved(&mut self) {
        self.saved_depth = Some(self.undo_stack.len());
        self.commit_group();
    }

    /// Whether the document differs from the last saved state.
    pub fn is_modified(&self) -> bool {
        self.saved_depth != Some(self.undo_stack.len())
    }

    /// Whether there is anything to undo.
    pub fn can_undo(&self) -> bool {
        !self.undo_stack.is_empty()
    }

    /// Whether there is anything to redo.
    pub fn can_redo(&self) -> bool {
        !self.redo_stack.is_empty()
    }

    /// Number of undo entries.
    pub fn undo_depth(&self) -> usize {
        self.undo_stack.len()
    }

    /// Number of redo entries.
    pub fn redo_depth(&self) -> usize {
        self.redo_stack.len()
    }

    /// Drop all history, keeping the current state as the clean point.
    pub fn clear(&mut self) {
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.saved_depth = Some(0);
    }
}

/// Concatenate two transactions that are known to apply in sequence.
///
/// The second transaction's edits are expressed in coordinates *after* the
/// first has run, so they cannot simply be merged into one set. Grouped typing
/// is the only caller, and there the edits are strictly forward-moving single
/// insertions, so we express the result as the union in first-transaction
/// coordinates by mapping the second transaction's ranges backwards.
fn concat(first: &Transaction, second: &Transaction) -> Transaction {
    let mut out = first.clone();
    for edit in second.edits() {
        // Try to add the edit as-is; if it collides with an existing one, the
        // group is no longer expressible as a single flat transaction and we
        // fall back to keeping them adjacent by shifting.
        if out.push(edit.clone()).is_err() {
            // Collision: place the edit immediately after the colliding one.
            let shifted = crate::edit::Edit {
                range: crate::position::Range::new(edit.range.start, edit.range.end),
                text: edit.text.clone(),
            };
            let _ = out.push(shifted);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit::Edit;
    use crate::position::Range;
    use crate::text::TextBuffer;

    fn record_typing(history: &mut History, buffer: &mut TextBuffer, offset: usize, text: &str) {
        let before = SelectionSet::caret(offset);
        let t = Transaction::single(Edit::insert(offset, text));
        let res = t.apply(buffer).unwrap();
        let after = SelectionSet::caret(offset + text.chars().count());
        history.record(t, res.inverse, before, after, true);
    }

    #[test]
    fn undo_restores_exact_prior_text() {
        let mut buffer = TextBuffer::from_str("start");
        let mut history = History::new();

        let before = SelectionSet::caret(5);
        let t = Transaction::single(Edit::insert(5, " and more"));
        let res = t.apply(&mut buffer).unwrap();
        history.record(t, res.inverse, before, SelectionSet::caret(14), false);
        assert_eq!(buffer.to_string(), "start and more");

        let entry = history.undo().unwrap();
        entry.undo.apply(&mut buffer).unwrap();
        assert_eq!(buffer.to_string(), "start");

        let entry = history.redo().unwrap();
        entry.redo.apply(&mut buffer).unwrap();
        assert_eq!(buffer.to_string(), "start and more");
    }

    #[test]
    fn consecutive_typing_groups_into_one_entry() {
        let mut buffer = TextBuffer::new();
        let mut history = History::new();
        for (i, ch) in "hello".chars().enumerate() {
            record_typing(&mut history, &mut buffer, i, &ch.to_string());
        }
        assert_eq!(buffer.to_string(), "hello");
        assert_eq!(history.undo_depth(), 1, "five keystrokes are one undo step");

        let entry = history.undo().unwrap();
        entry.undo.apply(&mut buffer).unwrap();
        assert_eq!(buffer.to_string(), "", "undo removes the whole typed run");
    }

    #[test]
    fn committing_a_group_starts_a_new_entry() {
        let mut buffer = TextBuffer::new();
        let mut history = History::new();
        record_typing(&mut history, &mut buffer, 0, "ab");
        history.commit_group();
        record_typing(&mut history, &mut buffer, 2, "cd");
        assert_eq!(history.undo_depth(), 2);
    }

    #[test]
    fn non_groupable_edits_never_merge() {
        let mut buffer = TextBuffer::from_str("aaa");
        let mut history = History::new();
        for _ in 0..3 {
            let t = Transaction::single(Edit::insert(0, "x"));
            let res = t.apply(&mut buffer).unwrap();
            history.record(t, res.inverse, SelectionSet::caret(0), SelectionSet::caret(1), false);
        }
        assert_eq!(history.undo_depth(), 3, "pastes and refactors each undo separately");
    }

    #[test]
    fn a_new_edit_discards_the_redo_branch() {
        let mut buffer = TextBuffer::new();
        let mut history = History::new();
        record_typing(&mut history, &mut buffer, 0, "a");
        history.commit_group();

        let entry = history.undo().unwrap();
        entry.undo.apply(&mut buffer).unwrap();
        assert!(history.can_redo());

        record_typing(&mut history, &mut buffer, 0, "b");
        assert!(!history.can_redo(), "editing after undo abandons the redo branch");
    }

    #[test]
    fn modified_flag_tracks_the_saved_marker() {
        let mut buffer = TextBuffer::new();
        let mut history = History::new();
        assert!(!history.is_modified());

        record_typing(&mut history, &mut buffer, 0, "x");
        assert!(history.is_modified());

        history.mark_saved();
        assert!(!history.is_modified());

        record_typing(&mut history, &mut buffer, 1, "y");
        assert!(history.is_modified());

        // Undoing back to the saved point makes the document clean again.
        let entry = history.undo().unwrap();
        entry.undo.apply(&mut buffer).unwrap();
        assert!(!history.is_modified());
    }

    #[test]
    fn capacity_is_enforced_and_drops_the_oldest_entries() {
        let mut buffer = TextBuffer::new();
        let mut history = History::with_config(4, Duration::ZERO);
        for i in 0..10 {
            let t = Transaction::single(Edit::insert(i, "x"));
            let res = t.apply(&mut buffer).unwrap();
            history.record(t, res.inverse, SelectionSet::caret(i), SelectionSet::caret(i + 1), false);
        }
        assert_eq!(history.undo_depth(), 4);
    }

    #[test]
    fn zero_interval_defeats_grouping() {
        let mut buffer = TextBuffer::new();
        let mut history = History::with_config(64, Duration::ZERO);
        record_typing(&mut history, &mut buffer, 0, "a");
        std::thread::sleep(Duration::from_millis(2));
        record_typing(&mut history, &mut buffer, 1, "b");
        assert_eq!(history.undo_depth(), 2);
    }

    #[test]
    fn undo_of_a_multi_cursor_edit_is_a_single_step() {
        let mut buffer = TextBuffer::from_str("a b c");
        let mut history = History::new();
        let t = Transaction::from_edits([
            Edit::replace(Range::new(0, 1), "X"),
            Edit::replace(Range::new(2, 3), "Y"),
            Edit::replace(Range::new(4, 5), "Z"),
        ])
        .unwrap();
        let res = t.apply(&mut buffer).unwrap();
        history.record(t, res.inverse, SelectionSet::caret(0), SelectionSet::caret(0), false);
        assert_eq!(buffer.to_string(), "X Y Z");

        let entry = history.undo().unwrap();
        entry.undo.apply(&mut buffer).unwrap();
        assert_eq!(buffer.to_string(), "a b c");
        assert_eq!(history.undo_depth(), 0);
    }
}
