//! Atomic, invertible edits.
//!
//! A [`Transaction`] is the only way text changes in Nebula. It holds a set of
//! non-overlapping [`Edit`]s expressed in the coordinates of the document
//! *before* the transaction runs, which means:
//!
//! * multi-cursor edits are naturally expressible — no offset bookkeeping in
//!   the caller;
//! * applying is atomic — the whole set is validated before anything mutates;
//! * undo is exact — applying produces the inverse transaction as a by-product,
//!   so history never has to re-derive what changed.

use smallvec::SmallVec;

use crate::position::Range;
use crate::selection::SelectionSet;
use crate::text::TextBuffer;
use crate::{CoreError, Result};

/// A single replacement: swap the text in `range` for `text`.
///
/// An insert is a replacement over an empty range; a delete is a replacement
/// with empty text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    /// The characters to replace, in pre-transaction coordinates.
    pub range: Range,
    /// The text to put in their place.
    pub text: String,
}

impl Edit {
    /// Replace `range` with `text`.
    pub fn replace(range: Range, text: impl Into<String>) -> Self {
        Self { range, text: text.into() }
    }

    /// Insert `text` at `offset`.
    pub fn insert(offset: usize, text: impl Into<String>) -> Self {
        Self { range: Range::empty(offset), text: text.into() }
    }

    /// Delete the characters in `range`.
    pub fn delete(range: Range) -> Self {
        Self { range, text: String::new() }
    }

    /// How much longer (or, if negative, shorter) the document gets.
    #[inline]
    pub fn char_delta(&self) -> isize {
        self.text.chars().count() as isize - self.range.len() as isize
    }

    /// Whether this edit changes nothing.
    #[inline]
    pub fn is_noop(&self) -> bool {
        self.range.is_empty() && self.text.is_empty()
    }
}

/// A set of edits applied as one atomic unit.
///
/// Edits are kept sorted by start offset and are guaranteed non-overlapping;
/// [`Transaction::push`] rejects overlaps, which is what makes the inverse
/// well-defined.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Transaction {
    edits: SmallVec<[Edit; 4]>,
}

/// What applying a [`Transaction`] produced.
#[derive(Debug, Clone)]
pub struct TransactionResult {
    /// The transaction that undoes what was just applied.
    pub inverse: Transaction,
    /// Net change in document length, in characters.
    pub char_delta: isize,
    /// The smallest range in *post*-edit coordinates containing every change,
    /// or `None` if the transaction was empty. The renderer uses this to decide
    /// how much of the screen it must repaint, and the syntax layer uses it to
    /// bound its re-parse.
    pub changed: Option<Range>,
}

impl Transaction {
    /// An empty transaction.
    pub fn new() -> Self {
        Self::default()
    }

    /// A transaction containing a single edit.
    pub fn single(edit: Edit) -> Self {
        let mut t = Self::new();
        t.edits.push(edit);
        t
    }

    /// Build a transaction from an iterator of edits, validating as it goes.
    pub fn from_edits(edits: impl IntoIterator<Item = Edit>) -> Result<Self> {
        let mut t = Self::new();
        for edit in edits {
            t.push(edit)?;
        }
        Ok(t)
    }

    /// Add an edit, keeping the set sorted and non-overlapping.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::InvertedRange`] if the new edit overlaps one that is
    /// already present — two edits touching the same character have no
    /// well-defined combined meaning, and silently picking one would corrupt
    /// undo.
    pub fn push(&mut self, edit: Edit) -> Result<()> {
        if edit.is_noop() {
            return Ok(());
        }
        let idx = self.edits.partition_point(|e| e.range.start < edit.range.start);
        if let Some(prev) = idx.checked_sub(1).and_then(|i| self.edits.get(i))
            && prev.range.overlaps(&edit.range)
        {
            return Err(CoreError::InvertedRange { start: edit.range.start, end: prev.range.end });
        }
        if let Some(next) = self.edits.get(idx)
            && next.range.overlaps(&edit.range)
        {
            return Err(CoreError::InvertedRange { start: next.range.start, end: edit.range.end });
        }
        self.edits.insert(idx, edit);
        Ok(())
    }

    /// The edits, sorted ascending by start offset.
    #[inline]
    pub fn edits(&self) -> &[Edit] {
        &self.edits
    }

