//! Cursors and selections.
//!
//! A cursor is just an empty selection, which keeps the multi-cursor code path
//! and the single-cursor code path identical — there is no special case for
//! "no selection" anywhere above this module.

use smallvec::SmallVec;

use crate::position::Range;
use crate::text::TextBuffer;

/// One cursor and its selection.
///
/// `anchor` is the end that stays put when the selection is extended; `head` is
/// the end that moves and where the caret is drawn. `head < anchor` is a
/// perfectly normal backwards selection, not an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    /// The fixed end of the selection.
    pub anchor: usize,
    /// The moving end — where the caret is drawn.
    pub head: usize,
    /// Column the cursor "wants" during vertical motion.
    ///
    /// Moving down from a long line through a short one and back must return to
    /// the original column; that only works if the desired column survives the
    /// trip through the short line.
    pub desired_column: Option<usize>,
}

impl Selection {
    /// A caret (empty selection) at `offset`.
    #[inline]
    pub const fn caret(offset: usize) -> Self {
        Self { anchor: offset, head: offset, desired_column: None }
    }

    /// A selection from `anchor` to `head`.
    #[inline]
    pub const fn new(anchor: usize, head: usize) -> Self {
        Self { anchor, head, desired_column: None }
    }

    /// The covered range, normalised so `start <= end`.
    #[inline]
    pub fn range(&self) -> Range {
        Range::new(self.anchor, self.head)
    }

    /// Lower offset of the selection.
    #[inline]
    pub fn start(&self) -> usize {
        self.anchor.min(self.head)
    }

    /// Upper offset of the selection.
    #[inline]
    pub fn end(&self) -> usize {
        self.anchor.max(self.head)
    }

    /// Whether this is a bare caret with nothing selected.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    /// Number of characters selected.
    #[inline]
    pub fn len(&self) -> usize {
        self.end() - self.start()
    }

    /// Whether the head is before the anchor.
    #[inline]
    pub fn is_reversed(&self) -> bool {
        self.head < self.anchor
    }

    /// Collapse to a caret at the head.
    #[inline]
    pub fn collapse(&self) -> Selection {
        Selection::caret(self.head)
    }

    /// Move the head to `offset`, keeping the anchor (i.e. extend).
    #[inline]
    pub fn extend_to(&self, offset: usize) -> Selection {
        Selection { anchor: self.anchor, head: offset, desired_column: None }
    }

    /// Clamp both ends into `buffer`.
    #[inline]
    pub fn clamped(&self, buffer: &TextBuffer) -> Selection {
        Selection {
            anchor: buffer.clamp_offset(self.anchor),
            head: buffer.clamp_offset(self.head),
            desired_column: self.desired_column,
        }
    }
}

impl From<Range> for Selection {
    fn from(r: Range) -> Self {
        Selection::new(r.start, r.end)
    }
}

/// A non-empty, sorted, non-overlapping set of selections.
///
/// The invariants are maintained by construction: every constructor and mutator
/// funnels through one private normalisation step, which sorts by start offset
/// and merges any selections that touch. Nothing above this type ever has to
/// defend against two cursors landing on the same character.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionSet {
    /// Invariant: non-empty, sorted by `start()`, pairwise non-touching.
    selections: SmallVec<[Selection; 2]>,
    /// Index of the selection that receives "primary" operations such as
    /// centring the viewport. Always in bounds.
    primary: usize,
}

impl Default for SelectionSet {
    fn default() -> Self {
        Self::single(Selection::caret(0))
    }
}

impl SelectionSet {
    /// A set containing exactly one selection.
    pub fn single(selection: Selection) -> Self {
        Self { selections: SmallVec::from_elem(selection, 1), primary: 0 }
    }

    /// A set containing exactly one caret at `offset`.
    pub fn caret(offset: usize) -> Self {
        Self::single(Selection::caret(offset))
    }

    /// Build from an iterator, normalising the result.
    ///
    /// An empty iterator yields a single caret at offset 0 — the type never
    /// represents "no cursors", because an editor with no cursor has no
    /// meaningful response to a keystroke.
    // FromIterator is implemented too; this is the inherent form, kept because it reads better at call sites.
    #[allow(clippy::should_implement_trait)]
    pub fn from_iter(iter: impl IntoIterator<Item = Selection>) -> Self {
        let mut selections: SmallVec<[Selection; 2]> = iter.into_iter().collect();
        if selections.is_empty() {
            selections.push(Selection::caret(0));
        }
        let mut set = Self { selections, primary: 0 };
        set.normalize();
        set
    }

    /// The selections, sorted ascending.
    #[inline]
    pub fn iter(&self) -> impl Iterator<Item = &Selection> {
        self.selections.iter()
    }

    /// Number of selections. Always at least 1.
    #[inline]
    pub fn len(&self) -> usize {
        self.selections.len()
    }

    /// Always false — a selection set is never empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        false
    }

    /// The primary selection, which drives viewport scrolling and status display.
    #[inline]
    pub fn primary(&self) -> &Selection {
        &self.selections[self.primary]
    }

    /// Mutable access to the primary selection.
    #[inline]
    pub fn primary_mut(&mut self) -> &mut Selection {
        &mut self.selections[self.primary]
    }

    /// Index of the primary selection.
    #[inline]
    pub fn primary_index(&self) -> usize {
        self.primary
    }

    /// Select which member is primary. Out-of-range indices are ignored.
    pub fn set_primary(&mut self, index: usize) {
        if index < self.selections.len() {
            self.primary = index;
        }
    }

    /// All covered ranges, sorted.
    pub fn ranges(&self) -> impl Iterator<Item = Range> + '_ {
        self.selections.iter().map(|s| s.range())
    }

    /// Add a selection, re-normalising.
    ///
    /// The newly added selection becomes primary — matching the universal
    /// editor convention that the cursor you just created is the one you're
    /// working with.
    pub fn push(&mut self, selection: Selection) {
        self.selections.push(selection);
        let target = selection.start();
        self.normalize();
        self.primary = self
            .selections
            .iter()
            .position(|s| s.range().contains_inclusive(target))
            .unwrap_or(self.selections.len() - 1);
    }

    /// Collapse every selection to a caret at its head.
    pub fn collapse(&mut self) {
        for sel in &mut self.selections {
            *sel = sel.collapse();
        }
        self.normalize();
    }

    /// Discard every selection except the primary one.
    pub fn keep_primary_only(&mut self) {
        let primary = self.selections[self.primary];
        self.selections.clear();
        self.selections.push(primary);
        self.primary = 0;
    }

    /// Replace the whole set with a single selection.
    pub fn replace_with(&mut self, selection: Selection) {
        self.selections.clear();
        self.selections.push(selection);
        self.primary = 0;
    }

    /// Apply `f` to every selection, then re-normalise.
    pub fn transform(&mut self, mut f: impl FnMut(&Selection) -> Selection) {
        let primary_start = self.selections[self.primary].start();
        for sel in &mut self.selections {
            *sel = f(sel);
        }
        self.normalize();
        // Keep the primary pointing at whichever selection swallowed the old one.
        self.primary = self
            .selections
            .iter()
            .position(|s| s.range().contains_inclusive(primary_start))
            .unwrap_or(0)
            .min(self.selections.len() - 1);
    }

    /// Clamp every selection into `buffer`.
    pub fn clamp(&mut self, buffer: &TextBuffer) {
        self.transform(|s| s.clamped(buffer));
    }

    /// Sort by start offset and merge touching selections.
    ///
    /// Merging preserves direction: if either input was reversed the merged
    /// selection is reversed, so extending a backwards selection into another
    /// one does not silently flip the caret to the far end.
    fn normalize(&mut self) {
        debug_assert!(!self.selections.is_empty());
        self.selections.sort_by_key(|s| (s.start(), s.end()));

        let mut merged: SmallVec<[Selection; 2]> = SmallVec::new();
        for sel in self.selections.drain(..) {
            match merged.last_mut() {
                Some(prev) if prev.range().touches(&sel.range()) => {
                    let start = prev.start().min(sel.start());
                    let end = prev.end().max(sel.end());
                    let reversed = prev.is_reversed() || sel.is_reversed();
                    *prev = if reversed {
                        Selection { anchor: end, head: start, desired_column: None }
                    } else {
                        Selection { anchor: start, head: end, desired_column: None }
                    };
                }
                _ => merged.push(sel),
            }
        }
        self.selections = merged;
        if self.primary >= self.selections.len() {
            self.primary = self.selections.len() - 1;
        }
    }
}

impl FromIterator<Selection> for SelectionSet {
    fn from_iter<I: IntoIterator<Item = Selection>>(iter: I) -> Self {
        SelectionSet::from_iter(iter)
    }
}