    /// Whether the transaction would change nothing.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.edits.is_empty()
    }

    /// Number of edits.
    #[inline]
    pub fn len(&self) -> usize {
        self.edits.len()
    }

    /// Apply the transaction to `buffer`, returning its inverse.
    ///
    /// Edits are applied back-to-front so that each edit's pre-transaction
    /// coordinates remain valid at the moment it runs. The whole set is bounds
    /// checked first: on error the buffer is untouched.
    pub fn apply(&self, buffer: &mut TextBuffer) -> Result<TransactionResult> {
        // Validate everything before mutating anything, so a failure can never
        // leave the buffer half-edited.
        let len = buffer.len_chars();
        for edit in &self.edits {
            if edit.range.end > len {
                return Err(CoreError::OffsetOutOfBounds { offset: edit.range.end, len });
            }
        }

        if self.edits.is_empty() {
            return Ok(TransactionResult {
                inverse: Transaction::new(),
                char_delta: 0,
                changed: None,
            });
        }

        // Build the inverse while we still have the pre-edit text available.
        let mut inverse_edits: SmallVec<[Edit; 4]> = SmallVec::new();
        let mut shift: isize = 0;
        let mut changed_start = usize::MAX;
        let mut changed_end = 0usize;

        for edit in &self.edits {
            let removed = buffer.slice(edit.range)?;
            let inserted_len = edit.text.chars().count();
            // Where this edit's replacement text lands once every preceding edit
            // has been applied.
            let post_start = (edit.range.start as isize + shift) as usize;
            let post_end = post_start + inserted_len;
            inverse_edits.push(Edit {
                range: Range { start: post_start, end: post_end },
                text: removed,
            });
            changed_start = changed_start.min(post_start);
            changed_end = changed_end.max(post_end);
            shift += edit.char_delta();
        }

        // Apply back-to-front: later edits do not disturb earlier coordinates.
        for edit in self.edits.iter().rev() {
            buffer.replace(edit.range, &edit.text)?;
        }

        Ok(TransactionResult {
            inverse: Transaction { edits: inverse_edits },
            char_delta: shift,
            changed: Some(Range { start: changed_start, end: changed_end }),
        })
    }

    /// Map an offset from pre-transaction to post-transaction coordinates.
    ///
    /// `bias_after` decides what happens to an offset sitting exactly at an
    /// edit's start: `true` pushes it past the inserted text (the behaviour you
    /// want for a cursor that typed the text), `false` keeps it before.
    pub fn map_offset(&self, offset: usize, bias_after: bool) -> usize {
        let mut shift: isize = 0;
        for edit in &self.edits {
            if edit.range.start > offset {
                // Edits are sorted, so nothing from here on can affect us.
                break;
            }
            let base = || (edit.range.start as isize + shift).max(0) as usize;

            if edit.range.start == offset && edit.range.is_empty() {
                // A pure insertion exactly at the offset is the one genuinely
                // ambiguous case: the cursor that typed the text rides forward
                // with it, an anchor pinned before it does not.
                let at = base();
                return if bias_after { at + edit.text.chars().count() } else { at };
            }
            if edit.range.end <= offset {
                // Entirely before us: shift by its delta.
                shift += edit.char_delta();
            } else {
                // start <= offset < end: the offset sat inside replaced text and
                // no longer exists. Collapse it to the start of the replacement;
                // a caret whose own selection was replaced is moved to the end
                // by the `range.end` case above on its other endpoint.
                return base();
            }
        }
        (offset as isize + shift).max(0) as usize
    }

    /// Map a range through the transaction.
    pub fn map_range(&self, range: Range) -> Range {
        Range::new(self.map_offset(range.start, false), self.map_offset(range.end, true))
    }

    /// Map a whole selection set through the transaction.
    pub fn map_selections(&self, selections: &SelectionSet) -> SelectionSet {
        let mapped = selections.iter().map(|sel| {
            let anchor = self.map_offset(sel.anchor, true);
            let head = self.map_offset(sel.head, true);
            crate::selection::Selection { anchor, head, desired_column: None }
        });
        SelectionSet::from_iter(mapped)
    }
}