impl std::ops::Index<usize> for SelectionSet {
    type Output = Selection;
    fn index(&self, index: usize) -> &Selection {
        &self.selections[index]
    }
}

impl<'a> IntoIterator for &'a SelectionSet {
    type Item = &'a Selection;
    type IntoIter = std::slice::Iter<'a, Selection>;
    fn into_iter(self) -> Self::IntoIter {
        self.selections.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_caret_is_an_empty_selection() {
        let s = Selection::caret(7);
        assert!(s.is_empty());
        assert_eq!(s.range(), Range::new(7, 7));
        assert_eq!(s.len(), 0);
    }

    #[test]
    fn reversed_selections_report_sorted_bounds() {
        let s = Selection::new(10, 4);
        assert!(s.is_reversed());
        assert_eq!(s.start(), 4);
        assert_eq!(s.end(), 10);
        assert_eq!(s.range(), Range::new(4, 10));
    }

    #[test]
    fn set_is_sorted_on_construction() {
        let set = SelectionSet::from_iter([
            Selection::new(30, 40),
            Selection::new(0, 5),
            Selection::new(10, 15),
        ]);
        let starts: Vec<_> = set.iter().map(|s| s.start()).collect();
        assert_eq!(starts, vec![0, 10, 30]);
    }

    #[test]
    fn touching_selections_merge() {
        let set = SelectionSet::from_iter([Selection::new(0, 5), Selection::new(5, 9)]);
        assert_eq!(set.len(), 1);
        assert_eq!(set[0].range(), Range::new(0, 9));
    }

    #[test]
    fn overlapping_selections_merge() {
        let set = SelectionSet::from_iter([Selection::new(0, 8), Selection::new(4, 12)]);
        assert_eq!(set.len(), 1);
        assert_eq!(set[0].range(), Range::new(0, 12));
    }

    #[test]
    fn merging_preserves_reversed_direction() {
        let set = SelectionSet::from_iter([Selection::new(8, 0), Selection::new(4, 12)]);
        assert_eq!(set.len(), 1);
        assert!(set[0].is_reversed(), "caret stays at the low end it was dragged to");
        assert_eq!(set[0].range(), Range::new(0, 12));
    }

    #[test]
    fn disjoint_selections_are_kept_apart() {
        let set = SelectionSet::from_iter([Selection::new(0, 4), Selection::new(6, 9)]);
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn an_empty_iterator_still_yields_one_cursor() {
        let set = SelectionSet::from_iter(std::iter::empty());
        assert_eq!(set.len(), 1);
        assert_eq!(set.primary(), &Selection::caret(0));
    }

    #[test]
    fn push_makes_the_new_selection_primary() {
        let mut set = SelectionSet::caret(0);
        set.push(Selection::new(20, 25));
        assert_eq!(set.len(), 2);
        assert_eq!(set.primary().range(), Range::new(20, 25));
    }

    #[test]
    fn keep_primary_only_drops_the_rest() {
        let mut set = SelectionSet::from_iter([
            Selection::new(0, 2),
            Selection::new(10, 12),
            Selection::new(20, 22),
        ]);
        set.set_primary(1);
        set.keep_primary_only();
        assert_eq!(set.len(), 1);
        assert_eq!(set[0].range(), Range::new(10, 12));
    }

    #[test]
    fn collapse_leaves_one_caret_per_selection_at_its_head() {
        let mut set = SelectionSet::from_iter([Selection::new(0, 3), Selection::new(9, 5)]);
        set.collapse();
        assert_eq!(set.len(), 2);
        assert_eq!(set[0], Selection::caret(3));
        assert_eq!(set[1], Selection::caret(5), "a reversed selection collapses to its head");
    }

    #[test]
    fn selections_sharing_a_head_are_already_merged_before_collapse() {
        // Two selections that end at the same offset necessarily touch, so the
        // merge happens at construction — collapse never sees the duplicate.
        let set = SelectionSet::from_iter([Selection::new(0, 5), Selection::new(7, 5)]);
        assert_eq!(set.len(), 1);
        assert_eq!(set[0].range(), Range::new(0, 7));
    }

    #[test]
    fn clamp_pulls_selections_inside_the_buffer() {
        let buffer = TextBuffer::from_str("0123456789");
        let mut set = SelectionSet::from_iter([Selection::new(5, 99)]);
        set.clamp(&buffer);
        assert_eq!(set[0].range(), Range::new(5, 10));
    }
}