impl FromIterator<Edit> for Transaction {
    /// Collect edits, silently dropping any that overlap one already collected.
    ///
    /// Use [`Transaction::from_edits`] when an overlap should be an error.
    fn from_iter<I: IntoIterator<Item = Edit>>(iter: I) -> Self {
        let mut t = Transaction::new();
        for edit in iter {
            let _ = t.push(edit);
        }
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn buf(s: &str) -> TextBuffer {
        TextBuffer::from_str(s)
    }

    #[test]
    fn single_edit_applies_and_inverts() {
        let mut b = buf("hello world");
        let t = Transaction::single(Edit::replace(Range::new(6, 11), "nebula"));
        let res = t.apply(&mut b).unwrap();
        assert_eq!(b.to_string(), "hello nebula");
        assert_eq!(res.char_delta, 1);
        assert_eq!(res.changed, Some(Range::new(6, 12)));

        res.inverse.apply(&mut b).unwrap();
        assert_eq!(b.to_string(), "hello world");
    }

    #[test]
    fn multi_cursor_edits_apply_in_one_atomic_step() {
        let mut b = buf("aaa bbb ccc");
        let t = Transaction::from_edits([
            Edit::replace(Range::new(0, 3), "XXXX"),
            Edit::replace(Range::new(4, 7), "Y"),
            Edit::replace(Range::new(8, 11), "ZZ"),
        ])
        .unwrap();
        let res = t.apply(&mut b).unwrap();
        assert_eq!(b.to_string(), "XXXX Y ZZ");
        assert_eq!(res.char_delta, -2);

        res.inverse.apply(&mut b).unwrap();
        assert_eq!(b.to_string(), "aaa bbb ccc", "inverse restores exactly");
    }

    #[test]
    fn overlapping_edits_are_rejected() {
        let mut t = Transaction::new();
        t.push(Edit::replace(Range::new(0, 5), "a")).unwrap();
        let err = t.push(Edit::replace(Range::new(3, 8), "b"));
        assert!(err.is_err(), "overlapping edits have no well-defined meaning");
    }

    #[test]
    fn adjacent_edits_are_allowed() {
        let mut b = buf("abcdef");
        let t = Transaction::from_edits([
            Edit::replace(Range::new(0, 3), "X"),
            Edit::replace(Range::new(3, 6), "Y"),
        ])
        .unwrap();
        t.apply(&mut b).unwrap();
        assert_eq!(b.to_string(), "XY");
    }

    #[test]
    fn out_of_bounds_transaction_leaves_the_buffer_untouched() {
        let mut b = buf("short");
        let t = Transaction::from_edits([
            Edit::replace(Range::new(0, 2), "OK"),
            Edit::replace(Range::new(10, 20), "BAD"),
        ])
        .unwrap();
        assert!(t.apply(&mut b).is_err());
        assert_eq!(b.to_string(), "short", "validation happens before mutation");
    }

    #[test]
    fn offsets_map_across_an_insertion() {
        // "abc" -> insert "XY" at 1 -> "aXYbc"
        let t = Transaction::single(Edit::insert(1, "XY"));
        assert_eq!(t.map_offset(0, true), 0);
        assert_eq!(t.map_offset(1, true), 3, "cursor at the insert point rides forward");
        assert_eq!(t.map_offset(1, false), 1, "an anchor before it stays put");
        assert_eq!(t.map_offset(2, true), 4);
    }

    #[test]
    fn offsets_inside_a_deletion_collapse() {
        // "abcdef" -> delete [1,4) -> "aef"
        let t = Transaction::single(Edit::delete(Range::new(1, 4)));
        assert_eq!(t.map_offset(0, true), 0);
        assert_eq!(t.map_offset(2, true), 1, "an offset inside deleted text collapses to its start");
        assert_eq!(t.map_offset(4, true), 1);
        assert_eq!(t.map_offset(5, true), 2);
    }

    #[test]
    fn empty_transaction_is_a_noop_with_no_changed_range() {
        let mut b = buf("unchanged");
        let res = Transaction::new().apply(&mut b).unwrap();
        assert_eq!(b.to_string(), "unchanged");
        assert_eq!(res.changed, None);
        assert_eq!(res.char_delta, 0);
    }

    #[test]
    fn noop_edits_are_dropped_on_push() {
        let mut t = Transaction::new();
        t.push(Edit::insert(0, "")).unwrap();
        assert!(t.is_empty());
    }

    #[test]
    fn multibyte_text_deltas_are_counted_in_chars() {
        let mut b = buf("ab");
        let t = Transaction::single(Edit::insert(1, "🌌🌌"));
        let res = t.apply(&mut b).unwrap();
        assert_eq!(res.char_delta, 2, "two chars, eight bytes");
        assert_eq!(b.len_chars(), 4);
        res.inverse.apply(&mut b).unwrap();
        assert_eq!(b.to_string(), "ab");
    }
}
